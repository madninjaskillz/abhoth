//! The pig-slots lease registry, served by the router (ROUTER.md SHOULD "Server-side slots"):
//! a port of `$AI_ROOT/pig-slots/server.py` with the same JSON, TTL, layout file
//! (slot list + freeze) and `leases.json`, so `orca-core::slots::HttpSlotRegistry`, PiG and the
//! bench's `--wait-registry` work against it unchanged.
//!
//! - `POST /claim {key, owner, host, pid, label, kind: main|agent, copy? | slot? | avoid?}`
//!   -> `{ok, slot, copy, ttl}` (`existing: true` for a key that holds one) or
//!   `{ok: false, reason: full|switching}`
//! - `POST /renew {keys}` -> `{renewed, lost}`
//! - `POST /release {key}` or `{host, pid}` -> `{ok, released}`
//! - `GET /list` (and `/`) -> `{ttl, slots, free, load, layout, frozen, orphans}`
//!
//! What is new: the router knows which card each lease is on, so placement can follow leases
//! (`lease_for_key`) and on-demand decisions read leases without an HTTP call.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::Method;
use axum::response::Response;
use indexmap::IndexMap;
use orca_core::slots::registry::SlotClaim;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{FileSig, Snapshot};
use crate::proxy::{json_response, now_epoch};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub key: String,
    pub owner: String,
    pub host: String,
    pub pid: i64,
    pub label: String,
    pub kind: String,
    pub since: f64,
    pub renewed: f64,
}

#[derive(Debug, Default)]
struct LayoutFile {
    sig: Option<FileSig>,
    slots: Option<Vec<String>>,
    frozen: bool,
    layout: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
struct Inner {
    leases: IndexMap<String, Lease>,
    layout: LayoutFile,
    loaded: bool,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

/// The card ("copy") of a slot: its first letter, as pig-slots ("B1" -> "B").
pub fn copy_of(slot: &str) -> String {
    slot.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// The claim fields, parsed into orca-core's wire type (the client side of the same protocol).
pub fn parse_claim(b: &Map<String, Value>) -> SlotClaim {
    let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
    SlotClaim {
        key: match b.get("key") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        },
        owner: s("owner").unwrap_or_default(),
        host: s("host").unwrap_or_default(),
        pid: b.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        label: s("label").unwrap_or_default(),
        kind: s("kind").unwrap_or_default(),
        copy: s("copy"),
        slot: s("slot"),
        avoid: b.get("avoid").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Registry {
    fn slots_of(inner: &Inner, snap: &Snapshot) -> Vec<String> {
        if let Some(s) = &inner.layout.slots {
            return s.clone();
        }
        let mut out = Vec::new();
        for (card, u) in &snap.config.upstreams {
            if u.online {
                for i in 1..=u.lanes {
                    out.push(format!("{card}{i}"));
                }
            }
        }
        out
    }

    fn refresh(inner: &mut Inner, snap: &Snapshot) {
        let Some(path) = snap.config.slots.layout_file.as_deref() else {
            inner.layout = LayoutFile::default();
            return;
        };
        let sig = FileSig::of(std::path::Path::new(path));
        if sig == inner.layout.sig && inner.layout.sig.is_some() {
            return;
        }
        let mut lf = LayoutFile {
            sig,
            ..LayoutFile::default()
        };
        if sig.is_some() {
            match std::fs::read_to_string(path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                Some(d) => {
                    lf.slots = Some(
                        d.get("slots")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .filter(|x| !x.is_empty())
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    lf.frozen = d.get("frozen").and_then(Value::as_bool).unwrap_or(false);
                    lf.layout = d.get("layout").and_then(Value::as_str).map(str::to_string);
                    lf.reason = d.get("reason").and_then(Value::as_str).map(str::to_string);
                }
                None => lf.sig = None, // half-written: try again next call
            }
        }
        inner.layout = lf;
    }

    fn state_path(snap: &Snapshot) -> Option<PathBuf> {
        snap.config.slots.state_file.as_deref().map(PathBuf::from)
    }

    fn prepare(&self, snap: &Snapshot) -> std::sync::MutexGuard<'_, Inner> {
        let mut g = self.inner.lock().unwrap();
        if !g.loaded {
            g.loaded = true;
            if let Some(p) = Self::state_path(snap)
                && let Ok(t) = std::fs::read_to_string(&p)
                && let Ok(m) = serde_json::from_str::<IndexMap<String, Lease>>(&t)
            {
                Self::refresh(&mut g, snap);
                let slots = Self::slots_of(&g, snap);
                for (s, l) in m {
                    if slots.contains(&s) {
                        g.leases.insert(s, l);
                    }
                }
            }
        }
        Self::refresh(&mut g, snap);
        g
    }

    fn save(inner: &Inner, snap: &Snapshot) {
        let Some(p) = Self::state_path(snap) else {
            return;
        };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string(&inner.leases)
            && std::fs::write(&tmp, text).is_ok()
        {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    fn reap(inner: &mut Inner, now: f64, ttl: f64) -> bool {
        let before = inner.leases.len();
        inner.leases.retain(|_, l| now - l.renewed <= ttl);
        inner.leases.len() != before
    }

    pub fn claim(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        let now = now_epoch();
        let ttl = snap.config.slots.ttl_seconds;
        let c = parse_claim(b);
        if c.key.is_empty() {
            return (400, json!({"ok": false, "reason": "key required"}));
        }
        let mut g = self.prepare(snap);
        let mut changed = Self::reap(&mut g, now, ttl);
        let slots = Self::slots_of(&g, snap);
        let res = if let Some((s, l)) = g.leases.iter_mut().find(|(_, l)| l.key == c.key) {
            l.renewed = now;
            json!({"ok": true, "slot": s, "copy": copy_of(s), "ttl": ttl, "existing": true})
        } else {
            let cards: Vec<String> = {
                let mut v: Vec<String> = slots.iter().map(|s| copy_of(s)).collect();
                v.dedup();
                v
            };
            let avoid: Vec<String> = c
                .avoid
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|x| cards.contains(x))
                .collect();
            let hard = c.copy.clone().filter(|x| cards.contains(x));
            let cand: Vec<String> = match &c.slot {
                Some(want) if slots.contains(want) => {
                    if g.leases.contains_key(want) {
                        vec![]
                    } else {
                        vec![want.clone()]
                    }
                }
                _ => match &hard {
                    Some(h) => slots
                        .iter()
                        .filter(|s| copy_of(s) == *h && !g.leases.contains_key(*s))
                        .cloned()
                        .collect(),
                    None => order(&g.leases, &slots, &avoid),
                },
            };
            if g.layout.frozen {
                json!({"ok": false, "reason": "switching"})
            } else if let Some(s) = cand.first().cloned() {
                g.leases.insert(
                    s.clone(),
                    Lease {
                        key: c.key.clone(),
                        owner: clip(if c.owner.is_empty() { "?" } else { &c.owner }, 80),
                        host: clip(if c.host.is_empty() { "?" } else { &c.host }, 80),
                        pid: c.pid as i64,
                        label: clip(&c.label, 80),
                        kind: if c.kind == "main" {
                            "main".into()
                        } else {
                            "agent".into()
                        },
                        since: now,
                        renewed: now,
                    },
                );
                changed = true;
                json!({"ok": true, "slot": s, "copy": copy_of(&s), "ttl": ttl})
            } else {
                json!({"ok": false, "reason": "full"})
            }
        };
        if changed {
            Self::save(&g, snap);
        }
        (200, res)
    }

    pub fn renew(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        let now = now_epoch();
        let keys: Vec<String> = b
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|k| {
                        k.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| k.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut g = self.prepare(snap);
        let changed = Self::reap(&mut g, now, snap.config.slots.ttl_seconds);
        let (mut ok, mut lost) = (Vec::new(), Vec::new());
        for k in keys {
            match g.leases.values_mut().find(|l| l.key == k) {
                Some(l) => {
                    l.renewed = now;
                    ok.push(k);
                }
                None => lost.push(k),
            }
        }
        if changed {
            Self::save(&g, snap);
        }
        (200, json!({"renewed": ok, "lost": lost}))
    }

    pub fn release(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        let mut g = self.prepare(snap);
        let key = b
            .get("key")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let host = b
            .get("host")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let pid = b.get("pid").and_then(Value::as_i64).filter(|p| *p != 0);
        let gone: Vec<String> = g
            .leases
            .iter()
            .filter(|(_, l)| {
                key.is_some_and(|k| l.key == k)
                    || (host.is_some_and(|h| l.host == h) && pid.is_some_and(|p| l.pid == p))
            })
            .map(|(s, _)| s.clone())
            .collect();
        for s in &gone {
            g.leases.shift_remove(s);
        }
        if !gone.is_empty() {
            Self::save(&g, snap);
        }
        (200, json!({"ok": true, "released": gone}))
    }

    pub fn list(&self, snap: &Snapshot) -> Value {
        let now = now_epoch();
        let mut g = self.prepare(snap);
        if Self::reap(&mut g, now, snap.config.slots.ttl_seconds) {
            Self::save(&g, snap);
        }
        let slots = Self::slots_of(&g, snap);
        let mut sm = Map::new();
        for s in &slots {
            sm.insert(
                s.clone(),
                match g.leases.get(s) {
                    Some(l) => json!({"key": l.key, "owner": l.owner, "host": l.host, "pid": l.pid,
                        "label": l.label, "kind": l.kind, "age_s": (now - l.since) as i64,
                        "idle_s": (now - l.renewed) as i64}),
                    None => Value::Null,
                },
            );
        }
        let mut load: BTreeMap<String, usize> = BTreeMap::new();
        for s in &slots {
            load.entry(copy_of(s)).or_default();
        }
        for s in g.leases.keys() {
            *load.entry(copy_of(s)).or_default() += 1;
        }
        let mut orphans: Vec<&String> = g.leases.keys().filter(|s| !slots.contains(s)).collect();
        orphans.sort();
        json!({
            "ttl": snap.config.slots.ttl_seconds,
            "slots": sm,
            "free": slots.iter().filter(|s| !g.leases.contains_key(*s)).collect::<Vec<_>>(),
            "load": load,
            "layout": g.layout.layout,
            "frozen": g.layout.frozen,
            "orphans": orphans,
        })
    }

    /// The card and kind of the lease held by `key`, if any (placement).
    pub fn lease_for_key(&self, snap: &Snapshot, key: &str) -> Option<(String, String)> {
        let now = now_epoch();
        let g = self.prepare(snap);
        g.leases
            .iter()
            .find(|(_, l)| l.key == key && now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, l)| (copy_of(s), l.kind.clone()))
    }

    /// Cards holding a live `kind: main` lease (Lead reservation).
    pub fn lead_cards(&self, snap: &Snapshot) -> Vec<String> {
        let now = now_epoch();
        let g = self.prepare(snap);
        let mut v: Vec<String> = g
            .leases
            .iter()
            .filter(|(_, l)| l.kind == "main" && now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, _)| copy_of(s))
            .collect();
        v.dedup();
        v
    }

    /// `[(slot, who)]` of live leases (on-demand decisions; layoutctl.leases()).
    pub fn held(&self, snap: &Snapshot) -> Vec<(String, String)> {
        let now = now_epoch();
        let g = self.prepare(snap);
        g.leases
            .iter()
            .filter(|(_, l)| now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, l)| (s.clone(), format!("{}@{}", l.owner, l.host)))
            .collect()
    }
}

/// Free slots, least-loaded copy first, then by name (pig-slots `order`).
fn order(leases: &IndexMap<String, Lease>, slots: &[String], avoid: &[String]) -> Vec<String> {
    let load = |c: &str| leases.keys().filter(|s| copy_of(s) == c).count();
    let mut free: Vec<&String> = slots
        .iter()
        .filter(|s| !leases.contains_key(*s) && !avoid.contains(&copy_of(s)))
        .collect();
    free.sort_by_key(|s| (load(&copy_of(s)), (*s).clone()));
    free.into_iter().cloned().collect()
}

/// The pig-slots HTTP surface (on `/slots/...` of the main listener, or every path of the
/// compatibility listener).
pub async fn http(st: &AppState, method: &Method, path: &str, raw: &[u8]) -> Response {
    let snap = st.snapshot();
    if !snap.config.slots.enabled {
        return json_response(404, &json!({"error": "not found"}));
    }
    let reg = &st.ext.registry;
    let path = path.split('?').next().unwrap_or("/");
    if *method == Method::GET {
        return match path {
            "/health" => json_response(
                200,
                &json!({"status": "ok", "service": "pig-slots", "impl": "abhoth-router"}),
            ),
            "/list" | "/" => json_response(200, &reg.list(&snap)),
            _ => json_response(404, &json!({"error": "not found"})),
        };
    }
    if *method != Method::POST {
        return json_response(404, &json!({"error": "not found"}));
    }
    let body: Map<String, Value> = if raw.is_empty() || raw.len() >= 65536 {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(m)) => m,
            _ => return json_response(400, &json!({"error": "bad json"})),
        }
    };
    let (code, obj) = match path {
        "/claim" => reg.claim(&snap, &body),
        "/renew" => reg.renew(&snap, &body),
        "/release" => reg.release(&snap, &body),
        _ => return json_response(404, &json!({"error": "not found"})),
    };
    json_response(code, &obj)
}
