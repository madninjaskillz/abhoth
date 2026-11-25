//! `TokenAuth`: every request (pages, assets, the hub, the terminal websocket, the API) must
//! carry the token, as `?token=` (the printed URL), the cookie set on that first visit, or an
//! `X-OpenOrca-Token` header. A first page visit with the token in the query gets the cookie
//! and is redirected to the same URL without it. Websockets and state-changing requests also
//! have to come from this origin.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::audit::Caller;
use crate::server::AppState;

pub const QUERY_NAME: &str = "token";
pub const HEADER_NAME: &str = "X-OpenOrca-Token";

/// Cookie name; per port, since cookies are shared across ports of one host.
pub fn cookie_name(port: u16) -> String {
    format!("openorca_token_{port}")
}

const TOKEN_PAGE: &str = concat!(
    "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>OpenOrca</title><link rel=icon type=image/png href=/brand/favicon-32.png>",
    "<body style=\"background:#0e1019;color:#d9dfec;font:15px system-ui;margin:0;padding:48px 16px;text-align:center\">",
    "<div style=\"width:128px;height:128px;margin:0 auto 20px;border-radius:50%;",
    "background:radial-gradient(closest-side,rgba(219,244,250,.22),rgba(34,211,238,.08) 60%,transparent)\">",
    "<img src=/brand/mascot-256.webp width=128 height=128 alt=\"\"></div>",
    "<h2 style=\"margin:0 0 8px\">Token required</h2><p style=\"max-width:28rem;margin:0 auto;color:#98a0b3\">",
    "Open the URL OpenOrca printed (it ends in <code>?token=…</code>). ",
    "In the TUI, <code>/hostui status</code> shows it again.</p></body>"
);

/// Constant-time comparison.
fn matches(candidate: Option<&str>, token: &str) -> bool {
    let Some(c) = candidate.filter(|c| !c.is_empty()) else {
        return false;
    };
    let (a, b) = (c.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Query parameters, decoded (`+` as space, percent escapes).
pub fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    let Some(q) = query else { return vec![] };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

fn starts_with_segment(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Websockets and non-GET requests from a browser must come from the page this server served
/// (Origin host:port equal to Host). Non-browser clients send no Origin and pass.
pub fn same_origin(method: &Method, headers: &HeaderMap) -> bool {
    let needs_check = is_websocket(headers) || !(method == Method::GET || method == Method::HEAD);
    if !needs_check {
        return true;
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| !o.is_empty())
    else {
        return true;
    };
    let Ok(o) = reqwest::Url::parse(origin) else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Ok(h) = reqwest::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    let origin_port = o
        .port()
        .unwrap_or(if o.scheme() == "https" { 443 } else { 80 });
    let host_port = h.port().unwrap_or(80);
    o.host_str()
        .zip(h.host_str())
        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && origin_port == host_port
}

/// The client's address for the audit log.
pub fn caller_of(req: &Request<Body>) -> Caller {
    Caller {
        ip: req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string()),
        user: None,
    }
}

pub async fn token_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // The mascot and favicon are public (the token page shows them); the HTML preview's files
    // carry their own signed, expiring token in the path.
    if (method == Method::GET || method == Method::HEAD)
        && starts_with_segment(&path, crate::files::preview::PREFIX)
    {
        return next.run(req).await;
    }
    if method == Method::GET && starts_with_segment(&path, "/brand") {
        return next.run(req).await;
    }
    let token = state.options.token.as_str();
    let pairs = query_pairs(req.uri().query());
    let from_query = matches(
        pairs
            .iter()
            .find(|(k, _)| k == QUERY_NAME)
            .map(|(_, v)| v.as_str()),
        token,
    );
    let headers = req.headers();
    let ok = from_query
        || matches(
            headers.get(HEADER_NAME).and_then(|v| v.to_str().ok()),
            token,
        )
        || matches(cookie(headers, &cookie_name(state.port)).as_deref(), token);
    let is_api = starts_with_segment(&path, "/api") || starts_with_segment(&path, "/hub");
    if !ok {
        let mut res = if method == Method::GET && !is_api {
            (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                TOKEN_PAGE,
            )
                .into_response()
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        };
        res.headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        return res;
    }
    if !same_origin(&method, headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if from_query {
        let cookie = format!(
            "{}={}; max-age=2592000; path=/; samesite=strict; httponly",
            cookie_name(state.port),
            token
        );
        // A page visit: drop the token from the address bar.
        if method == Method::GET && !is_websocket(headers) && !is_api {
            let rest: Vec<String> = pairs
                .iter()
                .filter(|(k, _)| k != QUERY_NAME)
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        crate::util::escape_data(k),
                        crate::util::escape_data(v)
                    )
                })
                .collect();
            let location = if rest.is_empty() {
                path.clone()
            } else {
                format!("{path}?{}", rest.join("&"))
            };
            return Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, location)
                .header(header::CACHE_CONTROL, "no-store")
                .header(header::SET_COOKIE, cookie)
                .body(Body::empty())
                .unwrap();
        }
        let mut res = next.run(req).await;
        if let Ok(v) = cookie.parse() {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
        return res;
    }
    next.run(req).await
}
