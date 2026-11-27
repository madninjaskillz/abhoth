//! HTTP front: one dispatcher per listener kind. The main listener speaks LiteLLM's surface plus
//! the qfn-lb card paths; the compatibility listeners speak qfn-lb (`:8092`) and pig-slots
//! (`:8099`) on every path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::Snapshot;
use crate::proxy::{self, RequestCtx, json_response};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// LiteLLM's surface (`:8010`) plus `/a/`, `/b/`, `/<engine>/` and `/slots/...`.
    Gateway,
    /// qfn-lb on every path (`/` = the main engine's pool).
    Lb,
    /// pig-slots on every path.
    Slots,
}

#[derive(Clone)]
struct Ctx {
    st: Arc<AppState>,
    mode: Mode,
}

pub fn app(st: Arc<AppState>, mode: Mode) -> Router {
    Router::new()
        .fallback(dispatch)
        .with_state(Ctx { st, mode })
}

/// Serve `app` on a bound listener until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    st: Arc<AppState>,
    mode: Mode,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        app(st, mode).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

fn not_found() -> Response {
    json_response(404, &json!({"detail": "Not Found"}))
}

async fn dispatch(State(cx): State<Ctx>, req: Request) -> Response {
    let st = cx.st.clone();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let client = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_default();
    let loopback = peer.is_some_and(|p| p.ip().is_loopback());
    let ctx = RequestCtx {
        id: st.next_id(),
        client,
        started: Some(Instant::now()),
        peer: peer.map(|p| p.ip()),
    };
    // A bridge dialling in (ROUTER-MEDIA-PLAN.md 13.1): a WebSocket, before any body is read.
    if cx.mode == Mode::Gateway
        && crate::bridges::ws::is_bridge_path(req.method(), req.uri().path())
    {
        return crate::bridges::ws::upgrade(st.bridges.get().cloned(), req).await;
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let headers = req.headers().clone();
    let snap = st.snapshot();
    let limit = snap.config.settings.max_body_bytes;
    let raw = match axum::body::to_bytes(req.into_body(), limit).await {
        Ok(b) => b,
        Err(_) => {
            return proxy::error(413, "request body too large", "invalid_request_error", None);
        }
    };

    match cx.mode {
        Mode::Slots => return crate::slots::http(&st, &method, &path, &raw).await,
        Mode::Lb => {
            let (cards, rest) = lb_target(&snap, &path_q).unwrap_or((snap.main_pool(), path_q));
            return proxy::lb(st, ctx, cards, method, rest, headers, raw).await;
        }
        Mode::Gateway => {}
    }

    match (method.clone(), path.as_str()) {
        (Method::POST, "/v1/chat/completions" | "/chat/completions") => {
            proxy::chat(st, ctx, headers, raw).await
        }
        (Method::GET, "/v1/models" | "/models") => json_response(200, &models(&snap)),
        (Method::GET, "/v1/model/info" | "/model/info") => json_response(200, &model_info(&snap)),
        // D149: what this server is, and the models with their metadata (read-only).
        (Method::GET, orca_core::router_mode::WELL_KNOWN_PATH) => {
            crate::discovery::well_known(&st, peer.map(|p| p.ip())).await
        }
        (Method::GET, orca_core::router_mode::MODELS_PATH) => {
            crate::discovery::models(&st, &headers, peer.map(|p| p.ip())).await
        }
        // D149: asking for a key (an admin approves it in the web UI); never switches.
        (Method::POST, orca_core::router_mode::KEY_REQUESTS_PATH) => {
            crate::key_requests::start(&st, peer.map(|p| p.ip()), &raw).await
        }
        (Method::POST, orca_core::router_mode::KEY_REQUESTS_POLL_PATH) => {
            crate::key_requests::poll(&st, peer.map(|p| p.ip()), &raw).await
        }
        (Method::GET, "/health/liveliness" | "/health/liveness") => {
            json_response(200, &Value::String("I'm alive!".into()))
        }
        (Method::GET, "/health") => {
            json_response(200, &json!({"status": "ok", "service": "abhoth-router"}))
        }
        (Method::GET, "/status") => json_response(200, &status(&st)),
        (Method::GET, "/observe") => {
            let q = path_q.split_once('?').map(|(_, q)| q);
            json_response(200, &crate::observe::view(&st, q))
        }
        (Method::GET, "/metrics") => {
            let mut text = crate::status::metrics_text(&st);
            if let Some(cp) = st.control.get() {
                text.push_str(&crate::control_api::metrics_text(cp));
            }
            text.push_str(&crate::claude::metrics_text(&st));
            let mut r = text.into_response();
            r.headers_mut().insert(
                "content-type",
                HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            r
        }
        // The control plane's routes exist only while it is on; off, these paths go where they
        // went before it existed.
        (Method::GET, "/v1/abhoth/view" | "/v1/abhoth/events") if st.control.get().is_some() => {
            let cp = st.control.get().expect("checked");
            crate::control_api::user_view::handle(cp, &path, &headers, peer.map(|p| p.ip())).await
        }
        // D151: the router's own admin pages (and their sign-in) on this port.
        (_, p) if crate::admin_ui::is_ui_path(p) => {
            crate::admin_ui::handle(&st, &method, p, &headers, peer.map(|p| p.ip()), &raw).await
        }
        (_, p)
            if (p == "/admin/api/gateway" || p.starts_with("/admin/api/gateway/"))
                && st.admin_plane().is_some() =>
        {
            let cp = st.admin_plane().expect("checked");
            crate::admin_ui::gateway(&st, cp, &method, p, &headers, peer.map(|p| p.ip()), &raw)
                .await
        }
        (_, p)
            if (p == "/admin/api" || p.starts_with("/admin/api/"))
                && st.admin_plane().is_some() =>
        {
            let cp = st.admin_plane().expect("checked");
            crate::control_api::handle(cp, &method, &path_q, &headers, peer.map(|p| p.ip()), &raw)
                .await
        }
        (_, p) if p.starts_with("/admin/") => {
            if !loopback && !admin_allowed(&snap, &headers) {
                return proxy::error(403, "admin endpoints are loopback-only", "forbidden", None);
            }
            admin(&st, &method, p, &raw).await
        }
        (_, p) if p.starts_with("/slots/") || p == "/slots" => {
            let sub = p.strip_prefix("/slots").unwrap_or("/");
            crate::slots::http(&st, &method, if sub.is_empty() { "/" } else { sub }, &raw).await
        }
        (_, "/ondemand") => json_response(200, &crate::ondemand::view(&st).await),
        _ => match lb_target(&snap, &path_q) {
            Some((cards, rest)) => proxy::lb(st, ctx, cards, method, rest, headers, raw).await,
            None => not_found(),
        },
    }
}

/// An admin key file, when configured, also allows admin calls from the LAN.
pub(crate) fn admin_allowed(snap: &Snapshot, headers: &axum::http::HeaderMap) -> bool {
    let Some(file) = snap.config.settings.admin_key_file.as_deref() else {
        return false;
    };
    let Ok(key) = std::fs::read_to_string(file) else {
        return false;
    };
    let want = format!("Bearer {}", key.trim());
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == want)
}

/// `/a/v1/...` -> (["A"], "/v1/..."); `/ninfer/...` -> the engine's cards.
pub fn lb_target(snap: &Snapshot, path_q: &str) -> Option<(Vec<String>, String)> {
    let trimmed = path_q.strip_prefix('/')?;
    let (seg, rest) = match trimmed.find(['/', '?']) {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    };
    if seg.is_empty() {
        return None;
    }
    let cards = snap.lb_cards(seg)?;
    let rest = if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    };
    Some((cards, rest))
}

/// GET /v1/models, LiteLLM's shape: one entry per name, `owned_by: "openai"` (load-bearing for
/// OpenOrca's ServerIdentity gateway detection).
pub fn models(snap: &Snapshot) -> Value {
    let mut seen: Vec<&str> = Vec::new();
    let mut data = Vec::new();
    for d in &snap.config.deployments {
        if seen.contains(&d.name.as_str()) {
            continue;
        }
        seen.push(&d.name);
        let mut m = Map::new();
        m.insert("id".into(), json!(d.name));
        m.insert("object".into(), json!("model"));
        m.insert("created".into(), json!(1677610602));
        m.insert("owned_by".into(), json!("openai"));
        if let Some(n) = d.max_input_tokens() {
            m.insert("max_input_tokens".into(), json!(n));
        }
        if let Some(n) = max_output(d) {
            m.insert("max_output_tokens".into(), json!(n));
        }
        data.push(Value::Object(m));
    }
    json!({"data": data, "object": "list"})
}

/// LiteLLM reports max_output_tokens = max_input_tokens for deployments on a card and none for
/// the relay ones (observed 2026-11-25); an explicit value wins.
fn max_output(d: &crate::config::Deployment) -> Option<i64> {
    d.model_info
        .get("max_output_tokens")
        .and_then(Value::as_i64)
        .or_else(|| if d.relay { None } else { d.max_input_tokens() })
}

/// GET /v1/model/info: our keys only (no cost fields, never a key), `gateway_rev` = config rev.
pub fn model_info(snap: &Snapshot) -> Value {
    let mut data = Vec::new();
    for d in &snap.config.deployments {
        let base = if d.relay {
            snap.config
                .relay
                .as_ref()
                .map(|r| r.url.clone())
                .unwrap_or_default()
        } else {
            d.cards
                .iter()
                .filter_map(|c| snap.config.upstreams.get(c))
                .map(|u| format!("{}/v1", u.url.trim_end_matches('/')))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut h = Sha256::new();
        h.update(format!("{}|{}|{}", d.name, d.served_id, base).as_bytes());
        let id: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let mut mi = Map::new();
        mi.insert("id".into(), json!(id));
        // LiteLLM leaves out our keys whose value is null (e.g. "cards": null)
        for (k, v) in &d.model_info {
            if k != "gateway_rev" && !v.is_null() {
                mi.insert(k.clone(), v.clone());
            }
        }
        // LiteLLM: null in model_info when unknown (the relay names)
        mi.insert(
            "max_output_tokens".into(),
            max_output(d).map_or(Value::Null, |n| json!(n)),
        );
        mi.insert("gateway_rev".into(), json!(snap.config.rev));
        let mut lp = Map::new();
        lp.insert("model".into(), json!(format!("openai/{}", d.served_id)));
        lp.insert("api_base".into(), json!(base));
        // shown as LiteLLM shows it (the drop itself happens in rewrite)
        if !d.drop_params.is_empty() {
            lp.insert("additional_drop_params".into(), json!(d.drop_params));
        }
        data.push(json!({
            "model_name": d.name,
            "litellm_params": lp,
            "model_info": mi,
        }));
    }
    json!({"data": data})
}

/// GET /status: one JSON for the dash.
pub fn status(st: &AppState) -> Value {
    crate::status::status(st)
}

async fn admin(st: &Arc<AppState>, method: &Method, path: &str, raw: &Bytes) -> Response {
    match (method.clone(), path) {
        (Method::POST, "/admin/reload") => match crate::reload::reload_now(st) {
            Ok(msg) => json_response(200, &json!({"ok": true, "message": msg})),
            Err(e) => json_response(400, &json!({"ok": false, "error": e})),
        },
        (Method::POST, p) if p.starts_with("/admin/cards/") => {
            // /admin/cards/<card>/drain | /undrain
            let rest = &p["/admin/cards/".len()..];
            let mut parts = rest.split('/');
            let card = parts.next().unwrap_or("").to_uppercase();
            let action = parts.next().unwrap_or("");
            crate::ondemand::card_admin(st, &card, action, raw).await
        }
        _ => not_found(),
    }
}
