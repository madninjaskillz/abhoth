//! Alerts over Home Assistant's MQTT (SUPERVISOR.md 10.3, Q8): the `mqtt` sink payloads
//! (retained per-rule JSON plus the "Abhoth alert" event entity).
//!
//! The alert engine (orca-alerts) owns the sink trait and the `ha` mobile-notify sink
//! (`orca_alerts::sinks::HaNotify`: tags, critical priority, clear-on-resolve, tap URL).
//! [`MqttHandle`](crate::mqtt::MqttHandle) implements its `MqttPublish`, so its `MqttSink`
//! publishes through this crate's connection.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// One alert notification (open, repeat or resolve).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertNotice {
    /// `rule + dedupe parts`, e.g. `hot_card/A`. Used as the phone notification tag, so a
    /// repeat replaces the old notification and a resolve clears it.
    pub identity: String,
    pub rule: String,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub resolved: bool,
    /// Grouped children (10.2), listed in the message.
    #[serde(default)]
    pub children: Vec<String>,
    /// Tap target (the Abhoth alerts page).
    #[serde(default)]
    pub url: Option<String>,
    /// RFC 3339.
    pub at: String,
}

/// The `mqtt` sink: `abhoth/<server>/alert/<rule>` retained JSON.
pub fn mqtt_alert_payload(a: &AlertNotice) -> Value {
    json!({
        "identity": a.identity, "rule": a.rule, "severity": a.severity, "title": a.title,
        "message": a.message, "children": a.children, "state": if a.resolved { "resolved" } else { "open" }, "at": a.at,
    })
}

/// The "Abhoth alert" event entity's payload (not retained).
pub fn mqtt_event_payload(a: &AlertNotice) -> Value {
    let event_type = if a.resolved {
        "resolved"
    } else {
        match a.severity {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    };
    json!({"event_type": event_type, "identity": a.identity, "rule": a.rule, "title": a.title, "message": a.message, "at": a.at})
}

/// The event entity payload from an alert JSON published on `abhoth/<server>/alert/<rule>`
/// (this crate's [`mqtt_alert_payload`] or orca-alerts' `MqttSink` payload: `state` is
/// `open`/`firing`/`resolved`, `severity` is `info`/`warning`/`critical`).
pub fn event_from_alert_json(v: &Value) -> Option<Value> {
    let state = v.get("state")?.as_str()?;
    let event_type = if state == "resolved" {
        "resolved"
    } else {
        match v.get("severity")?.as_str()? {
            s @ ("info" | "warning" | "critical") => s,
            _ => return None,
        }
    };
    Some(json!({
        "event_type": event_type, "identity": v.get("identity"), "rule": v.get("rule"),
        "title": v.get("title"), "message": v.get("message"), "at": v.get("at"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(sev: Severity, resolved: bool) -> AlertNotice {
        AlertNotice {
            identity: "hot_card/A".into(),
            rule: "hot_card".into(),
            severity: sev,
            title: "GPU A at 86°C".into(),
            message: "core 86°C for 60 s".into(),
            resolved,
            children: vec![],
            url: Some("https://abhoth.example/supervisor/alerts".into()),
            at: "2026-11-26T10:00:00Z".into(),
        }
    }

    #[test]
    fn grouped_children_listed() {
        let mut n = notice(Severity::Critical, false);
        n.children = vec!["engine_crash/B".into()];
        assert_eq!(mqtt_event_payload(&n)["event_type"], "critical");
        assert_eq!(
            mqtt_alert_payload(&notice(Severity::Info, true))["state"],
            "resolved"
        );
        // orca-alerts' MqttSink JSON drives the same event entity.
        let g = json!({"state": "firing", "identity": "oom_kill/x", "rule": "oom_kill", "severity": "warning", "title": "t", "message": "m", "at": 1});
        assert_eq!(event_from_alert_json(&g).unwrap()["event_type"], "warning");
        assert_eq!(
            event_from_alert_json(&json!({"state": "resolved", "severity": "critical"})).unwrap()["event_type"],
            "resolved"
        );
        assert!(event_from_alert_json(&json!({"state": "firing", "severity": "panic"})).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_alert_engines_mqtt_sink_publishes_through_the_handle() {
        use orca_alerts::sinks::MqttPublish;
        let (h, mut rx) = crate::mqtt::MqttHandle::channel(1);
        let p: &dyn MqttPublish = &h;
        p.publish("abhoth/gpu/alert/hot_card", "{}", true)
            .await
            .unwrap();
        match rx.recv().await.unwrap() {
            crate::mqtt::Outbound::Publish { topic, retain, .. } => {
                assert_eq!(topic, "abhoth/gpu/alert/hot_card");
                assert!(retain);
            }
            o => panic!("{o:?}"),
        }
    }
}
