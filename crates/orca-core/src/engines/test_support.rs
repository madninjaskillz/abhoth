//! Test helpers: fake bridge transports that replay recorded CLI streams (ports of the C#
//! `FakeBridgeTransport`, `CodexFakeTransport` and `RecordingSink`). The recordings are the C#
//! test data, read in place.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::transport::{BridgeTransport, BridgeTransportFactory, ProcessSpec};
use super::{AgentEngineEvent, AgentEventSink, EngineToolRequest, HostToolResult, HostToolSpec};

macro_rules! fixture {
    ($path:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/engines/",
            $path
        ))
    };
}

pub const CLAUDE_MULTI_TURN_EDIT: &str = fixture!("claude/multi-turn-edit.jsonl");
pub const CLAUDE_DENIED_WRITE: &str = fixture!("claude/denied-write.jsonl");
pub const CLAUDE_INTERRUPT: &str = fixture!("claude/interrupt.jsonl");
pub const CODEX_EDIT_AND_ASK: &str = fixture!("codex/edit-and-ask.jsonl");
pub const CODEX_DECLINED_PATCH: &str = fixture!("codex/declined-patch.jsonl");
pub const CODEX_INTERRUPT: &str = fixture!("codex/interrupt.jsonl");

pub const CWD: &str = r"C:\work\proj";

/// The fixture's non-empty lines.
pub fn lines(fixture: &str) -> Vec<String> {
    fixture
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn split_turns(fixture: &str, end_marker: &str) -> Vec<Vec<String>> {
    let mut turns = Vec::new();
    let mut current = Vec::new();
    for line in lines(fixture) {
        let end = line.contains(end_marker);
        current.push(line);
        if end {
            turns.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

/// A Claude fixture split into turns (each ends with its result line).
pub fn load_claude_turns(fixture: &str) -> Vec<Vec<String>> {
    split_turns(fixture, "\"type\":\"result\"")
}

/// A Codex fixture split into turns (each ends with turn/completed).
pub fn load_codex_turns(fixture: &str) -> Vec<Vec<String>> {
    split_turns(fixture, "\"method\":\"turn/completed\"")
}

pub fn ask_user_spec() -> HostToolSpec {
    HostToolSpec {
        name: "ask_user".into(),
        description: "Ask the user a multiple-choice question.".into(),
        input_schema: serde_json::from_str(
            r#"{"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}},"required":["question","options"]}"#,
        )
        .unwrap(),
    }
}

pub async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(condition(), "condition not reached");
}

// ---------------------------------------------------------------------------------------------
// Claude Code

#[derive(Default)]
pub struct FakeClaudeState {
    turns: VecDeque<Vec<String>>,
    held: Option<Vec<String>>,
    /// Emit this many lines of a turn, then hold the rest until an interrupt arrives.
    pub hold_after: Option<usize>,
    /// Lines the CLI sends before answering initialize (an SDK MCP server's handshake).
    pub emit_on_initialize: Vec<String>,
    /// Ignore interrupts (the CLI hangs): the engine must kill it.
    pub ignore_interrupt: bool,
    pub written: Vec<Value>,
    pub killed: bool,
    pub disposed: bool,
    tx: Option<mpsc::UnboundedSender<String>>,
}

/// Shared handle on a fake Claude Code process (the test keeps one, the engine the transport).
#[derive(Clone, Default)]
pub struct FakeClaude(pub Arc<Mutex<FakeClaudeState>>);

impl FakeClaude {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeClaudeState> {
        self.0.lock().unwrap()
    }

    pub fn killed(&self) -> bool {
        self.state().killed
    }

    pub fn disposed(&self) -> bool {
        self.state().disposed
    }

    pub fn written(&self) -> Vec<Value> {
        self.state().written.clone()
    }

    /// The process exits (exit code 0): stdout ends after what was already written.
    pub fn exit(&self) {
        let mut s = self.state();
        s.killed = true;
        s.tx = None;
    }

    pub fn written_of_type(&self, kind: &str, subtype: Option<&str>) -> Vec<Value> {
        self.written()
            .into_iter()
            .filter(|w| {
                w["type"] == kind
                    && subtype.is_none_or(|s| w.get("request").is_some_and(|r| r["subtype"] == s))
            })
            .collect()
    }
}

pub struct FakeClaudeTransport {
    shared: FakeClaude,
    rx: mpsc::UnboundedReceiver<String>,
}

impl FakeClaudeTransport {
    pub fn new(turns: Vec<Vec<String>>) -> (Self, FakeClaude) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = FakeClaude(Arc::new(Mutex::new(FakeClaudeState {
            turns: turns.into(),
            tx: Some(tx),
            ..Default::default()
        })));
        (
            Self {
                shared: shared.clone(),
                rx,
            },
            shared,
        )
    }
}

fn emit(state: &FakeClaudeState, line: String) {
    if let Some(tx) = &state.tx {
        let _ = tx.send(line);
    }
}

#[async_trait]
impl BridgeTransport for FakeClaudeTransport {
    async fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let mut s = self.shared.state();
        if s.killed || s.disposed {
            return Err(std::io::Error::other("pipe closed"));
        }
        let msg: Value = serde_json::from_str(line).unwrap();
        s.written.push(msg.clone());
        let kind = msg["type"].as_str().unwrap_or("");
        if kind == "control_request" {
            let id = msg["request_id"].as_str().unwrap_or("").to_string();
            let sub = msg["request"]["subtype"].as_str().unwrap_or("");
            if sub == "initialize" {
                for l in s.emit_on_initialize.clone() {
                    emit(&s, l);
                }
                emit(
                    &s,
                    json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {"commands": []}}}).to_string(),
                );
            } else if sub == "interrupt" && !s.ignore_interrupt {
                emit(
                    &s,
                    json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {}}}).to_string(),
                );
                if let Some(held) = s.held.take() {
                    for l in held {
                        emit(&s, l);
                    }
                }
            }
        } else if kind == "user"
            && let Some(turn) = s.turns.pop_front()
        {
            let n = s.hold_after.unwrap_or(turn.len()).min(turn.len());
            for l in &turn[..n] {
                emit(&s, l.clone());
            }
            if s.hold_after.is_some() {
                s.held = Some(turn[n..].to_vec());
            }
        }
        Ok(())
    }

    async fn read_line(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    fn has_exited(&mut self) -> bool {
        let s = self.shared.state();
        s.killed || s.disposed
    }

    fn exit_code(&mut self) -> Option<i32> {
        self.has_exited().then_some(0)
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    fn close_input(&mut self) {}

    fn kill(&mut self) {
        let mut s = self.shared.state();
        s.killed = true;
        s.tx = None;
    }

    async fn dispose(&mut self) {
        let mut s = self.shared.state();
        s.disposed = true;
        s.tx = None;
    }
}

/// A factory like the C# tests': every start records its spec and gets a fresh fake that
/// plays the turns not yet played (a restarted process carries on with the next turn).
pub fn claude_factory(
    turns: Vec<Vec<String>>,
    fakes: Arc<Mutex<Vec<FakeClaude>>>,
    starts: Arc<Mutex<Vec<ProcessSpec>>>,
    configure: impl Fn(&mut FakeClaudeState) + Send + 'static,
) -> BridgeTransportFactory {
    Box::new(move |spec| {
        starts.lock().unwrap().push(spec.clone());
        let mut list = fakes.lock().unwrap();
        let played: usize = list
            .iter()
            .map(|f| f.written_of_type("user", None).len())
            .sum();
        let (t, handle) = FakeClaudeTransport::new(turns.iter().skip(played).cloned().collect());
        configure(&mut handle.state());
        list.push(handle);
        Ok(Box::new(t) as Box<dyn BridgeTransport>)
    })
}

// ---------------------------------------------------------------------------------------------
// Codex

#[derive(Default)]
pub struct FakeCodexState {
    turns: VecDeque<Vec<String>>,
    held: Option<Vec<String>>,
    turn_number: usize,
    /// Hold a turn's lines from the first one containing this text until turn/interrupt.
    pub hold_from: Option<String>,
    pub ignore_interrupt: bool,
    pub written: Vec<Value>,
    pub killed: bool,
    pub disposed: bool,
    tx: Option<mpsc::UnboundedSender<String>>,
}

#[derive(Clone, Default)]
pub struct FakeCodex(pub Arc<Mutex<FakeCodexState>>);

impl FakeCodex {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeCodexState> {
        self.0.lock().unwrap()
    }

    pub fn killed(&self) -> bool {
        self.state().killed
    }

    pub fn written(&self) -> Vec<Value> {
        self.state().written.clone()
    }

    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.written()
            .into_iter()
            .filter(|w| w.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }

    /// The engine's answer to the server request with this id.
    pub fn response_to(&self, id: i64) -> Value {
        let matches: Vec<Value> = self
            .written()
            .into_iter()
            .filter(|w| {
                w.get("method").is_none() && w.get("id").and_then(Value::as_i64) == Some(id)
            })
            .collect();
        assert_eq!(matches.len(), 1, "one response to {id}");
        matches[0].clone()
    }

    pub fn turns_started(&self) -> usize {
        self.requests("turn/start").len()
    }
}

pub struct FakeCodexTransport {
    shared: FakeCodex,
    rx: mpsc::UnboundedReceiver<String>,
}

impl FakeCodexTransport {
    pub fn new(turns: Vec<Vec<String>>, turns_already_played: usize) -> (Self, FakeCodex) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = FakeCodex(Arc::new(Mutex::new(FakeCodexState {
            turns: turns.into(),
            turn_number: turns_already_played,
            tx: Some(tx),
            ..Default::default()
        })));
        (
            Self {
                shared: shared.clone(),
                rx,
            },
            shared,
        )
    }
}

fn emit_codex(state: &FakeCodexState, line: String) {
    if let Some(tx) = &state.tx {
        let _ = tx.send(line);
    }
}

fn respond(state: &FakeCodexState, id: i64, result: Value) {
    emit_codex(state, json!({"id": id, "result": result}).to_string());
}

#[async_trait]
impl BridgeTransport for FakeCodexTransport {
    async fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let mut s = self.shared.state();
        if s.killed || s.disposed {
            return Err(std::io::Error::other("pipe closed"));
        }
        let msg: Value = serde_json::from_str(line).unwrap();
        s.written.push(msg.clone());
        let (Some(method), Some(id)) = (
            msg.get("method").and_then(Value::as_str),
            msg.get("id").and_then(Value::as_i64),
        ) else {
            return Ok(()); // notifications, answers to our requests
        };
        let p = &msg["params"];
        match method {
            "initialize" => respond(&s, id, json!({"userAgent": "fake"})),
            "thread/start" | "thread/resume" => {
                emit_codex(
                    &s,
                    r#"{"method":"mcpServer/startupStatus/updated","params":{}}"#.into(),
                );
                let thread = p
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or("thread-1");
                respond(
                    &s,
                    id,
                    json!({"thread": {"id": thread}, "model": "gpt-test"}),
                );
            }
            "turn/start" => {
                s.turn_number += 1;
                let n = s.turn_number;
                respond(&s, id, json!({"turn": {"id": format!("turn-{n}")}}));
                if let Some(turn) = s.turns.pop_front() {
                    let hold = s
                        .hold_from
                        .as_ref()
                        .and_then(|h| turn.iter().position(|l| l.contains(h.as_str())));
                    let upto = hold.unwrap_or(turn.len());
                    for l in &turn[..upto] {
                        emit_codex(&s, l.clone());
                    }
                    if hold.is_some() {
                        s.held = Some(turn[upto..].to_vec());
                    }
                }
            }
            "turn/interrupt" if !s.ignore_interrupt => {
                respond(&s, id, json!({}));
                for l in s.held.take().unwrap_or_default() {
                    emit_codex(&s, l);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn read_line(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    fn has_exited(&mut self) -> bool {
        let s = self.shared.state();
        s.killed || s.disposed
    }

    fn exit_code(&mut self) -> Option<i32> {
        self.has_exited().then_some(0)
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    fn close_input(&mut self) {}

    fn kill(&mut self) {
        let mut s = self.shared.state();
        s.killed = true;
        s.tx = None;
    }

    async fn dispose(&mut self) {
        let mut s = self.shared.state();
        s.disposed = true;
        s.tx = None;
    }
}

pub fn codex_factory(
    turns: Vec<Vec<String>>,
    fakes: Arc<Mutex<Vec<FakeCodex>>>,
    starts: Arc<Mutex<Vec<ProcessSpec>>>,
    configure: impl Fn(&mut FakeCodexState) + Send + 'static,
) -> BridgeTransportFactory {
    Box::new(move |spec| {
        starts.lock().unwrap().push(spec.clone());
        let mut list = fakes.lock().unwrap();
        let played: usize = list.iter().map(FakeCodex::turns_started).sum();
        let (t, handle) =
            FakeCodexTransport::new(turns.iter().skip(played).cloned().collect(), played);
        configure(&mut handle.state());
        list.push(handle);
        Ok(Box::new(t) as Box<dyn BridgeTransport>)
    })
}

// ---------------------------------------------------------------------------------------------
// Sink

#[derive(Default)]
pub struct Recorded {
    pub events: Vec<AgentEngineEvent>,
    pub approvals: Vec<EngineToolRequest>,
    pub host_calls: Vec<(String, String)>,
}

/// Records events; approves or denies everything; host tools answer `host_answer`.
#[derive(Clone)]
pub struct RecordingSink {
    pub rec: Arc<Mutex<Recorded>>,
    pub approve: bool,
    pub host_answer: String,
}

impl RecordingSink {
    pub fn new(approve: bool) -> Self {
        Self {
            rec: Arc::default(),
            approve,
            host_answer: "Purple".into(),
        }
    }

    pub fn events(&self) -> Vec<AgentEngineEvent> {
        self.rec.lock().unwrap().events.clone()
    }

    pub fn approvals(&self) -> Vec<EngineToolRequest> {
        self.rec.lock().unwrap().approvals.clone()
    }

    pub fn host_calls(&self) -> Vec<(String, String)> {
        self.rec.lock().unwrap().host_calls.clone()
    }

    pub fn tool_calls(&self) -> Vec<(String, String, String, String)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEngineEvent::ToolCall {
                    id,
                    name,
                    original_name,
                    args_json,
                } => Some((id, name, original_name, args_json)),
                _ => None,
            })
            .collect()
    }

    pub fn tool_results(&self) -> Vec<(String, String, String, bool)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEngineEvent::ToolResult {
                    id,
                    name,
                    result,
                    is_error,
                    ..
                } => Some((id, name, result, is_error)),
                _ => None,
            })
            .collect()
    }

    pub fn completed(&self) -> Vec<super::EngineTurnCompleted> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEngineEvent::TurnCompleted(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    pub fn file_edits(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEngineEvent::FileEdited { path, .. } => Some(path),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl AgentEventSink for RecordingSink {
    fn on_event(&mut self, event: AgentEngineEvent) {
        self.rec.lock().unwrap().events.push(event);
    }

    async fn approve(&mut self, request: EngineToolRequest) -> bool {
        self.rec.lock().unwrap().approvals.push(request);
        self.approve
    }

    async fn run_host_tool(&mut self, name: &str, args_json: &str) -> HostToolResult {
        self.rec
            .lock()
            .unwrap()
            .host_calls
            .push((name.to_string(), args_json.to_string()));
        HostToolResult::ok(self.host_answer.clone())
    }
}
