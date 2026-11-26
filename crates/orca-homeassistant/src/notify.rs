//! Alerts to Home Assistant (SUPERVISOR.md 10.3, Q8): the `ha` sink (mobile notify through
//! the HA REST notify service) and the `mqtt` sink payloads (retained per-rule JSON plus the
//! "Abhoth alert" event entity).
//!
//! The alert engine (orca-alerts) owns the sink trait; [`AlertNotice`] is this crate's view
//! of one notification, and the adapter to that trait is a few lines in the supervisor.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::rest::HaRest;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveAction {
    /// Remove the phone notification (`clear_notification` with the tag).
    Clear,
    /// Replace it with a "resolved" notification.
    Notify,
}

/// The HA `notify.<service>` call body for an alert.
pub fn notify_payload(a: &AlertNotice, on_resolve: ResolveAction) -> Value {
    let tag = format!("abhoth-{}", a.identity.replace('/', "-"));
    if a.resolved && on_resolve == ResolveAction::Clear {
        return json!({"message": "clear_notification", "data": {"tag": tag}});
    }
    let mut message = a.message.clone();
    if !a.children.is_empty() {
        message.push_str("\n• ");
        message.push_str(&a.children.join("\n• "));
    }
    let title = if a.resolved {
        format!("Resolved: {}", a.title)
    } else {
        a.title.clone()
    };
    let mut data = json!({"tag": tag, "group": "abhoth"});
    if let Some(u) = &a.url {
        data["url"] = json!(u); // iOS
        data["clickAction"] = json!(u); // Android
    }
    if a.severity == Severity::Critical && !a.resolved {
        // Android: deliver at once, even in doze; its own channel so the user can make it loud.
        data["ttl"] = json!(0);
        data["priority"] = json!("high");
        data["channel"] = json!("Abhoth critical");
        // iOS: time-sensitive interruption level.
        data["push"] = json!({"interruption-level": "time-sensitive"});
    } else {
        data["channel"] = json!("Abhoth");
    }
    json!({"title": title, "message": message, "data": data})
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

/// Something that delivers alert notices (the shape the alert engine's sink trait adapts to).
#[async_trait]
pub trait NoticeSink: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notice: &AlertNotice) -> anyhow::Result<()>;
}

/// The `ha` sink: HA REST `POST /api/services/notify/<service>`. Only the one configured
/// service is ever called.
pub struct HaNotifySink {
    rest: HaRest,
    service: String,
    on_resolve: ResolveAction,
}

impl HaNotifySink {
    pub fn new(rest: HaRest, service: &str, on_resolve: ResolveAction) -> anyhow::Result<Self> {
        anyhow::ensure!(
            crate::config::valid_slug(service),
            "notify service {service:?} is not a service name"
        );
        Ok(Self {
            rest,
            service: service.to_string(),
            on_resolve,
        })
    }
}

#[async_trait]
impl NoticeSink for HaNotifySink {
    fn name(&self) -> &str {
        "ha"
    }

    async fn send(&self, notice: &AlertNotice) -> anyhow::Result<()> {
        self.rest
            .call_service(
                "notify",
                &self.service,
                &notify_payload(notice, self.on_resolve),
            )
            .await
    }
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
    fn notify_bodies() {
        let w = notify_payload(&notice(Severity::Warning, false), ResolveAction::Clear);
        assert_eq!(w["data"]["tag"], "abhoth-hot_card-A");
        assert_eq!(
            w["data"]["clickAction"],
            "https://abhoth.example/supervisor/alerts"
        );
        assert!(w["data"].get("priority").is_none());
        let c = notify_payload(&notice(Severity::Critical, false), ResolveAction::Clear);
        assert_eq!(c["data"]["priority"], "high");
        assert_eq!(c["data"]["ttl"], 0);
        let r = notify_payload(&notice(Severity::Critical, true), ResolveAction::Clear);
        assert_eq!(
            r,
            json!({"message": "clear_notification", "data": {"tag": "abhoth-hot_card-A"}})
        );
        let r2 = notify_payload(&notice(Severity::Critical, true), ResolveAction::Notify);
        assert_eq!(r2["title"], "Resolved: GPU A at 86°C");
        assert!(r2["data"].get("priority").is_none());
    }

    #[test]
    fn grouped_children_listed() {
        let mut n = notice(Severity::Critical, false);
        n.children = vec!["engine_crash/B".into()];
        assert!(
            notify_payload(&n, ResolveAction::Clear)["message"]
                .as_str()
                .unwrap()
                .ends_with("• engine_crash/B")
        );
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
}
