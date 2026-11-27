//! `/comfyui/*`: ComfyUI's own API, UI and `/ws`, passed through (ROUTER-MEDIA-PLAN.md 4.4).
//! `/comfyui/@<server>/...` pins a server. A prompt's later calls (`/history/<id>`, the client's
//! `/ws`) go to the server that took it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::WebSocket;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use serde_json::Value;

use super::place::{self, Target, Want};
use super::send::{self, Patience, Prepared, Tried};
use crate::bridges::{Hub, Upstream};
use crate::control::schema::{ControlConfig, MediaApi, ModelKind};

pub const PREFIX: &str = "/comfyui";

/// `/comfyui/@pc/prompt?x=1` -> (Some("pc"), "/prompt?x=1"); `/comfyui` -> (None, "/").
pub fn split(path_q: &str) -> (Option<String>, String) {
    let rest = path_q.strip_prefix(PREFIX).unwrap_or(path_q);
    let rest = if rest.is_empty() || rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    };
    if let Some(r) = rest.strip_prefix("/@") {
        let end = r.find(['/', '?']).unwrap_or(r.len());
        let server = &r[..end];
        let tail = &r[end..];
        let tail = if tail.is_empty() || tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_string()
        };
        return (Some(server.to_string()), tail);
    }
    (None, rest)
}

const KEEP: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
struct Affinity {
    prompts: HashMap<String, (Target, Instant)>,
    clients: HashMap<String, (Target, Instant)>,
}

fn aff() -> &'static Mutex<Affinity> {
    static A: OnceLock<Mutex<Affinity>> = OnceLock::new();
    A.get_or_init(Default::default)
}

fn put(m: &mut HashMap<String, (Target, Instant)>, k: &str, t: &Target) {
    let now = Instant::now();
    if m.len() > 4000 {
        m.retain(|_, (_, at)| now.duration_since(*at) < KEEP);
    }
    m.insert(k.to_string(), (t.clone(), now));
}

pub fn remember_prompt(id: &str, t: &Target) {
    put(&mut aff().lock().unwrap().prompts, id, t);
}
pub fn remember_client(id: &str, t: &Target) {
    put(&mut aff().lock().unwrap().clients, id, t);
}
fn prompt_owner(id: &str) -> Option<Target> {
    aff()
        .lock()
        .unwrap()
        .prompts
        .get(id)
        .map(|(t, _)| t.clone())
}
fn client_owner(id: &str) -> Option<Target> {
    aff()
        .lock()
        .unwrap()
        .clients
        .get(id)
        .map(|(t, _)| t.clone())
}

fn query_param<'a>(path_q: &'a str, key: &str) -> Option<&'a str> {
    let q = path_q.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then_some(v)
    })
}

pub fn targets(
    c: &ControlConfig,
    server: Option<&str>,
    bridge_priority: &dyn Fn(&str) -> Option<i32>,
) -> Vec<Target> {
    place::candidates(
        c,
        &Want {
            kinds: &[ModelKind::Image, ModelKind::Video],
            apis: &[MediaApi::Comfyui],
            model: None,
            server,
        },
        bridge_priority,
    )
}

/// The server a call belongs to: its prompt's, its client's, else the best candidate.
fn affine(path: &str, path_q: &str, body: &[u8]) -> Option<Target> {
    if let Some(id) = path
        .strip_prefix("/history/")
        .or_else(|| path.strip_prefix("/api/history/"))
        && let Some(t) = prompt_owner(id)
    {
        return Some(t);
    }
    let client = query_param(path_q, "clientId")
        .map(str::to_string)
        .or_else(|| {
            serde_json::from_slice::<Value>(body)
                .ok()?
                .get("client_id")?
                .as_str()
                .map(str::to_string)
        });
    client.and_then(|c| client_owner(&c))
}

const MODEL_EXT: &[&str] = &[
    ".safetensors",
    ".gguf",
    ".ckpt",
    ".pt",
    ".pth",
    ".bin",
    ".sft",
    ".onnx",
];

/// The model files an API-format workflow loads: string inputs naming a model file.
pub fn workflow_files(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return vec![];
    };
    let mut out: Vec<String> = Vec::new();
    for node in v["prompt"].as_object().into_iter().flat_map(|o| o.values()) {
        for (_, x) in node["inputs"].as_object().into_iter().flatten() {
            if let Some(s) = x.as_str()
                && MODEL_EXT
                    .iter()
                    .any(|e| s.to_ascii_lowercase().ends_with(e))
                && !out.iter().any(|o| o == s)
            {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn base_name(f: &str) -> &str {
    f.rsplit(['/', '\\']).next().unwrap_or(f)
}

/// The candidates whose last probe lists every file the prompt loads (those never probed are
/// kept: ComfyUI itself will say). None left: why, for the client.
fn with_files(all: &[Target], body: &[u8]) -> Result<Vec<Target>, String> {
    let files = workflow_files(body);
    if files.is_empty() || all.is_empty() {
        return Ok(all.to_vec());
    }
    let mut missing: Vec<String> = Vec::new();
    let keep: Vec<Target> = all
        .iter()
        .filter(
            |t| match crate::control::caps::files_on(&t.engine, &t.server) {
                None => true,
                Some(folders) => {
                    let has = |f: &String| {
                        folders
                            .values()
                            .flatten()
                            .any(|x| x == f || base_name(x) == base_name(f))
                    };
                    let gone: Vec<&String> = files.iter().filter(|f| !has(f)).collect();
                    for g in &gone {
                        if !missing.contains(g) {
                            missing.push((*g).clone());
                        }
                    }
                    gone.is_empty()
                }
            },
        )
        .cloned()
        .collect();
    if keep.is_empty() {
        return Err(format!(
            "no server has every file this workflow loads (missing: {})",
            missing.join(", ")
        ));
    }
    Ok(keep)
}

fn is_prompt(method: &str, path: &str) -> bool {
    method == "POST" && matches!(path, "/prompt" | "/api/prompt")
}

pub async fn http(
    cx: &super::Cx<'_>,
    method: &str,
    path_q: &str,
    headers: &HeaderMap,
    raw: &Bytes,
) -> Tried {
    let (pin, rest) = split(path_q);
    let path = rest.split('?').next().unwrap_or("/").to_string();
    let all = targets(cx.c, pin.as_deref(), &cx.bridge_priority());
    let ts: Vec<Target> = match affine(&path, &rest, raw) {
        Some(t) if all.contains(&t) => vec![t],
        _ if is_prompt(method, &path) => match with_files(&all, raw) {
            Ok(v) => v,
            Err(msg) => {
                return Tried {
                    response: super::error(400, &msg),
                    target: None,
                    failed: vec![],
                };
            }
        },
        _ => all,
    };
    let mut h = send::engine_headers(headers);
    h.remove("accept-encoding");
    let tried = send::try_all(
        cx.http,
        cx.hub,
        cx.c,
        &ts,
        "ComfyUI",
        Patience(Duration::from_secs(300)),
        |_| {
            Ok(Prepared {
                method: method.to_string(),
                path_q: rest.clone(),
                headers: h.clone(),
                body: raw.clone(),
            })
        },
    )
    .await;
    let Some(t) = tried.target.clone() else {
        return tried;
    };
    let base = match &pin {
        Some(s) => format!("{PREFIX}/@{s}"),
        None => PREFIX.to_string(),
    };
    let mut r = tried.response;
    rebase_location(&mut r, &base);
    if !is_prompt(method, &path) {
        return Tried {
            response: r,
            target: Some(t),
            failed: tried.failed,
        };
    }
    // `/prompt`: note who took it (and the client), then answer as the engine did
    if let Some(c) = serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|v| v.get("client_id")?.as_str().map(str::to_string))
    {
        remember_client(&c, &t);
    }
    let (parts, body) = r.into_parts();
    let b = axum::body::to_bytes(body, 16 << 20)
        .await
        .unwrap_or_default();
    if let Some(id) = serde_json::from_slice::<Value>(&b)
        .ok()
        .and_then(|v| v.get("prompt_id")?.as_str().map(str::to_string))
    {
        remember_prompt(&id, &t);
    }
    let mut parts = parts;
    parts.headers.remove("content-length");
    Tried {
        response: Response::from_parts(parts, axum::body::Body::from(b)),
        target: Some(t),
        failed: tried.failed,
    }
}

/// A redirect to `/x` from the engine stays under the prefix.
fn rebase_location(r: &mut Response, base: &str) {
    let Some(loc) = r.headers().get("location").and_then(|v| v.to_str().ok()) else {
        return;
    };
    if loc.starts_with('/')
        && !loc.starts_with("//")
        && !loc.starts_with(base)
        && let Ok(v) = HeaderValue::from_str(&format!("{base}{loc}"))
    {
        r.headers_mut().insert("location", v);
    }
}

/// The target for a client's `/ws` (its prompts' server when it has one).
pub fn ws_target(
    c: &ControlConfig,
    path_q: &str,
    bridge_priority: &dyn Fn(&str) -> Option<i32>,
) -> Option<(Target, String)> {
    let (pin, rest) = split(path_q);
    let all = targets(c, pin.as_deref(), bridge_priority);
    let client = query_param(&rest, "clientId").map(str::to_string);
    let t = client
        .as_deref()
        .and_then(client_owner)
        .filter(|t| all.contains(t))
        .or_else(|| all.into_iter().find(|t| !place::is_cooled(t)))?;
    if let Some(c) = &client {
        remember_client(c, &t);
    }
    Some((t, rest))
}

/// Relay a client's WebSocket to `t`'s ComfyUI `/ws` (frames unchanged).
pub async fn relay(
    hub: Option<std::sync::Arc<Hub>>,
    c: ControlConfig,
    t: Target,
    rest: String,
    headers: HeaderMap,
    client: WebSocket,
) {
    let _busy = place::begin(&t);
    match crate::bridges::upstream_for(&c, &t.server, &t.engine, &t.card) {
        Some(Upstream::Bridge {
            bridge,
            engine,
            card,
        }) => {
            if let Some(hub) = hub {
                crate::bridges::forward_ws(&hub, &bridge, &engine, &card, &rest, &headers, client)
                    .await;
            }
        }
        Some(Upstream::Http(base)) => {
            let url = format!("{}{}", base.replacen("http://", "ws://", 1), rest);
            relay_direct(&url, client).await;
        }
        None => {}
    }
}

async fn relay_direct(url: &str, client: WebSocket) {
    use axum::extract::ws::Message as A;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as T;
    let Ok(Ok((up, _))) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(url),
    )
    .await
    else {
        tracing::info!("media: ComfyUI websocket {url}: no connection");
        return;
    };
    let (mut up_tx, mut up_rx) = up.split();
    let (mut c_tx, mut c_rx) = client.split();
    let to_engine = async {
        while let Some(Ok(m)) = c_rx.next().await {
            let m = match m {
                A::Text(t) => T::Text(t.as_str().into()),
                A::Binary(b) => T::Binary(b),
                A::Ping(b) => T::Ping(b),
                A::Pong(b) => T::Pong(b),
                A::Close(_) => break,
            };
            if up_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let to_client = async {
        while let Some(Ok(m)) = up_rx.next().await {
            let m = match m {
                T::Text(t) => A::Text(t.as_str().into()),
                T::Binary(b) => A::Binary(b),
                T::Ping(b) => A::Ping(b),
                T::Pong(b) => A::Pong(b),
                T::Close(_) => break,
                T::Frame(_) => continue,
            };
            if c_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = c_tx.close().await;
    };
    tokio::select! {
        _ = to_engine => {}
        _ = to_client => {}
    }
}
