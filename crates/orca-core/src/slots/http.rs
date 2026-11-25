//! Client for the pig-slots service (`HttpSlotRegistry`, the protocol PiG's subagent/slots.ts
//! speaks) and the copies' health probe (`HttpSlotHealthProbe`).
//!
//! - POST /claim {key, owner, host, pid, label, kind, copy? | slot? | avoid?} -> {ok, slot, copy, ttl} or {ok:false, reason}
//! - POST /renew {keys} -> {renewed, lost}
//! - POST /release {key} or {host, pid} -> {ok, released}
//! - GET /list -> {slots: {name: holder | null}}
//!
//! Leases expire after the server's TTL (90 s) unless renewed. Transport errors and non-2xx
//! answers are errors, so the allocator can fall back to the local file.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::registry::{SlotClaim, SlotClaimResult, SlotError, SlotHealthProbe, SlotRegistry};

pub struct HttpSlotRegistry {
    http: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

fn err(e: impl std::fmt::Display) -> SlotError {
    SlotError(e.to_string())
}

fn str_of(v: &Value, name: &str) -> Option<String> {
    v.get(name).and_then(Value::as_str).map(str::to_string)
}

impl HttpSlotRegistry {
    pub fn new(base_url: &str, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout,
        }
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, SlotError> {
        // A buffered body with Content-Length: pig-slots (a plain Python server) rejects
        // chunked uploads.
        let r = self
            .http
            .post(format!("{}{path}", self.base_url))
            .timeout(self.timeout)
            .header("content-type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(err)?;
        if !r.status().is_success() {
            return Err(SlotError(format!(
                "Response status code does not indicate success: {}",
                r.status().as_u16()
            )));
        }
        let text = r.text().await.map_err(err)?;
        serde_json::from_str(&text).map_err(err)
    }
}

#[async_trait]
impl SlotRegistry for HttpSlotRegistry {
    async fn claim(&self, claim: &SlotClaim) -> Result<SlotClaimResult, SlotError> {
        let mut body = Map::new();
        body.insert("key".into(), json!(claim.key));
        body.insert("owner".into(), json!(claim.owner));
        body.insert("host".into(), json!(claim.host));
        body.insert("pid".into(), json!(claim.pid));
        body.insert("label".into(), json!(claim.label));
        body.insert("kind".into(), json!(claim.kind));
        if let Some(slot) = &claim.slot {
            body.insert("slot".into(), json!(slot));
        } else if let Some(copy) = &claim.copy {
            body.insert("copy".into(), json!(copy));
        }
        if let Some(avoid) = claim.avoid.as_ref().filter(|a| !a.is_empty()) {
            body.insert("avoid".into(), json!(avoid));
        }
        let r = self.post("/claim", Value::Object(body)).await?;
        if r.get("ok") != Some(&Value::Bool(true)) {
            return Ok(SlotClaimResult {
                ok: false,
                reason: Some(str_of(&r, "reason").unwrap_or_else(|| "full".into())),
                ..SlotClaimResult::default()
            });
        }
        let slot = str_of(&r, "slot");
        Ok(SlotClaimResult {
            ok: true,
            copy: str_of(&r, "copy").or_else(|| slot.as_ref().map(|s| s.chars().take(1).collect())),
            slot,
            ttl: r.get("ttl").and_then(Value::as_f64),
            reason: None,
        })
    }

    async fn renew(&self, keys: &[String]) -> Result<Vec<String>, SlotError> {
        let r = self.post("/renew", json!({"keys": keys})).await?;
        Ok(r.get("lost")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn release(&self, key: &str) -> Result<(), SlotError> {
        self.post("/release", json!({"key": key})).await.map(|_| ())
    }

    async fn release_all(&self, host: &str, pid: u32) -> Result<(), SlotError> {
        self.post("/release", json!({"host": host, "pid": pid}))
            .await
            .map(|_| ())
    }

    async fn busy_slots(&self) -> Result<Option<Vec<String>>, SlotError> {
        let r = self
            .http
            .get(format!("{}/list", self.base_url))
            .timeout(self.timeout)
            .send()
            .await
            .map_err(err)?;
        if !r.status().is_success() {
            return Err(SlotError(format!(
                "Response status code does not indicate success: {}",
                r.status().as_u16()
            )));
        }
        let v: Value = serde_json::from_str(&r.text().await.map_err(err)?).map_err(err)?;
        let Some(slots) = v.get("slots").and_then(Value::as_object) else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(
            slots
                .iter()
                .filter(|(_, v)| !matches!(v, Value::Null | Value::Bool(false)))
                .map(|(k, _)| k.clone())
                .collect(),
        ))
    }
}

/// GET {copy root}/health. A copy that refuses the connection or answers non-2xx is "down"; one
/// that is merely slow gets a second, longer try before it counts as down.
pub struct HttpSlotHealthProbe {
    http: reqwest::Client,
    health_urls: HashMap<String, String>,
    timeout: Duration,
    slow_timeout: Duration,
}

impl HttpSlotHealthProbe {
    /// `timeout`: the first try's (slots.healthTimeoutMs); `slow_timeout`: the second try's,
    /// default 4 x timeout, at least 10 s.
    pub fn new(
        copy_base_urls: &[(String, String)],
        timeout: Duration,
        slow_timeout: Option<Duration>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            health_urls: copy_base_urls
                .iter()
                .map(|(c, u)| (c.to_lowercase(), Self::health_url(u)))
                .collect(),
            timeout,
            slow_timeout: slow_timeout.unwrap_or((timeout * 4).max(Duration::from_secs(10))),
        }
    }

    /// "http://h:8080/v1" -> "http://h:8080/health".
    pub fn health_url(base_url: &str) -> String {
        let mut root = base_url.trim_end_matches('/').to_string();
        if root.to_lowercase().ends_with("/v1") {
            root.truncate(root.len() - 3);
        }
        root + "/health"
    }

    /// True/false for an answer or a refused connection; `None` when it timed out.
    async fn probe(&self, url: &str, timeout: Duration) -> Option<bool> {
        match self.http.get(url).timeout(timeout).send().await {
            Ok(r) => Some(r.status().is_success()),
            Err(e) if e.is_timeout() => None,
            Err(_) => Some(false),
        }
    }
}

#[async_trait]
impl SlotHealthProbe for HttpSlotHealthProbe {
    async fn is_healthy(&self, copy: &str) -> bool {
        let Some(url) = self.health_urls.get(&copy.to_lowercase()) else {
            return false;
        };
        // Test hook, as PiG's PIG_SLOTS_FAIL: OPENORCA_SLOTS_FAIL=B simulates copy B being down.
        if crate::brand::env_var("SLOTS_FAIL")
            .ok_or(std::env::VarError::NotPresent)
            .unwrap_or_default()
            .to_lowercase()
            .contains(&copy.to_lowercase())
        {
            return false;
        }
        match self.probe(url, self.timeout).await {
            Some(ok) => ok,
            None => self.probe(url, self.slow_timeout).await.unwrap_or(false),
        }
    }
}
