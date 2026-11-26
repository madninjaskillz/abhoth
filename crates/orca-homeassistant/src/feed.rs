//! From the telemetry hub to [`HaSnapshot`]: subscribe to the feed (orca-sensors
//! `TelemetryFeed`), keep the merged state, and turn its sections into the HA view.
//!
//! Sections with a typed sample (`gpus`, `host`, `fans`) are read through the orca-sensors
//! types. Sections whose producers are still being written (`clocks`, `engines`, `leases`,
//! `calendar`, `alerts`, `health`) are read tolerantly by key name; a missing key is simply
//! `None` (unknown in HA), never 0.

use std::sync::Arc;

use orca_sensors::feed::{Resume, TelemetryFeed, apply_merge_patch, sections};
use orca_sensors::samples::{FanSample, GpuSample, MemSample, NetSample};
use serde_json::{Map, Value};
use tokio::sync::{broadcast, watch};

use crate::model::{
    CardState, ControlState, EngineState, FanState, HaSnapshot, HostState, ServerInfo, round1,
};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// NVML throttle-reason bits that mean "slowed down" (not idle, not app-clock settings).
const THROTTLE_BITS: &[(u64, &str)] = &[
    (0x4, "sw_power_cap"),
    (0x8, "hw_slowdown"),
    (0x10, "sync_boost"),
    (0x20, "sw_thermal"),
    (0x40, "hw_thermal"),
    (0x80, "hw_power_brake"),
    (0x100, "display_clock"),
];

pub fn throttle_names(mask: u64) -> Vec<String> {
    THROTTLE_BITS
        .iter()
        .filter(|(b, _)| mask & b != 0)
        .map(|(_, n)| n.to_string())
        .collect()
}

/// Per-card settings from config the feed does not carry.
#[derive(Debug, Clone, Default)]
pub struct CardConfig {
    pub id: String,
    /// orca-gpuctl's `cap_offered()` for this card (`allow_cap`; false for card A).
    pub allow_cap: bool,
}

fn obj<'a>(state: &'a Value, section: &str) -> Option<&'a Map<String, Value>> {
    state.get(section).and_then(Value::as_object)
}
fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(String::from)
}
fn f(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}
fn u(v: Option<&Value>) -> Option<u32> {
    v.and_then(Value::as_u64)
        .and_then(|x| u32::try_from(x).ok())
}
fn b(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}
fn first<'a>(m: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| m.get(*k))
}

/// Builds the HA view of one feed state.
pub fn snapshot_from_state(state: &Value, server: &ServerInfo, cards: &[CardConfig]) -> HaSnapshot {
    let empty = Map::new();
    let gpus = obj(state, sections::GPUS).unwrap_or(&empty);
    let clocks = obj(state, sections::CLOCKS).unwrap_or(&empty);
    let engines = obj(state, sections::ENGINES).unwrap_or(&empty);
    let engine_cards = engines
        .get("cards")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    // Cards: config order first, then any extra the feed knows.
    let mut ids: Vec<String> = cards.iter().map(|c| c.id.clone()).collect();
    for k in gpus.keys().chain(engine_cards.keys()) {
        if !ids.contains(k) {
            ids.push(k.clone());
        }
    }
    let mut out_cards = Vec::new();
    for id in ids {
        let g: Option<GpuSample> = gpus
            .get(&id)
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let ck = clocks.get(&id).and_then(Value::as_object).unwrap_or(&empty);
        let ec = engine_cards
            .get(&id)
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let cfg_allow = cards
            .iter()
            .find(|c| c.id == id)
            .is_some_and(|c| c.allow_cap);
        let mut c = CardState {
            id: id.clone(),
            present: g.is_some(),
            ..Default::default()
        };
        if let Some(g) = &g {
            c.gpu = Some(g.name.clone());
            c.core_c = g.core_c;
            c.hbm_c = g.mem_temp_c;
            c.power_w = g.power_w;
            c.power_limit_w = g.power_limit_w;
            c.sm_clock_mhz = g.sm_mhz;
            c.util_pct = g.util_pct;
            c.vram_used_gb = g.vram_used_mib.map(|m| round1(m / 1024.0));
            c.vram_total_gb = g.vram_total_mib.map(|m| round1(m / 1024.0));
            c.throttle_reasons = g.throttle_reasons.map(throttle_names).unwrap_or_default();
        }
        // The clocks section wins when it says; a card config does not know is never offered.
        c.cap_offered = b(first(ck, &["cap_offered", "allow_cap"])).unwrap_or(cfg_allow);
        c.clock_locked = b(first(ck, &["locked", "clock_locked"]));
        c.clock_cap_mhz = u(first(ck, &["cap_mhz", "clock_cap_mhz", "cap"]));
        c.online = b(first(ec, &["online"])).unwrap_or(c.present);
        c.engine = s(first(ec, &["engine"])).filter(|e| e != "off");
        c.engine_state = s(first(ec, &["state", "engine_state"]));
        c.model = s(first(ec, &["model_title", "model"]));
        c.summary = s(first(ec, &["summary"]));
        c.tok_s = f(first(ec, &["tok_s"]));
        c.running = u(first(ec, &["running"]));
        c.queued = u(first(ec, &["queued"]));
        out_cards.push(c);
    }

    let fans = obj(state, sections::FANS)
        .map(|m| {
            m.iter()
                .map(|(id, v)| {
                    let fs: Option<FanSample> = serde_json::from_value(v.clone()).ok();
                    let o = v.as_object().unwrap_or(&empty);
                    FanState {
                        id: id.clone(),
                        name: s(o.get("name")),
                        cools: o
                            .get("cools")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        duty_pct: fs.as_ref().and_then(|x| x.duty_pct),
                        rpm: fs.as_ref().and_then(|x| x.rpm),
                        control_temp_c: fs.as_ref().and_then(|x| x.control_temp_c),
                        source: s(first(o, &["source", "control_source"])),
                        push_age_s: f(first(o, &["push_age_s"])),
                        failsafe: fs.as_ref().and_then(|x| x.failsafe),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Field by field, so one new required field in HostSample can't blank the whole section.
    let host = obj(state, sections::HOST)
        .map(|h| {
            let mem: Option<MemSample> = h
                .get("mem")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let net: Vec<NetSample> = h
                .get("net")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let gb = |x: u64| Some(round1(x as f64 / GIB));
            let rx: Vec<f64> = net.iter().filter_map(|n| n.rx_bps).collect();
            let tx: Vec<f64> = net.iter().filter_map(|n| n.tx_bps).collect();
            let mbps =
                |v: Vec<f64>| (!v.is_empty()).then(|| round1(v.iter().sum::<f64>() * 8.0 / 1e6));
            HostState {
                cpu_pct: f(h.get("cpu_pct")),
                ram_total_gb: mem.as_ref().and_then(|m| gb(m.total)),
                ram_used_gb: mem.as_ref().and_then(|m| gb(m.used)),
                ram_arc_gb: mem.as_ref().and_then(|m| gb(m.arc)),
                ram_consumed_gb: mem.as_ref().and_then(|m| gb(m.consumed)),
                ram_free_gb: mem.as_ref().and_then(|m| gb(m.free)),
                net_in_mbps: mbps(rx),
                net_out_mbps: mbps(tx),
            }
        })
        .unwrap_or_default();

    let engine_list = engines
        .get("engines")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(id, v)| {
                    let o = v.as_object().unwrap_or(&empty);
                    EngineState {
                        id: id.clone(),
                        title: s(o.get("title")),
                        kind: s(o.get("kind")),
                        cards: o
                            .get("cards")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        state: s(o.get("state")),
                        model: s(o.get("model")),
                        tok_s: f(o.get("tok_s")),
                        healthy: b(o.get("healthy")),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let leases = state.get(sections::LEASES);
    let calendar = obj(state, sections::CALENDAR).unwrap_or(&empty);
    let alerts = state.get(sections::ALERTS);
    let open_alerts: Vec<String> = match alerts {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| s(x.get("identity")).or_else(|| s(Some(x))))
            .collect(),
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        _ => vec![],
    };
    let control = ControlState {
        layout: s(engines.get("layout")),
        layout_title: s(engines.get("layout_title")),
        layouts: engines
            .get("layouts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        switch_state: s(first(engines, &["switch_state", "switch"])),
        last_switch: engines.get("last_switch").cloned(),
        on_demand: s(engines.get("on_demand")),
        leases_held: match leases {
            Some(Value::Array(a)) => u32::try_from(a.len()).ok(),
            Some(Value::Object(m)) => u(m.get("held")).or_else(|| {
                m.get("leases")
                    .and_then(Value::as_array)
                    .and_then(|a| u32::try_from(a.len()).ok())
            }),
            _ => None,
        },
        slots_busy: u(engines.get("slots_busy")),
        slots_total: u(engines.get("slots_total")),
        bench_running: b(first(calendar, &["bench_running"]))
            .or_else(|| b(engines.get("bench_running"))),
        calendar_block: b(first(calendar, &["block", "blocked"])),
        event_active: s(first(calendar, &["active_event", "event"])),
        problem: alerts.map(|_| !open_alerts.is_empty()),
        open_alerts,
        fan_mode: s(engines.get("fan_mode")),
    };
    let mut server = server.clone();
    if let Some(h) = obj(state, sections::HEALTH).and_then(|m| m.get("hostd")) {
        server.hostd = s(h.get("status")).or_else(|| s(Some(h)));
    }
    HaSnapshot {
        server,
        cards: out_cards,
        fans,
        engines: engine_list,
        host,
        control,
    }
}

/// Follows the hub: keeps the merged state and sends a new [`HaSnapshot`] after every
/// snapshot or patch. Resubscribes from a fresh snapshot when it lags. Ends when every
/// receiver of `tx` is gone.
pub async fn follow(
    feed: Arc<dyn TelemetryFeed>,
    server: ServerInfo,
    cards: Vec<CardConfig>,
    tx: watch::Sender<HaSnapshot>,
) {
    loop {
        let sub = feed.subscribe(None);
        let mut state = match sub.start {
            Resume::Snapshot(s) => s.state,
            Resume::Replay(_) => feed.snapshot().state,
        };
        if tx
            .send(snapshot_from_state(&state, &server, &cards))
            .is_err()
        {
            return;
        }
        let mut rx = sub.rx;
        loop {
            match rx.recv().await {
                Ok(orca_sensors::feed::FeedMessage::Snapshot(s)) => state = s.state,
                Ok(orca_sensors::feed::FeedMessage::Patch { patch, .. }) => {
                    apply_merge_patch(&mut state, &patch)
                }
                Ok(orca_sensors::feed::FeedMessage::Event { .. }) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => return,
            }
            if tx
                .send(snapshot_from_state(&state, &server, &cards))
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sections_to_snapshot() {
        let state = json!({
            "gpus": {"B": {"index": 0, "uuid": "GPU-b", "bus": "01:00.0", "name": "V100-PCIe-32GB", "ts": 1,
                            "core_c": 26.0, "mem_temp_c": 24.0, "power_w": 24.27, "power_limit_w": 250.0,
                            "sm_mhz": 135.0, "util_pct": 0.0, "vram_used_mib": 29081.6, "vram_total_mib": 32768.0,
                            "throttle_reasons": 0x45}},
            "host": {"ts": 1, "cpu_pct": 3.0, "cpu_count": 64, "uptime_s": 1.0,
                     "mem": {"total": 549755813888u64, "free": 107374182400u64, "available": 1, "arc": 193273528320u64, "cache": 1,
                             "used": 139586437120u64, "consumed": 442381631488u64, "swap_total": 0, "swap_free": 0},
                     "net": [{"iface": "eth0", "rx_bytes": 1, "tx_bytes": 1, "rx_bps": 125000.0, "tx_bps": 62500.0}]},
            "fans": {"0": {"fan": "0", "ts": 1, "duty_pct": 30.0, "rpm": 1200.0, "failsafe": false, "control_temp_c": 44.0,
                           "cools": ["B"], "source": "gpu.B.hbm"}},
            "clocks": {"B": {"locked": true, "cap_mhz": 1300}},
            "engines": {"layout": "flash-x2", "layouts": ["flash-x2", "dense-x2"], "switch_state": "idle",
                        "cards": {"B": {"engine": "strata", "state": "ready", "model": "flash", "tok_s": 12.5}},
                        "engines": {"strata-b": {"title": "Strata B", "kind": "llm", "cards": ["B"], "state": "ready", "healthy": true}}},
            "leases": [{"id": 1}],
            "alerts": [{"identity": "hot_card/A"}],
        });
        let server = ServerInfo {
            id: "deepthought".into(),
            name: "DeepThought".into(),
            ..Default::default()
        };
        let cards = vec![
            CardConfig {
                id: "A".into(),
                allow_cap: false,
            },
            CardConfig {
                id: "B".into(),
                allow_cap: true,
            },
        ];
        let s = snapshot_from_state(&state, &server, &cards);
        assert_eq!(s.cards.len(), 2);
        let a = &s.cards[0];
        assert!(
            !a.present && !a.cap_offered && a.core_c.is_none(),
            "card A has no sample: missing, not zero"
        );
        let b = &s.cards[1];
        assert_eq!(
            (b.core_c, b.hbm_c, b.vram_used_gb),
            (Some(26.0), Some(24.0), Some(28.4))
        );
        assert_eq!(b.throttle_reasons, vec!["sw_power_cap", "hw_thermal"]);
        assert_eq!((b.clock_locked, b.clock_cap_mhz), (Some(true), Some(1300)));
        assert_eq!((b.engine.as_deref(), b.tok_s), (Some("strata"), Some(12.5)));
        assert_eq!(s.host.ram_arc_gb, Some(180.0));
        assert_eq!(s.host.ram_free_gb, Some(100.0));
        assert_eq!(s.host.net_in_mbps, Some(1.0));
        assert_eq!(s.fans[0].source.as_deref(), Some("gpu.B.hbm"));
        assert_eq!(s.fans[0].cools, vec!["B"]);
        assert_eq!(s.control.layouts.len(), 2);
        assert_eq!(s.control.leases_held, Some(1));
        assert_eq!(s.control.problem, Some(true));
        assert_eq!(s.engines[0].id, "strata-b");
    }
}
