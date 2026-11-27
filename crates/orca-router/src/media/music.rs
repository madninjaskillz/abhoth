//! `/v1/audio/music` (an Abhoth extension; the body is Stable Audio 3's `{prompt, seconds, bpm,
//! seed, negative}`).
//!
//! - `POST /v1/audio/music`: waits and answers with the MP3.
//! - `POST /v1/audio/music?async=1`: `202` with the job; `GET /v1/audio/music/<id>` for its state
//!   and `GET /v1/audio/music/<id>.mp3` for the file, from the server that ran it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::Value;

use super::place::{self, Target, Want};
use super::send::{self, Patience, Prepared, Tried};
use crate::control::schema::{MediaApi, ModelKind};

/// Jobs are remembered this long (their files stay on the engine).
const KEEP: Duration = Duration::from_secs(24 * 3600);
const MAX_JOBS: usize = 2000;

fn jobs() -> &'static Mutex<HashMap<String, (Target, Instant)>> {
    static J: OnceLock<Mutex<HashMap<String, (Target, Instant)>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

pub fn remember(id: &str, t: &Target) {
    let mut g = jobs().lock().unwrap();
    let now = Instant::now();
    g.retain(|_, (_, at)| now.duration_since(*at) < KEEP);
    if g.len() >= MAX_JOBS
        && let Some(oldest) = g
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(k, _)| k.clone())
    {
        g.remove(&oldest);
    }
    g.insert(id.to_string(), (t.clone(), now));
}

pub fn owner(id: &str) -> Option<Target> {
    jobs().lock().unwrap().get(id).map(|(t, _)| t.clone())
}

fn json_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h
}

/// The engine's job as the client sees it: its URLs under `/v1/audio/music/`.
pub fn public_job(mut v: Value) -> Value {
    if let Some(id) = v.get("id").and_then(Value::as_str).map(str::to_string) {
        let done = v.get("download_url").is_some_and(|d| !d.is_null());
        v["status_url"] = Value::String(format!("/v1/audio/music/{id}"));
        v["download_url"] = if done {
            Value::String(format!("/v1/audio/music/{id}.mp3"))
        } else {
            Value::Null
        };
    }
    v
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn read_json(r: axum::response::Response) -> Option<(u16, Value)> {
    let status = r.status().as_u16();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.ok()?;
    Some((status, serde_json::from_slice(&b).ok()?))
}

fn targets(cx: &super::Cx<'_>, model: Option<&str>, server: Option<&str>) -> Vec<Target> {
    place::candidates(
        cx.c,
        &Want {
            kinds: &[ModelKind::Music],
            apis: &[MediaApi::Sa3],
            model,
            server,
        },
        &cx.bridge_priority(),
    )
}

pub async fn run(cx: &super::Cx<'_>, method: &str, path: &str, query: &str, raw: &Bytes) -> Tried {
    let fail = |status: u16, msg: &str| Tried {
        response: super::error(status, msg),
        target: None,
        failed: vec![],
    };
    let rest = path.strip_prefix("/v1/audio/music").unwrap_or("");
    if rest.is_empty() || rest == "/" {
        if method != "POST" {
            return fail(405, "POST a {prompt, seconds} body here");
        }
        return generate(cx, query, raw).await;
    }
    if method != "GET" {
        return fail(405, "only GET here");
    }
    let id = rest.trim_start_matches('/');
    let (id, file) = match id.strip_suffix(".mp3") {
        Some(i) => (i, true),
        None => (id, false),
    };
    if !valid_id(id) {
        return fail(404, "unknown job");
    }
    let path_q = if file {
        format!("/audio/{id}.mp3")
    } else {
        format!("/jobs/{id}")
    };
    // the server that ran it; after a restart, each one in turn
    let ts = match owner(id) {
        Some(t) => vec![t],
        None => targets(cx, None, None),
    };
    for t in &ts {
        let p = Prepared {
            method: "GET".into(),
            path_q: path_q.clone(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        let r = match send::send(
            cx.http,
            cx.hub,
            cx.c,
            t,
            p,
            Patience(Duration::from_secs(60)),
        )
        .await
        {
            send::Sent::Answer(r) => r,
            send::Sent::Retry(..) => continue,
        };
        if r.status().as_u16() == 404 && ts.len() > 1 {
            continue;
        }
        remember(id, t);
        if file {
            return Tried {
                response: r,
                target: Some(t.clone()),
                failed: vec![],
            };
        }
        let mut hdrs = r.headers().clone();
        hdrs.remove("content-length");
        return match read_json(r).await {
            Some((status, v)) => {
                let mut out = crate::proxy::json_response(status, &public_job(v));
                for k in ["x-abhoth-server", "x-abhoth-card", "x-abhoth-engine"] {
                    if let Some(v) = hdrs.get(k) {
                        out.headers_mut().insert(k, v.clone());
                    }
                }
                Tried {
                    response: out,
                    target: Some(t.clone()),
                    failed: vec![],
                }
            }
            None => fail(502, "the music engine's answer was not JSON"),
        };
    }
    fail(404, "unknown job")
}

async fn generate(cx: &super::Cx<'_>, query: &str, raw: &Bytes) -> Tried {
    let fail = |status: u16, msg: &str| Tried {
        response: super::error(status, msg),
        target: None,
        failed: vec![],
    };
    let mut body = match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => m,
        _ => return fail(400, "the body must be a JSON object"),
    };
    let is_async = query
        .split('&')
        .any(|kv| matches!(kv, "async=1" | "async=true"))
        || body.get("async").and_then(Value::as_bool) == Some(true);
    body.remove("async");
    let (model, server) = match body.remove("model") {
        Some(Value::String(m)) => {
            let (m, s) = place::split_pin(&m);
            (Some(m.to_string()), s.map(str::to_string))
        }
        _ => (None, None),
    };
    if is_async {
        body.remove("wait");
    } else {
        body.insert("wait".into(), Value::Bool(true));
    }
    let payload = Bytes::from(Value::Object(body).to_string());
    let ts = targets(cx, model.as_deref(), server.as_deref());
    // a job runs for minutes behind others in the engine's queue
    let patience = Patience(Duration::from_secs(if is_async { 60 } else { 3600 }));
    let tried = send::try_all(cx.http, cx.hub, cx.c, &ts, "music", patience, |_| {
        Ok(Prepared {
            method: "POST".into(),
            path_q: "/generate".into(),
            headers: json_headers(),
            body: payload.clone(),
        })
    })
    .await;
    let Some(t) = tried.target.clone() else {
        return tried;
    };
    if let Some(id) = tried
        .response
        .headers()
        .get("x-job-id")
        .and_then(|v| v.to_str().ok())
    {
        remember(id, &t);
    }
    if !is_async || tried.response.status().as_u16() != 202 {
        return tried;
    }
    match read_json(tried.response).await {
        Some((status, v)) => {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                remember(id, &t);
            }
            let mut r = crate::proxy::json_response(status, &public_job(v));
            send::mark(&mut r, &t);
            Tried {
                response: r,
                target: Some(t),
                failed: tried.failed,
            }
        }
        None => fail(502, "the music engine's answer was not JSON"),
    }
}
