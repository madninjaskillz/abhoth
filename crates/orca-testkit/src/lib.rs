//! A scripted fake OpenAI-compatible server for tests.
//!
//! Each `POST /v1/chat/completions` gets the next scripted reply (the last one repeats once
//! the script runs out), streamed as SSE when the request asks for it. Every request body and
//! its headers are recorded for assertions.
//!
//! A reply can also stream its reasoning in many small chunks, open with a role chunk whose
//! `content` is null (no empty text part), or break off mid-stream (the connection drops
//! without the end of the chunked body). Every other request (`GET /v1/models`, `GET /health`,
//! LM Studio's `/api/v0/models`, Ollama's `/api/show`, ...) is recorded as a probe and answered
//! from [`ServerOptions::routes`] (404 when not listed; `/v1/models` has a default list).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use futures_util::StreamExt;
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
    /// Stream the reasoning in chunks of this many characters (0: one chunk).
    pub reasoning_chunk_chars: usize,
    /// The opening role chunk carries `"content": null` (as llama.cpp sends it) instead of
    /// `""`, so the stream has no empty text part.
    pub null_role_content: bool,
    /// Break the stream off after this many SSE events: the connection drops without the end
    /// of the chunked body (a server or proxy dying mid-response).
    pub die_after_events: Option<usize>,
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

    /// A stream with no content at all: a bare role chunk, the finish chunk, no usage.
    pub fn empty() -> Self {
        Self {
            null_role_content: true,
            ..Self::default()
        }
    }

    /// Reasoning streamed `chars` characters per chunk.
    pub fn with_reasoning_chunks(mut self, chars: usize) -> Self {
        self.reasoning_chunk_chars = chars;
        self
    }

    /// The role chunk's `content` is null.
    pub fn with_null_role_content(mut self) -> Self {
        self.null_role_content = true;
        self
    }

    /// Break the stream off after `events` SSE events.
    pub fn dying_after(mut self, events: usize) -> Self {
        self.die_after_events = Some(events);
        self
    }
}

/// Answers for requests other than chat completions, by `"METHOD /path"` (e.g.
/// `"GET /health"`): `(status, body)`.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    pub routes: BTreeMap<String, (u16, String)>,
    /// Routes that answer only a `Bearer` with this key (401 `invalid_api_key` otherwise).
    pub keyed: BTreeMap<String, String>,
}

impl ServerOptions {
    pub fn route(mut self, key: &str, status: u16, body: impl Into<String>) -> Self {
        self.routes.insert(key.to_string(), (status, body.into()));
        self
    }

    /// `key` (e.g. `"GET /v1/models"`) answers only requests carrying `Bearer {api_key}`.
    pub fn requiring_key(mut self, key: &str, api_key: &str) -> Self {
        self.keyed.insert(key.to_string(), api_key.to_string());
        self
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
    routes: BTreeMap<String, (u16, String)>,
    keyed: BTreeMap<String, String>,
    /// Every other endpoint answers 503 (a server that is down or still loading).
    down: bool,
    /// `"METHOD /path"` of every request other than a chat completion, with its
    /// `Authorization` header.
    probes: Vec<(String, Option<String>)>,
}

pub struct FakeOpenAi {
    pub addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl FakeOpenAi {
    /// Start on an ephemeral 127.0.0.1 port.
    pub async fn start(script: Vec<Reply>) -> Self {
        Self::start_with(script, ServerOptions::default()).await
    }

    /// Start with answers for the other endpoints.
    pub async fn start_with(script: Vec<Reply>, options: ServerOptions) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            script,
            routes: options.routes,
            keyed: options.keyed,
            ..Inner::default()
        }));
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .fallback(other)
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

    /// `"METHOD /path"` of every request other than a chat completion, in order.
    pub fn probes(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .probes
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Answer every endpoint other than chat completions with 503 (or normally again).
    pub fn set_down(&self, down: bool) {
        self.inner.lock().unwrap().down = down;
    }

    /// The `Authorization` header of each probe (None when absent).
    pub fn probe_auth(&self) -> Vec<Option<String>> {
        self.inner
            .lock()
            .unwrap()
            .probes
            .iter()
            .map(|(_, a)| a.clone())
            .collect()
    }
}

/// The default `GET /v1/models` answer.
pub const DEFAULT_MODELS: &str =
    r#"{"object":"list","data":[{"id":"fake-model","object":"model","owned_by":"fake"}]}"#;

async fn other(State(inner): State<Arc<Mutex<Inner>>>, request: Request) -> Response<Body> {
    let key = format!("{} {}", request.method(), request.uri().path());
    let auth = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let answer = {
        let mut g = inner.lock().unwrap();
        let refused = g
            .keyed
            .get(&key)
            .is_some_and(|k| auth.as_deref() != Some(format!("Bearer {k}").as_str()));
        let down = g.down;
        g.probes.push((key.clone(), auth));
        if down {
            Some((503, String::new()))
        } else if refused {
            Some((401, r#"{"error":{"code":"invalid_api_key"}}"#.to_string()))
        } else {
            g.routes.get(&key).cloned()
        }
    };
    let (status, body) = match answer {
        Some(a) => a,
        None if key == "GET /v1/models" => (200, DEFAULT_MODELS.to_string()),
        None => (404, String::new()),
    };
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
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
    let role_content = if reply.null_role_content {
        Value::Null
    } else {
        json!("")
    };
    let mut events: Vec<Value> = vec![chunk(
        json!({"role":"assistant","content":role_content}),
        None,
    )];
    if !reply.reasoning.is_empty() {
        if reply.reasoning_chunk_chars == 0 {
            events.push(chunk(json!({"reasoning_content": reply.reasoning}), None));
        } else {
            let chars: Vec<char> = reply.reasoning.chars().collect();
            for piece in chars.chunks(reply.reasoning_chunk_chars) {
                events.push(chunk(
                    json!({"reasoning_content": piece.iter().collect::<String>()}),
                    None,
                ));
            }
        }
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
    let builder = builder.header("content-type", "text/event-stream");
    if let Some(n) = reply.die_after_events {
        // Send the first n events, give the client a moment to read them, then fail the body:
        // hyper drops the connection without the final chunk.
        let sent: Vec<Result<String, std::io::Error>> = events
            .iter()
            .take(n)
            .map(|e| Ok(format!("data: {e}\n\n")))
            .collect();
        let stream = futures_util::stream::iter(sent).chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "fake server: stream broken off",
            ))
        }));
        return builder.body(Body::from_stream(stream)).unwrap();
    }
    let mut sse = String::new();
    for e in events {
        sse.push_str("data: ");
        sse.push_str(&e.to_string());
        sse.push_str("\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    builder.body(Body::from(sse)).unwrap()
}
