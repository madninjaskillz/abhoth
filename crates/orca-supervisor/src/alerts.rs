//! Alerts (orca-alerts) over the hub: every 5 s the built-in conditions and the expression
//! rules are evaluated from the feed state, hub events become event signals, open alerts go
//! to the `alerts` section and notifications to the sinks (`[alerts.sinks.*]`, plus `mqtt`
//! through the HA connection when it runs). Calendar mutes come from the `calendar` section
//! (the router's `calendar/now`); critical alerts are never muted.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use orca_alerts::sinks::{Sink, dispatch};
use orca_alerts::{AlertState, AlertsConfig, Engine, Notification, Signal};
use orca_sensors::feed::{FeedMessage, TelemetryFeed, TelemetrySink, sections};
use orca_sensors::{Expr, Reading, SensorId, SensorStore, Unit};
use serde_json::{Value, json};

use crate::TelemetryHub;

/// Card temperature that makes `hot_card` true (°C, the hotter of core and HBM).
pub const HOT_CARD_C: f64 = 85.0;

fn vars(kv: &[(&str, String)]) -> BTreeMap<String, String> {
    kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

/// An alert's `when`: `<expr> <op> <expr>` with `>=`, `<=`, `>`, `<` (the sensor language
/// has no comparisons; alert conditions add exactly one).
#[derive(Debug, Clone)]
pub struct When {
    lhs: Expr,
    op: &'static str,
    rhs: Expr,
}

impl When {
    pub fn parse(text: &str) -> Result<When, String> {
        for op in [">=", "<=", ">", "<"] {
            if let Some(i) = text.find(op) {
                let (l, r) = (&text[..i], &text[i + op.len()..]);
                let lhs = Expr::parse(l.trim()).map_err(|e| format!("{e}"))?;
                let rhs = Expr::parse(r.trim()).map_err(|e| format!("{e}"))?;
                return Ok(When { lhs, op, rhs });
            }
        }
        Err("`when` needs a comparison: <expr> >= <value> (or <=, >, <)".into())
    }

    /// `None` when an input is missing (nothing is sent, never "fine").
    pub fn eval(&self, src: &dyn orca_sensors::SensorSource, now_ms: i64) -> Option<(bool, f64)> {
        let a = self.lhs.eval_once(src, now_ms).ok()?.v;
        let b = self.rhs.eval_once(src, now_ms).ok()?.v;
        let r = match self.op {
            ">=" => a >= b,
            "<=" => a <= b,
            ">" => a > b,
            _ => a < b,
        };
        Some((r, a))
    }
}

/// The sensor store rebuilt from the feed's `sensors` section (for expression rules).
pub fn store_from_state(state: &Value) -> SensorStore {
    let mut s = SensorStore::new();
    if let Some(m) = state[sections::SENSORS].as_object() {
        for (k, v) in m {
            if let (Ok(id), Some(x)) = (SensorId::parse(k), v["v"].as_f64()) {
                let unit = v["u"]
                    .as_str()
                    .map(Unit::from_text)
                    .unwrap_or(Unit::Unknown);
                s.put(
                    id,
                    Reading::new(x, unit, v["ts"].as_i64().unwrap_or(0), "feed"),
                );
            }
        }
    }
    s
}

/// The condition signals for one state at `at` (seconds).
pub fn conditions(
    state: &Value,
    server: &str,
    rules: &[(String, When)],
    now_ms: i64,
) -> Vec<Signal> {
    let at = now_ms / 1000;
    let mut out = Vec::new();
    if let Some(g) = state[sections::GPUS].as_object() {
        for (card, s) in g {
            let t = [s["core_c"].as_f64(), s["mem_temp_c"].as_f64()]
                .into_iter()
                .flatten()
                .fold(f64::NAN, f64::max);
            if t.is_finite() {
                out.push(Signal::Condition {
                    rule: "hot_card".into(),
                    vars: vars(&[("card", card.clone()), ("value", format!("{t:.0}"))]),
                    active: t >= HOT_CARD_C,
                    severity: None,
                    cause: Some(format!("card:{card}")),
                    at,
                });
            }
        }
    }
    if let Some(f) = state[sections::FANS].as_object() {
        for (fan, s) in f {
            if let Some(st) = s["stalled"].as_bool() {
                out.push(Signal::Condition {
                    rule: "fan_stalled".into(),
                    vars: vars(&[
                        ("fan", fan.clone()),
                        (
                            "value",
                            s["rpm"]
                                .as_f64()
                                .map(|r| format!("{r:.0}"))
                                .unwrap_or("?".into()),
                        ),
                    ]),
                    active: st,
                    severity: None,
                    cause: None,
                    at,
                });
            }
        }
    }
    let h = &state[sections::HEALTH]["hostd"];
    if h.is_object() {
        out.push(Signal::Condition {
            rule: "hostd_down".into(),
            vars: vars(&[
                ("server", server.into()),
                (
                    "detail",
                    h["error"]
                        .as_str()
                        .or(h["status"].as_str())
                        .unwrap_or("")
                        .into(),
                ),
            ]),
            active: h["status"] != "ok",
            severity: None,
            cause: None,
            at,
        });
    }
    if !rules.is_empty() {
        let store = store_from_state(state);
        for (id, e) in rules {
            // Missing input = not firing (and not resolving a firing one into a false "ok"
            // either: a missing value simply sends nothing).
            if let Some((active, v)) = e.eval(&store, now_ms) {
                out.push(Signal::Condition {
                    rule: id.clone(),
                    vars: vars(&[("value", format!("{v:.1}"))]),
                    active,
                    severity: None,
                    cause: None,
                    at,
                });
            }
        }
    }
    out
}

/// A hub event → an event signal, for the kinds the built-in rules cover.
pub fn event_signal(kind: &str, text: &str, data: &Value, now_ms: i64) -> Option<Signal> {
    let at = now_ms / 1000;
    let s = |k: &str| data[k].as_str().unwrap_or("").to_string();
    let (rule, v) = match kind {
        "hostd.fan.failsafe" | "hostd.fan.stale" => (
            "fan_stale",
            vars(&[("fan", s("fan")), ("detail", text.into())]),
        ),
        "hostd.clock.mismatch" | "hostd.clock.drift" => (
            "clock_mismatch",
            vars(&[("card", s("card")), ("detail", text.into())]),
        ),
        "hostd.oom" => (
            "oom_kill",
            vars(&[
                ("process", s("process")),
                ("container", s("container")),
                ("detail", text.into()),
            ]),
        ),
        _ => return None,
    };
    Some(Signal::Event {
        rule: rule.into(),
        vars: v,
        severity: None,
        cause: None,
        at,
    })
}

pub fn alerts_section(e: &Engine) -> Value {
    Value::Array(
        e.alerts()
            .filter(|a| a.state == AlertState::Open)
            .map(|a| json!({"identity": a.identity, "severity": a.severity.as_str(), "text": a.message, "since": a.since, "count": a.count}))
            .collect(),
    )
}

/// Runs the engine. `extra_sinks`: e.g. the `mqtt` sink over the HA connection.
pub fn spawn(
    cfg: AlertsConfig,
    hub: Arc<TelemetryHub>,
    server: String,
    extra_sinks: Vec<Arc<dyn Sink>>,
) -> Vec<String> {
    let (mut engine, mut issues) = Engine::new(&cfg);
    let (mut sinks, si) = orca_alerts::sinks::from_config(&cfg.sinks);
    issues.extend(si);
    sinks.extend(extra_sinks);
    let mut rules: Vec<(String, When)> = Vec::new();
    for r in engine.rules().filter(|r| r.enabled) {
        if let Some(w) = &r.when {
            match When::parse(w) {
                Ok(w) => rules.push((r.id.clone(), w)),
                Err(e) => issues.push(format!("alerts.rules.{}: {e}", r.id)),
            }
        }
    }
    tokio::spawn(async move {
        let feed: Arc<dyn TelemetryFeed> = hub.clone();
        let mut mutes = calendar_mutes(&feed.snapshot().state);
        let mut sub = feed.subscribe(None).rx;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            let mut notes: Vec<Notification> = Vec::new();
            tokio::select! {
                _ = tick.tick() => {
                    let now = orca_sensors::now_ms();
                    let state = feed.snapshot().state;
                    mutes = calendar_mutes(&state);
                    let m = mutes.clone();
                    let mute = move |rule: &str, critical: bool| muted_by(&m, rule, critical, now / 1000);
                    for s in conditions(&state, &server, &rules, now) {
                        notes.extend(engine.observe(s, &mute));
                    }
                    notes.extend(engine.tick(now / 1000, &mute));
                    hub.set_section(sections::ALERTS, alerts_section(&engine));
                }
                m = sub.recv() => match m {
                    Ok(FeedMessage::Event { event, .. }) => {
                        let now = orca_sensors::now_ms();
                        let m = mutes.clone();
                        let mute = move |rule: &str, critical: bool| muted_by(&m, rule, critical, now / 1000);
                        if let Some(s) = event_signal(&event.kind, &event.text, &event.data, now) {
                            notes.extend(engine.observe(s, &mute));
                            hub.set_section(sections::ALERTS, alerts_section(&engine));
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => sub = feed.subscribe(None).rx,
                },
            }
            for n in notes {
                for (sink, r) in dispatch(&n, &sinks).await {
                    if let Err(e) = r {
                        tracing::warn!("alert {} to {sink}: {e}", n.identity);
                    }
                }
            }
        }
    });
    issues
}

/// `(selector, entry, until_s)` from the `calendar` section's `mutes`.
pub fn calendar_mutes(state: &Value) -> Vec<(String, String, i64)> {
    state["calendar"]["mutes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some((
                        m["rules"].as_str()?.to_string(),
                        m["entry"].as_str().unwrap_or("").to_string(),
                        m["until"].as_i64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The calendar entry muting `rule` now, if any. Critical alerts are never muted.
pub fn muted_by(
    mutes: &[(String, String, i64)],
    rule: &str,
    critical: bool,
    now_s: i64,
) -> Option<String> {
    if critical {
        return None;
    }
    mutes
        .iter()
        .find(|(sel, _, until)| {
            *until > now_s && orca_calendar::effects::selector_matches(sel, rule)
        })
        .map(|(_, e, _)| e.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_from_state() {
        let st = json!({
            "gpus": {"A": {"core_c": 70.0, "mem_temp_c": 86.0}, "D": {"core_c": 30.0}},
            "fans": {"duct0": {"stalled": true, "rpm": 0.0}},
            "health": {"hostd": {"status": "down", "error": "connection refused"}},
            "sensors": {"gpu.A.core": {"v": 70.0, "u": "celsius", "ts": 1000}},
        });
        let rules = vec![
            (
                "hot_nic".to_string(),
                When::parse("gpu.A.core >= 60").unwrap(),
            ),
            (
                "missing".to_string(),
                When::parse("gpu.Z.core > 1").unwrap(),
            ),
        ];
        assert!(When::parse("gpu.A.core").is_err());
        let s = conditions(&st, "gpu-host", &rules, 2_000);
        let active = |rule: &str, k: &str, v: &str| {
            s.iter().any(|x| matches!(x, Signal::Condition { rule: r, vars, active: true, .. } if r == rule && vars.get(k).map(String::as_str) == Some(v)))
        };
        assert!(active("hot_card", "card", "A"));
        assert!(!active("hot_card", "card", "D"));
        assert!(active("fan_stalled", "fan", "duct0"));
        assert!(active("hostd_down", "server", "gpu-host"));
        assert!(s.iter().any(
            |x| matches!(x, Signal::Condition { rule, active: true, .. } if rule == "hot_nic")
        ));
        // A missing input sends nothing for that rule (never "0 = fine").
        assert!(
            !s.iter()
                .any(|x| matches!(x, Signal::Condition { rule, .. } if rule == "missing"))
        );
    }

    #[test]
    fn engine_opens_hot_card_after_its_hold() {
        let (mut e, issues) = Engine::new(&AlertsConfig {
            enabled: true,
            ..Default::default()
        });
        assert!(issues.is_empty(), "{issues:?}");
        let st = json!({"gpus": {"A": {"core_c": 90.0}}});
        let mute = |_: &str, _: bool| None;
        for t in [0i64, 30, 61, 70] {
            for s in conditions(&st, "h", &[], t * 1000) {
                e.observe(s, &mute);
            }
            e.tick(t, &mute);
        }
        let a = alerts_section(&e);
        assert_eq!(a[0]["identity"], "hot_card/A", "{a}");
    }

    #[test]
    fn calendar_mutes_never_critical_and_expire() {
        let st =
            json!({"calendar": {"mutes": [{"rules": "hot_*", "entry": "video", "until": 900}]}});
        let m = calendar_mutes(&st);
        assert_eq!(
            muted_by(&m, "hot_card", false, 100).as_deref(),
            Some("video")
        );
        assert_eq!(muted_by(&m, "hot_card", true, 100), None);
        assert_eq!(muted_by(&m, "fan_stalled", false, 100), None);
        assert_eq!(muted_by(&m, "hot_card", false, 900), None);
        assert!(calendar_mutes(&json!({})).is_empty());
    }
}
