//! The Claude engine (ROUTER.md 12): Claude models (Opus, Sonnet, Haiku, ...) served as ordinary
//! routes, answered by the Claude Code CLI under the operator's own Claude subscription.
//!
//! It is a virtual server with one virtual card: `claude` in `router.json` adds an upstream
//! (`engine = "claude-cli"`, url `claude-cli://local`, `lanes = maxSessions`) and one deployment
//! per route. Routing, fallbacks, placement, leases (slots), metrics and the request log treat
//! it like any card; layout switches, on-demand switching and the qfn-lb paths never touch it.
//!
//! Each request runs one short `claude -p` with stream-json in and out, Claude Code's own tools
//! off, no permission prompts, the caller's system prompt in place of Claude Code's and a
//! neutral working directory ([`engine`], behind the `claude-engine` cargo feature). The pieces
//! here (config, expansion into the snapshot, access) build without the feature so a config
//! that names the engine is refused clearly by a router built without it.

pub mod convert;
#[cfg(feature = "claude-engine")]
pub mod engine;

use std::net::IpAddr;

use axum::http::HeaderMap;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{Deployment, RouterConfig, Snapshot, Upstream};
use crate::state::AppState;

/// The engine kind of the virtual card.
pub const ENGINE: &str = "claude-cli";
/// The virtual card's upstream url (never dialled).
pub const VIRTUAL_URL: &str = "claude-cli://local";

/// `claude` in `router.json`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    /// The virtual card's id (an upstream key; must not be a real card).
    pub card: String,
    /// The CLI: `claude` on PATH, or a full path.
    pub command: String,
    /// Added after the engine's own arguments (e.g. `["--fallback-model", "sonnet"]`).
    pub extra_args: Vec<String>,
    /// Concurrent CLI sessions (the card's lanes). The limit is the subscription's rate
    /// limits, not hardware: processes are spawned per request, none is kept idle.
    pub max_sessions: u32,
    /// How long a request waits for a free session before it gets 429 + Retry-After.
    pub queue_seconds: u64,
    /// One request, start to end (the CLI is killed after it).
    pub timeout_seconds: u64,
    /// While the subscription is rate limited, a request waits this long for the reset rather
    /// than failing (backpressure); a longer wait answers 429 with Retry-After.
    pub rate_limit_wait_seconds: u64,
    /// Empty directory the CLI runs in; default `<temp>/abhoth-claude`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    /// A file holding a `claude setup-token` OAuth token (containers); passed to the CLI as
    /// `CLAUDE_CODE_OAUTH_TOKEN`, read at each start, never logged. None = the CLI's own login.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_token_file: Option<String>,
    /// The CLI's HOME (its `~/.claude.json` and `~/.claude/`), created if missing. Set it when
    /// the router shares a HOME with another Claude Code user (the single container's web UI),
    /// so the two never write the same files. None = the router's own HOME.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Who may use the routes: `admins` (default), `users`, `anonymous` or `keys:<names>`.
    /// It is one person's subscription and Anthropic's consumer terms are about personal use,
    /// so it is not opened to service keys or other users unless the operator says so.
    pub access: String,
    /// Run the CLI with `--safe-mode` (no CLAUDE.md, hooks, plugins, skills or MCP servers
    /// from the operator's own setup).
    pub safe_mode: bool,
    /// Used when the caller sends no system message (Claude Code's own prompt is never used).
    pub default_system_prompt: String,
    /// Route name -> the CLI's `--model` value (an alias such as `opus` or a full model id).
    pub routes: IndexMap<String, String>,
    /// Reported as max_input_tokens in /v1/models.
    pub context_tokens: i64,
    /// Reported as max_output_tokens in /v1/models.
    pub max_output_tokens: i64,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        let mut routes = IndexMap::new();
        routes.insert("claude-opus".into(), "opus".into());
        routes.insert("claude-sonnet".into(), "sonnet".into());
        routes.insert("claude-haiku".into(), "haiku".into());
        Self {
            enabled: false,
            card: "claude".into(),
            command: "claude".into(),
            extra_args: Vec::new(),
            max_sessions: 32,
            queue_seconds: 300,
            timeout_seconds: 900,
            rate_limit_wait_seconds: 30,
            work_dir: None,
            oauth_token_file: None,
            home: None,
            access: "admins".into(),
            safe_mode: true,
            default_system_prompt: "You are Claude, an AI assistant made by Anthropic.".into(),
            routes,
            context_tokens: 200_000,
            max_output_tokens: 32_000,
        }
    }
}

impl Upstream {
    /// The Claude engine's virtual card (no URL to dial, no engine process to manage).
    pub fn is_virtual(&self) -> bool {
        self.url.starts_with("claude-cli:")
    }
}

/// Add the virtual card and its deployments to a config (idempotent). Called on every
/// snapshot build, before validation.
pub fn expand(c: &mut RouterConfig) -> Result<(), String> {
    if !c.claude.enabled {
        return Ok(());
    }
    if !cfg!(feature = "claude-engine") {
        return Err(
            "claude.enabled: this router was built without the claude-engine feature".into(),
        );
    }
    let cc = c.claude.clone();
    let card = cc.card.trim().to_string();
    if card.is_empty() {
        return Err("claude.card is empty".into());
    }
    if cc.command.trim().is_empty() {
        return Err("claude.command is empty".into());
    }
    if cc.max_sessions == 0 {
        return Err("claude.maxSessions must be at least 1".into());
    }
    if !valid_access(&cc.access) {
        return Err(format!(
            "claude.access must be admins, users, anonymous or keys:<names>: {}",
            cc.access
        ));
    }
    if let Some(u) = c.upstreams.get(&card)
        && !u.is_virtual()
    {
        return Err(format!(
            "claude.card `{card}` is already a real upstream; pick another id"
        ));
    }
    c.upstreams.insert(
        card.clone(),
        Upstream {
            url: VIRTUAL_URL.into(),
            engine: ENGINE.into(),
            served_id: String::new(),
            lanes: cc.max_sessions,
            key_file: None,
            online: true,
            catalog_model: None,
        },
    );
    for (name, model) in &cc.routes {
        if model.trim().is_empty() {
            return Err(format!("claude.routes.{name}: empty model"));
        }
        if let Some(d) = c
            .deployments
            .iter_mut()
            .find(|d| &d.name == name && d.cards == [card.clone()])
        {
            d.served_id = model.clone();
            continue;
        }
        let mut mi = Map::new();
        mi.insert("engine".into(), json!(ENGINE));
        mi.insert("max_input_tokens".into(), json!(cc.context_tokens));
        mi.insert("max_output_tokens".into(), json!(cc.max_output_tokens));
        mi.insert("access".into(), json!(cc.access));
        c.deployments.push(Deployment {
            name: name.clone(),
            cards: vec![card.clone()],
            served_id: model.clone(),
            model_info: mi,
            ..Deployment::default()
        });
    }
    // never switched by on-demand
    if !c.on_demand.exclude_cards.contains(&card) {
        c.on_demand.exclude_cards.push(card);
    }
    Ok(())
}

fn valid_access(a: &str) -> bool {
    matches!(a.trim(), "admins" | "users" | "anonymous")
        || a.trim()
            .strip_prefix("keys:")
            .is_some_and(|l| !l.trim().is_empty())
}

/// Is the claude card among a request's candidates?
pub fn is_claude_card(snap: &Snapshot, card: &str) -> bool {
    snap.config.claude.enabled
        && snap
            .config
            .upstreams
            .get(card)
            .is_some_and(Upstream::is_virtual)
}

/// May this caller use the Claude routes (`claude.access`)? With the control plane on, its
/// principals decide (`Principal::may_use_route`); with it off, an admin is a loopback peer or
/// a caller with the `settings.adminKeyFile` key as Bearer, and `keys:` lists admit admins
/// only (there are no key checks without the control plane).
pub async fn allowed(
    st: &AppState,
    snap: &Snapshot,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> bool {
    let access = snap.config.claude.access.trim();
    if let Some(cp) = st.control.get() {
        return match cp
            .access
            .authenticate(headers, peer, "POST", "/v1/chat/completions")
            .await
        {
            Ok(p) => p.may_use_route(access),
            Err(_) => false,
        };
    }
    // an admin (loopback, the admin key) may use any route, as with the control plane
    if peer.is_some_and(|p| p.is_loopback()) || crate::server::admin_allowed(snap, headers) {
        return true;
    }
    matches!(access, "anonymous" | "users")
}

/// One attempt on the virtual card: the engine's answer, or why it failed with the answer to
/// pass back if nothing else can take the request (429, 503).
pub type Outcome = Result<reqwest::Response, Box<(String, Option<reqwest::Response>)>>;

/// A failed attempt: why (for the log), and the answer to pass back if no other card takes it.
pub fn failed(why: impl Into<String>, resp: Option<reqwest::Response>) -> Outcome {
    Err(Box::new((why.into(), resp)))
}

/// Run one chat on the virtual card. `body` is the rewritten request (`model` = the CLI model).
#[cfg(feature = "claude-engine")]
pub async fn send(
    st: &std::sync::Arc<AppState>,
    snap: &Snapshot,
    card: &str,
    body: &Map<String, Value>,
) -> Outcome {
    engine::send(st, snap, card, body).await
}

#[cfg(not(feature = "claude-engine"))]
pub async fn send(
    _st: &std::sync::Arc<AppState>,
    _snap: &Snapshot,
    _card: &str,
    _body: &Map<String, Value>,
) -> Outcome {
    failed("built without the claude-engine feature", None)
}

/// Prometheus lines for the engine (none without the feature or with it off).
pub fn metrics_text(st: &AppState) -> String {
    #[cfg(feature = "claude-engine")]
    {
        if st.snapshot().config.claude.enabled {
            return st.ext.claude.metrics_text();
        }
    }
    let _ = st;
    String::new()
}

/// A JSON error answer as a `reqwest::Response`, so the proxy streams it back like an engine's.
pub fn error_response(
    status: u16,
    message: &str,
    etype: &str,
    retry_after: Option<u64>,
) -> reqwest::Response {
    let body = json!({"error": {"message": message, "type": etype, "param": null,
                                "code": status.to_string()}});
    let mut b = axum::http::Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(s) = retry_after {
        b = b.header("retry-after", s.to_string());
    }
    reqwest::Response::from(
        b.body(reqwest::Body::from(body.to_string()))
            .expect("static response parts"),
    )
}

#[cfg(test)]
mod tests;
