//! Alert sinks (SUPERVISOR.md 10.3). Secrets (the HA token, the ntfy token, a webhook header)
//! are read from files when a message is sent and never logged.
//!
//! - [`HaNotify`]: HA REST `POST /api/services/notify/<service>` (the mobile app), with a `tag`
//!   so a newer message replaces the older one and `clear_notification` on resolve. The
//!   primary channel (15 Q8).
//! - [`MqttSink`]: retained JSON on `abhoth/<server>/alert/<rule>` through an [`MqttPublish`]
//!   that the Home Assistant agent (`orca-homeassistant`) provides; this crate has no broker
//!   client of its own.
//! - [`Ntfy`] (optional) and [`Webhook`].
//! - [`FakeSink`] records what it was sent (tests, the admin page's "send a test").

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::engine::{Notification, NotifyKind};
use crate::rules::Severity;

#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, n: &Notification) -> Result<(), String>;
}

/// Read a secret file (trimmed). The error never contains the contents.
pub fn read_secret(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("secret file {path}: {}", e.kind()))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn body_text(n: &Notification) -> String {
    let mut s = n.message.clone();
    for c in &n.children {
        s.push_str(&format!("\n• {}", c.message));
    }
    if n.count > 1 {
        s.push_str(&format!(" (×{})", n.count));
    }
    s
}

pub struct HaNotify {
    pub name: String,
    /// `http://homeassistant.local:8123`
    pub base_url: String,
    /// `mobile_app_pixel`
    pub service: String,
    pub token_file: String,
    http: reqwest::Client,
}

impl HaNotify {
    pub fn new(name: &str, base_url: &str, service: &str, token_file: &str) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.trim_end_matches('/').into(),
            service: service.into(),
            token_file: token_file.into(),
            http: client(),
        }
    }

    /// The JSON body HA's notify service gets.
    pub fn payload(n: &Notification) -> Value {
        if n.kind == NotifyKind::Resolved {
            return json!({"message": "clear_notification", "data": {"tag": n.tag}});
        }
        let mut data = json!({"tag": n.tag, "group": "abhoth"});
        if n.severity == Severity::Critical {
            data["ttl"] = json!(0);
            data["priority"] = json!("high");
            data["push"] = json!({"interruption-level": "time-sensitive"});
        }
        json!({"title": format!("Abhoth: {}", n.title), "message": body_text(n), "data": data})
    }
}

#[async_trait::async_trait]
impl Sink for HaNotify {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, n: &Notification) -> Result<(), String> {
        let token = read_secret(&self.token_file)?;
        let url = format!("{}/api/services/notify/{}", self.base_url, self.service);
        let r = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&Self::payload(n))
            .send()
            .await
            .map_err(|e| format!("ha notify: {}", e.without_url()))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(format!("ha notify: HTTP {}", r.status().as_u16()))
        }
    }
}

/// What the HA agent's MQTT client offers this crate.
#[async_trait::async_trait]
pub trait MqttPublish: Send + Sync {
    async fn publish(&self, topic: &str, payload: &str, retain: bool) -> Result<(), String>;
}

pub struct MqttSink {
    pub server: String,
    pub publisher: Arc<dyn MqttPublish>,
}

impl MqttSink {
    pub fn topic(&self, n: &Notification) -> String {
        format!("abhoth/{}/alert/{}", self.server, n.rule)
    }
    pub fn payload(n: &Notification) -> Value {
        json!({
            "state": if n.kind == NotifyKind::Resolved { "resolved" } else { "firing" },
            "identity": n.identity,
            "rule": n.rule,
            "severity": n.severity.as_str(),
            "title": n.title,
            "message": body_text(n),
            "count": n.count,
            "at": n.at,
            "children": n.children.iter().map(|c| c.identity.clone()).collect::<Vec<_>>(),
        })
    }
}

#[async_trait::async_trait]
impl Sink for MqttSink {
    fn name(&self) -> &str {
        "mqtt"
    }
    async fn send(&self, n: &Notification) -> Result<(), String> {
        self.publisher
            .publish(&self.topic(n), &Self::payload(n).to_string(), true)
            .await
    }
}

pub struct Ntfy {
    pub url: String,
    pub topic: String,
    pub token_file: Option<String>,
    http: reqwest::Client,
}

impl Ntfy {
    pub fn new(url: &str, topic: &str, token_file: Option<&str>) -> Self {
        Self {
            url: url.trim_end_matches('/').into(),
            topic: topic.into(),
            token_file: token_file.map(String::from),
            http: client(),
        }
    }
    /// ntfy priority: 5 critical, 4 warning, 3 info, 2 resolved.
    pub fn priority(n: &Notification) -> u8 {
        if n.kind == NotifyKind::Resolved {
            return 2;
        }
        match n.severity {
            Severity::Critical => 5,
            Severity::Warning => 4,
            Severity::Info => 3,
        }
    }
}

#[async_trait::async_trait]
impl Sink for Ntfy {
    fn name(&self) -> &str {
        "ntfy"
    }
    async fn send(&self, n: &Notification) -> Result<(), String> {
        let mut rq = self
            .http
            .post(format!("{}/{}", self.url, self.topic))
            .header("Title", format!("Abhoth: {}", n.title))
            .header("Priority", Self::priority(n).to_string())
            .header("Tags", n.rule.clone())
            .body(body_text(n));
        if let Some(f) = &self.token_file {
            rq = rq.bearer_auth(read_secret(f)?);
        }
        let r = rq.send().await.map_err(|e| format!("ntfy: {}", e.without_url()))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(format!("ntfy: HTTP {}", r.status().as_u16()))
        }
    }
}

pub struct Webhook {
    pub name: String,
    pub url: String,
    /// `(header name, secret file)`
    pub header: Option<(String, String)>,
    http: reqwest::Client,
}

impl Webhook {
    pub fn new(name: &str, url: &str, header: Option<(String, String)>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            header,
            http: client(),
        }
    }
}

#[async_trait::async_trait]
impl Sink for Webhook {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, n: &Notification) -> Result<(), String> {
        let mut rq = self.http.post(&self.url).json(n);
        if let Some((h, f)) = &self.header {
            rq = rq.header(h.as_str(), read_secret(f)?);
        }
        let r = rq.send().await.map_err(|e| format!("webhook: {}", e.without_url()))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(format!("webhook: HTTP {}", r.status().as_u16()))
        }
    }
}

#[derive(Default, Clone)]
pub struct FakeSink {
    pub name: String,
    pub sent: Arc<Mutex<Vec<Notification>>>,
    pub fail: bool,
}

impl FakeSink {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
    pub fn sent(&self) -> Vec<Notification> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Sink for FakeSink {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, n: &Notification) -> Result<(), String> {
        if self.fail {
            return Err(format!("{}: refused", self.name));
        }
        self.sent.lock().unwrap().push(n.clone());
        Ok(())
    }
}

/// Send `n` to the sinks it names (unknown names are reported, not fatal).
pub async fn dispatch(n: &Notification, sinks: &[Arc<dyn Sink>]) -> Vec<(String, Result<(), String>)> {
    let mut out = Vec::new();
    for name in &n.sinks {
        match sinks.iter().find(|s| s.name() == name) {
            Some(s) => out.push((name.clone(), s.send(n).await)),
            None => out.push((name.clone(), Err(format!("no sink '{name}' is configured")))),
        }
    }
    out
}

/// Build sinks from `[alerts.sinks.<name>]` tables. `type` = `ha` | `ntfy` | `webhook`;
/// `mqtt` sinks are added by the HA agent with its publisher.
pub fn from_config(cfg: &indexmap::IndexMap<String, Value>) -> (Vec<Arc<dyn Sink>>, Vec<String>) {
    let mut out: Vec<Arc<dyn Sink>> = Vec::new();
    let mut issues = Vec::new();
    for (name, v) in cfg {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let ty = s("type").unwrap_or_else(|| name.clone());
        match ty.as_str() {
            "ha" => match (s("url"), s("service"), s("token_file")) {
                (Some(u), Some(svc), Some(t)) => out.push(Arc::new(HaNotify::new(name, &u, &svc, &t))),
                _ => issues.push(format!("alerts.sinks.{name}: ha needs url, service and token_file")),
            },
            "ntfy" => match (s("url"), s("topic")) {
                (Some(u), Some(t)) => out.push(Arc::new(Ntfy::new(&u, &t, s("token_file").as_deref()))),
                _ => issues.push(format!("alerts.sinks.{name}: ntfy needs url and topic")),
            },
            "webhook" => match s("url") {
                Some(u) => {
                    let header = s("header").zip(s("secret_file"));
                    out.push(Arc::new(Webhook::new(name, &u, header)))
                }
                None => issues.push(format!("alerts.sinks.{name}: webhook needs url")),
            },
            "mqtt" => {}
            other => issues.push(format!("alerts.sinks.{name}: unknown type '{other}'")),
        }
    }
    (out, issues)
}
