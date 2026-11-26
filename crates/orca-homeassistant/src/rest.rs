//! The HA REST client: state reads (`[homeassistant].reads`, e.g. the EcoFlow AC output that
//! Glance shows) and service calls (notify). The token comes from a file or env reference.

use std::time::Duration;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{HomeAssistantConfig, Secret, valid_entity_id, valid_slug};

#[derive(Clone)]
pub struct HaRest {
    base: String,
    token: Secret,
    http: reqwest::Client,
}

impl std::fmt::Debug for HaRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HaRest").field("base", &self.base).field("token", &self.token).finish()
    }
}

/// `GET /api/states/<entity>`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HaEntityState {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: serde_json::Map<String, Value>,
    #[serde(default)]
    pub last_updated: Option<String>,
}

/// One `ha.<name>` reading for the telemetry hub. `value` is `None` when HA says
/// `unavailable`/`unknown` or the state is not a number (never 0, D-SV4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaReading {
    pub name: String,
    pub entity_id: String,
    pub value: Option<f64>,
    pub unit: Option<String>,
    pub raw: Option<String>,
    pub error: Option<String>,
}

impl HaRest {
    pub fn new(base_url: &str, token: Secret) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
        Ok(Self { base: base_url.trim_end_matches('/').to_string(), token, http })
    }

    pub fn from_config(c: &HomeAssistantConfig) -> anyhow::Result<Self> {
        let token = c.token.resolve().context("homeassistant.token")?;
        Self::new(&c.url, token)
    }

    pub async fn get_state(&self, entity_id: &str) -> anyhow::Result<HaEntityState> {
        if !valid_entity_id(entity_id) {
            bail!("{entity_id:?} is not an entity id");
        }
        let r = self
            .http
            .get(format!("{}/api/states/{entity_id}", self.base))
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HA unreachable: {}", e.without_url()))?;
        let code = r.status();
        if !code.is_success() {
            bail!("HA answered HTTP {} for {entity_id}", code.as_u16());
        }
        Ok(r.json().await?)
    }

    /// `POST /api/services/<domain>/<service>`.
    pub async fn call_service(&self, domain: &str, service: &str, body: &Value) -> anyhow::Result<()> {
        if !valid_slug(domain) || !valid_slug(service) {
            bail!("bad service name {domain}.{service}");
        }
        let r = self
            .http
            .post(format!("{}/api/services/{domain}/{service}", self.base))
            .bearer_auth(self.token.expose())
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HA unreachable: {}", e.without_url()))?;
        let code = r.status();
        if !code.is_success() {
            bail!("HA answered HTTP {} for {domain}.{service}", code.as_u16());
        }
        Ok(())
    }

    /// Reads every configured entity once.
    pub async fn read_all(&self, reads: &IndexMap<String, String>) -> Vec<HaReading> {
        let mut out = Vec::with_capacity(reads.len());
        for (name, entity) in reads {
            out.push(match self.get_state(entity).await {
                Ok(s) => to_reading(name, s),
                Err(e) => HaReading {
                    name: name.clone(),
                    entity_id: entity.clone(),
                    value: None,
                    unit: None,
                    raw: None,
                    error: Some(e.to_string()),
                },
            });
        }
        out
    }
}

pub fn to_reading(name: &str, s: HaEntityState) -> HaReading {
    let value = match s.state.as_str() {
        "unavailable" | "unknown" | "" => None,
        v => v.parse::<f64>().ok().filter(|x| x.is_finite()),
    };
    HaReading {
        name: name.to_string(),
        entity_id: s.entity_id,
        value,
        unit: s.attributes.get("unit_of_measurement").and_then(Value::as_str).map(String::from),
        raw: Some(s.state),
        error: None,
    }
}

/// Polls `reads` every `read_interval_seconds` and sends each round to `tx` (the hub turns
/// them into `ha.<name>` sensors). Ends when the receiver is dropped.
pub async fn poll_reads(rest: HaRest, cfg: HomeAssistantConfig, tx: tokio::sync::mpsc::Sender<Vec<HaReading>>) {
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.read_interval_seconds.max(5)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let round = rest.read_all(&cfg.reads).await;
        if tx.send(round).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn readings_never_zero_on_missing() {
        let st = |s: &str| HaEntityState {
            entity_id: "sensor.ac_out".into(),
            state: s.into(),
            attributes: json!({"unit_of_measurement": "W"}).as_object().unwrap().clone(),
            last_updated: None,
        };
        assert_eq!(to_reading("ecoflow_ac", st("412.5")).value, Some(412.5));
        assert_eq!(to_reading("ecoflow_ac", st("412.5")).unit.as_deref(), Some("W"));
        assert_eq!(to_reading("ecoflow_ac", st("unavailable")).value, None);
        assert_eq!(to_reading("ecoflow_ac", st("nan")).value, None);
    }
}
