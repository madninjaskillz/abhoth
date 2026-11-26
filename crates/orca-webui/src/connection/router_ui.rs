//! "Router admin" in the settings' connection panel (D151): when the app uses an Abhoth router
//! (router mode) and the key it uses there is an admin's, the router's own admin pages open inside
//! the app, through this server.
//!
//! - `GET /api/settings/connection/router-admin`: is the configured router's key an admin key?
//!   (`/admin/api/whoami` with the key) and where the router's own page is.
//! - `/api/settings/connection/router-admin/ui/*`: the router's `/admin/*`, with the key added
//!   here (it never reaches the browser), no cookies either way. `session` is answered with this
//!   app's own CSRF header and token, so the page's state-changing calls pass this server's CSRF
//!   check; sign-in and sign-out are not needed (and refused) here.
//!
//! Admins only (all of `/api/settings` is). Every call is to the configured router's root, with
//! `x-abhoth-no-switch: 1`, and no redirects are followed.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use serde_json::{Value, json};

use orca_core::config::LmStudioConfig;
use orca_core::router_mode;

use crate::login::{CSRF_HEADER, Via, principal_of};
use crate::server::AppState;

/// The router's admin page, relative to its root.
pub const ADMIN_PATH: &str = "/admin/";

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client")
    })
}

fn bad(status: StatusCode, error: &str) -> Response {
    let mut r = (status, axum::Json(json!({"error": error}))).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The router root and key, when the app is in router mode with an Abhoth key.
fn target(lm: &LmStudioConfig) -> Result<(String, String), &'static str> {
    if !lm.is_router() {
        return Err("The app is not connected to an Abhoth router (router mode is off).");
    }
    let key = lm.resolve_api_key();
    if !router_mode::is_router_key(&key) {
        return Err("No Abhoth key (abk_…) is saved for the router.");
    }
    let root = router_mode::root_of(&lm.base_url);
    if !orca_core::config::schema::is_http_url(&root) {
        return Err("The router URL is not http(s).");
    }
    Ok((root, key))
}

/// Whoami at the router with `key`: (admin, principal) or why not.
pub async fn whoami(root: &str, key: &str) -> Result<(bool, String), String> {
    let r = client()
        .get(format!("{root}/admin/api/whoami"))
        .bearer_auth(key)
        .header(router_mode::NO_SWITCH_HEADER, "1")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "The router did not answer in time.".to_string()
            } else {
                "The router could not be reached.".to_string()
            }
        })?;
    match r.status().as_u16() {
        200 => {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            Ok((
                v["admin"].as_bool().unwrap_or(false),
                v["principal"].as_str().unwrap_or("").to_string(),
            ))
        }
        401 => Err("The router refused the key.".into()),
        404 => Err(
            "This router has no admin pages (it needs --control, and version 0.16.13 or later)."
                .into(),
        ),
        s => Err(format!("The router answered {s}.")),
    }
}

fn lm_of(state: &AppState) -> LmStudioConfig {
    let mut m = orca_core::config::ConfigManager::new(state.options.config_home.as_deref());
    let _ = m.load();
    m.config.lm_studio
}

/// `GET /api/settings/connection/router-admin`.
pub async fn status(State(state): State<Arc<AppState>>) -> Response {
    let lm = lm_of(&state);
    let (root, key) = match target(&lm) {
        Ok(t) => t,
        Err(why) => return axum::Json(json!({"available": false, "reason": why})).into_response(),
    };
    let url = format!("{root}{ADMIN_PATH}");
    match whoami(&root, &key).await {
        Ok((true, principal)) => axum::Json(json!({
            "available": true,
            "principal": principal,
            "url": url,
            "embedded": "/api/settings/connection/router-admin/ui/",
        }))
        .into_response(),
        Ok((false, _)) => axum::Json(json!({
            "available": false,
            "url": url,
            "reason": "The app's router key is not an admin key.",
        }))
        .into_response(),
        Err(why) => {
            axum::Json(json!({"available": false, "url": url, "reason": why})).into_response()
        }
    }
}

/// `/api/settings/connection/router-admin/ui` (no slash): to the folder, so relative URLs work.
pub async fn ui_root() -> Response {
    let mut r = StatusCode::PERMANENT_REDIRECT.into_response();
    r.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("ui/"));
    r
}

pub async fn ui_index(state: State<Arc<AppState>>, req: Request<Body>) -> Response {
    ui(state, Path(String::new()), req).await
}

/// Headers passed to the router and back.
const FORWARD_IN: &[&str] = &["content-type", "if-match", "accept", "last-event-id"];
const FORWARD_OUT: &[&str] = &[
    "content-type",
    "etag",
    "cache-control",
    "retry-after",
    "content-security-policy",
    "location",
];

/// `/api/settings/connection/router-admin/ui/<rest>` -> `<router>/admin/<rest>` with the key.
pub async fn ui(
    State(state): State<Arc<AppState>>,
    Path(rest): Path<String>,
    req: Request<Body>,
) -> Response {
    let Some(p) = principal_of(&req).filter(|p| p.admin) else {
        return bad(StatusCode::FORBIDDEN, "Only an admin can do that.");
    };
    if rest.split('/').any(|s| s == ".." || s == ".") || rest.contains('\\') {
        return bad(StatusCode::BAD_REQUEST, "Bad path.");
    }
    if rest == "login" || rest == "logout" {
        return bad(
            StatusCode::NOT_FOUND,
            "Signed in through the app; nothing to do here.",
        );
    }
    let lm = lm_of(&state);
    let (root, key) = match target(&lm) {
        Ok(t) => t,
        Err(why) => return bad(StatusCode::NOT_FOUND, why),
    };
    let method = req.method().clone();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let mut out = reqwest::header::HeaderMap::new();
    for h in FORWARD_IN {
        if let Some(v) = req.headers().get(*h)
            && let Ok(v) = reqwest::header::HeaderValue::from_bytes(v.as_bytes())
        {
            out.insert(*h, v);
        }
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
        out.insert(reqwest::header::AUTHORIZATION, v);
    }
    out.insert(
        router_mode::NO_SWITCH_HEADER,
        reqwest::header::HeaderValue::from_static("1"),
    );
    let body = match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return bad(StatusCode::PAYLOAD_TOO_LARGE, "Request too large."),
    };
    let rm =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let url = format!("{root}/admin/{rest}{query}");
    let sent = client()
        .request(rm, url)
        .headers(out)
        .body(body)
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(_) => return bad(StatusCode::BAD_GATEWAY, "The router could not be reached."),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // The page asks who it is: the router's answer, with this server's CSRF header and token
    // (state-changing calls go through this server's CSRF check).
    if rest == "session" && method == Method::GET {
        let mut v: Value = resp.json().await.unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            let csrf = match &p.via {
                Via::Session { csrf, .. } => Some(csrf.clone()),
                _ => None,
            };
            o.insert("via".into(), json!("app"));
            o.insert("embedded".into(), json!(true));
            o.insert("csrfHeader".into(), json!(CSRF_HEADER));
            o.insert("csrf".into(), json!(csrf));
            o.insert("hasKeys".into(), json!(true));
        }
        let mut r = (status, axum::Json(v)).into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }
    let mut headers = axum::http::HeaderMap::new();
    for h in FORWARD_OUT {
        if let Some(v) = resp.headers().get(*h)
            && let Ok(v) = HeaderValue::from_bytes(v.as_bytes())
        {
            headers.insert(*h, v);
        }
    }
    // Streamed (the live state is SSE); no cookie from the router reaches the browser.
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_router_mode_with_an_abhoth_key() {
        let mut lm = LmStudioConfig::default();
        assert!(target(&lm).is_err());
        lm.connection = Some("router".into());
        lm.base_url = "http://router.lan:8010/v1".into();
        lm.api_key = "lm-studio".into();
        assert!(target(&lm).is_err(), "a placeholder key is not an admin's");
        lm.api_key = "abk_0123456789abcdef0123456789".into();
        let (root, key) = target(&lm).unwrap();
        assert_eq!(root, "http://router.lan:8010");
        assert!(key.starts_with("abk_"));
    }
}
