//! One sub-agent: who it is (number, letter, task, type) and its run state (`AgentContext`).
//!
//! The identity fields are fixed at creation. Everything a run changes lives behind a mutex
//! ([`AgentContext::state`]): a run that is cut off (time limit, cancellation) drops its future,
//! and the conversation and progress so far must survive that. The lock is never held across
//! an `.await`.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime};

use tokio::sync::Notify;

use super::cancel::CancelToken;
use super::palette;
use super::transcript::AgentTranscript;
use super::types::{AgentOrigin, AgentStatus};
use crate::chat::Conversation;
use crate::client::ToolDefinition;

/// Everything a run of the agent changes.
#[derive(Default)]
pub struct AgentState {
    /// Short bar label (at most four words).
    pub label: String,
    pub status: AgentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub conversation: Conversation,
    pub iteration_count: i32,
    /// Iteration budget for the current run.
    pub max_iterations: i32,
    /// What the agent is doing right now ("read src/Foo.cs", "thinking").
    pub current_activity: Option<String>,
    /// Progress the agent reported itself (0..1).
    pub reported_progress: Option<f64>,
    pub progress_note: Option<String>,
    /// The inference slot this agent is pinned to ("A2"; "A1*" when borrowed).
    pub slot: Option<String>,
    /// The last slot it held (a follow-up re-claims it when free).
    pub last_slot: Option<String>,
    /// Identity of the slot lease it holds now (slot changes of an older lease are ignored).
    pub lease_key: Option<usize>,
    /// The named provider it runs on (agent.providerSchedule); None = the default model.
    pub provider: Option<String>,
    /// The model id of `provider` once resolved.
    pub provider_model: Option<String>,
    /// "haiku" / "local" in a mixed swarm, else None.
    pub model_tag: Option<String>,
    pub started_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
    /// When its last model request finished.
    pub last_request_at: Option<Instant>,
    /// The agent timeout it hit (seconds), when it stopped at its time limit.
    pub time_limit_seconds: Option<i32>,
    files_written: Vec<String>,
    user_messages: Vec<String>,
    /// The user's message resumed this finished agent; cleared when that run ends.
    pub resumed_by_user: bool,
    /// The tools its requests send (a report after a timeout reuses them).
    pub request_tools: Option<Vec<ToolDefinition>>,
    /// The tool call it ran last returned an error.
    pub last_tool_call_failed: bool,
    pub(super) cancellation: Option<CancelToken>,
}

/// How an agent is created (the parts of `AgentContext` that are `init` in C#).
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    pub number: i32,
    pub task: String,
    pub label: String,
    pub color_slot: i32,
    pub agent_type: String,
    pub depth: i32,
    pub caller_depth: i32,
    pub origin: AgentOrigin,
    pub read_only: bool,
    pub max_output_tokens_override: Option<i32>,
    pub effort: Option<String>,
    pub timeout_seconds_override: Option<i32>,
    pub max_iterations_override: Option<i32>,
    pub full_result: bool,
    pub schedule_position: Option<i32>,
    pub heavy: bool,
    pub retain_conversation: bool,
}

pub struct AgentContext {
    /// 8 hex digits (`Guid.NewGuid().ToString("N")[..8]`).
    pub id: String,
    /// Short per-session number (1, 2, 3 ...).
    pub number: i32,
    pub task: String,
    /// Footer colour slot: 0 = blue A, 1 = green B, ...
    pub color_slot: i32,
    pub agent_type: String,
    /// Spawn depth (0 = spawned by the main agent or the user).
    pub depth: i32,
    /// Depth of whoever spawned it (the true nesting shown in the UI).
    pub caller_depth: i32,
    pub origin: AgentOrigin,
    /// Read-only agent (the planner): only commands that look and list run.
    pub read_only: bool,
    pub max_output_tokens_override: Option<i32>,
    /// Effort sent with each request on a named provider.
    pub effort: Option<String>,
    pub timeout_seconds_override: Option<i32>,
    pub max_iterations_override: Option<i32>,
    /// The result is not cut at `MAX_RESULT_CHARS` (the planner's plan).
    pub full_result: bool,
    /// 0-based place in the session's creation order; None = not scheduled.
    pub schedule_position: Option<i32>,
    /// The biggest task of its batch (it gets a model copy to itself).
    pub heavy: bool,
    /// Keep the conversation after the run so a follow-up can resume the agent.
    pub retain_conversation: bool,
    /// Bounded, display-oriented record of what the agent did.
    pub transcript: AgentTranscript,
    state: Mutex<AgentState>,
    inbox: Mutex<VecDeque<String>>,
    on_placed: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    finished: Notify,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AgentContext(#{} {} {} {:?})",
            self.number,
            self.tag(),
            self.agent_type,
            self.status()
        )
    }
}

/// A new 8-digit hex id.
pub fn new_agent_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:08x}", h.finish() as u32)
}

impl AgentContext {
    pub fn new(spec: AgentSpec) -> Self {
        let state = AgentState {
            label: spec.label,
            ..AgentState::default()
        };
        Self {
            id: new_agent_id(),
            number: spec.number,
            task: spec.task,
            color_slot: spec.color_slot,
            agent_type: spec.agent_type,
            depth: spec.depth,
            caller_depth: spec.caller_depth,
            origin: spec.origin,
            read_only: spec.read_only,
            max_output_tokens_override: spec.max_output_tokens_override,
            effort: spec.effort,
            timeout_seconds_override: spec.timeout_seconds_override,
            max_iterations_override: spec.max_iterations_override,
            full_result: spec.full_result,
            schedule_position: spec.schedule_position,
            heavy: spec.heavy,
            retain_conversation: spec.retain_conversation,
            transcript: AgentTranscript::default(),
            state: Mutex::new(state),
            inbox: Mutex::new(VecDeque::new()),
            on_placed: Mutex::new(None),
            finished: Notify::new(),
        }
    }

    /// The run state (never hold the guard across an `.await`).
    pub fn state(&self) -> MutexGuard<'_, AgentState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The bar letter (A, B, ... never L).
    pub fn letter(&self) -> char {
        palette::letter_of(self.color_slot)
    }

    /// "Agent A".
    pub fn tag(&self) -> String {
        format!("Agent {}", self.letter())
    }

    pub fn label(&self) -> String {
        self.state().label.clone()
    }

    pub fn status(&self) -> AgentStatus {
        self.state().status
    }

    pub fn set_status(&self, status: AgentStatus) {
        self.state().status = status;
    }

    pub fn result(&self) -> Option<String> {
        self.state().result.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.state().error.clone()
    }

    pub fn iteration_count(&self) -> i32 {
        self.state().iteration_count
    }

    pub fn max_iterations(&self) -> i32 {
        self.state().max_iterations
    }

    pub fn current_activity(&self) -> Option<String> {
        self.state().current_activity.clone()
    }

    pub fn reported_progress(&self) -> Option<f64> {
        self.state().reported_progress
    }

    pub fn progress_note(&self) -> Option<String> {
        self.state().progress_note.clone()
    }

    pub fn model_tag(&self) -> Option<String> {
        self.state().model_tag.clone()
    }

    pub fn provider(&self) -> Option<String> {
        self.state().provider.clone()
    }

    pub fn completed_at(&self) -> Option<SystemTime> {
        self.state().completed_at
    }

    /// The conversation's messages (a copy).
    pub fn messages(&self) -> Vec<crate::client::ChatMessage> {
        self.state().conversation.messages().to_vec()
    }

    /// Running or pending: not yet in a final state.
    pub fn is_active(&self) -> bool {
        matches!(self.status(), AgentStatus::Running | AgentStatus::Pending)
    }

    /// Stopped at its time limit rather than its step limit.
    pub fn stopped_at_time_limit(&self) -> bool {
        self.state().time_limit_seconds.is_some()
    }

    /// "step limit (40 steps)" or "time limit (1200 s, 37 steps)".
    pub fn limit_description(&self) -> String {
        let s = self.state();
        match s.time_limit_seconds {
            Some(t) => format!("time limit ({t} s, {} steps)", s.iteration_count),
            None => format!("step limit ({} steps)", s.iteration_count),
        }
    }

    /// Files this agent wrote or edited (as its calls named them), first write first.
    pub fn files_written(&self) -> Vec<String> {
        self.state().files_written.clone()
    }

    pub fn record_file_written(&self, path: &str) {
        let p = path.trim().replace('\\', "/");
        let mut s = self.state();
        if !s.files_written.iter().any(|f| f.eq_ignore_ascii_case(&p)) {
            s.files_written.push(p);
        }
    }

    /// Best available progress estimate: the agent's own report, else the budget used.
    pub fn estimated_progress(&self) -> f64 {
        let s = self.state();
        if s.status == AgentStatus::Completed {
            return 1.0;
        }
        s.reported_progress.unwrap_or(if s.max_iterations > 0 {
            (f64::from(s.iteration_count) / f64::from(s.max_iterations)).clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    /// Messages queued for its next turn.
    pub fn pending_message_count(&self) -> usize {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(super) fn enqueue_message(&self, message: &str) {
        self.inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(message.to_string());
    }

    pub(super) fn try_dequeue_message(&self) -> Option<String> {
        self.inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    pub(super) fn record_user_message(&self, message: &str) {
        self.state().user_messages.push(message.to_string());
    }

    pub(super) fn take_user_messages(&self) -> Vec<String> {
        std::mem::take(&mut self.state().user_messages)
    }

    pub(super) fn arm_placed(&self, on_placed: Option<Box<dyn FnOnce() + Send>>) {
        *self.on_placed.lock().unwrap_or_else(|e| e.into_inner()) = on_placed;
    }

    /// Run the "placed" callback once (its slot is leased, or it ended before that).
    pub(super) fn signal_placed(&self) {
        let f = self
            .on_placed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(f) = f {
            f();
        }
    }

    /// Cancel this agent's run (no effect when it is not running).
    pub(super) fn cancel(&self) -> bool {
        match self.state().cancellation.clone() {
            Some(c) => {
                c.cancel();
                true
            }
            None => false,
        }
    }

    pub(super) fn notify_finished(&self) {
        self.finished.notify_waiters();
    }

    /// Wait until the agent is in a final state (`Completion` in C#).
    pub async fn wait(&self) {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_active() {
                return;
            }
            notified.await;
        }
    }
}
