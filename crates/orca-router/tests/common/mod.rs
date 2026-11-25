//! Fake engines and a router on loopback for the integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use orca_router::config::RouterConfig;
use orca_router::server::{self, Mode};
use orca_router::state::AppState;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Clone)]
pub enum Reply {
    Json(u16, Value),
    /// SSE events with a delay (ms) before each.
    Sse(Vec<(u64, String)>),
    /// SSE that never ends (one event every 50 ms).
    Endless,
}

type Script = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

#[derive(Clone)]
pub struct Fake {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub open_streams: Arc<Mutex<i64>>,
}

#[derive(Clone)]
struct FakeState {
    seen: Arc<Mutex<Vec<Seen>>>,
    script: Script,
    open: Arc<Mutex<i64>>,
}

struct OpenGuard(Arc<Mutex<i64>>);
impl Drop for OpenGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap() -= 1;
    }
}

async fn fake_handler(State(s): State<FakeState>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let headers = req.headers().clone();
    let raw = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let seen = Seen {
        method,
        path,
        headers,
        body,
    };
    s.seen.lock().unwrap().push(seen.clone());
    match (s.script)(&seen) {
        Reply::Json(status, v) => {
            let mut r = (
                axum::http::StatusCode::from_u16(status).unwrap(),
                v.to_string(),
            )
                .into_response();
            r.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            if status == 429 {
                r.headers_mut().insert("retry-after", "3".parse().unwrap());
            }
            r
        }
        Reply::Sse(events) => {
            let st = stream::unfold(events.into_iter(), |mut it| async move {
                let (delay, ev) = it.next()?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(ev)), it))
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(st))
                .unwrap()
        }
        Reply::Endless => {
            *s.open.lock().unwrap() += 1;
            let guard = Arc::new(OpenGuard(s.open.clone()));
            let st = stream::unfold(guard, |g| async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                    )),
                    g,
                ))
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(st))
                .unwrap()
        }
    }
}

impl Fake {
    pub async fn start(script: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Fake {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(Mutex::new(0));
        let st = FakeState {
            seen: seen.clone(),
            script: Arc::new(script),
            open: open.clone(),
        };
        let app = Router::new().fallback(fake_handler).with_state(st);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        Fake {
            addr,
            seen,
            open_streams: open,
        }
    }

    /// A fake answering every chat with a fixed completion naming `who`.
    pub async fn ok(who: &str) -> Fake {
        let who = who.to_string();
        Fake::start(move |s| {
            if s.body.get("stream") == Some(&Value::Bool(true)) {
                Reply::Sse(vec![
                    (0, format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{who}\"}}}}]}}\n\n")),
                    (0, "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n".into()),
                    (0, "data: [DONE]\n\n".into()),
                ])
            } else {
                Reply::Json(200, json!({"id": "c1", "object": "chat.completion", "model": s.body["model"],
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": who}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}}))
            }
        })
        .await
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn chats(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.path.ends_with("/chat/completions"))
            .collect()
    }
}

/// A port nothing listens on.
pub async fn dead_url() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    format!("http://{a}")
}

pub struct Router_ {
    pub url: String,
    pub st: Arc<AppState>,
}

pub async fn start_router(
    cfg: RouterConfig,
    path: Option<std::path::PathBuf>,
    mode: Mode,
) -> Router_ {
    let snap = orca_router::config::Snapshot::build(cfg, path.as_deref().and_then(|p| p.parent()))
        .unwrap();
    let st = AppState::new(snap, path);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let s2 = st.clone();
    tokio::spawn(async move {
        server::serve(l, s2, mode, std::future::pending())
            .await
            .unwrap();
    });
    Router_ {
        url: format!("http://{addr}"),
        st,
    }
}

/// The live layout's shape on two fakes: aliases over A+B, pinned -a/-b, inkling-2-mini per
/// card, one relay name. Key files in `dir`.
pub fn two_cards(a: &str, b: &str, relay: Option<&str>, dir: &std::path::Path) -> RouterConfig {
    std::fs::write(dir.join("key"), "test-engine-key\n").unwrap();
    let key = dir.join("key").to_string_lossy().to_string();
    let ninfer_info = json!({"max_input_tokens": 65536, "engine": "ninfer",
        "effort_levels": ["none", "low", "medium", "xhigh"], "thinking_off_effort": "none"});
    let mut v = json!({
        "rev": "rev1",
        "layout": "test-layout",
        "upstreams": {
            "A": {"url": a, "engine": "ninfer", "servedId": "inkling-2-mini", "lanes": 4, "keyFile": key},
            "B": {"url": b, "engine": "ninfer", "servedId": "inkling-2-mini", "lanes": 4, "keyFile": key}
        },
        "deployments": [
            {"name": "jarvis-brain", "cards": ["A", "B"], "servedId": "inkling-2-mini",
             "dropParams": ["response_format"], "modelInfo": ninfer_info},
            {"name": "qwen3.8-27b", "cards": ["A", "B"], "servedId": "inkling-2-mini",
             "dropParams": ["response_format"], "modelInfo": ninfer_info},
            {"name": "qwen3.8-flash-next-a", "cards": ["A"], "servedId": "inkling-2-mini",
             "dropParams": ["response_format"], "modelInfo": ninfer_info},
            {"name": "qwen3.8-flash-next-b", "cards": ["B"], "servedId": "inkling-2-mini",
             "dropParams": ["response_format"], "modelInfo": ninfer_info}
        ],
        "settings": {"cooldownSeconds": 10}
    });
    if let Some(r) = relay {
        v["relay"] = json!({"url": format!("{r}/ondemand/v1"), "key": "ondemand-relay"});
        v["deployments"].as_array_mut().unwrap().push(json!(
            {"name": "flash-next", "relay": true, "servedId": "flash-next", "params": {"reasoning_effort": "low"},
             "modelInfo": {"max_input_tokens": 262144, "engine": "strata", "live": false, "ondemand": true,
                           "catalog_model": "flash-next", "switch_to": {"engine": "strata", "engineProfile": "strata-1slot"}}}));
    }
    serde_json::from_value(v).unwrap()
}

pub fn http() -> reqwest::Client {
    reqwest::Client::new()
}

pub async fn chat(url: &str, body: Value) -> reqwest::Response {
    http()
        .post(format!("{url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}
