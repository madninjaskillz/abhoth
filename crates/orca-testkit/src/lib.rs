//! A scripted fake OpenAI-compatible server for tests.
//!
//! Each `POST /v1/chat/completions` gets the next scripted reply (the last one repeats once
//! the script runs out), streamed as SSE when the request asks for it. Every request body and
//! its headers are recorded for assertions.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};

/// One scripted model turn.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    /// `(name, arguments JSON)` tool calls.
    pub tool_calls: Vec<(String, String)>,
    pub finish_reason: Option<String>,
    /// Sent as `x-litellm-model-name` when set.
    pub served_model_header: Option<String>,
    /// The `model` field in the chunks (defaults to the requested model).
    pub model: Option<String>,
    /// Answer with this HTTP status and body instead of a completion.
    pub error: Option<(u16, String)>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

impl Reply {
    pub fn text(t: impl Into<String>) -> Self {
        Self {
            text: t.into(),
            prompt_tokens: 100,
            completion_tokens: 10,
            ..Self::default()
        }
    }

    pub fn tool(name: impl Into<String>, args: Value) -> Self {
        Self {
            tool_calls: vec![(name.into(), args.to_string())],
            prompt_tokens: 100,
            completion_tokens: 20,
            ..Self::default()
        }
    }

    pub fn with_reasoning(mut self, r: impl Into<String>) -> Self {
        self.reasoning = r.into();
        self
    }

    pub fn with_served_header(mut self, m: impl Into<String>) -> Self {
        self.served_model_header = Some(m.into());
        self
    }

    pub fn with_finish_reason(mut self, f: impl Into<String>) -> Self {
        self.finish_reason = Some(f.into());
        self
    }

    pub fn error(status: u16, body: impl Into<String>) -> Self {
        Self {
            error: Some((status, body.into())),
            ..Self::default()
        }
    }
}

/// A request the server received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub body: Value,
    pub headers: Vec<(String, String)>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Inner {
    script: Vec<Reply>,
    next: usize,
    requests: Vec<Recorded>,
    image_requests: Vec<Value>,
}

pub struct FakeOpenAi {
    pub addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl FakeOpenAi {
    /// Start on an ephemeral 127.0.0.1 port.
    pub async fn start(script: Vec<Reply>) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            script,
            ..Inner::default()
        }));
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .route("/v1/models", get(models))
            .route("/v1/images/generations", post(image_generations))
            .route("/v1/images/edits", post(image_edits))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            addr,
            inner,
            _shutdown: tx,
        }
    }

    /// `http://127.0.0.1:port/v1`
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// The images API requests: `{"path", "authorization", "body"}` with the raw JSON body
    /// text for generations and the form parts (`name`, `filename`, `content_type`, `text` or
    /// `bytes`) for edits.
    pub fn image_requests(&self) -> Vec<Value> {
        self.inner.lock().unwrap().image_requests.clone()
    }
}

/// A 1x1 PNG, the "image" every images API call returns.
pub const TINY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// The images answer: the tiny PNG, or a 500 when the prompt asks for a failure (`FAIL`).
fn image_reply(prompt_fails: bool) -> Response<Body> {
    if prompt_fails {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("image backend exploded"))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"data": [{"b64_json": TINY_PNG_BASE64}]}).to_string(),
        ))
        .unwrap()
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn image_generations(
    State(inner): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let fails = body.contains("FAIL");
    inner.lock().unwrap().image_requests.push(json!({
        "path": "/v1/images/generations",
        "authorization": header_value(&headers, "authorization"),
        "body": body,
    }));
    image_reply(fails)
}

async fn image_edits(
    State(inner): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let content_type = header_value(&headers, "content-type");
    let parts = parse_multipart(&content_type, &body);
    let fails = parts
        .iter()
        .any(|p| p["text"].as_str().is_some_and(|t| t.contains("FAIL")));
    inner.lock().unwrap().image_requests.push(json!({
        "path": "/v1/images/edits",
        "authorization": header_value(&headers, "authorization"),
        "body": parts,
    }));
    image_reply(fails)
}

/// Splits a `multipart/form-data` body into its parts (enough for tests: no nested parts).
fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<Value> {
    let Some(boundary) = content_type.split(';').find_map(|p| {
        p.trim()
            .strip_prefix("boundary=")
            .map(|b| b.trim_matches('"').to_string())
    }) else {
        return Vec::new();
    };
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = find(rest, delim.as_bytes()) {
        rest = &rest[start + delim.len()..];
        if rest.starts_with(b"--") {
            break;
        }
        let Some(end) = find(rest, delim.as_bytes()) else {
            break;
        };
        const CRLF: &[u8] = &[13, 10];
        let part = rest[..end].strip_prefix(CRLF).unwrap_or(&rest[..end]);
        let part = part.strip_suffix(CRLF).unwrap_or(part);
        let Some(split) = find(part, &[13, 10, 13, 10]) else {
            continue;
        };
        let head = String::from_utf8_lossy(&part[..split]).to_string();
        let data = &part[split + 4..];
        let mut name = String::new();
        let mut filename = Value::Null;
        let mut ctype = Value::Null;
        for line in head.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                for attr in line.split(';').skip(1) {
                    let (k, v) = attr.trim().split_once('=').unwrap_or(("", ""));
                    let v = v.trim_matches('"').to_string();
                    match k {
                        "name" => name = v,
                        "filename" => filename = json!(v),
                        _ => {}
                    }
                }
            } else if lower.starts_with("content-type:") {
                ctype = json!(line["content-type:".len()..].trim());
            }
        }
        let mut v = json!({"name": name, "filename": filename, "content_type": ctype});
        if filename.is_null() {
            v["text"] = json!(String::from_utf8_lossy(data));
        } else {
            v["bytes"] = json!(data.len());
        }
        out.push(v);
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn models() -> axum::Json<Value> {
    axum::Json(
        json!({"object":"list","data":[{"id":"fake-model","object":"model","owned_by":"fake"}]}),
    )
}

async fn chat(
    State(inner): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let reply = {
        let mut g = inner.lock().unwrap();
        g.requests.push(Recorded {
            body: body.clone(),
            headers: headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
        });
        let idx = g.next.min(g.script.len().saturating_sub(1));
        g.next += 1;
        g.script
            .get(idx)
            .cloned()
            .unwrap_or_else(|| Reply::text("(no script)"))
    };
    if let Some((status, text)) = &reply.error {
        return Response::builder()
            .status(StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
            .header("content-type", "application/json")
            .body(Body::from(text.clone()))
            .unwrap();
    }
    let model = reply
        .model
        .clone()
        .or_else(|| body["model"].as_str().map(str::to_string))
        .unwrap_or_default();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(h) = &reply.served_model_header {
        builder = builder.header("x-litellm-model-name", h.as_str());
    }
    let finish = reply.finish_reason.clone().unwrap_or_else(|| {
        if reply.tool_calls.is_empty() {
            "stop".into()
        } else {
            "tool_calls".into()
        }
    });
    let usage = json!({"prompt_tokens": reply.prompt_tokens, "completion_tokens": reply.completion_tokens,
        "total_tokens": reply.prompt_tokens + reply.completion_tokens});
    if !stream {
        let calls: Vec<Value> = reply
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, (n, a))| json!({"id": format!("call_{i}"), "type":"function", "function":{"name":n,"arguments":a}}))
            .collect();
        let mut message = json!({"role":"assistant","content":reply.text});
        if !reply.reasoning.is_empty() {
            message["reasoning_content"] = json!(reply.reasoning);
        }
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        let v = json!({"id":"chatcmpl-fake","object":"chat.completion","model":model,
            "choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage});
        return builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap();
    }

    let chunk = |delta: Value, finish: Option<&str>| {
        json!({"id":"chatcmpl-fake","object":"chat.completion.chunk","model":model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    };
    let mut events: Vec<Value> = vec![chunk(json!({"role":"assistant","content":""}), None)];
    if !reply.reasoning.is_empty() {
        events.push(chunk(json!({"reasoning_content": reply.reasoning}), None));
    }
    // Text in a few pieces, as a real server sends it.
    let chars: Vec<char> = reply.text.chars().collect();
    for piece in chars.chunks(8) {
        events.push(chunk(
            json!({"content": piece.iter().collect::<String>()}),
            None,
        ));
    }
    for (i, (name, args)) in reply.tool_calls.iter().enumerate() {
        events.push(chunk(
            json!({"tool_calls":[{"index":i,"id":format!("call_{i}"),"type":"function","function":{"name":name,"arguments":""}}]}),
            None,
        ));
        let mid = args
            .char_indices()
            .nth(args.chars().count() / 2)
            .map(|(b, _)| b)
            .unwrap_or(0);
        for part in [&args[..mid], &args[mid..]] {
            events.push(chunk(
                json!({"tool_calls":[{"index":i,"function":{"arguments":part}}]}),
                None,
            ));
        }
    }
    events.push(chunk(json!({}), Some(&finish)));
    // A reply without token counts sends no usage chunk (an empty stream stays empty).
    if reply.prompt_tokens + reply.completion_tokens > 0 {
        events.push(json!({"id":"chatcmpl-fake","object":"chat.completion.chunk","model":model,"choices":[],"usage":usage}));
    }
    let mut sse = String::new();
    for e in events {
        sse.push_str("data: ");
        sse.push_str(&e.to_string());
        sse.push_str("\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    builder
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

/// The path of one of this crate's helper programs (`fake-mcp-server`, `fake-lsp-server`):
/// the nearest `target/<profile>/<name>` above the running test executable. `cargo test
/// --workspace` builds them before any test runs; a narrower `cargo test -p <crate>` builds
/// them here on first use.
pub fn fake_bin(name: &str) -> std::path::PathBuf {
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().expect("current exe");
    let find = || {
        exe.ancestors()
            .skip(1)
            .map(|d| d.join(&file))
            .find(|p| p.is_file())
    };
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = find() {
        return p;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "-p", "orca-testkit", "--bin", name])
        .status()
        .expect("running cargo build for a fake server");
    assert!(status.success(), "building {name} failed");
    find().unwrap_or_else(|| panic!("{file} not found above {}", exe.display()))
}
