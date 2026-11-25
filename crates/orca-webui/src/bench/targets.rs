//! `BenchTargets`: where a targeted suite's runs are served (one OpenAI-compatible endpoint per
//! target, at most one run per target at a time), and the seam that hands a run its target.
//! The API key is named by environment variable only: never in a suite, a row or a log.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchTarget {
    pub name: String,
    pub endpoint: String,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
}

impl BenchTarget {
    pub fn new(name: &str, endpoint: &str) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into(),
            model: None,
            api_key_env: None,
        }
    }

    /// What "the same target" means for the one-run-per-target rule: the endpoint, not the name.
    pub fn key(&self) -> String {
        self.endpoint.trim().trim_end_matches('/').to_lowercase()
    }

    /// The endpoint as results rows record it: no user:password part.
    pub fn public_endpoint(&self) -> String {
        match reqwest::Url::parse(&self.endpoint) {
            Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                u.to_string()
            }
            _ => self.endpoint.clone(),
        }
    }
}

/// A run's hold on its target while it runs: the config overrides that send the run there,
/// and what served it. Disposed once the run's session has stopped.
#[async_trait]
pub trait BenchLease: Send + Sync {
    /// Config overrides ("a.b.c=json"), applied on top of the arm's.
    fn sets(&self) -> Vec<String>;
    /// The model the target serves, when known.
    fn model(&self) -> Option<String>;
    async fn dispose(&self) {}
}

/// Hands out inference capacity. The runner already guarantees one run per target; an
/// implementation adds what the target needs on top (a pig-slots one would lease a slot here).
#[async_trait]
pub trait BenchTargetLeaser: Send + Sync {
    async fn acquire(&self, target: &BenchTarget, run_id: &str) -> Box<dyn BenchLease>;
}

/// Targets used directly: the run talks straight to the endpoint as its only model server,
/// with slot leasing off (`slots.enabled=false`).
pub struct DirectEndpointLeaser {
    http: reqwest::Client,
}

impl Default for DirectEndpointLeaser {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }
}

struct DirectLease {
    sets: Vec<String>,
    model: Option<String>,
}

#[async_trait]
impl BenchLease for DirectLease {
    fn sets(&self) -> Vec<String> {
        self.sets.clone()
    }
    fn model(&self) -> Option<String> {
        self.model.clone()
    }
}

fn blank(s: &Option<String>) -> bool {
    s.as_deref().is_none_or(|v| v.trim().is_empty())
}

impl DirectEndpointLeaser {
    /// The overrides that point a run at `target` and nothing else.
    pub fn sets_for(target: &BenchTarget) -> Vec<String> {
        let js = |v: &str| serde_json::to_string(v).unwrap_or_default();
        let mut sets = vec![
            "slots.enabled=false".to_string(),
            format!("lmStudio.baseUrl={}", js(target.endpoint.trim())),
        ];
        if let Some(m) = target.model.as_deref().filter(|_| !blank(&target.model)) {
            sets.push(format!("lmStudio.models=[{}]", js(m)));
        }
        if let Some(env) = target
            .api_key_env
            .as_deref()
            .filter(|_| !blank(&target.api_key_env))
        {
            // The variable's name, never its value.
            sets.push(format!("lmStudio.apiKeyEnv={}", js(env)));
        }
        sets
    }

    /// The first model the endpoint lists (`GET {endpoint}/models`); `None` when it doesn't say.
    async fn served_model(&self, target: &BenchTarget) -> Option<String> {
        let url = format!("{}/models", target.endpoint.trim().trim_end_matches('/'));
        let mut req = self.http.get(url);
        if let Some(env) = target.api_key_env.as_deref().filter(|e| !e.is_empty())
            && let Ok(key) = std::env::var(env)
            && !key.is_empty()
        {
            req = req.bearer_auth(key);
        }
        let res = req.send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let doc: Value = serde_json::from_str(&res.text().await.ok()?).ok()?;
        doc.get("data")?
            .as_array()?
            .first()?
            .get("id")?
            .as_str()
            .map(str::to_string)
    }
}

#[async_trait]
impl BenchTargetLeaser for DirectEndpointLeaser {
    async fn acquire(&self, target: &BenchTarget, _run_id: &str) -> Box<dyn BenchLease> {
        let model = if blank(&target.model) {
            self.served_model(target).await
        } else {
            target.model.clone()
        };
        Box::new(DirectLease {
            sets: Self::sets_for(target),
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_name_the_key_variable_never_its_value() {
        let t = BenchTarget {
            name: "b".into(),
            endpoint: " http://127.0.0.1:18091/v1 ".into(),
            model: Some("iq3s".into()),
            api_key_env: Some("KEY_VAR".into()),
        };
        assert_eq!(
            DirectEndpointLeaser::sets_for(&t),
            [
                "slots.enabled=false",
                "lmStudio.baseUrl=\"http://127.0.0.1:18091/v1\"",
                "lmStudio.models=[\"iq3s\"]",
                "lmStudio.apiKeyEnv=\"KEY_VAR\"",
            ]
        );
        assert_eq!(t.key(), "http://127.0.0.1:18091/v1");
        let secret = BenchTarget::new("s", "http://user:pw@host:8080/v1");
        assert_eq!(secret.public_endpoint(), "http://host:8080/v1");
        assert_eq!(
            BenchTarget::new("p", "http://host/v1").public_endpoint(),
            "http://host/v1"
        );
    }
}
