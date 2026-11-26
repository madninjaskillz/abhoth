//! `[mqtt]` and `[homeassistant]` (SUPERVISOR.md 11.1). Secrets are references only.

use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// `{ file = "/run/secrets/x" }` or `{ env = "NAME" }`. Never a value (same shape as the
/// router's `SecretRef`, so one config section reads the same everywhere).
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The reference itself is not secret, but say so explicitly: no value is ever held here.
        f.debug_struct("SecretRef")
            .field("file", &self.file)
            .field("env", &self.env)
            .finish()
    }
}

/// A resolved secret. `Debug` and `Display` never show the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretError {
    #[error("a secret reference needs exactly one of `file` or `env`")]
    Shape,
    #[error("environment variable {0} is not set or not unicode")]
    Env(String),
    #[error("secret file {0} could not be read")]
    File(String),
    #[error("secret from {0} is empty")]
    Empty(String),
}

impl SecretRef {
    pub fn env(name: &str) -> Self {
        Self {
            file: None,
            env: Some(name.to_string()),
        }
    }

    pub fn file(path: &str) -> Self {
        Self {
            file: Some(path.to_string()),
            env: None,
        }
    }

    /// Reads the value. A trailing newline in a file is dropped (as `echo x > file` leaves one).
    pub fn resolve(&self) -> Result<Secret, SecretError> {
        let (value, from) = match (&self.file, &self.env) {
            (Some(path), None) => {
                let v =
                    std::fs::read_to_string(path).map_err(|_| SecretError::File(path.clone()))?;
                (v.trim_end_matches(['\r', '\n']).to_string(), path.clone())
            }
            (None, Some(name)) => (
                std::env::var(name).map_err(|_| SecretError::Env(name.clone()))?,
                format!("${name}"),
            ),
            _ => return Err(SecretError::Shape),
        };
        if value.is_empty() {
            return Err(SecretError::Empty(from));
        }
        Ok(Secret(value))
    }
}

fn d_true() -> bool {
    true
}
fn d_base_topic() -> String {
    "abhoth".into()
}
fn d_discovery_prefix() -> String {
    "homeassistant".into()
}
fn d_state_interval() -> u64 {
    10
}
fn d_keep_alive() -> u64 {
    30
}
fn d_legacy_card() -> String {
    "B".into()
}
fn d_clock_cap_options() -> Vec<u32> {
    vec![1200, 1300, 1400, 1500]
}
fn d_clock_cap_ttl() -> u64 {
    3600
}
fn d_scopes() -> Vec<String> {
    ["switch", "card_state", "clock_cap", "fan_profile"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn d_read_interval() -> u64 {
    20
}

/// `[mqtt]`. Off unless `enabled = true` (the whole HA integration sits behind this flag).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `mqtt://host:1883` or `mqtts://host:8883`.
    pub url: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
    /// PEM CA bundle for `mqtts://` with a private CA. Without it the system roots are used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_file: Option<String>,
    #[serde(default = "d_base_topic")]
    pub base_topic: String,
    #[serde(default = "d_discovery_prefix")]
    pub discovery_prefix: String,
    /// Reuse ha-bridge's `deepthought_*` unique_ids for the entities that replace them (Q7), so
    /// HA history and automations carry on. Only turn on once ha-bridge is stopped: two
    /// publishers with the same unique_ids fight over the entities.
    #[serde(default)]
    pub legacy_ids: bool,
    /// The card ha-bridge saw (its container had only this card): the legacy GPU sensors map here.
    #[serde(default = "d_legacy_card")]
    pub legacy_card: String,
    /// At startup, clear ha-bridge's retained state/availability/settings topics (cutover).
    #[serde(default)]
    pub clear_legacy_topics: bool,
    /// Extra discovery topics this publisher no longer uses; cleared (retained empty) at startup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_discovery: Vec<String>,
    #[serde(default = "d_state_interval")]
    pub state_interval_seconds: u64,
    #[serde(default = "d_keep_alive")]
    pub keep_alive_seconds: u64,
    /// Offer HA controls at all (selects, switches, buttons). Sensors are always published.
    #[serde(default = "d_true")]
    pub controls: bool,
    /// The scopes HA commands act with (the MQTT service principal, D-SV12). Dropping one
    /// removes its controls from discovery and refuses its commands.
    #[serde(default = "d_scopes")]
    pub scopes: Vec<String>,
    #[serde(default = "d_clock_cap_options")]
    pub clock_cap_options: Vec<u32>,
    #[serde(default = "d_clock_cap_ttl")]
    pub clock_cap_ttl_seconds: u64,
}

impl MqttConfig {
    pub fn state_interval(&self) -> Duration {
        Duration::from_secs(self.state_interval_seconds.max(1))
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Host, port and TLS from `url`.
    pub fn endpoint(&self) -> Result<Endpoint, String> {
        let (tls, rest) = if let Some(r) = self.url.strip_prefix("mqtts://") {
            (true, r)
        } else if let Some(r) = self.url.strip_prefix("mqtt://") {
            (false, r)
        } else {
            return Err(format!(
                "mqtt.url must start with mqtt:// or mqtts:// (got {:?})",
                self.url
            ));
        };
        let rest = rest.trim_end_matches('/');
        if rest.contains('@') {
            return Err(
                "mqtt.url must not carry credentials; use username/password references".into(),
            );
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .map_err(|_| format!("bad port in mqtt.url: {p:?}"))?,
            ),
            None => (rest, if tls { 8883 } else { 1883 }),
        };
        if host.is_empty() {
            return Err("mqtt.url has no host".into());
        }
        Ok(Endpoint {
            host: host.to_string(),
            port,
            tls,
        })
    }

    /// Config problems, as `config check` would print them.
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = self.endpoint() {
            out.push(e);
        }
        for (what, r) in [("username", &self.username), ("password", &self.password)] {
            if let Some(r) = r
                && r.file.is_some() == r.env.is_some()
            {
                out.push(format!("mqtt.{what} needs exactly one of file or env"));
            }
        }
        if self.client_id.trim().is_empty() {
            out.push("mqtt.client_id is empty".into());
        }
        for t in [&self.base_topic, &self.discovery_prefix] {
            if t.is_empty() || t.contains(['#', '+']) || t.starts_with('/') || t.ends_with('/') {
                out.push(format!("bad topic prefix {t:?}"));
            }
        }
        for s in &self.scopes {
            if !d_scopes().contains(s) {
                out.push(format!("mqtt.scopes: unknown scope {s:?}"));
            }
        }
        if self.clock_cap_ttl_seconds == 0 || self.clock_cap_ttl_seconds > 3600 {
            out.push(
                "mqtt.clock_cap_ttl_seconds must be 1..=3600 (overrides expire within 1 h)".into(),
            );
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// `[homeassistant]`: the HA REST API, for state reads (EcoFlow) and mobile notify.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistantConfig {
    #[serde(default)]
    pub enabled: bool,
    pub url: String,
    pub token: SecretRef,
    /// `{ name = "sensor.entity_id" }`: each becomes an `ha.<name>` reading.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub reads: IndexMap<String, String>,
    #[serde(default = "d_read_interval")]
    pub read_interval_seconds: u64,
    /// The `notify.<service>` for alerts (the phone's mobile_app service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_service: Option<String>,
}

impl HomeAssistantConfig {
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !(self.url.starts_with("http://") || self.url.starts_with("https://")) {
            out.push("homeassistant.url must be http(s)://".into());
        }
        if self.token.file.is_some() == self.token.env.is_some() {
            out.push("homeassistant.token needs exactly one of file or env".into());
        }
        for (k, e) in &self.reads {
            if !valid_entity_id(e) {
                out.push(format!(
                    "homeassistant.reads.{k}: {e:?} is not an entity id"
                ));
            }
        }
        if let Some(s) = &self.notify_service
            && !valid_slug(s)
        {
            out.push(format!(
                "homeassistant.notify_service {s:?} is not a service name"
            ));
        }
        out
    }
}

pub(crate) fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub(crate) fn valid_entity_id(s: &str) -> bool {
    matches!(s.split_once('.'), Some((d, o)) if valid_slug(d) && valid_slug(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> MqttConfig {
        toml::from_str(&format!("url = {url:?}\nclient_id = \"x\"")).unwrap()
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            cfg("mqtt://ha.lan").endpoint().unwrap(),
            Endpoint {
                host: "ha.lan".into(),
                port: 1883,
                tls: false
            }
        );
        assert_eq!(cfg("mqtts://ha.lan").endpoint().unwrap().port, 8883);
        assert_eq!(cfg("mqtt://10.0.0.2:1884/").endpoint().unwrap().port, 1884);
        assert!(cfg("http://x").endpoint().is_err());
        assert!(cfg("mqtt://u:p@x").endpoint().is_err());
    }

    #[test]
    fn defaults_and_validation() {
        let c = cfg("mqtt://ha.lan:1883");
        assert!(!c.enabled);
        assert!(!c.legacy_ids);
        assert_eq!(c.base_topic, "abhoth");
        assert_eq!(c.legacy_card, "B");
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        let bad: MqttConfig = toml::from_str(
            "url = \"mqtt://x\"\nclient_id = \"x\"\nscopes = [\"root\"]\npassword = { file = \"a\", env = \"B\" }",
        )
        .unwrap();
        assert_eq!(bad.validate().len(), 2);
        // A literal password is not a reference: refused at parse.
        assert!(
            toml::from_str::<MqttConfig>(
                "url = \"mqtt://x\"\nclient_id = \"x\"\npassword = \"hunter2\""
            )
            .is_err()
        );
    }

    #[test]
    fn secrets_resolve_and_redact() {
        let dir = std::env::temp_dir().join(format!("orca-ha-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pw");
        std::fs::write(&f, "s3cret\n").unwrap();
        let s = SecretRef::file(f.to_str().unwrap()).resolve().unwrap();
        assert_eq!(s.expose(), "s3cret");
        assert_eq!(format!("{s:?} {s}"), "Secret(<redacted>) <redacted>");
        assert_eq!(
            SecretRef::env("ORCA_HA_TEST_SURELY_UNSET").resolve(),
            Err(SecretError::Env("ORCA_HA_TEST_SURELY_UNSET".into()))
        );
        assert_eq!(SecretRef::default().resolve(), Err(SecretError::Shape));
        std::fs::write(&f, "\n").unwrap();
        assert!(matches!(
            SecretRef::file(f.to_str().unwrap()).resolve(),
            Err(SecretError::Empty(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ha_config() {
        let c: HomeAssistantConfig = toml::from_str(
            "url = \"http://ha.lan:8123\"\ntoken = { file = \"/run/secrets/ha-token\" }\nreads = { ecoflow_ac = \"sensor.ecoflow_ac_out\" }\nnotify_service = \"mobile_app_phone\"",
        )
        .unwrap();
        assert!(c.validate().is_empty());
        assert_eq!(c.read_interval_seconds, 20);
        let mut bad = c.clone();
        bad.reads.insert("x".into(), "nope".into());
        bad.notify_service = Some("notify.x".into());
        assert_eq!(bad.validate().len(), 2);
    }
}
