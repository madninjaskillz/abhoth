//! Media routes on `:8010` (D153 phase 3, ROUTER-MEDIA-PLAN.md 4):
//!
//! - `POST /v1/audio/speech`: text to speech (Chatterbox, Kokoro);
//! - `POST /v1/audio/transcriptions`: speech to text (whisper over Wyoming);
//! - `/v1/audio/music`: Stable Audio 3, waiting or as a job;
//! - `/v1/images/*`: to the image API shim (phase 4 brings its logic into the router);
//! - `/comfyui/*`: ComfyUI's API, UI and `/ws`, signed in only.
//!
//! Each request goes to the best card that lists the kind (`cards.<id>.engines`), DeepThought
//! before the bridges, and to the next one when an engine does not answer (place.rs). Answers
//! stream back; request bodies have a limit per route.

pub mod comfy;
pub mod multipart;
pub mod music;
pub mod place;
pub mod send;
pub mod speech;
pub mod stt;
pub mod wav;
pub mod wyoming;

#[cfg(test)]
mod tests;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::access::{Perm, Principal};
use crate::bridges::Hub;
use crate::control::schema::{ControlConfig, MediaApi, ModelKind};
use crate::proxy::RequestCtx;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Speech,
    Transcribe,
    Music,
    Images,
    Comfy,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::Speech => "speech",
            Route::Transcribe => "transcriptions",
            Route::Music => "music",
            Route::Images => "images",
            Route::Comfy => "comfyui",
        }
    }

    /// The largest request body (uploads for edits, audio and ComfyUI's `/upload/image`).
    pub fn limit(self) -> usize {
        match self {
            Route::Speech => 1 << 20,
            Route::Music => 64 << 10,
            Route::Transcribe => 100 << 20,
            Route::Images => 100 << 20,
            Route::Comfy => 512 << 20,
        }
    }
}

pub fn route_of(method: &Method, path: &str) -> Option<Route> {
    let post = method == Method::POST;
    match path {
        "/v1/audio/speech" | "/audio/speech" if post => Some(Route::Speech),
        "/v1/audio/transcriptions" | "/audio/transcriptions" if post => Some(Route::Transcribe),
        p if p == "/v1/audio/music" || p.starts_with("/v1/audio/music/") => Some(Route::Music),
        p if p.starts_with("/v1/images/") => Some(Route::Images),
        p if p == comfy::PREFIX || p.starts_with("/comfyui/") => Some(Route::Comfy),
        _ => None,
    }
}

/// The media routes' HTTP client: redirects go back to the client (ComfyUI's UI makes them).
pub fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("http client")
    })
}

pub fn error(status: u16, msg: &str) -> Response {
    crate::proxy::error(status, msg, "media_error", None)
}

/// What one request works with.
pub struct Cx<'a> {
    pub http: &'a reqwest::Client,
    pub hub: Option<&'a Hub>,
    pub c: &'a ControlConfig,
}

impl Cx<'_> {
    /// A bridge's priority while it is connected and enabled.
    pub fn bridge_priority(&self) -> impl Fn(&str) -> Option<i32> + '_ {
        move |server: &str| {
            let h = self.hub?;
            let s = h.settings(server);
            (h.online(server) && s.enabled).then_some(s.priority)
        }
    }
}

/// Who may call: the media API as chat (keys, or anonymous from the LAN); ComfyUI signed in only
/// (an access key or the web UI's session), and a browser session from this site only. `Some` is
/// the refusal.
async fn refusal(
    st: &AppState,
    route: Route,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Option<Response> {
    let Some(cp) = st.admin_plane() else {
        // no access control configured: as chat; ComfyUI from this machine only
        return (route == Route::Comfy && !peer.is_some_and(|p| p.is_loopback()))
            .then(|| error(403, "ComfyUI needs the router's access control"));
    };
    let (p, session) = match cp
        .access
        .authenticate_session(headers, peer, method.as_str(), path)
        .await
    {
        Ok(x) => x,
        Err(crate::access::AuthError::Unauthorized(m)) => return Some(error(401, m)),
        Err(_) => return Some(error(503, "the accounts service is not answering")),
    };
    if !cp.access.allows(&p, Perm::Chat) && !p.is_admin() {
        return Some(error(
            if matches!(p, Principal::Anonymous { .. }) {
                401
            } else {
                403
            },
            "this key may not use the media API",
        ));
    }
    if route == Route::Comfy {
        if matches!(p, Principal::Anonymous { .. }) {
            return Some(error(401, "sign in (or send an access key) to use ComfyUI"));
        }
        if session.is_some() && !same_origin(headers) {
            return Some(error(
                403,
                "ComfyUI calls with a session must come from this site",
            ));
        }
    }
    None
}

/// A browser's `Origin`, when it sends one, is this host.
fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = h
        .get("x-forwarded-host")
        .or_else(|| h.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let o = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    !host.is_empty() && o.eq_ignore_ascii_case(host)
}

fn is_ws(req: &Request) -> bool {
    req.headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Handle a media request (before any body is read).
pub async fn handle(st: Arc<AppState>, ctx: RequestCtx, route: Route, req: Request) -> Response {
    let started = ctx.started.unwrap_or_else(Instant::now);
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let headers = req.headers().clone();
    if let Some(r) = refusal(&st, route, &method, &path, &headers, ctx.peer).await {
        return log(&st, &ctx, route, &path, started, None, r);
    }
    let hub = st.bridges.get().cloned();
    let c = match &hub {
        Some(h) => h.config(),
        None => return error(503, "the media routes need the control plane"),
    };
    if route == Route::Comfy && path == comfy::PREFIX {
        let mut r = (axum::http::StatusCode::FOUND, "").into_response();
        r.headers_mut().insert(
            "location",
            axum::http::HeaderValue::from_static("/comfyui/"),
        );
        return r;
    }
    if route == Route::Comfy && is_ws(&req) {
        let cx = Cx {
            http: client(),
            hub: hub.as_deref(),
            c: &c,
        };
        let Some((t, rest)) = comfy::ws_target(&c, &path_q, &cx.bridge_priority()) else {
            return error(503, "no ComfyUI is available");
        };
        let (mut parts, _) = req.into_parts();
        let up = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(u) => u,
            Err(e) => return e.into_response(),
        };
        let fwd = send::engine_headers(&headers);
        let mut r = up
            .max_message_size(64 << 20)
            .on_upgrade(move |socket| comfy::relay(hub, c, t, rest, fwd, socket));
        r.headers_mut().insert(
            "x-abhoth-media",
            axum::http::HeaderValue::from_static("comfyui-ws"),
        );
        return r;
    }
    let raw = match axum::body::to_bytes(req.into_body(), route.limit()).await {
        Ok(b) => b,
        Err(_) => {
            let r = error(
                413,
                &format!(
                    "request body too large (the {} limit is {} MiB)",
                    route.name(),
                    route.limit() >> 20
                ),
            );
            return log(&st, &ctx, route, &path, started, None, r);
        }
    };
    let cx = Cx {
        http: client(),
        hub: hub.as_deref(),
        c: &c,
    };
    let query = path_q.split_once('?').map(|(_, q)| q).unwrap_or("");
    let tried = match route {
        Route::Speech => speech::run(&cx, &raw).await,
        Route::Transcribe => {
            let ct = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            stt::run(&cx, ct, &raw).await
        }
        Route::Music => music::run(&cx, method.as_str(), &path, query, &raw).await,
        Route::Images => images(&cx, method.as_str(), &path_q, &headers, &raw).await,
        Route::Comfy => comfy::http(&cx, method.as_str(), &path_q, &headers, &raw).await,
    };
    let mut r = tried.response;
    if !tried.failed.is_empty()
        && let Ok(v) = axum::http::HeaderValue::from_str(&tried.failed.len().to_string())
    {
        r.headers_mut().insert("x-abhoth-failovers", v);
    }
    log(&st, &ctx, route, &path, started, tried.target.as_ref(), r)
}

/// `/v1/images/*` as it is: the image API shim (its own queue, buckets and backends).
async fn images(
    cx: &Cx<'_>,
    method: &str,
    path_q: &str,
    headers: &HeaderMap,
    raw: &Bytes,
) -> send::Tried {
    let ts = place::candidates(
        cx.c,
        &place::Want {
            kinds: &[ModelKind::Image],
            apis: &[MediaApi::OpenaiImages],
            model: None,
            server: None,
        },
        &cx.bridge_priority(),
    );
    let mut h = HeaderMap::new();
    if let Some(v) = headers.get("content-type") {
        h.insert("content-type", v.clone());
    }
    send::try_all(
        cx.http,
        cx.hub,
        cx.c,
        &ts,
        "images",
        // the shim waits for a free backend for up to an hour
        send::Patience(Duration::from_secs(3700)),
        |_| {
            Ok(send::Prepared {
                method: method.to_string(),
                path_q: path_q.to_string(),
                headers: h.clone(),
                body: raw.clone(),
            })
        },
    )
    .await
}

fn log(
    st: &AppState,
    ctx: &RequestCtx,
    route: Route,
    path: &str,
    started: Instant,
    t: Option<&place::Target>,
    r: Response,
) -> Response {
    // ComfyUI's UI makes many small calls: its prompts only
    if route == Route::Comfy && !path.ends_with("/prompt") {
        return r;
    }
    let mut rec = serde_json::Map::new();
    rec.insert(
        "at".into(),
        json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    rec.insert("id".into(), json!(ctx.id));
    rec.insert("client".into(), json!(ctx.client));
    rec.insert("path".into(), json!(path));
    rec.insert("kind".into(), json!(route.name()));
    if let Some(t) = t {
        rec.insert("server".into(), json!(t.server));
        rec.insert("card".into(), json!(t.card));
        rec.insert("engine".into(), json!(t.engine));
    }
    rec.insert("status".into(), json!(r.status().as_u16()));
    rec.insert(
        "seconds".into(),
        json!((started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0),
    );
    st.log.write(&Value::Object(rec));
    r
}

/// `GET /v1/models?kind=<kind>|media`: the inventory's media models (recipes and whole models,
/// not their components), with their kind. Without `kind` the list stays the chat models'.
pub fn models(c: &ControlConfig, kind: &str) -> Value {
    let want = |k: ModelKind| match kind {
        "media" | "all" => !matches!(k, ModelKind::Llm | ModelKind::Embedding),
        other => k.as_str() == other,
    };
    let mut data = Vec::new();
    for (id, m) in &c.models {
        let Some(k) = m.kind else { continue };
        if !want(k) || m.role.is_some() {
            continue;
        }
        data.push(json!({"id": id, "object": "model", "created": 1677610602,
                         "owned_by": "abhoth", "kind": k.as_str()}));
    }
    json!({"data": data, "object": "list"})
}
