//! The native agent loop as a [`LiveSession`] (the headless `--hostui` session: C#'s REPL in
//! headless mode behind `LiveSessionAdapter`). Prompts queue in arrival order and run one at a
//! time on a worker thread; the turn streams on the bus (turnStart, user, deltas, tool calls and
//! results, notices, turnEnd, busy); tool approvals go to the browser (`LivePermissionBroker`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use orca_abstractions::ToolResult;
use orca_core::agent::permissions::{PermissionDecision, PermissionPrompter};
use orca_core::agent::{AgentObserver, AgentSession};
use orca_core::chat::conversation::TEXT_TOOL_RESULTS_PREFIX;
use orca_core::client::{ChatMessage, Role, ToolCall};
use orca_core::live::{
    LiveEventBus, LivePermission, LiveSnapshot, LiveTranscriptItem, truncate, types,
};
use serde_json::json;

use crate::live::{AgentHost, LiveAgentMessageResult, LiveAgentTranscript, LiveSession};
use crate::util::{group_thousands, utf16_len};

pub const MAX_PROMPT_CHARS: usize = 200_000;

/// Tool-approval requests answered in the browser. Each request is an event the browser shows
/// with Yes / Yes to all / No; the agent loop waits for the answer.
pub struct PermissionBroker {
    bus: Arc<LiveEventBus>,
    pending: Mutex<BTreeMap<String, (LivePermission, std::sync::mpsc::Sender<PermissionDecision>)>>,
    next: Mutex<u32>,
}

impl PermissionBroker {
    pub fn new(bus: Arc<LiveEventBus>) -> Self {
        Self {
            bus,
            pending: Mutex::new(BTreeMap::new()),
            next: Mutex::new(0),
        }
    }

    pub fn pending(&self) -> Vec<LivePermission> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Ask the browser and wait for the answer (blocking: called from the agent loop's thread).
    pub fn request(&self, tool: &str, risk: &str) -> PermissionDecision {
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            format!("p{:04}", *n)
        };
        let request = LivePermission {
            id: id.clone(),
            tool: tool.into(),
            risk: risk.into(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), (request.clone(), tx));
        self.bus.publish(
            types::PERMISSION,
            serde_json::to_value(&request).unwrap_or_default(),
        );
        let d = rx.recv().unwrap_or(PermissionDecision::Denied);
        self.pending.lock().unwrap().remove(&id);
        d
    }

    /// Answer a request: "yes", "all" or "no". False when the id isn't pending.
    pub fn resolve(&self, id: &str, decision: &str) -> bool {
        let Some((_, tx)) = self.pending.lock().unwrap().remove(id) else {
            return false;
        };
        let d = match decision.trim().to_lowercase().as_str() {
            "yes" | "approve" | "approved" => PermissionDecision::Approved,
            "all" | "approveall" | "always" => PermissionDecision::ApproveAll,
            _ => PermissionDecision::Denied,
        };
        let word = match d {
            PermissionDecision::Approved => "yes",
            PermissionDecision::ApproveAll => "all",
            _ => "no",
        };
        if tx.send(d).is_err() {
            return false;
        }
        self.bus.publish(
            types::PERMISSION_RESOLVED,
            json!({"id": id, "decision": word}),
        );
        true
    }

    /// Deny everything still waiting (cancel, or the web UI stopped).
    pub fn deny_all(&self) {
        let ids: Vec<String> = self.pending.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.resolve(&id, "no");
        }
    }
}

struct BrowserPrompter(Arc<PermissionBroker>);

impl PermissionPrompter for BrowserPrompter {
    fn prompt(&mut self, tool_name: &str, risk: &str) -> PermissionDecision {
        self.0.request(tool_name, risk)
    }
}

/// Streams one turn onto the bus.
struct LiveObserver {
    bus: Arc<LiveEventBus>,
    started: HashMap<String, Instant>,
}

impl AgentObserver for LiveObserver {
    fn on_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.bus.publish_text(types::ASSISTANT_DELTA, text);
        }
    }

    fn on_reasoning(&mut self, text: &str) {
        if !text.is_empty() {
            self.bus.publish_text(types::THINKING_DELTA, text);
        }
    }

    fn on_tool_start(&mut self, call: &ToolCall) {
        self.started.insert(call.id.clone(), Instant::now());
        self.bus.publish_tool_call(&call.name, &call.arguments);
    }

    fn on_tool_end(&mut self, call: &ToolCall, result: &ToolResult) {
        let ms = self
            .started
            .remove(&call.id)
            .map(|t| t.elapsed().as_millis() as i64);
        self.bus
            .publish_tool_result(&call.name, &result.content, result.is_error, ms);
    }

    fn on_notice(&mut self, message: &str) {
        self.bus
            .publish(types::NOTICE, json!({"level": "warning", "text": message}));
    }
}

struct Queue {
    items: Mutex<(VecDeque<(String, String)>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn len(&self) -> usize {
        self.items.lock().unwrap().0.len()
    }

    /// The next prompt; `None` once closed.
    fn wait(&self) -> Option<(String, String)> {
        let mut g = self.items.lock().unwrap();
        loop {
            if g.1 {
                return None;
            }
            if let Some(item) = g.0.pop_front() {
                return Some(item);
            }
            g = self.ready.wait(g).unwrap();
        }
    }
}

struct Shared {
    bus: Arc<LiveEventBus>,
    queue: Queue,
    /// The conversation as of the last turn boundary (the snapshot's history).
    history: Mutex<Vec<ChatMessage>>,
    permissions: Arc<PermissionBroker>,
    /// Set while a turn runs: sending on it cancels the turn.
    cancel: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Shared {
    fn publish_busy(&self) {
        self.bus.publish(
            types::BUSY,
            json!({"busy": self.bus.in_turn(), "queued": self.queue.len()}),
        );
    }
}

/// The headless session over the native agent loop.
pub struct AgentLiveSession {
    shared: Arc<Shared>,
    working_directory: String,
    mode: String,
    model: Option<String>,
    uncensored: bool,
    session_id: Option<String>,
    agents: Option<Arc<dyn AgentHost>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl AgentLiveSession {
    /// Start the worker for `session`; its approvals go to the browser.
    pub fn start(mut session: AgentSession, bus: Arc<LiveEventBus>, mode: &str) -> Arc<Self> {
        let permissions = Arc::new(PermissionBroker::new(bus.clone()));
        session.prompter = Some(Box::new(BrowserPrompter(permissions.clone())));
        let shared = Arc::new(Shared {
            bus,
            queue: Queue {
                items: Mutex::new((VecDeque::new(), false)),
                ready: Condvar::new(),
            },
            history: Mutex::new(session.messages.clone()),
            permissions,
            cancel: Mutex::new(None),
        });
        let working_directory = session.ctx.cwd.to_string_lossy().to_string();
        let model = session.config.lm_studio.primary_model().map(str::to_string);
        let uncensored = session.config.agent.uncensored.is_on();
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("orca-webui-session".into())
            .spawn(move || worker_loop(session, worker))
            .expect("spawning the session thread");
        Arc::new(Self {
            shared,
            working_directory,
            mode: mode.into(),
            model,
            uncensored,
            session_id: None,
            agents: None,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// The broker (the CLI denies everything still waiting when it stops).
    pub fn permissions(&self) -> &Arc<PermissionBroker> {
        &self.shared.permissions
    }
}

fn worker_loop(mut session: AgentSession, shared: Arc<Shared>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("web UI session: no runtime: {e}");
            return;
        }
    };
    let mut prepared = false;
    while let Some((text, source)) = shared.queue.wait() {
        if !prepared {
            session.native_tools = session.resolve_native_tools();
            let system = session.default_system_prompt();
            session.set_system_prompt(system);
            prepared = true;
        }
        rt.block_on(run_one(&mut session, &shared, &text, &source));
    }
}

async fn run_one(session: &mut AgentSession, shared: &Shared, text: &str, source: &str) {
    let bus = shared.bus.clone();
    *shared.history.lock().unwrap() = session.messages.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    *shared.cancel.lock().unwrap() = Some(tx);
    // The web UI: a turn starts here, history before it is complete.
    bus.publish_turn_start(session.messages.len() as i64, source);
    bus.publish(types::USER, json!({"text": text, "source": source}));
    shared.publish_busy();
    let tokens_before = session.total_output_tokens;
    let started = Instant::now();
    let mut observer = LiveObserver {
        bus: bus.clone(),
        started: HashMap::new(),
    };
    let outcome = tokio::select! {
        r = session.run_turn(text, &mut observer) => match r {
            Ok(_) => "done",
            Err(e) => {
                bus.publish(types::NOTICE, json!({"level": "error", "text": format!("Error: {e}")}));
                "error"
            }
        },
        _ = rx => "cancelled",
    };
    if outcome == "cancelled" {
        session.close_dangling_tool_calls();
    }
    shared.cancel.lock().unwrap().take();
    *shared.history.lock().unwrap() = session.messages.clone();
    bus.publish_turn_end(
        started.elapsed().as_millis() as i64,
        session.total_output_tokens - tokens_before,
        outcome,
    );
    shared.publish_busy();
}

#[async_trait]
impl LiveSession for AgentLiveSession {
    fn bus(&self) -> Arc<LiveEventBus> {
        self.shared.bus.clone()
    }

    fn working_directory(&self) -> String {
        self.working_directory.clone()
    }

    fn mode(&self) -> String {
        self.mode.clone()
    }

    async fn get_snapshot(&self) -> Result<LiveSnapshot, String> {
        let bus = &self.shared.bus;
        let (turn, seq, base, in_turn) = bus.capture_turn();
        let messages = self.shared.history.lock().unwrap().clone();
        let up_to = if in_turn && base >= 0 {
            (base as usize).min(messages.len())
        } else {
            messages.len()
        };
        Ok(LiveSnapshot {
            session_id: self.session_id.clone(),
            title: None,
            working_directory: self.working_directory.clone(),
            mode: self.mode.clone(),
            model: self.model.clone(),
            busy: in_turn,
            queued: self.shared.queue.len() as i32,
            last_seq: seq,
            history: to_transcript(&messages[..up_to]),
            current_turn: if in_turn { turn } else { Vec::new() },
            agents: self.agents.as_ref().map(|a| a.agents()).unwrap_or_default(),
            slots: None,
            pending_permissions: self.shared.permissions.pending(),
            engine: Some("native".into()),
            pending_questions: Some(Vec::new()),
            uncensored: self.uncensored,
        })
    }

    async fn submit_prompt(&self, text: &str, source: &str) -> Option<String> {
        if text.trim().is_empty() {
            return Some("Empty prompt.".into());
        }
        let n = utf16_len(text);
        if n > MAX_PROMPT_CHARS {
            return Some(format!(
                "Prompt too long ({} characters; the limit is {}).",
                group_thousands(n),
                group_thousands(MAX_PROMPT_CHARS)
            ));
        }
        {
            let mut g = self.shared.queue.items.lock().unwrap();
            if g.1 {
                return Some("This session has stopped.".into());
            }
            g.0.push_back((text.trim().to_string(), source.to_string()));
        }
        self.shared.queue.ready.notify_one();
        self.shared.publish_busy();
        None
    }

    async fn cancel(&self) -> bool {
        let tx = self.shared.cancel.lock().unwrap().take();
        match tx {
            Some(tx) => {
                // A turn waiting for an approval is blocked in it: deny, then stop the turn.
                self.shared.permissions.deny_all();
                tx.send(()).is_ok()
            }
            None => false,
        }
    }

    async fn resolve_permission(&self, id: &str, decision: &str) -> bool {
        self.shared.permissions.resolve(id, decision)
    }

    async fn get_agent_transcript(&self, agent_id: &str) -> Option<LiveAgentTranscript> {
        self.agents.as_ref()?.transcript(agent_id).await
    }

    async fn send_agent_message(&self, agent_id: &str, text: &str) -> LiveAgentMessageResult {
        match &self.agents {
            Some(a) => a.send_message(agent_id, text).await,
            None => LiveAgentMessageResult::new(
                false,
                "notFound",
                Some("This session has no sub-agents."),
            ),
        }
    }

    async fn cancel_agent(&self, agent_id: &str) -> bool {
        match &self.agents {
            Some(a) => a.cancel(agent_id).await,
            None => false,
        }
    }

    async fn shutdown(&self) {
        {
            let mut g = self.shared.queue.items.lock().unwrap();
            g.1 = true;
            g.0.clear();
        }
        self.shared.queue.ready.notify_all();
        self.cancel().await;
        self.shared.permissions.deny_all();
        let handle = self.thread.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
        }
    }
}

/// The finished conversation as transcript items (tool calls paired with their results).
pub fn to_transcript(messages: &[ChatMessage]) -> Vec<LiveTranscriptItem> {
    let item = |kind: &str, text: Option<String>| LiveTranscriptItem {
        kind: kind.into(),
        text,
        tool_id: None,
        tool_name: None,
        args: None,
        result: None,
        is_error: false,
    };
    let mut items: Vec<LiveTranscriptItem> = Vec::new();
    let mut tool_index: HashMap<String, usize> = HashMap::new();
    for m in messages {
        let text = m.content.clone().unwrap_or_default();
        match m.role {
            Role::System => {}
            Role::User => {
                if text.trim().is_empty() {
                    continue;
                }
                // Text-mode tool results travel as user messages; they belong to the tool calls.
                if text.starts_with(TEXT_TOOL_RESULTS_PREFIX) {
                    items.push(item("system", Some(truncate(&text, 4000))));
                } else {
                    items.push(item("user", Some(text)));
                }
            }
            Role::Assistant => {
                let clean = clean_assistant(&text);
                if !clean.trim().is_empty() {
                    items.push(item("assistant", Some(clean)));
                }
                for call in &m.tool_calls {
                    let args = serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| call.arguments.clone());
                    tool_index.insert(call.id.clone(), items.len());
                    items.push(LiveTranscriptItem {
                        tool_id: Some(call.id.clone()),
                        tool_name: Some(call.name.clone()),
                        args: Some(truncate(&args, 4000)),
                        ..item("tool", None)
                    });
                }
            }
            Role::Tool => {
                let result = truncate(&text, 20000);
                let id = m.tool_call_id.clone().unwrap_or_default();
                let is_error = result.to_lowercase().starts_with("error");
                match tool_index.get(&id) {
                    Some(&at) => {
                        items[at].result = Some(result);
                        items[at].is_error = is_error;
                    }
                    None => items.push(LiveTranscriptItem {
                        tool_id: Some(id),
                        tool_name: Some("tool".into()),
                        result: Some(result),
                        ..item("tool", None)
                    }),
                }
            }
        }
    }
    items
}

/// Drop reasoning and text-mode tool-call markup from a stored assistant message.
pub fn clean_assistant(text: &str) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let mut v: Vec<regex::Regex> = ["think", "thinking", "reasoning", "thought"]
            .iter()
            .map(|t| regex::Regex::new(&format!(r"(?is)<{t}>.*?(</{t}>|$)")).unwrap())
            .collect();
        v.push(regex::Regex::new(r"(?s)<tool_call>.*?(</tool_call>|$)").unwrap());
        v.push(regex::Regex::new(r"(?s)<next-prompt>.*?(</next-prompt>|$)").unwrap());
        v
    });
    let mut t = text.to_string();
    for p in patterns {
        t = p.replace_all(&t, "").into_owned();
    }
    t.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_assistant_strips_markup() {
        assert_eq!(
            clean_assistant("<think>hmm</think>Hello <tool_call>{}</tool_call>"),
            "Hello"
        );
        assert_eq!(clean_assistant("A<THINKING>open"), "A");
    }

    #[test]
    fn transcript_pairs_tool_calls_with_results() {
        let mut a = ChatMessage::assistant("Let me look.");
        a.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{ \"path\": \"a\" }".into(),
        });
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            a,
            ChatMessage::tool_result("c1", "Error: nope"),
        ];
        let t = to_transcript(&msgs);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].kind, "user");
        assert_eq!(t[2].args.as_deref(), Some("{\"path\":\"a\"}"));
        assert!(t[2].is_error);
    }

    #[test]
    fn broker_resolves_and_denies() {
        let bus = Arc::new(LiveEventBus::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let _sub = bus.subscribe(move |e| s2.lock().unwrap().push(e.data.clone()));
        let broker = Arc::new(PermissionBroker::new(bus));
        let b2 = broker.clone();
        let h = std::thread::spawn(move || b2.request("bash", "Dangerous"));
        while broker.pending().is_empty() {
            std::thread::yield_now();
        }
        assert_eq!(broker.pending()[0].id, "p0001");
        assert!(broker.resolve("p0001", "Always"));
        assert_eq!(h.join().unwrap(), PermissionDecision::ApproveAll);
        assert!(!broker.resolve("p0001", "yes"));
        let events = seen.lock().unwrap();
        assert_eq!(events[1]["decision"], "all");
    }
}
