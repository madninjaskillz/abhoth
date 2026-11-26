//! The mock feed: a recorded (or polled) state of the legacy dash (`/api/state`) turned into the
//! telemetry hub's sections, so the page runs against the real feed types before hostd and the
//! control plane publish their own.
//!
//! - `gpus` ([`GpuSample`] keyed by card label) and `host` ([`HostSample`]) in the hub's shapes;
//! - `calendar`, `alerts`, `engines`: the shapes Glance proposes for those sections;
//! - `dash`: everything else of the legacy state (model servers, slots, leases, ESP, BMC,
//!   services, image generation, pools, news, OOM kills), without its history, until the
//!   owning subsystems publish sections of their own;
//! - `history`: the legacy history, once at start (the hub's rings are not in the snapshot).

use orca_sensors::TimestampMs;
use orca_sensors::samples::{GpuSample, HostSample, MemSample, NetSample};
use serde_json::{Map, Value, json};

/// Sections of one legacy state. `with_history` adds the `history` section.
pub fn sections(legacy: &Value, with_history: bool) -> Map<String, Value> {
    let mut out = Map::new();
    let ts = (legacy["time"].as_f64().unwrap_or(0.0) * 1000.0) as TimestampMs;
    let cards = card_labels(legacy);
    let mut gpus = Map::new();
    for g in legacy["gpus"].as_array().into_iter().flatten() {
        let s = gpu_sample(g, ts);
        let label = cards
            .iter()
            .find(|(_, bus)| bus.eq_ignore_ascii_case(&s.bus))
            .map(|(l, _)| l.clone())
            .unwrap_or_else(|| format!("gpu{}", s.index));
        gpus.insert(label, serde_json::to_value(s).unwrap_or(Value::Null));
    }
    if !gpus.is_empty() {
        out.insert("gpus".into(), Value::Object(gpus));
    }
    if legacy["host"].is_object() {
        out.insert(
            "host".into(),
            serde_json::to_value(host_sample(&legacy["host"], ts)).unwrap_or(Value::Null),
        );
    }
    for k in ["calendar", "alerts", "engines"] {
        if let Some(v) = legacy.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    let mut dash = legacy.clone();
    if let Some(o) = dash.as_object_mut() {
        for k in ["calendar", "alerts", "engines"] {
            o.remove(k);
        }
    }
    let history = with_history.then(|| history_of(legacy));
    super::feed_strip_hist(&mut dash);
    out.insert("dash".into(), dash);
    if let Some(h) = history {
        out.insert("history".into(), h);
    }
    out
}

/// Card labels (A, B, …) from the profiles API's card list: "V100-SXM3 (fast, 21:00.0)".
fn card_labels(legacy: &Value) -> Vec<(String, String)> {
    legacy["profile"]["cards"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(label, c)| {
            let gpu = c["gpu"].as_str()?;
            let bus = gpu
                .trim_end_matches(')')
                .rsplit([' ', '('])
                .next()?
                .to_ascii_lowercase();
            Some((label.clone(), bus))
        })
        .collect()
}

fn f(v: &Value) -> Option<f64> {
    v.as_f64().filter(|x| x.is_finite())
}

/// The exporter's names → [`GpuSample`]; throttle flags → the NVML bitmask.
pub fn gpu_sample(g: &Value, ts: TimestampMs) -> GpuSample {
    const MIB: f64 = 1024.0 * 1024.0;
    let mut throttle = 0u64;
    for (k, bit) in [
        ("clocks_event_reasons_sw_power_cap", 0x4),
        ("clocks_event_reasons_hw_slowdown", 0x8),
        ("clocks_event_reasons_sw_thermal_slowdown", 0x20),
        ("clocks_event_reasons_hw_thermal_slowdown", 0x40),
        ("clocks_event_reasons_hw_power_brake_slowdown", 0x80),
    ] {
        if f(&g[k]).unwrap_or(0.0) > 0.0 {
            throttle |= bit;
        }
    }
    GpuSample {
        index: g["index"].as_u64().unwrap_or(0) as u32,
        uuid: g["uuid"].as_str().unwrap_or_default().into(),
        bus: g["pci"].as_str().unwrap_or_default().to_ascii_lowercase(),
        name: g["name"].as_str().unwrap_or_default().into(),
        ts,
        core_c: f(&g["temperature_gpu"]),
        mem_temp_c: f(&g["temperature_memory"]),
        hotspot_c: None,
        power_w: f(&g["power_draw_watts"]),
        power_limit_w: f(&g["enforced_power_limit_watts"]),
        sm_mhz: f(&g["clocks_current_sm_clock_hz"]).map(|x| x / 1e6),
        mem_mhz: f(&g["clocks_current_memory_clock_hz"]).map(|x| x / 1e6),
        max_sm_mhz: f(&g["clocks_max_sm_clock_hz"]).map(|x| x / 1e6),
        util_pct: f(&g["utilization_gpu_ratio"]).map(|x| x * 100.0),
        mem_util_pct: f(&g["utilization_memory_ratio"]).map(|x| x * 100.0),
        vram_used_mib: f(&g["memory_used_bytes"]).map(|x| x / MIB),
        vram_total_mib: f(&g["memory_total_bytes"]).map(|x| x / MIB),
        fan_pct: f(&g["fan_speed_ratio"]).map(|x| x * 100.0),
        pstate: f(&g["pstate"]).map(|p| format!("P{p}")),
        throttle_reasons: Some(throttle),
        persistence: None,
        ..Default::default()
    }
}

/// The dash's host block → [`HostSample`] (`available` is MemAvailable; the page computes the
/// dash's used / ARC / consumed / free from total, available and arc).
pub fn host_sample(h: &Value, ts: TimestampMs) -> HostSample {
    let total = h["mem_total"].as_u64().unwrap_or(0);
    let available = h["mem_avail"].as_u64().unwrap_or(0);
    let arc = h["arc"].as_u64().unwrap_or(0);
    let load = |i: usize| h["load"][i].as_f64().unwrap_or(0.0);
    HostSample {
        ts,
        cpu_pct: f(&h["cpu"]),
        cpu_count: 0,
        load1: load(0),
        load5: load(1),
        load15: load(2),
        uptime_s: h["uptime_s"].as_f64().unwrap_or(0.0),
        mem: MemSample {
            total,
            free: available,
            available,
            arc,
            arc_max: None,
            cache: 0,
            used: total.saturating_sub(available).saturating_sub(arc),
            consumed: total.saturating_sub(available),
            swap_total: 0,
            swap_free: 0,
        },
        net: vec![NetSample {
            iface: "lan".into(),
            rx_bytes: 0,
            tx_bytes: 0,
            rx_bps: f(&h["net_rx"]),
            tx_bps: f(&h["net_tx"]),
        }],
    }
}

/// `{time, hist, gpus: {uuid: hist}, llm: {key: {hist, slot_hist}}}`.
fn history_of(legacy: &Value) -> Value {
    let mut gpus = Map::new();
    for g in legacy["gpus"].as_array().into_iter().flatten() {
        if let (Some(u), Some(h)) = (g["uuid"].as_str(), g.get("hist")) {
            gpus.insert(u.into(), h.clone());
        }
    }
    let mut llm = Map::new();
    for (k, i) in legacy["llm"].as_object().into_iter().flatten() {
        let mut e = Map::new();
        for part in ["hist", "slot_hist"] {
            if let Some(h) = i.get(part) {
                e.insert(part.into(), h.clone());
            }
        }
        if !e.is_empty() {
            llm.insert(k.clone(), Value::Object(e));
        }
    }
    json!({"time": legacy["time"], "hist": legacy.get("hist").cloned().unwrap_or(json!({})), "gpus": gpus, "llm": llm})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recording_becomes_hub_sections() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/glance-feed.jsonl");
        let frames = super::super::load_fixture(&path).unwrap();
        let s = sections(&frames[0], true);
        let a = &s["gpus"]["A"];
        assert_eq!(a["bus"], "21:00.0");
        assert!(a["core_c"].as_f64().unwrap() > 0.0);
        assert!(a.get("hotspot_c").is_none(), "absent, never null or 0");
        assert!(s["gpus"].as_object().unwrap().contains_key("B"));
        assert!(s["host"]["mem"]["arc"].as_u64().unwrap() > 0);
        assert!(s["history"]["hist"]["cpu"].is_array());
        assert!(s["dash"].get("hist").is_none());
        assert!(s["dash"]["gpus"][0].get("hist").is_none());
        assert!(s["calendar"]["now"].is_array());
        assert!(sections(&frames[1], false).get("history").is_none());
        let g = gpu_sample(
            &json!({"clocks_event_reasons_sw_power_cap": 1.0, "clocks_event_reasons_hw_thermal_slowdown": 1.0}),
            0,
        );
        assert_eq!(g.throttle_reasons, Some(0x44));
    }
}
