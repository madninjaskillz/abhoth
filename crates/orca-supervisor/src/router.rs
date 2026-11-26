//! The router side: polls the router's admin API (`GET /admin/api/state`, `/leases`) into the
//! `engines` and `leases` feed sections, and implements orca-homeassistant's `ControlPort`
//! over it (layout switch, card online/offline) and over hostd (clock caps). Every control goes
//! through the router's own guards, calendar and audit; the supervisor never bypasses them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use orca_homeassistant::{ControlOutcome, ControlPort, FanMode, ServicePrincipal};
use orca_hostd::protocol::{FromHostd, Override, ToHostd};
use orca_sensors::feed::{TelemetrySink, sections};
use serde_json::{Map, Value, json};

use crate::hostd::HostdClient;

pub struct Router {
    base: String,
    key: String,
    server: String,
    http: reqwest::Client,
    /// Card label → router card id, from the last state.
    card_ids: Mutex<BTreeMap<String, String>>,
}

impl Router {
    pub fn new(base: &str, key: String, server: &str) -> Router {
        Router {
            base: base.trim_end_matches('/').to_string(),
            key,
            server: server.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            card_ids: Mutex::new(BTreeMap::new()),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let r = self
            .http
            .get(format!("{}/admin/api/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let st = r.status();
        let v: Value = r.json().await.map_err(|e| e.to_string())?;
        if !st.is_success() {
            return Err(format!("{st}: {}", v["error"].as_str().unwrap_or("")));
        }
        Ok(v)
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        match self
            .http
            .post(format!("{}/admin/api/{path}", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => {
                let st = r.status().as_u16();
                (st, r.json().await.unwrap_or(Value::Null))
            }
            Err(e) => (0, json!({"error": e.to_string()})),
        }
    }

    /// One poll: the `engines` and `leases` sections.
    pub async fn poll(&self, sink: &dyn TelemetrySink) -> Result<(), String> {
        let st = self.get("state").await?;
        let (engines, ids) = engines_section(&st, &self.server);
        *self.card_ids.lock().unwrap_or_else(|e| e.into_inner()) = ids;
        sink.set_section(sections::ENGINES, engines);
        if let Ok(l) = self.get("leases").await {
            let arr = match l {
                Value::Array(a) => Value::Array(a),
                Value::Object(mut m) => m
                    .remove("leases")
                    .filter(Value::is_array)
                    .unwrap_or(json!([])),
                _ => json!([]),
            };
            sink.set_section(sections::LEASES, strip_nulls(arr));
        }
        // The router's calendar (absent or off: no section, nothing muted).
        if let Ok(n) = self.get("calendar/now").await {
            sink.set_section(sections::CALENDAR, calendar_section(&n));
        }
        Ok(())
    }

    pub fn spawn_poller(
        self: &Arc<Self>,
        sink: Arc<dyn TelemetrySink>,
        every: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                if let Err(e) = me.poll(sink.as_ref()).await {
                    tracing::warn!("router state: {e}");
                }
            }
        })
    }

    fn card_id(&self, label: &str) -> String {
        self.card_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(label)
            .cloned()
            .unwrap_or_else(|| label.to_string())
    }
}

/// A field by its snake_case name, or the camelCase spelling the live router sends.
fn field<'a>(v: &'a Value, snake: &str) -> &'a Value {
    let x = &v[snake];
    if !x.is_null() {
        return x;
    }
    let mut camel = String::new();
    let mut up = false;
    for ch in snake.chars() {
        if ch == '_' {
            up = true;
        } else if up {
            camel.extend(ch.to_uppercase());
            up = false;
        } else {
            camel.push(ch);
        }
    }
    &v[camel.as_str()]
}

fn strip_nulls(mut v: Value) -> Value {
    fn go(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.retain(|_, x| !x.is_null());
                m.values_mut().for_each(go);
            }
            Value::Array(a) => a.iter_mut().for_each(go),
            _ => {}
        }
    }
    go(&mut v);
    v
}

/// The router's `StateView` → the `engines` section (orca-sensors `sections::ENGINES` shape),
/// plus label → router card id.
pub fn engines_section(st: &Value, server: &str) -> (Value, BTreeMap<String, String>) {
    let servers = st["servers"].as_array().cloned().unwrap_or_default();
    let srv = servers
        .iter()
        .find(|s| s["id"] == server)
        .or_else(|| servers.first())
        .cloned()
        .unwrap_or(Value::Null);
    let sid = srv["id"].as_str().unwrap_or(server).to_string();
    let mut out = Map::new();
    let active = field(&srv, "active_layout").as_str().map(String::from);
    let layouts: Vec<&Value> = st["layouts"]
        .as_array()
        .map(|a| a.iter().filter(|l| l["server"] == sid.as_str()).collect())
        .unwrap_or_default();
    if let Some(a) = &active {
        out.insert("layout".into(), json!(a));
        if let Some(l) = layouts.iter().find(|l| l["id"] == a.as_str()) {
            out.insert("layout_title".into(), l["title"].clone());
        }
    }
    out.insert(
        "layouts".into(),
        json!(
            layouts
                .iter()
                .filter_map(|l| l["id"].as_str())
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "switch_state".into(),
        json!(if st["job"].is_object() {
            "switching"
        } else {
            "idle"
        }),
    );
    if st["job"].is_object() {
        out.insert("last_switch".into(), st["job"].clone());
    }
    let mut cards = Map::new();
    let mut engines = Map::new();
    let mut ids = BTreeMap::new();
    let (mut busy, mut total) = (0u64, 0u64);
    for c in srv["cards"].as_array().cloned().unwrap_or_default() {
        // The card's short name: the router id without a "server/" prefix ("A"), else the label.
        let label = c["id"]
            .as_str()
            .map(|id| id.rsplit('/').next().unwrap_or(id))
            .filter(|s| !s.is_empty())
            .or(c["label"].as_str())
            .unwrap_or("")
            .to_string();
        if label.is_empty() {
            continue;
        }
        ids.insert(
            label.clone(),
            c["id"].as_str().unwrap_or(&label).to_string(),
        );
        let e = &c["engine"];
        let mut m = Map::new();
        if let Some(t) = c["label"].as_str() {
            m.insert("title".into(), json!(t));
        }
        m.insert(
            "online".into(),
            json!(field(&c, "admin_state").as_str().unwrap_or("online") == "online"),
        );
        if e.is_object() {
            m.insert("engine".into(), e["engine"].clone());
            m.insert("state".into(), e["state"].clone());
            m.insert("model".into(), e["model"].clone());
            m.insert(
                "summary".into(),
                json!(
                    format!(
                        "{} {}",
                        e["engine"].as_str().unwrap_or(""),
                        e["model"].as_str().unwrap_or("")
                    )
                    .trim()
                ),
            );
            let eid = field(e, "served_id")
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| {
                    format!(
                        "{}-{}",
                        e["engine"].as_str().unwrap_or("engine"),
                        label.to_lowercase()
                    )
                });
            let ent = engines.entry(eid).or_insert_with(|| {
                json!({"title": e["model"], "kind": "llm", "cards": [], "state": e["state"], "healthy": e["healthy"]})
            });
            if let Some(a) = ent["cards"].as_array_mut() {
                a.push(json!(label));
            }
        } else {
            m.insert("engine".into(), json!("off"));
        }
        m.insert("running".into(), c["inflight"].clone());
        m.insert("queued".into(), c["queued"].clone());
        busy += c["inflight"].as_u64().unwrap_or(0);
        total += c["lanes"].as_u64().unwrap_or(0);
        cards.insert(label, Value::Object(m));
    }
    out.insert("slots_busy".into(), json!(busy));
    out.insert("slots_total".into(), json!(total));
    out.insert("cards".into(), Value::Object(cards));
    out.insert("engines".into(), Value::Object(engines));
    (strip_nulls(Value::Object(out)), ids)
}

/// The router's `calendar/now` (orca-calendar `NowState`) → the `calendar` section:
/// `{ active_event, active: [{entry, title, end}], block, blocks: [...], mutes: [{rules, entry,
/// until}] }` (times in epoch seconds). The alerts engine reads `mutes`.
pub fn calendar_section(n: &Value) -> Value {
    let arr = |k: &str| n[k].as_array().cloned().unwrap_or_default();
    let active: Vec<Value> = arr("active")
        .iter()
        .map(|o| json!({"entry": o["entry"], "title": o["title"], "end": o["end"]}))
        .collect();
    let blocks: Vec<Value> = arr("blocks")
        .iter()
        .map(|b| json!({"entry": b["entry"], "action": b["action"], "until": b["until"], "reason": b["reason"]}))
        .collect();
    // NowState.mutes is `[(selector, entry, until)]`, serialised as arrays.
    let mutes: Vec<Value> = arr("mutes")
        .iter()
        .filter_map(|m| {
            let m = m.as_array()?;
            Some(json!({"rules": m.first()?, "entry": m.get(1)?, "until": m.get(2)?}))
        })
        .collect();
    let mut out = json!({
        "block": !blocks.is_empty(),
        "active": active,
        "blocks": blocks,
        "mutes": mutes,
    });
    if let Some(e) = out["active"][0]["entry"].as_str().map(String::from) {
        out["active_event"] = json!(e);
    }
    strip_nulls(out)
}

/// The supervisor's controls (HA, later the UI): layout and card state through the router,
/// clock caps through hostd. Fan modes wait for the calendar (`fan_profile`).
pub struct Controls {
    pub router: Option<Arc<Router>>,
    pub hostd: HostdClient,
}

fn outcome(st: u16, v: &Value, ok: &str) -> ControlOutcome {
    match st {
        200..=299 => ControlOutcome::Accepted {
            message: ok.into(),
            job: v["id"].as_str().map(String::from),
        },
        409 if v["error"].as_str().is_some_and(|e| e.contains("already")) => {
            ControlOutcome::Already {
                message: v["error"].as_str().unwrap_or("").into(),
            }
        }
        0 => ControlOutcome::Refused {
            reason: format!("router unreachable: {}", v["error"].as_str().unwrap_or("")),
        },
        _ => ControlOutcome::Refused {
            reason: v["error"].as_str().unwrap_or("refused").to_string(),
        },
    }
}

#[async_trait]
impl ControlPort for Controls {
    async fn switch_layout(&self, who: &ServicePrincipal, layout: &str) -> ControlOutcome {
        let Some(r) = &self.router else {
            return ControlOutcome::Refused {
                reason: "no router configured".into(),
            };
        };
        let (st, v) = r
            .post(
                "switch",
                json!({"server": r.server, "layout": layout, "reason": format!("{who:?}")}),
            )
            .await;
        outcome(st, &v, &format!("switching to {layout}"))
    }

    async fn set_card_online(
        &self,
        who: &ServicePrincipal,
        card: &str,
        online: bool,
    ) -> ControlOutcome {
        let Some(r) = &self.router else {
            return ControlOutcome::Refused {
                reason: "no router configured".into(),
            };
        };
        let act = if online { "online" } else { "offline" };
        let (st, v) = r
            .post(
                &format!("cards/{}/{act}", r.card_id(card)),
                json!({"reason": format!("{who:?}")}),
            )
            .await;
        outcome(st, &v, &format!("card {card} {act}"))
    }

    async fn set_clock_cap(
        &self,
        _who: &ServicePrincipal,
        card: &str,
        mhz: Option<u32>,
        ttl: Duration,
    ) -> ControlOutcome {
        let o = match mhz {
            Some(m) => Override::ClockCap {
                card: card.into(),
                mhz: m,
                ttl_s: ttl.as_secs().max(1),
            },
            None => Override::Clear {
                fan: None,
                card: Some(card.into()),
            },
        };
        match self.hostd.call(ToHostd::Override(o)).await {
            Ok(FromHostd::Ack) => ControlOutcome::Accepted {
                message: match mhz {
                    Some(m) => format!("card {card} capped at {m} MHz for {} s", ttl.as_secs()),
                    None => format!("card {card} cap cleared"),
                },
                job: None,
            },
            Ok(FromHostd::Error { message, .. }) => ControlOutcome::Refused { reason: message },
            Ok(other) => ControlOutcome::Refused {
                reason: format!("unexpected hostd reply {other:?}"),
            },
            Err(e) => ControlOutcome::Refused { reason: e },
        }
    }

    async fn set_fan_mode(
        &self,
        _who: &ServicePrincipal,
        _mode: FanMode,
        _ttl: Option<Duration>,
    ) -> ControlOutcome {
        ControlOutcome::Refused {
            reason: "fan modes are not wired in this build (calendar fan_profile)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_to_engines_section() {
        let st = json!({
            "rev": "r", "mode": "on", "updated": 1, "guards": [],
            "job": null,
            "servers": [{"id": "gpu-host", "title": "G", "reachability": "ok", "exec": "local", "active_layout": "flash-x2", "drift": false, "hostctl_ok": true,
                "cards": [
                    {"id": "gpu-host/B", "server": "gpu-host", "gpu_uuid": "GPU-b", "label": "B", "managed": true, "admin_state": "online",
                     "engine": {"engine": "strata", "profile": "p", "model": "flash", "served_id": "flash-b", "state": "ready", "since": 1, "healthy": true},
                     "lanes": 4, "inflight": 1, "queued": 0, "leases": 0, "on_demand": false, "foreign": []},
                    {"id": "gpu-host/D", "server": "gpu-host", "gpu_uuid": "GPU-d", "label": "D", "managed": false, "admin_state": "offline",
                     "engine": null, "lanes": 0, "inflight": 0, "queued": 0, "leases": 0, "on_demand": false, "foreign": []}
                ]}],
            "layouts": [{"id": "flash-x2", "title": "Flash ×2", "server": "gpu-host", "mode": "m", "cards": {}, "active": true, "source": "s"},
                        {"id": "other", "title": "x", "server": "elsewhere", "mode": "m", "cards": {}, "active": false, "source": "s"}]
        });
        let (e, ids) = engines_section(&st, "gpu-host");
        assert_eq!(e["layout"], "flash-x2");
        assert_eq!(e["layout_title"], "Flash ×2");
        assert_eq!(e["layouts"], json!(["flash-x2"]));
        assert_eq!(e["cards"]["B"]["engine"], "strata");
        assert_eq!(e["cards"]["B"]["online"], true);
        assert_eq!(e["cards"]["D"]["online"], false);
        assert_eq!(e["engines"]["flash-b"]["cards"], json!(["B"]));
        assert_eq!(e["slots_busy"], 1);
        assert_eq!(ids["B"], "gpu-host/B");
        // And H's feed reads it.
        let snap = orca_homeassistant::feed::snapshot_from_state(
            &json!({"engines": e}),
            &Default::default(),
            &[orca_homeassistant::feed::CardConfig {
                id: "B".into(),
                allow_cap: true,
            }],
        );
        assert_eq!(snap.control.layout.as_deref(), Some("flash-x2"));
        assert_eq!(snap.card("B").unwrap().engine.as_deref(), Some("strata"));
    }

    #[test]
    fn calendar_now_to_section() {
        let n = json!({"at": 100, "active": [{"entry": "video", "id": "video@1", "title": "Video", "end": 900}],
            "blocks": [{"entry": "video", "occurrence": "video@1", "action": "block_switch", "target": "*", "until": 900, "priority": "high", "reason": "r"}],
            "holders": {}, "slot_reserves": [], "holds": [], "clock_profiles": {}, "fan_profiles": {},
            "mutes": [["hot_*", "video", 900]]});
        let c = calendar_section(&n);
        assert_eq!(c["active_event"], "video");
        assert_eq!(c["block"], true);
        assert_eq!(
            c["mutes"],
            json!([{"rules": "hot_*", "entry": "video", "until": 900}])
        );
        let empty = calendar_section(&json!({"active": [], "blocks": [], "mutes": []}));
        assert_eq!(empty["block"], false);
        assert!(empty.get("active_event").is_none());
    }

    #[test]
    fn live_router_camel_case_and_bare_ids() {
        let st = json!({"rev": "r", "mode": "act", "guards": [], "updated": 1,
            "servers": [{"id": "gpu-host", "activeLayout": "two", "hostctlOk": true, "cards": [
                {"id": "A", "label": "V100 (fast)", "managed": true, "adminState": "offline",
                 "engine": {"engine": "ninfer", "model": "m", "servedId": "", "state": "ready", "healthy": true},
                 "lanes": 4, "inflight": 2, "queued": 0},
                {"id": "X1", "label": "Quadro", "managed": false, "adminState": "unmanaged", "engine": null,
                 "lanes": 0, "inflight": 0, "queued": 0}]}],
            "layouts": [{"id": "two", "title": "Two", "server": "gpu-host", "active": true}]});
        let (e, ids) = engines_section(&st, "gpu-host");
        assert_eq!(e["layout"], "two");
        assert_eq!(e["cards"]["A"]["online"], false);
        assert_eq!(e["cards"]["A"]["title"], "V100 (fast)");
        assert_eq!(e["engines"]["ninfer-a"]["cards"], json!(["A"]));
        assert_eq!(e["cards"]["X1"]["engine"], "off");
        assert_eq!(ids["A"], "A");
    }
}
