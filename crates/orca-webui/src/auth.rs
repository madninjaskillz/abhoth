//! Request helpers shared by the account middleware ([`crate::login`], D140): the launch token's
//! query and header names, cookies, websocket and origin checks.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, header};

use crate::audit::Caller;

pub const QUERY_NAME: &str = "token";
pub const HEADER_NAME: &str = "X-OpenOrca-Token";

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

pub fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

pub fn starts_with_segment(path: &str, prefix: &str) -> bool {
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
        ..Default::default()
    }
}
