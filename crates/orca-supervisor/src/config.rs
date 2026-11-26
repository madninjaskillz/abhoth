//! The supervisor's config file. One TOML with:
//!
//! - `[supervisor]`: this server, how to reach hostd and the router, where to serve the feed;
//! - the policy tables hostd enforces: `[cards.*]`, `[sensors.*]`, `[power.*]`,
//!   `[fan_backends.*]`, `[fan_defaults]`, `[fan_profiles.*]`, `[fans.*]` (sent as
//!   `policy.put` when `push_policy = true`; hostd validates them against its limits);
//! - `[mqtt]` and `[homeassistant]` (orca-homeassistant, both off unless `enabled`);
//! - `[alerts]` (orca-alerts, off unless `enabled`); `[calendar]` is the router's and is
//!   only accepted here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orca_homeassistant::{HomeAssistantConfig, MqttConfig};
use serde::Deserialize;

/// A secret given by reference: `{ file = "…" }` or `{ env = "…" }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub file: Option<String>,
    pub env: Option<String>,
}

impl SecretRef {
    pub fn resolve(&self) -> Result<String, String> {
        match (&self.file, &self.env) {
            (Some(f), None) => std::fs::read_to_string(f)
                .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
                .map_err(|e| format!("secret file {f}: {e}")),
            (None, Some(e)) => std::env::var(e).map_err(|_| format!("secret env {e} is not set")),
            _ => Err("a secret needs exactly one of file or env".into()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSection {
    /// Slug used in MQTT topics and unique ids (`deepthought`).
    pub server: String,
    /// Display name (`DeepThought`).
    pub name: String,
    /// hostd's socket on this host.
    #[serde(default)]
    pub hostd_socket: Option<PathBuf>,
    /// Or a command whose stdin/stdout is the hostd protocol (the forced-command SSH relay).
    #[serde(default)]
    pub hostd_command: Option<Vec<String>>,
    /// Send the policy tables to hostd at start and on reload. Off: hostd keeps the policy it
    /// has (its last-known-good), and the tables here only describe the cards.
    #[serde(default)]
    pub push_policy: bool,
    /// The router's admin API base (`http://127.0.0.1:8099`). Without it there are no engine
    /// sections and no layout / card controls.
    #[serde(default)]
    pub router_url: Option<String>,
    /// A router key with the HA role (plan, switch, card state).
    #[serde(default)]
    pub router_key: Option<SecretRef>,
    /// The router's server id for this host, if it differs from `server`.
    #[serde(default)]
    pub router_server: Option<String>,
    /// Serve the feed (`/api/v1/supervisor/snapshot`, `/stream`) here, e.g. `127.0.0.1:9890`.
    #[serde(default)]
    pub listen: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub supervisor: SupervisorSection,
    /// The policy tables as TOML text (for `orca_hostd::policy::from_toml`).
    pub policy_toml: String,
    /// Card labels with `allow_cap` (for HA's per-card cap selects).
    pub cards: Vec<(String, bool)>,
    pub mqtt: Option<MqttConfig>,
    pub homeassistant: Option<HomeAssistantConfig>,
    pub alerts: Option<orca_alerts::AlertsConfig>,
    /// Tables accepted but not wired in this build (reported by `check`).
    pub not_wired: Vec<String>,
}

const POLICY_KEYS: [&str; 7] = [
    "cards",
    "sensors",
    "power",
    "fan_backends",
    "fan_defaults",
    "fan_profiles",
    "fans",
];

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut t: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let supervisor: SupervisorSection = t
            .remove("supervisor")
            .ok_or("[supervisor] is missing")?
            .try_into()
            .map_err(|e| format!("supervisor: {e}"))?;
        if supervisor.hostd_socket.is_some() && supervisor.hostd_command.is_some() {
            return Err("supervisor: hostd_socket and hostd_command are exclusive".into());
        }
        let mqtt = t
            .remove("mqtt")
            .map(|v| v.try_into::<MqttConfig>())
            .transpose()
            .map_err(|e| format!("mqtt: {e}"))?;
        let homeassistant = t
            .remove("homeassistant")
            .map(|v| v.try_into::<HomeAssistantConfig>())
            .transpose()
            .map_err(|e| format!("homeassistant: {e}"))?;
        let alerts = t
            .remove("alerts")
            .map(|v| v.try_into::<orca_alerts::AlertsConfig>())
            .transpose()
            .map_err(|e| format!("alerts: {e}"))?;
        let mut not_wired = Vec::new();
        // The calendar lives in the router (serve --control); the supervisor reads its effects.
        if t.remove("calendar").is_some() {
            not_wired.push("calendar".to_string());
        }
        let mut policy = toml::Table::new();
        for k in POLICY_KEYS {
            if let Some(v) = t.remove(k) {
                policy.insert(k.into(), v);
            }
        }
        if let Some(k) = t.keys().next() {
            return Err(format!("unknown table {k:?}"));
        }
        let power = policy.get("power").and_then(|v| v.as_table());
        let cards = policy
            .get("cards")
            .and_then(|v| v.as_table())
            .map(|c| {
                c.keys()
                    .map(|l| {
                        let cap = power
                            .and_then(|p| p.get(l))
                            .and_then(|v| v.clone().try_into::<orca_gpuctl::CardPowerConfig>().ok())
                            .map(|p| p.cap_offered())
                            .unwrap_or(false);
                        (l.clone(), cap)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Config {
            supervisor,
            policy_toml: toml::to_string(&policy).map_err(|e| e.to_string())?,
            cards,
            mqtt,
            homeassistant,
            alerts,
            not_wired,
        })
    }

    /// Problems that stop `run` (empty = fine). Secrets are resolved to check they exist.
    pub fn check(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.supervisor.hostd_socket.is_none() && self.supervisor.hostd_command.is_none() {
            out.push("supervisor: set hostd_socket or hostd_command".into());
        }
        if self.supervisor.router_url.is_some() != self.supervisor.router_key.is_some() {
            out.push("supervisor: router_url and router_key go together".into());
        }
        if let Some(k) = &self.supervisor.router_key
            && let Err(e) = k.resolve()
        {
            out.push(format!("supervisor.router_key: {e}"));
        }
        if let Some(m) = &self.mqtt
            && m.enabled
        {
            out.extend(m.validate().into_iter().map(|e| format!("mqtt: {e}")));
            for (n, s) in [("username", &m.username), ("password", &m.password)] {
                if let Some(s) = s
                    && let Err(e) = s.resolve()
                {
                    out.push(format!("mqtt.{n}: {e}"));
                }
            }
        }
        if let Some(h) = &self.homeassistant
            && h.enabled
        {
            out.extend(
                h.validate()
                    .into_iter()
                    .map(|e| format!("homeassistant: {e}")),
            );
        }
        if let Some(a) = &self.alerts
            && a.enabled
        {
            let (_, issues) = orca_alerts::Engine::new(a);
            out.extend(issues.into_iter().map(|e| format!("alerts: {e}")));
        }
        // The policy tables must at least build (hostd checks them against its limits).
        if let Err(e) = self.policy(0) {
            out.push(format!("policy: {e}"));
        }
        out
    }

    /// The policy document for hostd, with `rev`. Fans are compiled with orca-fans' default
    /// limits and every kind allowed; hostd re-checks against its own limits file.
    pub fn policy(&self, rev: u64) -> Result<orca_hostd::protocol::PolicyDoc, String> {
        let lim = orca_hostd::limits::Limits::parse(
            "[backends]\nwrite = [\"esphome\", \"hwmon\", \"ipmi\", \"liquidctl\", \"nvml\", \"windows\", \"fake\"]\n",
        )?;
        orca_hostd::policy::from_toml(&self.policy_toml, &lim, rev)
    }

    pub fn router_server(&self) -> &str {
        self.supervisor
            .router_server
            .as_deref()
            .unwrap_or(&self.supervisor.server)
    }
}

pub type CardCaps = BTreeMap<String, bool>;

#[cfg(test)]
mod tests {
    use super::*;

    pub const EXAMPLE: &str = r#"
[supervisor]
server = "gpu-host"
name = "GPU host"
hostd_socket = "/run/abhoth/hostd.sock"

[cards.A]
uuid = "GPU-a"
[cards.B]
uuid = "GPU-b"
[power.A]
serving = "unlocked"
allow_cap = false
[power.B]
serving = "unlocked"
[sensors.ctrl_A]
expr = "first(gpu.A.hbm, gpu.A.core)"

[mqtt]
enabled = false
url = "mqtt://broker.invalid:1883"
client_id = "abhoth-supervisor-gpu-host"

[alerts]
enabled = false

[calendar]
x = 1
"#;

    #[test]
    fn deploy_example_parses() {
        let c = Config::parse(include_str!("../deploy/supervisor.example.toml")).unwrap();
        assert_eq!(c.cards, vec![("A".into(), false)]);
        assert!(c.policy(1).is_ok());
    }

    #[test]
    fn parses_and_splits() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.supervisor.server, "gpu-host");
        assert_eq!(c.cards, vec![("A".into(), false), ("B".into(), true)]);
        assert_eq!(c.not_wired, vec!["calendar".to_string()]);
        assert!(c.alerts.is_some());
        assert!(c.check().is_empty(), "{:?}", c.check());
        let p = c.policy(3).unwrap();
        assert_eq!(p.rev, 3);
        assert_eq!(p.cards.len(), 2);
        assert!(Config::parse("[supervisor]\nserver = \"x\"\nname = \"x\"\n[bogus]\n").is_err());
    }
}
