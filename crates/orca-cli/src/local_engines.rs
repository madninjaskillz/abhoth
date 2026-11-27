//! Local Engines (docs/design/LOCAL-ENGINES.md, D156): the endpoints this machine uses besides
//! its managed engines, and which engines the agent may use as tools.
//!
//! - `machine/endpoints.json`: **unmanaged** engines (already running, here or elsewhere: a kind,
//!   a name and a URL, an API style, a key, models or Engine default) and **Cloud** providers.
//! - `machine/tools.json`: Expose as tool, and each tool kind's default engine.
//! - `machine/secrets/<id>`: an endpoint's key (written once, never returned).
//! - `share.json` `endpoints`: Expose on Migo Bridge.
//!
//! Pure file and validation logic here; `machine.rs` serves it at `/api/machine/local-engines`
//! and `/api/machine/endpoints/<id>`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use orca_router::control::schema::{ControlConfig, ModelKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The kinds an engine can serve (the inventory's model kinds).
pub const KINDS: [&str; 7] = ["llm", "tts", "stt", "image", "music", "embedding", "video"];

/// The API styles each kind may speak; the first is the default.
pub fn apis(kind: &str) -> &'static [&'static str] {
    match kind {
        "llm" => &["openai", "anthropic", "ollama"],
        "tts" => &["openai-speech", "wyoming", "synth"],
        "stt" => &["openai-stt", "wyoming"],
        "image" => &["openai-images", "comfyui"],
        "music" => &["sa3"],
        "embedding" => &["openai"],
        "video" => &["comfyui"],
        _ => &[],
    }
}

/// The agent tool a kind is exposed as (one per kind).
pub fn tool_of(kind: &str) -> Option<&'static str> {
    match kind {
        "image" => Some("generate_image"),
        "tts" => Some("speak"),
        "stt" => Some("transcribe"),
        "music" => Some("make_music"),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Endpoint {
    pub title: String,
    /// `unmanaged` (default) or `cloud`.
    pub group: String,
    /// One of [`KINDS`].
    pub kind: String,
    pub url: String,
    /// One of [`apis`] for the kind.
    pub api: String,
    /// `secret:<id>` when a key is stored (the value is in `secrets/<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Empty: Engine default.
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Image engines: the model used while /uncensored is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncensored_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Endpoints {
    pub endpoints: BTreeMap<String, Endpoint>,
}

/// `tools.json`: which engines (managed or endpoints) the agent's tools may use, and the
/// default engine of each tool kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    /// Engine or endpoint id -> exposed.
    pub exposed: BTreeMap<String, bool>,
    /// Kind (`image`, `tts`, `stt`, `music`) -> the default engine or endpoint id.
    pub defaults: BTreeMap<String, String>,
}

fn load_json<T: Default + for<'a> Deserialize<'a>>(p: &Path) -> Result<T, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

impl Endpoints {
    pub fn load(p: &Path) -> Result<Endpoints, String> {
        load_json(p)
    }
    pub fn rev(&self) -> String {
        orca_bridge::proto::token_hash(&serde_json::to_string(self).unwrap_or_default())[..12]
            .to_string()
    }
}

impl Tools {
    pub fn load(p: &Path) -> Tools {
        load_json(p).unwrap_or_default()
    }
}

/// An id for an engine or endpoint: `a-z`, `0-9`, `-` and `_`, 1-40 long, starting with a
/// letter or digit.
pub fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// An id made from a title (`LM Studio` -> `lm-studio`), not one of `taken`.
pub fn id_from(title: &str, taken: &BTreeSet<String>) -> String {
    let mut s = String::new();
    for c in title.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let base: String = s.trim_end_matches('-').chars().take(32).collect();
    let base = if base.is_empty() {
        "engine".to_string()
    } else {
        base
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// Ids of managed engines (the inventory's engines).
pub fn managed_ids(c: &ControlConfig) -> BTreeSet<String> {
    c.engines.keys().cloned().collect()
}

/// An endpoint as typed, cleaned and checked. `others`: ids already used by managed engines.
pub fn check(id: &str, e: &Endpoint, managed: &BTreeSet<String>) -> Result<Endpoint, String> {
    if !valid_id(id) {
        return Err("The id may only have a-z, 0-9, - and _ (at most 40).".into());
    }
    if managed.contains(id) {
        return Err(format!(
            "`{id}` is already a managed engine on this machine."
        ));
    }
    let mut e = e.clone();
    e.title = e.title.trim().to_string();
    if e.title.is_empty() {
        e.title = id.to_string();
    }
    e.group = match e.group.trim() {
        "" | "unmanaged" => "unmanaged".into(),
        "cloud" => "cloud".into(),
        g => return Err(format!("Unknown group `{g}` (unmanaged or cloud).")),
    };
    e.kind = e.kind.trim().to_lowercase();
    if !KINDS.contains(&e.kind.as_str()) {
        return Err(format!("Pick a kind: {}.", KINDS.join(", ")));
    }
    let allowed = apis(&e.kind);
    e.api = e.api.trim().to_lowercase();
    if e.api.is_empty() {
        e.api = allowed[0].to_string();
    }
    if !allowed.contains(&e.api.as_str()) {
        return Err(format!(
            "A {} engine speaks {}.",
            e.kind,
            allowed.join(" or ")
        ));
    }
    e.url = e.url.trim().trim_end_matches('/').to_string();
    if e.api == "ollama" {
        // Ollama's root (its own API at /api, the OpenAI-compatible one at /v1)
        e.url = e.url.trim_end_matches("/v1").to_string();
    }
    let scheme_ok = e.url.starts_with("http://")
        || e.url.starts_with("https://")
        || (e.api == "wyoming" && e.url.starts_with("tcp://"));
    let parsed = reqwest::Url::parse(&e.url).ok();
    if !scheme_ok || parsed.as_ref().and_then(|u| u.host_str()).is_none() {
        return Err(if e.api == "wyoming" {
            "The URL is tcp://host:port (Wyoming) or http(s)://….".into()
        } else {
            "The URL starts with http:// or https:// and names a host.".into()
        });
    }
    let mut seen = BTreeSet::new();
    e.models = e
        .models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect();
    e.uncensored_model = e
        .uncensored_model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    if e.timeout_seconds == Some(0) {
        e.timeout_seconds = None;
    }
    Ok(e)
}

/// The model ids served by more than one place (endpoints and the inventory's models), each with
/// where: a model name must lead to one engine.
pub fn clashes(c: &ControlConfig, eps: &Endpoints) -> Vec<String> {
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, m) in &c.models {
        if !m.is_component() {
            by.entry(id.clone())
                .or_default()
                .push("this machine's models".into());
        }
    }
    for (id, e) in &eps.endpoints {
        for m in &e.models {
            by.entry(m.clone()).or_default().push(format!("`{id}`"));
        }
    }
    by.into_iter()
        .filter(|(_, w)| w.len() > 1)
        .map(|(m, w)| format!("`{m}` is served by {}", w.join(" and ")))
        .collect()
}

// ── keys ─────────────────────────────────────────────────────────────────────────────────────

pub fn secret_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("secrets").join(id)
}

/// Stores (Some) or removes (None) an endpoint's key; returns the reference to keep.
pub fn store_key(dir: &Path, id: &str, key: Option<&str>) -> Result<Option<String>, String> {
    let p = secret_path(dir, id);
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        None => {
            let _ = std::fs::remove_file(&p);
            Ok(None)
        }
        Some(k) => {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            std::fs::write(&p, k).map_err(|e| format!("{}: {e}", p.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
            Ok(Some(format!("secret:{id}")))
        }
    }
}

pub fn read_key(dir: &Path, e: &Endpoint) -> Option<String> {
    let id = e.key.as_deref()?.strip_prefix("secret:")?;
    std::fs::read_to_string(secret_path(dir, id))
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

// ── the page's view ──────────────────────────────────────────────────────────────────────────

/// Kinds each managed engine serves: from the cards' engines per kind and the models' artifacts.
pub fn managed_kinds(c: &ControlConfig) -> BTreeMap<String, BTreeSet<String>> {
    let name = |k: ModelKind| {
        serde_json::to_value(k)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default()
    };
    let mut out: BTreeMap<String, BTreeSet<String>> = c
        .engines
        .keys()
        .map(|k| (k.clone(), BTreeSet::new()))
        .collect();
    for card in c.cards.values() {
        for (kind, engines) in card.engines.iter().flatten() {
            for e in engines {
                if let Some(s) = out.get_mut(e) {
                    s.insert(kind.clone());
                }
            }
        }
    }
    for m in c.models.values().filter(|m| !m.is_component()) {
        for e in m.artifacts.keys() {
            if let Some(s) = out.get_mut(e) {
                s.insert(name(m.kind()));
            }
        }
    }
    out
}

/// The models a managed engine serves (the inventory's models with an artifact for it).
pub fn managed_models(c: &ControlConfig, engine: &str) -> Vec<String> {
    c.models
        .iter()
        .filter(|(_, m)| !m.is_component() && m.artifacts.contains_key(engine))
        .map(|(id, _)| id.clone())
        .collect()
}

/// What `/api/machine/local-engines` answers: managed engines, endpoints (keys only as "set"),
/// sharing and tools. `states`: engine id -> its state on its cards (from the supervisor).
pub fn view(
    c: Option<&ControlConfig>,
    eps: &Endpoints,
    dir: &Path,
    share: &BTreeMap<String, bool>,
    engines_share: &BTreeMap<String, bool>,
    tools: &Tools,
    states: &BTreeMap<String, Vec<Value>>,
) -> Value {
    let mut managed = Vec::new();
    if let Some(c) = c {
        let kinds = managed_kinds(c);
        for (id, e) in &c.engines {
            managed.push(json!({
                "id": id,
                "title": if e.title.is_empty() { id.clone() } else { e.title.clone() },
                "kinds": kinds.get(id).cloned().unwrap_or_default(),
                "driver": serde_json::to_value(e.driver).unwrap_or(Value::Null),
                "models": managed_models(c, id),
                "cards": match e.port.cards() {
                    v if v.is_empty() => c.cards.keys().cloned().collect::<Vec<_>>(),
                    v => v.into_iter().map(String::from).collect(),
                },
                "states": states.get(id).cloned().unwrap_or_default(),
                "shared": engines_share.get(id).copied().unwrap_or(false),
                "tool": tools.exposed.get(id).copied().unwrap_or(false),
            }));
        }
    }
    let endpoints: Vec<Value> = eps
        .endpoints
        .iter()
        .map(|(id, e)| {
            let mut v = serde_json::to_value(e).unwrap_or(json!({}));
            v["id"] = json!(id);
            v.as_object_mut().map(|o| o.remove("key"));
            v["keySet"] = json!(read_key(dir, e).is_some());
            v["shared"] = json!(share.get(id).copied().unwrap_or(false));
            v["tool"] = json!(tools.exposed.get(id).copied().unwrap_or(false));
            v
        })
        .collect();
    json!({
        "managed": managed,
        "endpoints": endpoints,
        "defaults": tools.defaults,
        "kinds": KINDS,
        "apis": KINDS.iter().map(|k| (k.to_string(), json!(apis(k)))).collect::<BTreeMap<_, _>>(),
        "tools": KINDS.iter().filter_map(|k| tool_of(k).map(|t| (k.to_string(), json!(t)))).collect::<BTreeMap<_, _>>(),
        "rev": eps.rev(),
        "clashes": c.map(|c| clashes(c, eps)).unwrap_or_default(),
    })
}

/// Tools after `exposed`/`defaults` changes: a kind's default must be an exposed engine of that
/// kind (else the first exposed one, else none). `kind_of`: engine or endpoint id -> kinds.
pub fn settle_defaults(t: &mut Tools, kind_of: &BTreeMap<String, BTreeSet<String>>) {
    t.exposed.retain(|id, _| kind_of.contains_key(id));
    for kind in KINDS.iter().filter(|k| tool_of(k).is_some()) {
        let exposed: Vec<&String> = kind_of
            .iter()
            .filter(|(id, ks)| ks.contains(*kind) && t.exposed.get(*id).copied().unwrap_or(false))
            .map(|(id, _)| id)
            .collect();
        let keep = t.defaults.get(*kind).is_some_and(|d| exposed.contains(&d));
        if !keep {
            match exposed.first() {
                Some(first) => {
                    t.defaults.insert(kind.to_string(), (*first).clone());
                }
                None => {
                    t.defaults.remove(*kind);
                }
            }
        }
    }
}

/// The OpenAI-compatible base of an LLM endpoint (Ollama's is `/v1` under its root).
pub fn openai_base(e: &Endpoint) -> String {
    if e.api == "ollama" {
        format!("{}/v1", e.url)
    } else {
        e.url.clone()
    }
}

// ── Check ────────────────────────────────────────────────────────────────────────────────────

/// Asks an endpoint whether it is there (and, for LLMs, its models): `{ok, models?, error?}`.
pub async fn probe(http: &reqwest::Client, e: &Endpoint, key: Option<&str>) -> Value {
    if e.url.starts_with("tcp://") {
        let addr = e.url.trim_start_matches("tcp://").to_string();
        let r = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await;
        return match r {
            Ok(Ok(_)) => json!({"ok": true}),
            Ok(Err(err)) => json!({"ok": false, "error": err.to_string()}),
            Err(_) => json!({"ok": false, "error": "no answer in 5 s"}),
        };
    }
    let (url, list) = match (e.kind.as_str(), e.api.as_str()) {
        ("llm" | "embedding", "ollama") => (format!("{}/api/tags", e.url), true),
        ("llm" | "embedding", _) => (format!("{}/models", e.url), true),
        (_, "comfyui") => (format!("{}/system_stats", e.url), false),
        _ => (e.url.clone(), false),
    };
    let mut rb = http.get(&url).timeout(Duration::from_secs(8));
    if let Some(k) = key {
        rb = if e.api == "anthropic" {
            rb.header("x-api-key", k)
                .header("anthropic-version", "2023-06-01")
        } else {
            rb.bearer_auth(k)
        };
    }
    match rb.send().await {
        Ok(r) => {
            let status = r.status();
            let body: Value = r.json().await.unwrap_or(Value::Null);
            if list && status.is_success() {
                let models: Vec<String> = body["data"]
                    .as_array()
                    .or_else(|| body["models"].as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()))
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"ok": true, "models": models})
            } else if list {
                json!({"ok": false, "error": format!("{url} answered {status}")})
            } else {
                // Any answer means something is listening (media servers' roots often 404).
                json!({"ok": true, "status": status.as_u16()})
            }
        }
        Err(err) => json!({"ok": false, "error": err.without_url().to_string()}),
    }
}

#[cfg(test)]
#[path = "local_engines_tests.rs"]
mod tests;
