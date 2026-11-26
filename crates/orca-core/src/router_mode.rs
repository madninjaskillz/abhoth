//! Router mode (D149, `docs/design/ROUTER-MODE.md`): the client's side of talking to an
//! `abhoth router` rather than to an OpenAI-compatible server it configures itself.
//!
//! - **Identify** ([`identify`]): `GET /.well-known/abhoth-router` on any URL tells whether it is
//!   a router (name, version, capabilities, whether a key is needed). Unauthenticated, read-only,
//!   sent with `x-abhoth-no-switch: 1`; nothing else is probed (a LiteLLM `/health` would send a
//!   real request to every model).
//! - **Model metadata** ([`fetch_models`]): `GET /v1/abhoth/models` gives each model the caller
//!   may use with its context window, vision, tool calling, reasoning levels and sampling
//!   defaults, so router mode needs no model settings here.
//! - **Overrides** ([`overrides`]): what router mode puts over config.json for a session, as a
//!   profile does (never written to the file): the router's metadata for the chosen model, and
//!   the settings the router owns (backups, slots, inference profiles) turned off.
//!
//! LAN discovery (mDNS, [`MDNS_SERVICE_TYPE`]) lives in the router (advertising) and the web
//! server (browsing); the names they share are here.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::OrcaConfig;

/// The identification document's path (RFC 8615 well-known URI).
pub const WELL_KNOWN_PATH: &str = "/.well-known/abhoth-router";
/// The per-model metadata path.
pub const MODELS_PATH: &str = "/v1/abhoth/models";
/// `service` in the identification document.
pub const SERVICE: &str = "abhoth-router";
/// The identification document's version.
pub const API_VERSION: u32 = 1;
/// The DNS-SD service type routers advertise on the LAN.
pub const MDNS_SERVICE_TYPE: &str = "_abhoth-router._tcp.local.";
/// Every request the client makes for discovery or metadata carries this, so it can never start
/// an on-demand layout switch (D-RC7), whatever the router's policy.
pub const NO_SWITCH_HEADER: &str = "x-abhoth-no-switch";
/// Each identification or metadata request may take this long.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long fetched model metadata is reused.
pub const MODELS_TTL: Duration = Duration::from_secs(30);

/// `GET /.well-known/abhoth-router`: what a router says about itself. No secrets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterInfo {
    /// Always [`SERVICE`].
    pub service: String,
    /// [`API_VERSION`].
    pub api: u32,
    /// A display name ("Lab router").
    pub name: String,
    /// The router's Abhoth version.
    pub version: String,
    /// What it offers: `chat`, `models`, `model-meta`, `user-view`, `claude`, `mdns`, ...
    pub capabilities: Vec<String>,
    pub auth: RouterAuth,
    pub paths: RouterPaths,
}

/// How a caller like this one gets in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterAuth {
    /// A key is needed for a caller from where this request came from.
    pub required: bool,
    /// Abhoth API keys (`abk_...`, made in the web UI's Account settings) are accepted.
    pub keys: bool,
    /// The prefix of those keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// Where to make a key (the web UI that issues them), when the router knows a public one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_url: Option<String>,
}

/// Where things are, relative to the router's root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterPaths {
    /// The OpenAI-compatible base (what `lmStudio.baseUrl` should be, under the root).
    pub openai: String,
    pub models: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
}

impl Default for RouterPaths {
    fn default() -> Self {
        Self {
            openai: "/v1".into(),
            models: MODELS_PATH.into(),
            view: None,
        }
    }
}

/// One model as `GET /v1/abhoth/models` describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterModel {
    /// The name requests send as `model`.
    pub id: String,
    /// Prompt window in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    /// Takes images; `None` = the router does not know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Native (OpenAI) tool calling; `None` = the router does not know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    /// Thinks before answering (has reasoning levels).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// The `reasoning_effort` values it takes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effort_levels: Vec<String>,
    /// The engine behind it (`ninfer`, `strata`, `claude-cli`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// `local` (a GPU card), `cloud` (Claude and other providers) or `relay`.
    pub kind: String,
    /// `live`, `cold` or `switching` (routers with the control plane on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// What a request does while it is cold: `reject`, `fallback` or `on_demand`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold: Option<String>,
    /// Sampling defaults the router applies when a request leaves them out (`temperature`,
    /// `top_p`, `top_k`, `min_p`, `presence_penalty`, `repetition_penalty`).
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub sampling: Map<String, Value>,
}

/// One GPU card, for the read-only view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterCard {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub online: bool,
}

/// `GET /v1/abhoth/models`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterModels {
    /// Who the router took the caller for (`anonymous-lan`, `user:name#3`): never a key.
    pub principal: String,
    pub models: Vec<RouterModel>,
    /// A Claude model is among them (this key may use Claude).
    pub claude: bool,
    /// The cards, when this caller may see them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<RouterCard>,
    /// The router's config revision.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rev: String,
}

impl RouterModels {
    pub fn find(&self, id: &str) -> Option<&RouterModel> {
        self.models.iter().find(|m| m.id.eq_ignore_ascii_case(id))
    }
}

/// The root of a base URL: `http://h:8010/v1/` -> `http://h:8010`. A URL without a scheme gets
/// `http://`.
pub fn root_of(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if !u.contains("://") {
        u = format!("http://{u}");
    }
    if u.len() >= 3 && u[u.len() - 3..].eq_ignore_ascii_case("/v1") {
        u.truncate(u.len() - 3);
    }
    u.trim_end_matches('/').to_string()
}

/// The OpenAI-compatible base URL of a router at `url` (its root plus `paths.openai`).
pub fn base_url_of(url: &str, info: Option<&RouterInfo>) -> String {
    let path = info
        .map(|i| i.paths.openai.as_str())
        .filter(|p| p.starts_with('/'))
        .unwrap_or("/v1");
    format!("{}{}", root_of(url), path)
}

/// A client for probes: short timeout, no redirects (a probe never follows one elsewhere).
pub fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builds with static settings")
}

/// Whether the server at `url` (any form: root, `/v1`, with or without a slash) is an Abhoth
/// router. `Ok(None)`: it answered and is not one. `Err`: it did not answer.
pub async fn identify(url: &str, http: &reqwest::Client) -> Result<Option<RouterInfo>, String> {
    let root = root_of(url);
    let response = http
        .get(format!("{root}{WELL_KNOWN_PATH}"))
        .header(NO_SWITCH_HEADER, "1")
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| short_error(&e))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let text = response.text().await.map_err(|e| short_error(&e))?;
    Ok(parse_info(&text))
}

/// The identification document, when `text` is one.
pub fn parse_info(text: &str) -> Option<RouterInfo> {
    let info: RouterInfo = serde_json::from_str(text).ok()?;
    (info.service == SERVICE).then_some(info)
}

fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "could not connect".into()
    } else {
        // reqwest's text can carry the URL; it never carries a header (the key).
        e.to_string()
    }
}

/// `GET /v1/abhoth/models` of the router at `base_url`, as `api_key` (blank: none).
pub async fn fetch_models(
    base_url: &str,
    api_key: &str,
    http: &reqwest::Client,
) -> Result<RouterModels, String> {
    let root = root_of(base_url);
    let mut req = http
        .get(format!("{root}{MODELS_PATH}"))
        .header(NO_SWITCH_HEADER, "1")
        .header("accept", "application/json");
    if !api_key.trim().is_empty() && !is_placeholder_key(api_key) {
        req = req.bearer_auth(api_key.trim());
    }
    let response = req.send().await.map_err(|e| short_error(&e))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "the router refused the API key ({})",
            status.as_u16()
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("this router has no model metadata (older than D149)".into());
    }
    if !status.is_success() {
        return Err(format!("the router answered {}", status.as_u16()));
    }
    let text = response.text().await.map_err(|e| short_error(&e))?;
    serde_json::from_str(&text).map_err(|e| format!("unreadable model metadata: {e}"))
}

/// LM Studio's default key (`lm-studio`): a placeholder, never sent to a router.
pub fn is_placeholder_key(key: &str) -> bool {
    key.trim().eq_ignore_ascii_case("lm-studio")
}

type CacheKey = (String, String);
static CACHE: LazyLock<Mutex<HashMap<CacheKey, (Instant, RouterModels)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// [`fetch_models`], reused for [`MODELS_TTL`] per router and key.
pub async fn models_cached(
    base_url: &str,
    api_key: &str,
    http: &reqwest::Client,
) -> Result<RouterModels, String> {
    let key = (
        root_of(base_url),
        crate::router_link::key_digest(api_key.trim()),
    );
    if let Some((at, m)) = CACHE.lock().unwrap().get(&key)
        && at.elapsed() < MODELS_TTL
    {
        return Ok(m.clone());
    }
    let m = fetch_models(base_url, api_key, http).await?;
    CACHE
        .lock()
        .unwrap()
        .insert(key, (Instant::now(), m.clone()));
    Ok(m)
}

/// Forget cached metadata (the settings changed).
pub fn clear_cache() {
    CACHE.lock().unwrap().clear();
}

/// What router mode puts over config.json: dotted config paths and values (applied like a
/// profile's, never saved), and notices for the user. `models`: the router's metadata, or
/// `None` when it could not be fetched (only the settings the router owns are turned off).
pub fn overrides(
    config: &OrcaConfig,
    models: Option<&RouterModels>,
) -> (Vec<(String, Value)>, Vec<String>) {
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut notices = Vec::new();
    let lm = &config.lm_studio;
    // The router speaks the OpenAI API (it adapts to Anthropic itself).
    if lm.provider.is_some() {
        out.push(("lmStudio.provider".into(), Value::Null));
    }
    // LM Studio's placeholder key is no key to a router (which refuses a key it doesn't know).
    if lm
        .api_key_env
        .as_deref()
        .is_none_or(|e| e.trim().is_empty())
        && is_placeholder_key(&lm.api_key)
    {
        out.push(("lmStudio.apiKey".into(), json!("")));
        for (name, p) in &config.providers {
            if !name.contains('.')
                && root_of(&p.base_url) == root_of(&lm.base_url)
                && p.api_key.as_deref().is_some_and(is_placeholder_key)
            {
                out.push((format!("providers.{name}.apiKey"), json!("")));
            }
        }
    }
    // Backups and pair mode: the router's routes fail over; one model is asked.
    if lm.backup_min_response_length != 0 {
        out.push(("lmStudio.backupMinResponseLength".into(), json!(0)));
    }
    if lm.backup_on_refusal {
        out.push(("lmStudio.backupOnRefusal".into(), json!(false)));
    }
    if lm.pair_model_programming {
        out.push(("lmStudio.pairModelProgramming".into(), json!(false)));
    }
    // The window comes from the router, not from probing LM Studio / Ollama paths.
    if config.context.auto_detect_context_window {
        out.push(("context.autoDetectContextWindow".into(), json!(false)));
    }
    // pig-slots and the inference profiles (layout switching) are the router's.
    if config.slots.as_ref().is_some_and(|s| s.enabled) {
        out.push(("slots.enabled".into(), json!(false)));
    }
    if config.profiles.active.is_some() {
        out.push(("profiles.active".into(), Value::Null));
    }
    let Some(models) = models else {
        return (out, notices);
    };
    let chosen = lm.primary_model().filter(|m| !m.trim().is_empty());
    let meta = match chosen {
        Some(m) => {
            let found = models.find(m);
            if found.is_none() {
                notices.push(format!(
                    "The router does not offer model '{m}' to this key; requests may be refused. Pick one in Settings > Models."
                ));
            }
            found
        }
        None => {
            // No model chosen yet: the router's first live one, else its first.
            let pick = models
                .models
                .iter()
                .find(|m| m.status.as_deref() == Some("live"))
                .or_else(|| models.models.first());
            if let Some(p) = pick {
                notices.push(format!(
                    "No model is chosen: using the router's '{}' (choose one in Settings > Models).",
                    p.id
                ));
            }
            pick
        }
    };
    // One model: the router's routes do the failing over.
    let model_list: Vec<String> = match (chosen, meta) {
        (_, Some(found)) => vec![found.id.clone()],
        (Some(m), None) => vec![m.to_string()],
        (None, None) => Vec::new(),
    };
    if model_list != lm.models {
        out.push(("lmStudio.models".into(), json!(model_list)));
    }
    let Some(meta) = meta else {
        return (out, notices);
    };
    if let Some(n) = meta
        .context_window
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n > 0)
    {
        out.push(("context.contextWindowSize".into(), json!(n)));
    }
    if let Some(v) = meta.vision {
        out.push(("lmStudio.vision".into(), json!(v)));
    }
    if let Some(t) = meta.tools {
        out.push(("lmStudio.nativeToolCalling".into(), json!(t)));
    }
    let sampling = sampling_of(&meta.sampling);
    if !sampling.is_empty() {
        // The whole map (model names have dots, which a dotted path would split).
        let mut all: Map<String, Value> = lm
            .model_sampling
            .as_ref()
            .and_then(|m| serde_json::to_value(m).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let entry = all
            .entry(meta.id.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(o) = entry {
            for (k, v) in sampling {
                // A value set here for this model still wins.
                o.entry(k).or_insert(v);
            }
        }
        out.push(("lmStudio.modelSampling".into(), Value::Object(all)));
    }
    (out, notices)
}

/// The router's sampling defaults as `SamplingConfig` keys (camelCase).
fn sampling_of(m: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, name) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
        ("min_p", "minP"),
        ("presence_penalty", "presencePenalty"),
        ("repetition_penalty", "repetitionPenalty"),
        ("repeat_penalty", "repetitionPenalty"),
    ] {
        if let Some(v) = m.get(k).filter(|v| v.is_number())
            && !out.contains_key(name)
        {
            out.insert(name.into(), v.clone());
        }
    }
    out
}

/// Put router mode over `manager`'s config for a session: fetch the router's metadata (cached)
/// and apply [`overrides`]. Nothing outside router mode. Returns notices for the user.
pub async fn apply(
    manager: &mut crate::config::ConfigManager,
    http: &reqwest::Client,
) -> Vec<String> {
    if !manager.config.lm_studio.is_router() {
        return Vec::new();
    }
    let base = manager.config.lm_studio.base_url.clone();
    let key = manager.config.lm_studio.resolve_api_key();
    let fetched = models_cached(&base, &key, http).await;
    let mut notices = Vec::new();
    let models = match fetched {
        Ok(m) => Some(m),
        Err(e) => {
            notices.push(format!(
                "Router model metadata unavailable ({e}): using this config's model settings."
            ));
            None
        }
    };
    let (ov, more) = overrides(&manager.config, models.as_ref());
    notices.extend(more);
    if let Err(e) = manager.apply_router(&ov) {
        notices.push(format!("Router settings not applied: {e}"));
    }
    notices
}

#[cfg(test)]
mod tests;
