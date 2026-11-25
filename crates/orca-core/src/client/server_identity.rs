//! Which inference server answers at an OpenAI-compatible base URL (`ServerIdentity.cs`), and
//! the context window the Lead's servers report (`ServerContextWindow`).
//!
//! One probe per server root for the process: `GET /v1/models`, then `GET /health` unless the
//! model list settled it. A gateway's list (several models, or LiteLLM's `"owned_by":
//! "openai"`) is "another server" and its `/health` is never asked: LiteLLM's `/health` sends a
//! real request to every model it lists, which makes an on-demand switcher load models.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::OrcaConfig;

/// Which server answers at a base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// Not determined: the server did not answer the probe (down, busy, timed out).
    Unknown,
    /// Strata ("Flash"): `GET /health` says `"service": "strata"`.
    Strata,
    /// ninfer: `GET /v1/models` lists a model `"owned_by": "ninfer"`.
    Ninfer,
    /// The server answered and is neither of the above (llama.cpp, LM Studio, vLLM, a gateway).
    Other,
}

/// What a server says about itself (`ServerInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub kind: ServerKind,
    /// Its context window in tokens (Strata `/health` `max_context`; vLLM / ninfer
    /// `/v1/models` `max_model_len`).
    pub max_context: Option<i32>,
    /// Whether it takes image input, when it says (Strata `/health`).
    pub images: Option<bool>,
    /// `/v1/models` refused the key (401 / 403) and `/health` did not name the server: asked
    /// again when another key is offered.
    pub key_rejected: bool,
}

impl ServerInfo {
    pub const UNKNOWN: ServerInfo = ServerInfo {
        kind: ServerKind::Unknown,
        max_context: None,
        images: None,
        key_rejected: false,
    };

    pub fn new(kind: ServerKind, max_context: Option<i32>, images: Option<bool>) -> Self {
        Self {
            kind,
            max_context,
            images,
            key_rejected: false,
        }
    }

    /// The kind as shown to the user.
    pub fn display_name(&self) -> &'static str {
        match self.kind {
            ServerKind::Strata => "Strata",
            ServerKind::Ninfer => "ninfer",
            ServerKind::Other => "another server",
            ServerKind::Unknown => "an unidentified server",
        }
    }
}

/// Each probe request may take this long; a slower server counts as not answering.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long "the server did not answer" stands before the next lookup probes again.
pub const RETRY_UNKNOWN_AFTER: Duration = Duration::from_secs(60);

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

struct Entry {
    info: ServerInfo,
    at: Instant,
    key: Option<String>,
}

#[derive(Default)]
struct Slot {
    /// One probe at a time per server; waiters share its answer.
    probing: tokio::sync::Mutex<()>,
    entry: Mutex<Option<Entry>>,
    /// Keys a probe was already made with (a rejected key is not tried twice).
    tried: Mutex<HashSet<String>>,
}

/// Identifies servers, once per server root for the process (cheap to clone; clones share
/// the answers).
#[derive(Clone)]
pub struct ServerIdentity {
    http: reqwest::Client,
    clock: Clock,
    slots: Arc<Mutex<HashMap<String, Arc<Slot>>>>,
}

static SHARED: LazyLock<ServerIdentity> = LazyLock::new(|| {
    ServerIdentity::new(
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("reqwest client builds with static settings"),
    )
});

impl ServerIdentity {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_clock(http, Arc::new(Instant::now))
    }

    pub fn with_clock(http: reqwest::Client, clock: Clock) -> Self {
        Self {
            http,
            clock,
            slots: Default::default(),
        }
    }

    /// The process-wide instance.
    pub fn shared() -> ServerIdentity {
        SHARED.clone()
    }

    fn slot(&self, root: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap()
            .entry(root.to_lowercase())
            .or_default()
            .clone()
    }

    /// The server at `base_url` ("http://host:8080/v1"). Probes on the first call and shares
    /// the result; never fails (a failed probe is [`ServerInfo::UNKNOWN`]).
    pub async fn get(&self, base_url: &str, api_key: Option<&str>) -> ServerInfo {
        let root = root(base_url);
        let slot = self.slot(&root);
        let _probing = slot.probing.lock().await;
        let now = (self.clock)();
        let stale = slot.entry.lock().unwrap().as_ref().is_some_and(|e| {
            e.info.kind == ServerKind::Unknown
                && now.saturating_duration_since(e.at) >= RETRY_UNKNOWN_AFTER
        });
        if stale {
            *slot.entry.lock().unwrap() = None;
        }
        if slot.entry.lock().unwrap().is_none() {
            let info = probe(&self.http, &root, api_key).await;
            *slot.entry.lock().unwrap() = Some(Entry {
                info,
                at: now,
                key: api_key.map(str::to_string),
            });
        }
        let (info, key) = {
            let g = slot.entry.lock().unwrap();
            let e = g.as_ref().expect("entry set above");
            (e.info.clone(), e.key.clone())
        };
        slot.tried.lock().unwrap().insert(key.unwrap_or_default());
        // "That key is not mine" and this caller has another key: ask with it.
        if info.key_rejected
            && let Some(k) = api_key.filter(|k| !k.trim().is_empty())
            && slot.tried.lock().unwrap().insert(k.to_string())
        {
            let info = probe(&self.http, &root, Some(k)).await;
            *slot.entry.lock().unwrap() = Some(Entry {
                info: info.clone(),
                at: now,
                key: Some(k.to_string()),
            });
            return info;
        }
        info
    }

    /// What is known right now without waiting: None while never asked or still probing.
    pub fn known(&self, base_url: &str) -> Option<ServerInfo> {
        let slot = self
            .slots
            .lock()
            .unwrap()
            .get(&root(base_url).to_lowercase())?
            .clone();
        let g = slot.entry.lock().unwrap();
        g.as_ref().map(|e| e.info.clone())
    }

    /// Starts the probe (if not started) in the background.
    pub fn warm(&self, base_url: &str, api_key: Option<&str>) {
        let me = self.clone();
        let (url, key) = (base_url.to_string(), api_key.map(str::to_string));
        tokio::spawn(async move {
            me.get(&url, key.as_deref()).await;
        });
    }
}

async fn probe(http: &reqwest::Client, root: &str, api_key: Option<&str>) -> ServerInfo {
    detect(root, api_key, http).await
}

/// "http://h:8080/v1/" -> "http://h:8080" (health is served at the root).
pub fn root(base_url: &str) -> String {
    let root = base_url.trim().trim_end_matches('/');
    if root.len() >= 3 && root[root.len() - 3..].eq_ignore_ascii_case("/v1") {
        root[..root.len() - 3].to_string()
    } else {
        root.to_string()
    }
}

/// One probe (`ServerIdentity.DetectAsync`): `GET {root}/v1/models`, then `GET {root}/health`
/// unless the model list already settled it (ninfer names itself; a gateway's list is "other"
/// and its health is never asked). "Other" needs an answer that rules Strata out; no
/// connection, a timeout or a 5xx is Unknown, and so is a refused key (marked `key_rejected`).
pub async fn detect(base_url: &str, api_key: Option<&str>, http: &reqwest::Client) -> ServerInfo {
    let root = root(base_url);
    let (models_status, models_body) = get(http, &format!("{root}/v1/models"), api_key).await;
    let models = models_body.as_deref().and_then(parse_models);
    if let Some(m) = &models {
        if m.kind == ServerKind::Ninfer {
            return m.clone();
        }
        if m.kind == ServerKind::Other && is_gateway_list(models_body.as_deref().unwrap_or("")) {
            return m.clone();
        }
    }
    let (health_status, health_body) = get(http, &format!("{root}/health"), api_key).await;
    let health = health_body.as_deref().map(parse_health);
    if let Some(h) = &health
        && h.kind == ServerKind::Strata
    {
        return ServerInfo {
            max_context: h
                .max_context
                .or_else(|| models.as_ref().and_then(|m| m.max_context)),
            ..h.clone()
        };
    }
    if let Some(m) = &models
        && m.kind == ServerKind::Strata
    {
        return ServerInfo {
            max_context: m
                .max_context
                .or_else(|| health.as_ref().and_then(|h| h.max_context)),
            images: health.as_ref().and_then(|h| h.images),
            ..m.clone()
        };
    }
    if matches!(models_status, Some(401 | 403)) {
        return ServerInfo {
            key_rejected: true,
            ..ServerInfo::new(
                ServerKind::Unknown,
                health.as_ref().and_then(|h| h.max_context),
                health.as_ref().and_then(|h| h.images),
            )
        };
    }
    let answered =
        health_body.is_some() || models.is_some() || matches!(health_status, Some(404 | 405));
    ServerInfo::new(
        if answered {
            ServerKind::Other
        } else {
            ServerKind::Unknown
        },
        models
            .as_ref()
            .and_then(|m| m.max_context)
            .or_else(|| health.as_ref().and_then(|h| h.max_context)),
        health.as_ref().and_then(|h| h.images),
    )
}

/// (status, body of a 2xx answer); (None, None) when the server did not answer.
async fn get(
    http: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
) -> (Option<u16>, Option<String>) {
    let mut req = http.get(url);
    if let Some(k) = api_key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(k);
    }
    let Ok(response) = req.send().await else {
        return (None, None);
    };
    let status = response.status();
    if !status.is_success() {
        return (Some(status.as_u16()), None);
    }
    match response.text().await {
        Ok(body) => (Some(status.as_u16()), Some(body)),
        Err(_) => (None, None),
    }
}

/// A `/v1/models` body that is a gateway's: more than one model, or an entry owned by
/// "openai" / "litellm".
pub fn is_gateway_list(json: &str) -> bool {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return false;
    };
    let Some(Value::Array(data)) = root.get("data") else {
        return false;
    };
    if data.len() > 1 {
        return true;
    }
    data.iter().any(|m| {
        m.get("owned_by")
            .and_then(Value::as_str)
            .is_some_and(|o| o.eq_ignore_ascii_case("openai") || o.eq_ignore_ascii_case("litellm"))
    })
}

/// The model ids a `/v1/models` body lists, in order (empty when it is not a model list).
pub fn model_ids(json: &str) -> Vec<String> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(Value::Array(data)) = root.get("data") else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

fn positive_int(v: &Value, name: &str) -> Option<i32> {
    v.get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n > 0)
}

/// A 2xx `/health` body: Strata says `"service": "strata"`; anything else is Other.
pub fn parse_health(json: &str) -> ServerInfo {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return ServerInfo::new(ServerKind::Other, None, None);
    };
    if !v.is_object() {
        return ServerInfo::new(ServerKind::Other, None, None);
    }
    let strata = v
        .get("service")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("strata"));
    let images = v.get("images").and_then(Value::as_bool);
    ServerInfo::new(
        if strata {
            ServerKind::Strata
        } else {
            ServerKind::Other
        },
        positive_int(&v, "max_context"),
        if strata { images } else { None },
    )
}

/// A `/v1/models` body: the kind from `owned_by` ("ninfer", "strata"), the window from
/// `max_model_len` (the smallest listed). None when it is not a model list.
pub fn parse_models(json: &str) -> Option<ServerInfo> {
    let v = serde_json::from_str::<Value>(json).ok()?;
    let data = v.as_object()?.get("data")?.as_array()?;
    let mut kind = ServerKind::Other;
    let mut window: Option<i32> = None;
    for model in data.iter().filter(|m| m.is_object()) {
        if let Some(owner) = model.get("owned_by").and_then(Value::as_str) {
            if owner.eq_ignore_ascii_case("ninfer") {
                kind = ServerKind::Ninfer;
            } else if owner.eq_ignore_ascii_case("strata") && kind == ServerKind::Other {
                kind = ServerKind::Strata;
            }
        }
        if let Some(len) = positive_int(model, "max_model_len") {
            window = Some(window.map_or(len, |w| w.min(len)));
        }
    }
    Some(ServerInfo::new(kind, window, None))
}

/// `ServerContextWindow.SmallestAsync`: the smallest window reported by `lmStudio.baseUrl`
/// (unless that is Anthropic) and, with slots enabled, the Lead's copy; None when neither says
/// (or a `slots.lead` endpoint sets its own window, which wins). The caller lowers
/// `context.contextWindowSize` to it, never raises it.
pub async fn smallest_context_window(
    config: &OrcaConfig,
    identity: &ServerIdentity,
) -> Option<i32> {
    if config
        .slots
        .as_ref()
        .is_some_and(|s| s.compute_lead_context_window().is_some())
    {
        return None;
    }
    let mut probes: Vec<(String, String)> = Vec::new();
    let lm = &config.lm_studio;
    if !lm.is_anthropic() && !lm.base_url.trim().is_empty() {
        probes.push((lm.base_url.clone(), lm.resolve_api_key()));
    }
    if let Some(slots) = config
        .slots
        .as_ref()
        .filter(|s| s.enabled && s.lead.as_ref().is_none_or(|l| l.base_url.is_empty()))
    {
        let lead_copy = slots
            .copies
            .iter()
            .find(|c| {
                slots
                    .lead_copy
                    .as_deref()
                    .is_some_and(|n| c.name.eq_ignore_ascii_case(n))
            })
            .or_else(|| slots.copies.first());
        if let Some(copy) = lead_copy.filter(|c| !c.base_url.is_empty()) {
            let env_key = slots
                .api_key_env
                .as_deref()
                .filter(|e| !e.is_empty())
                .and_then(|e| std::env::var(e).ok());
            let key = env_key
                .or_else(|| slots.api_key.clone())
                .unwrap_or_else(|| lm.api_key.clone());
            probes.push((copy.base_url.clone(), key));
        }
    }
    let answers = futures_util::future::join_all(
        probes
            .iter()
            .map(|(url, key)| identity.get(url, Some(key.as_str()))),
    )
    .await;
    answers.iter().filter_map(|i| i.max_context).min()
}
