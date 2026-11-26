//! How clients find and understand the router (D149, `docs/design/ROUTER-MODE.md`):
//!
//! - `GET /.well-known/abhoth-router` ([`info`]): unauthenticated, no secrets: the router's
//!   name, version, capabilities and whether a caller from where this one is needs a key. Pasting
//!   any URL into the app's settings asks this to tell a router from another server.
//! - `GET /v1/abhoth/models` ([`models`]): the models this caller may use, each with its context
//!   window, output limit, vision, tool calling, reasoning levels, engine, live/cold state and
//!   sampling defaults, normalised from the deployments' `model_info` (LiteLLM's own
//!   `supports_vision` / `supports_function_calling` / `supports_reasoning` keys win over what
//!   the engine implies), plus whether Claude is among them and, for callers that may see them,
//!   the cards.
//! - mDNS / DNS-SD advertising ([`mdns`]) of `_abhoth-router._tcp`.
//!
//! Image input and the context window also come from the engines themselves ([`caps`]: each
//! online card's `GET /v1/models` / `GET /props`, cached) and, for cold catalog models, from the
//! control plane's catalog (`models.<id>.vision` and the on-demand pick's engine profile).
//!
//! All of it is read-only: the only requests to engines are those cached GETs to cards that are
//! online, and nothing can start a switch (GETs never do; see `switch::gate`).

pub mod caps;
pub mod mdns;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;
use orca_core::router_mode::{
    API_VERSION, MODELS_PATH, RouterAuth, RouterCard, RouterInfo, RouterModel, RouterModels,
    RouterPaths, SERVICE,
};
use serde_json::{Map, Value};

use crate::access::{AuthError, Perm, Principal};
use crate::config::{Deployment, Snapshot};
use crate::proxy::json_response;
use crate::state::AppState;

/// `--name`, `--key-url` and whether mDNS is advertised (set once by `abhoth router`).
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// The display name; blank = [`display_name`]'s default.
    pub name: String,
    /// Where users make API keys (the public web UI), shown to clients.
    pub key_url: Option<String>,
    /// mDNS is being advertised.
    pub mdns: bool,
}

/// This machine's host name (`HOSTNAME`, `COMPUTERNAME`, `/etc/hostname`), or "".
pub fn host_name() -> String {
    for v in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(v)
            && !h.trim().is_empty()
        {
            return h.trim().to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The host name, else `fallback` (the mDNS host label).
pub fn host_name_or(fallback: &str) -> String {
    let h = host_name();
    if h.is_empty() {
        fallback.to_string()
    } else {
        h
    }
}

/// The name a router shows: `--name`, else `ABHOTH_ROUTER_NAME`, else "Abhoth router on
/// <Host>" (the host's short name, capitalised when it is all lower case; plain "Abhoth router"
/// when the host name is a container id).
pub fn display_name(d: Option<&Discovery>) -> String {
    if let Some(n) = d.map(|d| d.name.trim()).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if let Ok(n) = std::env::var("ABHOTH_ROUTER_NAME")
        && !n.trim().is_empty()
    {
        return n.trim().to_string();
    }
    default_name(&host_name())
}

/// "Abhoth router on <Host>" for host name `host` (see [`display_name`]).
pub fn default_name(host: &str) -> String {
    let short = host.trim().split('.').next().unwrap_or_default().trim();
    let container_id = matches!(short.len(), 12 | 64)
        && short
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if short.is_empty() || container_id || short.eq_ignore_ascii_case("localhost") {
        return "Abhoth router".into();
    }
    let pretty = if short.chars().any(|c| c.is_uppercase()) {
        short.to_string()
    } else {
        let mut c = short.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    };
    format!("Abhoth router on {pretty}")
}

fn is_claude(engine: Option<&str>) -> bool {
    engine.is_some_and(|e| e.eq_ignore_ascii_case("claude-cli") || e.eq_ignore_ascii_case("claude"))
}

/// The router's identification document for a caller at `peer`.
pub async fn info(st: &AppState, peer: Option<IpAddr>) -> RouterInfo {
    let snap = st.snapshot();
    let d = st.discovery.get();
    let mut caps: Vec<String> = ["chat", "models", "model-meta"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let control = st.control.get();
    // Whether a caller from here needs a key: only with the control plane's access rules on.
    let required = match control {
        None => false,
        Some(cp) => {
            let anon = cp
                .access
                .authenticate(&HeaderMap::new(), peer, "GET", MODELS_PATH)
                .await
                .unwrap_or(Principal::Anonymous { lan: false });
            !cp.access.allows(&anon, Perm::Models)
        }
    };
    if control.is_some() {
        caps.push("user-view".into());
        caps.push("keys".into());
    }
    if snap
        .config
        .deployments
        .iter()
        .any(|x| is_claude(engine_of(&snap, x).as_deref()))
    {
        caps.push("claude".into());
    }
    if d.is_some_and(|d| d.mdns) {
        caps.push("mdns".into());
    }
    RouterInfo {
        service: SERVICE.into(),
        api: API_VERSION,
        name: display_name(d),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: caps,
        auth: RouterAuth {
            required,
            keys: control.is_some(),
            key_prefix: control.map(|_| orca_core::router_link::API_KEY_PREFIX.to_string()),
            key_url: d.and_then(|d| d.key_url.clone()),
        },
        paths: RouterPaths {
            openai: "/v1".into(),
            models: MODELS_PATH.into(),
            view: control.map(|_| "/v1/abhoth/view".to_string()),
        },
    }
}

/// `GET /.well-known/abhoth-router`.
pub async fn well_known(st: &AppState, peer: Option<IpAddr>) -> Response {
    let mut r = json_response(
        200,
        &serde_json::to_value(info(st, peer).await).unwrap_or_default(),
    );
    // Any page may ask (the app's settings in a browser): it is public and has no secrets.
    r.headers_mut().insert(
        "access-control-allow-origin",
        axum::http::HeaderValue::from_static("*"),
    );
    r
}

/// A deployment's engine: its `model_info.engine`, else its first card's.
fn engine_of(snap: &Snapshot, d: &Deployment) -> Option<String> {
    d.model_info
        .get("engine")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            d.cards
                .iter()
                .filter_map(|c| snap.config.upstreams.get(c))
                .map(|u| u.engine.clone())
                .find(|e| !e.is_empty())
        })
}

/// Engines that take OpenAI tool calls natively.
const TOOL_ENGINES: &[&str] = &[
    "ninfer",
    "strata",
    "vllm",
    "sglang",
    "llama.cpp",
    "llamacpp",
    "llama-server",
    "claude-cli",
    "claude",
];

fn flag(mi: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|k| mi.get(*k).and_then(Value::as_bool))
}

/// The sampling keys a deployment's `params` sets.
const SAMPLING_KEYS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "presence_penalty",
    "repetition_penalty",
    "repeat_penalty",
];

const VISION_KEYS: &[&str] = &["supports_vision", "vision"];

/// One deployment as a [`RouterModel`] (no route state yet). `caps`: what the online cards'
/// engines say ([`caps::CapsCache::current`]).
pub fn model_of(
    snap: &Snapshot,
    d: &Deployment,
    caps: &HashMap<String, caps::EngineCaps>,
) -> RouterModel {
    let mi = &d.model_info;
    let engine = engine_of(snap, d);
    let claude = is_claude(engine.as_deref());
    let efforts = d.effort_levels().unwrap_or_default();
    let int = |k: &str| mi.get(k).and_then(Value::as_i64).filter(|n| *n > 0);
    // The engines on its online cards (not the relay's, not Claude's).
    let said = if d.relay || claude {
        caps::EngineCaps::default()
    } else {
        caps::merged(caps, &d.cards)
    };
    let context = d.max_input_tokens().filter(|n| *n > 0).or(said.context);
    RouterModel {
        id: d.name.clone(),
        context_window: context,
        max_output_tokens: int("max_output_tokens").or(if d.relay { None } else { context }),
        // The config's own key wins (an override), then the engine, then Claude's.
        vision: flag(mi, VISION_KEYS)
            .or(said.vision)
            .or(claude.then_some(true)),
        tools: flag(
            mi,
            &["supports_function_calling", "supports_tool_choice", "tools"],
        )
        .or_else(|| {
            engine
                .as_deref()
                .filter(|e| TOOL_ENGINES.iter().any(|t| t.eq_ignore_ascii_case(e)))
                .map(|_| true)
        }),
        reasoning: flag(mi, &["supports_reasoning", "reasoning"])
            .or_else(|| (!efforts.is_empty() || claude).then_some(true)),
        effort_levels: efforts,
        kind: if claude {
            "cloud"
        } else if d.relay {
            "relay"
        } else {
            "local"
        }
        .into(),
        status: if d.relay || claude {
            None
        } else {
            // Without the control plane: live when a card it balances over is online.
            let online = d
                .cards
                .iter()
                .filter_map(|c| snap.config.upstreams.get(c))
                .any(|u| u.online);
            Some(if online { "live" } else { "cold" }.into())
        },
        cold: None,
        sampling: SAMPLING_KEYS
            .iter()
            .filter_map(|k| {
                d.params
                    .get(*k)
                    .filter(|v| v.is_number())
                    .map(|v| (k.to_string(), v.clone()))
            })
            .collect(),
        engine,
    }
}

/// The models for `p` (`None`: no access rules, everyone sees everything).
pub fn models_for(st: &AppState, p: Option<&Principal>) -> RouterModels {
    let snap = st.snapshot();
    let cp = st.control.get();
    let caps = st.engine_caps.current(&snap);
    // Public names in order: the deployments', then routes that have none (catalog names).
    let mut by_name: Vec<RouterModel> = Vec::new();
    for d in &snap.config.deployments {
        if by_name.iter().any(|m| m.id == d.name) {
            continue;
        }
        by_name.push(model_of(&snap, d, &caps));
    }
    let mut cards_visible = p.is_none();
    if let (Some(cp), Some(p)) = (cp, p) {
        cards_visible = cp.access.allows(p, Perm::ReadControl);
        let routes = cp.backend.routes();
        let switching = cp.backend.state().job.is_some();
        let index: HashMap<String, usize> = routes
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.clone(), i))
            .collect();
        for r in &routes {
            if !by_name.iter().any(|m| m.id == r.name) {
                // A route with no deployment of its own: metadata from the one it is the same as.
                let base = r
                    .same_as
                    .as_deref()
                    .and_then(|s| by_name.iter().find(|m| m.id == s))
                    .cloned();
                let mut m = base.unwrap_or_default();
                m.id = r.name.clone();
                if m.kind.is_empty() {
                    m.kind = "local".into();
                }
                by_name.push(m);
            }
        }
        by_name.retain_mut(|m| {
            let Some(r) = index.get(&m.id).map(|i| &routes[*i]) else {
                // A data-plane name the control plane has no route for: its `model_info.access`.
                let access = snap
                    .config
                    .deployments
                    .iter()
                    .find(|d| d.name == m.id)
                    .and_then(|d| d.model_info.get("access"))
                    .and_then(Value::as_str)
                    .unwrap_or("users")
                    .to_string();
                return p.may_use_route(&access);
            };
            if (r.hidden && !p.is_admin()) || !p.may_use_route(&r.access) {
                return false;
            }
            if m.kind != "cloud" {
                let serving = serving_cards(&snap, r);
                let explicit = snap
                    .config
                    .deployments
                    .iter()
                    .find(|d| d.name == m.id)
                    .is_some_and(|d| flag(&d.model_info, VISION_KEYS).is_some());
                if !explicit {
                    if !serving.is_empty() {
                        // What runs it now says.
                        let said = caps::merged(&caps, &serving);
                        m.vision = said.vision.or(m.vision).or(r.vision);
                        if m.context_window.is_none() {
                            m.context_window = said.context;
                        }
                    } else if r.model.is_some() {
                        // Cold catalog model: the catalog (what runs on the cards is another).
                        m.vision = r.vision;
                    } else {
                        m.vision = m.vision.or(r.vision);
                    }
                }
                let live = !r.serving_now.is_empty() || !serving.is_empty();
                m.status = Some(
                    if live {
                        "live"
                    } else if switching && r.cold == "on_demand" {
                        "switching"
                    } else {
                        "cold"
                    }
                    .into(),
                );
                m.cold = Some(r.cold.clone());
            }
            true
        });
        if !cp.access.allows(p, Perm::Models) {
            by_name.clear();
        }
    }
    let claude = by_name.iter().any(|m| is_claude(m.engine.as_deref()));
    let cards = if cards_visible {
        snap.config
            .upstreams
            .iter()
            .map(|(id, u)| RouterCard {
                id: id.clone(),
                engine: Some(u.engine.clone()).filter(|e| !e.is_empty()),
                model: u
                    .catalog_model
                    .clone()
                    .or_else(|| Some(u.served_id.clone()).filter(|s| !s.is_empty())),
                online: u.online,
            })
            .collect()
    } else {
        Vec::new()
    };
    RouterModels {
        principal: p.map(Principal::label).unwrap_or_else(|| "anyone".into()),
        models: by_name,
        claude,
        cards,
        rev: snap.config.rev.clone(),
    }
}

/// The online cards that serve route `r` now: its first targets that have any, else its
/// fallback targets' (`pool:live [(engine)]`, `card:X`, `model:<catalog id>` by what each card
/// runs; another upstream's names are not counted).
fn serving_cards(snap: &Snapshot, r: &crate::control_api::model::RouteView) -> Vec<String> {
    let online: Vec<(&String, &crate::config::Upstream)> = snap
        .config
        .upstreams
        .iter()
        .filter(|(_, u)| u.online && !u.is_virtual())
        .collect();
    let hit = |t: &str| -> Vec<String> {
        let pick = |f: &dyn Fn(&String, &crate::config::Upstream) -> bool| {
            online
                .iter()
                .filter(|(id, u)| f(id, u))
                .map(|(id, _)| (*id).clone())
                .collect::<Vec<_>>()
        };
        if let Some(rest) = t.strip_prefix("pool:") {
            let engine = rest
                .split_once(" (")
                .map(|(_, e)| e.trim_end_matches(')').to_string());
            pick(&|_, u| engine.as_deref().is_none_or(|e| u.engine == e))
        } else if let Some(c) = t.strip_prefix("card:") {
            pick(&|id, _| id == c)
        } else if let Some(m) = t.strip_prefix("model:") {
            pick(&|_, u| u.catalog_model.as_deref() == Some(m))
        } else {
            Vec::new()
        }
    };
    let mut out: Vec<String> = Vec::new();
    for fallback in [false, true] {
        for t in &r.targets {
            let (t, fb) = match t.strip_suffix(" (fallback)") {
                Some(t) => (t, true),
                None => (t.as_str(), false),
            };
            if fb == fallback {
                for c in hit(t) {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// `GET /v1/abhoth/models`.
pub async fn models(st: &Arc<AppState>, headers: &HeaderMap, peer: Option<IpAddr>) -> Response {
    // What the online cards' engines say (cached; cold cards are never asked), once the caller
    // may list models.
    let refresh = || async {
        let snap = st.snapshot();
        caps::refresh(&st.engine_caps, &st.http, &snap).await
    };
    let Some(cp) = st.control.get() else {
        refresh().await;
        return json_response(
            200,
            &serde_json::to_value(models_for(st, None)).unwrap_or_default(),
        );
    };
    let p = match cp
        .access
        .authenticate(headers, peer, "GET", MODELS_PATH)
        .await
    {
        Ok(p) => p,
        Err(AuthError::Unauthorized(m)) => return err(401, m),
        Err(AuthError::Unavailable) => {
            return err(
                503,
                "the accounts server can't vouch for this key right now",
            );
        }
    };
    if !cp.access.allows(&p, Perm::Models) {
        return err(
            if matches!(p, Principal::Anonymous { .. }) {
                401
            } else {
                403
            },
            "this caller may not list models (an Abhoth API key is needed)",
        );
    }
    refresh().await;
    json_response(
        200,
        &serde_json::to_value(models_for(st, Some(&p))).unwrap_or_default(),
    )
}

fn err(status: u16, message: &str) -> Response {
    json_response(
        status,
        &serde_json::json!({"error": {"message": message, "type": "auth_error"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names_read_well() {
        assert_eq!(default_name("gpu-host"), "Abhoth router on Gpu-host");
        assert_eq!(default_name("gpu-host.lan"), "Abhoth router on Gpu-host");
        assert_eq!(default_name("MyServer"), "Abhoth router on MyServer");
        assert_eq!(
            default_name("3f2a9c1b7d4e"),
            "Abhoth router",
            "a container id"
        );
        assert_eq!(default_name(""), "Abhoth router");
        assert_eq!(default_name("localhost"), "Abhoth router");
        let d = Discovery {
            name: "  Lab box ".into(),
            ..Default::default()
        };
        assert_eq!(display_name(Some(&d)), "Lab box");
    }
}
