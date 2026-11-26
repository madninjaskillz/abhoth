//! State payloads: one retained JSON per device (SUPERVISOR.md 11.1), plus availability.

use serde_json::{Value, json};

use crate::model::{CardState, HaSnapshot, round1};
use crate::topics::Topics;

/// One retained publish.
#[derive(Debug, Clone, PartialEq)]
pub struct StateMessage {
    pub topic: String,
    pub payload: String,
}

fn onoff(v: Option<bool>) -> Value {
    match v {
        Some(true) => json!("ON"),
        Some(false) => json!("OFF"),
        None => Value::Null,
    }
}

fn r(v: Option<f64>) -> Value {
    v.map(|x| json!(round1(x))).unwrap_or(Value::Null)
}

/// Extra state the publisher itself owns (not from the hub).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalState {
    /// The layout the select points at (follows the live layout after each switch).
    pub layout_target: Option<String>,
    /// The last HA command's outcome, as `{command, ok, message, at}`.
    pub last_command: Option<Value>,
    pub clock_cap_ttl_seconds: u64,
}

pub fn server_state(snap: &HaSnapshot, local: &LocalState) -> Value {
    let c = &snap.control;
    let h = &snap.host;
    let last_switch_at = c
        .last_switch
        .as_ref()
        .and_then(|v| v.get("at").or_else(|| v.get("finished")))
        .and_then(|v| v.as_str())
        .map(|s| json!(s))
        .unwrap_or(Value::Null);
    json!({
        "tok_s_total": r(snap.total_tok_s()),
        "models": snap.models_summary(),
        "power_total": r(snap.total_power_w()),
        "cpu": r(h.cpu_pct),
        "ram_used": r(h.ram_used_gb),
        "ram_arc": r(h.ram_arc_gb),
        "ram_consumed": r(h.ram_consumed_gb),
        "ram_free": r(h.ram_free_gb),
        "net_in": r(h.net_in_mbps),
        "net_out": r(h.net_out_mbps),
        "layout": c.layout.clone().unwrap_or_else(|| "unknown".into()),
        "layout_title": c.layout_title,
        "layouts": c.layouts,
        "layout_target": local.layout_target,
        "switch_state": c.switch_state.clone().unwrap_or_else(|| "unknown".into()),
        "last_switch": c.last_switch,
        "last_switch_at": last_switch_at,
        "last_command": local.last_command,
        "on_demand": c.on_demand,
        "leases": c.leases_held,
        "slots_busy": c.slots_busy,
        "slots_total": c.slots_total,
        "bench_running": onoff(c.bench_running),
        "calendar_block": onoff(c.calendar_block),
        "event_active": onoff(Some(c.event_active.is_some())),
        "event": c.event_active,
        "problem": onoff(c.problem),
        "open_alerts": c.open_alerts,
        "fan_mode": c.fan_mode,
        "version": snap.server.version,
        "config_rev": snap.server.config_rev,
        "hostd": snap.server.hostd,
    })
}

pub fn card_state(card: &CardState, local: &LocalState) -> Value {
    let summary = card.summary.clone().or_else(|| {
        card.engine.as_ref().map(|e| match &card.model {
            Some(m) => format!("{m} ({e})"),
            None => e.clone(),
        })
    });
    json!({
        "gpu": card.gpu,
        "core_temp": r(card.core_c),
        "hbm_temp": r(card.hbm_c),
        "power": r(card.power_w),
        "power_pct": r(card.power_pct()),
        "sm_clock": r(card.sm_clock_mhz),
        "util": r(card.util_pct),
        "vram_used": r(card.vram_used_gb),
        "vram_total": r(card.vram_total_gb),
        "engine": summary.unwrap_or_else(|| "off".into()).chars().take(250).collect::<String>(),
        "engine_name": card.engine,
        "engine_state": card.engine_state,
        "model": card.model,
        "tok_s": r(card.tok_s),
        "running": card.running,
        "queued": card.queued,
        "clock_locked": onoff(card.clock_locked),
        "throttling": onoff(Some(!card.throttle_reasons.is_empty())),
        "throttle_reasons": card.throttle_reasons,
        "online": onoff(Some(card.online)),
        "pinned": card.pinned,
        "clock_cap": card.clock_cap_mhz.map(|m| m.to_string()).unwrap_or_else(|| "none".into()),
        "clock_cap_ttl_seconds": local.clock_cap_ttl_seconds,
    })
}

/// Every state publish for this snapshot (all retained).
pub fn build(t: &Topics, snap: &HaSnapshot, local: &LocalState) -> Vec<StateMessage> {
    let mut out = vec![StateMessage { topic: t.state(), payload: server_state(snap, local).to_string() }];
    if let Some(target) = &local.layout_target {
        out.push(StateMessage { topic: t.layout_target(), payload: target.clone() });
    }
    for c in &snap.cards {
        out.push(StateMessage {
            topic: t.card_availability(&c.id),
            payload: if c.present { "online" } else { "offline" }.into(),
        });
        out.push(StateMessage { topic: t.card_state(&c.id), payload: card_state(c, local).to_string() });
    }
    for f in &snap.fans {
        let v = json!({
            "duty": r(f.duty_pct),
            "rpm": f.rpm.map(|x| json!(x.round())).unwrap_or(Value::Null),
            "control_temp": r(f.control_temp_c),
            "source": f.source,
            "push_age": r(f.push_age_s),
            "failsafe": onoff(f.failsafe),
            "cools": f.cools,
        });
        out.push(StateMessage { topic: t.fan_state(&f.id), payload: v.to_string() });
    }
    for en in &snap.engines {
        let v = json!({
            "state": en.state.clone().unwrap_or_else(|| "unknown".into()),
            "model": en.model,
            "tok_s": r(en.tok_s),
            "healthy": onoff(en.healthy),
            "cards": en.cards,
            "kind": en.kind,
        });
        out.push(StateMessage { topic: t.engine_state(&en.id), payload: v.to_string() });
    }
    out
}

/// The fingerprint of discrete values: a change here publishes at once instead of waiting
/// for the next interval.
pub fn discrete_fingerprint(snap: &HaSnapshot) -> String {
    let c = &snap.control;
    let cards: Vec<_> = snap
        .cards
        .iter()
        .map(|x| (x.id.clone(), x.present, x.online, x.engine_state.clone(), x.model.clone(), x.clock_cap_mhz, x.throttle_reasons.is_empty()))
        .collect();
    let engines: Vec<_> = snap.engines.iter().map(|e| (e.id.clone(), e.state.clone(), e.healthy)).collect();
    let fans: Vec<_> = snap.fans.iter().map(|f| (f.id.clone(), f.failsafe, f.source.clone())).collect();
    format!(
        "{:?}",
        (
            (&c.layout, &c.layouts, &c.switch_state, &c.on_demand, c.leases_held, c.bench_running),
            (c.calendar_block, &c.event_active, c.problem, &c.fan_mode),
            (cards, engines, fans)
        )
    )
}
