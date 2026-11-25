//! Test host and a small websocket client for the hub protocol.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, ToolRegistry};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::live::{LiveEventBus, LiveSnapshot};
use orca_webui::live::LiveSession;
use orca_webui::native::AgentLiveSession;
use orca_webui::{WebUiOptions, WebUiServer};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

pub const TOKEN: &str = "test-token-123456";

pub struct Host {
    pub server: WebUiServer,
    pub state_dir: tempfile::TempDir,
}

impl Host {
    pub async fn start(session: Arc<dyn LiveSession>) -> Self {
        Self::start_with(session, |_| {}).await
    }

    pub async fn start_with(
        session: Arc<dyn LiveSession>,
        tweak: impl FnOnce(&mut WebUiOptions),
    ) -> Self {
        let state_dir = tempfile::tempdir().unwrap();
        let mut o = WebUiOptions::new(TOKEN);
        o.port = 0;
        o.projects_file = Some(state_dir.path().join("projects.json"));
        o.resume_grace = Duration::from_secs(10);
        tweak(&mut o);
        let server = WebUiServer::start(o, session).await.unwrap();
        Self { server, state_dir }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    pub fn primary(&self) -> String {
        self.server.projects().primary.id.clone()
    }

    pub async fn connect(&self) -> Ws {
        Ws::connect(self.server.port(), None).await
    }

    pub fn http(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    /// An authenticated request (token header).
    pub fn api(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http()
            .request(method, self.url(path))
            .header("X-OpenOrca-Token", TOKEN)
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Ws {
    socket: Socket,
    pub connection_id: String,
    pub resumed: bool,
    /// The highest message number seen.
    pub last: u64,
    next_id: u64,
    /// Events received while waiting for a result.
    pending: VecDeque<Value>,
    /// Live events of a batch after the one `events_until` stopped at.
    leftover: VecDeque<Value>,
}

impl Ws {
    pub async fn connect(port: u16, resume: Option<(&str, u64)>) -> Self {
        let mut url = format!("ws://127.0.0.1:{port}/hub/ws?token={TOKEN}");
        if let Some((id, last)) = resume {
            url.push_str(&format!("&resume={id}&last={last}"));
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let welcome = loop {
            let v = read(&mut socket).await;
            if v["type"] == "welcome" {
                break v;
            }
        };
        Self {
            socket,
            connection_id: welcome["connectionId"].as_str().unwrap().to_string(),
            resumed: welcome["resumed"].as_bool().unwrap(),
            last: 0,
            next_id: 0,
            pending: VecDeque::new(),
            leftover: VecDeque::new(),
        }
    }

    /// The next numbered message (pings skipped).
    pub async fn next(&mut self) -> Value {
        if let Some(v) = self.pending.pop_front() {
            return v;
        }
        loop {
            let v = read(&mut self.socket).await;
            if v["type"] == "ping" {
                continue;
            }
            if let Some(n) = v["n"].as_u64() {
                self.last = self.last.max(n);
            }
            return v;
        }
    }

    pub async fn send(&mut self, v: Value) {
        self.socket
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    pub async fn send_invoke(&mut self, method: &str, args: Value) -> String {
        self.next_id += 1;
        let id = self.next_id.to_string();
        self.send(json!({"type": "invoke", "id": id, "method": method, "args": args}))
            .await;
        id
    }

    /// Call a hub method; `Err` is the hub's error text. Events that arrive first are kept.
    pub async fn invoke(&mut self, method: &str, args: Value) -> Result<Value, String> {
        let id = self.send_invoke(method, args).await;
        let mut held = Vec::new();
        let r = loop {
            let v = self.next().await;
            if v["type"] == "result" && v["id"] == id.as_str() {
                break v;
            }
            held.push(v);
        };
        for v in held.into_iter().rev() {
            self.pending.push_front(v);
        }
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(e.to_string()),
            None => Ok(r["result"].clone()),
        }
    }

    /// Live events (the `events` callback's items) until one matches `stop` (20 s at most).
    pub async fn events_until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(20), self.events_until_inner(stop))
            .await
            .expect("timed out waiting for a live event")
    }

    async fn events_until_inner(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            while let Some(e) = self.leftover.pop_front() {
                out.push(e.clone());
                if stop(&e) {
                    return out;
                }
            }
            let v = self.next().await;
            if v["type"] == "event" && v["method"] == "events" {
                self.leftover
                    .extend(v["args"][1].as_array().unwrap().iter().cloned());
            }
        }
    }

    pub async fn drop_socket(mut self) {
        let _ = self.socket.close(None).await;
    }
}

async fn read(socket: &mut Socket) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("timed out waiting for a hub message")
            .expect("socket closed")
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

/// A native session against the fake model server, working in `cwd`.
pub fn native_session(base_url: &str, cwd: &std::path::Path, tools: bool) -> Arc<AgentLiveSession> {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = base_url.to_string();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["model-a".into()];
    config.lm_studio.native_tool_calling = true;
    config.agent.verification_mode = "off".into();
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let mut registry = ToolRegistry::new();
    if tools {
        registry.register_all(orca_tools_fs::all_tools());
    }
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    s.retry_base_delay = Duration::from_millis(1);
    s.failover_delay = Duration::from_millis(1);
    AgentLiveSession::start(s, Arc::new(LiveEventBus::new()), "headless")
}

/// A session whose bus the test drives directly.
pub struct FakeSession {
    pub bus: Arc<LiveEventBus>,
    pub cwd: String,
    pub prompts: Mutex<Vec<String>>,
}

impl FakeSession {
    pub fn new(cwd: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            bus: Arc::new(LiveEventBus::new()),
            cwd: cwd.to_string_lossy().to_string(),
            prompts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LiveSession for FakeSession {
    fn bus(&self) -> Arc<LiveEventBus> {
        self.bus.clone()
    }

    fn working_directory(&self) -> String {
        self.cwd.clone()
    }

    fn mode(&self) -> String {
        "headless".into()
    }

    async fn get_snapshot(&self) -> Result<LiveSnapshot, String> {
        let (turn, seq, _, in_turn) = self.bus.capture_turn();
        Ok(LiveSnapshot {
            session_id: None,
            title: None,
            working_directory: self.cwd.clone(),
            mode: "headless".into(),
            model: Some("fake".into()),
            busy: in_turn,
            queued: 0,
            last_seq: seq,
            history: Vec::new(),
            current_turn: turn,
            agents: Vec::new(),
            slots: None,
            pending_permissions: Vec::new(),
            engine: Some("native".into()),
            pending_questions: None,
            uncensored: false,
        })
    }

    async fn submit_prompt(&self, text: &str, _source: &str) -> Option<String> {
        self.prompts.lock().unwrap().push(text.to_string());
        None
    }

    async fn cancel(&self) -> bool {
        false
    }

    async fn resolve_permission(&self, _id: &str, _decision: &str) -> bool {
        false
    }
}
