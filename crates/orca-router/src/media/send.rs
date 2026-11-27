//! Sending one request to a target, directly or through its bridge, with the answer streamed
//! back; and trying the candidates in turn (failover).

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;

use super::place::{self, Target};
use crate::bridges::{self, Hub, Upstream};
use crate::control::schema::ControlConfig;

/// One request as a target should get it.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub method: String,
    /// Path and query on the engine.
    pub path_q: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// How long to wait for an answer's head.
#[derive(Debug, Clone, Copy)]
pub struct Patience(pub Duration);

pub enum Sent {
    /// An answer to give the client (any status the engine chose).
    Answer(Response),
    /// Worth trying elsewhere: why, and the engine's own answer when it gave one.
    Retry(String, Option<Response>),
}

/// Engine answers that mean "not now": try the next candidate.
pub fn retryable(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// Request headers an engine may see: no client credentials, no hop-by-hop, no host.
pub fn engine_headers(h: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in bridges::forward_headers(h) {
        if matches!(k.as_str(), "host" | "content-length" | "origin" | "referer") {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            out.append(n, v);
        }
    }
    out
}

pub fn mark(r: &mut Response, t: &Target) {
    let h = r.headers_mut();
    for (k, v) in [
        ("x-abhoth-server", t.server.as_str()),
        ("x-abhoth-card", t.card.as_str()),
        ("x-abhoth-engine", t.engine.as_str()),
    ] {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    }
}

/// Keep `busy` until the body has been sent.
fn hold(r: Response, busy: place::Busy) -> Response {
    let (parts, body) = r.into_parts();
    let s = body.into_data_stream().map(move |x| {
        let _ = &busy;
        x
    });
    Response::from_parts(parts, Body::from_stream(s))
}

/// Send `p` to `t`.
pub async fn send(
    http: &reqwest::Client,
    hub: Option<&Hub>,
    c: &ControlConfig,
    t: &Target,
    p: Prepared,
    patience: Patience,
) -> Sent {
    let busy = place::begin(t);
    let up = bridges::upstream_for(c, &t.server, &t.engine, &t.card);
    let mut r = match up {
        None => return Sent::Retry(format!("{} has no address", t.key()), None),
        Some(Upstream::Bridge {
            bridge,
            engine,
            card,
        }) => {
            let Some(hub) = hub else {
                return Sent::Retry("no bridges here".into(), None);
            };
            let mut h = p.headers.clone();
            h.remove("authorization");
            let fut = bridges::forward(
                hub, &bridge, &engine, &card, &p.method, &p.path_q, &h, p.body,
            );
            match tokio::time::timeout(patience.0, fut).await {
                Ok(r) => r,
                Err(_) => return Sent::Retry(format!("{} did not answer", t.key()), None),
            }
        }
        Some(Upstream::Http(base)) => {
            let url = format!("{}{}", base.trim_end_matches('/'), p.path_q);
            let method =
                reqwest::Method::from_bytes(p.method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let mut rb = http.request(method, &url).headers(p.headers.clone());
            if !p.body.is_empty() {
                rb = rb.body(p.body);
            }
            match tokio::time::timeout(patience.0, rb.send()).await {
                Err(_) => return Sent::Retry(format!("{} did not answer", t.key()), None),
                Ok(Err(e)) => {
                    let why = if e.is_connect() {
                        format!("{} refused the connection", t.key())
                    } else {
                        format!("{}: {e}", t.key())
                    };
                    return Sent::Retry(why, None);
                }
                Ok(Ok(resp)) => from_reqwest(resp),
            }
        }
    };
    mark(&mut r, t);
    let status = r.status().as_u16();
    let r = hold(r, busy);
    if retryable(status) {
        let (r, reason) = with_reason(r).await;
        let why = match reason {
            Some(reason) => format!("{} answered {status}: {reason}", t.key()),
            None => format!("{} answered {status}", t.key()),
        };
        return Sent::Retry(why, Some(r));
    }
    Sent::Answer(r)
}

/// The longest engine reason kept in a failover note.
const REASON_MAX: usize = 300;

/// A "not now" answer's body read (it is small: a bridge's "image X is not on this host and
/// pulling failed", a 429's message), the answer rebuilt with it, and the reason in it.
async fn with_reason(r: Response) -> (Response, Option<String>) {
    let (parts, body) = r.into_parts();
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(body, 64 << 10),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let reason = reason_in(&bytes);
    (Response::from_parts(parts, Body::from(bytes)), reason)
}

/// The reason in an error body: an OpenAI-style `error.message`, a `detail` / `message` /
/// `reason` / `error` string, else the text's first line; trimmed and cut to `REASON_MAX`.
pub fn reason_in(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let from_json = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            let pick = |v: &serde_json::Value| -> Option<String> {
                ["message", "detail", "reason", "error"]
                    .iter()
                    .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(str::to_string))
            };
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .or_else(|| pick(&v))
        });
    let reason = match from_json {
        Some(r) => r,
        None if text.starts_with('{') || text.starts_with('<') => return None,
        None => text.lines().next().unwrap_or_default().to_string(),
    };
    let reason = reason.trim();
    if reason.is_empty() {
        return None;
    }
    Some(match reason.char_indices().nth(REASON_MAX) {
        Some((i, _)) => format!("{}…", &reason[..i]),
        None => reason.to_string(),
    })
}

pub fn from_reqwest(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        if orca_bridge::proto::hop_by_hop(k.as_str()) {
            continue;
        }
        headers.append(k.clone(), v.clone());
    }
    let mut r = Response::new(Body::from_stream(resp.bytes_stream()));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

/// The outcome of trying the candidates.
pub struct Tried {
    pub response: Response,
    pub target: Option<Target>,
    /// Each failed attempt, in order.
    pub failed: Vec<String>,
}

/// Try `targets` in order until one answers; a failed one is cooled. With none answering, the
/// last engine answer (when there was one) or a 503 naming why.
pub async fn try_all(
    http: &reqwest::Client,
    hub: Option<&Hub>,
    c: &ControlConfig,
    targets: &[Target],
    what: &str,
    patience: Patience,
    prep: impl Fn(&Target) -> Result<Prepared, String>,
) -> Tried {
    let mut failed = Vec::new();
    let mut last: Option<(u16, Option<HeaderValue>)> = None;
    for t in targets {
        let p = match prep(t) {
            Ok(p) => p,
            Err(why) => {
                failed.push(why);
                continue;
            }
        };
        match send(http, hub, c, t, p, patience).await {
            Sent::Answer(r) => {
                return Tried {
                    response: r,
                    target: Some(t.clone()),
                    failed,
                };
            }
            Sent::Retry(why, r) => {
                tracing::info!("media: {what}: {why}; trying the next");
                place::cool(t);
                failed.push(why);
                if let Some(r) = r {
                    last = Some((r.status().as_u16(), r.headers().get("retry-after").cloned()));
                }
            }
        }
    }
    let msg = if targets.is_empty() {
        format!("no engine serves {what} here")
    } else {
        format!("no engine for {what} answered: {}", failed.join("; "))
    };
    Tried {
        response: all_failed(&msg, last),
        target: None,
        failed,
    }
}

/// The answer when every candidate failed: 503 (429 when the last one said "too many"), with
/// every failover's reason in the message and the last engine's `Retry-After`.
pub fn all_failed(msg: &str, last: Option<(u16, Option<HeaderValue>)>) -> Response {
    let status = match last {
        Some((429, _)) => 429,
        _ => 503,
    };
    let mut r = super::error(status, msg);
    if let Some((_, Some(v))) = last {
        r.headers_mut().insert("retry-after", v);
    }
    r
}
