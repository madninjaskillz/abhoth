//! The sub-agent orchestrator (`AgentOrchestrator`): creates agents (letters, labels, the
//! spawn-depth and agent caps), runs each one's own model/tool loop to a final state (step
//! budget, time limit, cancellation, the partial report), runs batches in parallel with
//! dependencies, and handles follow-ups and messages to agents.
//!
//! The spawn depth travels with the async flow like C#'s `AsyncLocal`: a task-local set around
//! each agent's run, so a spawn tool called from inside a sub-agent's tool call sees the depth of
//! that agent plus one, and `report_progress` knows whose bar to move.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::activity;
use super::brief_files;
use super::cancel::CancelToken;
use super::client::{AgentChatClient, AgentClientProvider, ProviderRoute, SubAgentToolExecutor};
use super::context::{AgentContext, AgentSpec};
use super::events::AgentEvent;
use super::labels;
use super::palette;
use super::prompt_loader::AgentPromptLoader;
use super::read_only_command;
use super::repetition::RepetitionDetector;
use super::step_budget::{self, StepBudget};
use super::types::{AgentOrigin, AgentStatus, AgentTypeDefinition, AgentTypeRegistry};
use crate::agent::tool_call_parser;
use crate::chat::adaptive_reasoning::AdaptiveReasoning;
use crate::chat::{
    CompactionAction, CompactionSettings, Conversation, ConversationCompactor, ReasoningCap,
    planner,
};
use crate::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, Role, StreamControl, StreamEvent, ToolCall,
    ToolDefinition, request_parameters,
};
use crate::config::OrcaConfig;
use crate::config::dotnet_json::NEWLINE;

/// Results longer than this are cut before they reach the parent's context.
pub const MAX_RESULT_CHARS: usize = 16_000;
const MAX_RETAINED_AGENTS: usize = 32;
const ARGS_PREVIEW_MAX_CHARS: usize = 80;
const TOOL_RESULT_PREVIEW_LINES: usize = 12;
/// Most of a dependency's report that a task waiting for it gets.
pub const MAX_DEPENDENCY_REPORT_CHARS: usize = 3000;
/// Longest start of a stopped or cut-off reply kept in a helper's history.
pub const CUT_OFF_STUB_CHARS: usize = 600;

/// One line appended to every sub-agent prompt: the optional progress marker.
pub const PROGRESS_HINT: &str = "Optional: when you know how far along you are, start a message with a marker like \"[progress 40%] reading tests\" so the user can see your progress.";

/// The progress tool every agent (and the Lead) may call; always allowed.
pub const REPORT_PROGRESS_TOOL_NAME: &str = "report_progress";
/// The Pi profile's progress tool; always allowed too.
pub const PI_PROGRESS_TOOL_NAME: &str = "progress";

/// Appended instead of [`PROGRESS_HINT`] when the agent has the report_progress tool.
pub const PROGRESS_TOOL_HINT: &str = "Your progress is shown to the user as a bar. At real milestones call report_progress with percent (0-100) and an optional label of at most 4 words (e.g. percent 40, label \"reading tests\") \u{2014} ideally alongside another tool call so it costs no extra turn. A message starting with \"[progress 40%] reading tests\" works too.";

/// Id used in progress events for the Lead (the main agent).
pub const LEAD_ID: &str = "lead";

/// Status text of an agent queued for a free slot.
pub const WAITING_FOR_SLOT: &str = "waiting for slot";

/// Appended to every sub-agent prompt in swarm mode.
pub const SWARM_WORKER_HINT: &str = "You are one of many agents working in parallel on small pieces. Keep your context small: read only the files your brief names and the ones you change (large files in ranges), do not explore the rest of the repository, do not re-read a file you just wrote, and keep command output short (e.g. pipe long build or test logs through a filter for errors). Stay inside the files your brief gives you. A parser or test fixture for a file format follows a real sample: read one real input file on this machine (or the official docs) before writing it, never an invented format; tests that only use invented fixtures do not verify anything. End with a short report (at most 10 lines): what you did, the check you ran and its result, the files you changed.";

/// A read-only agent's wrap-up notice ({0} steps left, step {1} of {2}).
pub fn read_only_wrap_up_notice(left: i32, step: i32, max: i32) -> String {
    format!(
        "[Step budget] {left} steps left (this is step {step} of {max}), and there are no extensions. Stop exploring soon: use at most two more looks, then reply with your complete answer (the full plan, every required section) as plain text, with no tool calls."
    )
}

/// A read-only agent out of steps: its answer now.
pub const READ_ONLY_FINAL_REQUEST: &str = "[Step limit reached] No more tools. Reply now with your complete answer (the full plan, every required section) from what you have seen; mark anything you could not check as unverified.";

/// Told to an agent whose reply hit the output token limit ({0}) without a complete tool call.
pub fn output_cut_off_notice(max: Option<i32>) -> String {
    format!(
        "[Output limit] Your last reply was cut off at the output limit ({} tokens) before it finished, so none of it ran and no file was written. Do the same work in smaller pieces: one file per write call, and split a large file into a first write plus edits. Keep each call well under the limit.",
        max.map(|m| m.to_string()).unwrap_or_default()
    )
}

/// Told to a helper whose reply was stopped for repeating {0}.
pub fn repetition_notice(unit: &str) -> String {
    format!(
        "[Output stopped] Your last reply got stuck repeating the same text (\"{unit}\") over and over, so it was stopped and none of it ran. Do that step differently and keep it short: no long generated strings or test inputs, a few plain lines per command or file, one tool call at a time."
    )
}

/// Added (peer mode) to [`output_cut_off_notice`].
pub const CUT_OFF_NO_REPEAT_HINT: &str =
    "If the reply was repeating the same text, do that step differently instead of retrying it.";

/// The final report request of a helper whose replies in a row were stopped or cut off.
pub fn cut_off_final_request() -> String {
    crate::agent::prompt_constants::platform_lines(
        "Your last replies were stopped or cut off before they finished, so you are stopping here. Do not call any more tools. Reply with your report in this shape:\n## Completed\nWhat works now, and the last check you ran with its result.\n## Remaining\nEach piece of work still to do, specific enough to continue from (file, what is missing or failing).\n## Files Changed\nThe files you created or changed.",
    )
}

/// What an agent at its step limit is asked for.
pub fn partial_report_request() -> String {
    crate::agent::prompt_constants::platform_lines(
        "You have reached your step limit. Do not call any more tools. Reply with your report in this shape:\n## Completed\nWhat works now, and the last check you ran with its result.\n## Remaining\nEach piece of work still to do, specific enough to continue from (file, what is missing or failing).\n## Files Changed\nThe files you created or changed.",
    )
}

/// The report request after the agent timeout: same shape as at the step limit.
pub fn time_limit_report_request(seconds: i32) -> String {
    partial_report_request().replace(
        "You have reached your step limit.",
        &format!("You have reached your time limit ({seconds} s); your last step was stopped."),
    )
}

/// Options for a spawn beyond task and type (`SpawnOptions`).
#[derive(Default)]
pub struct SpawnOptions {
    pub origin: AgentOrigin,
    /// Keep the conversation after the run so a follow-up can resume the agent.
    pub retain_conversation: bool,
    /// Short bar label; None = derived from the task.
    pub label: Option<String>,
    /// The biggest task of its batch.
    pub heavy: bool,
    /// Called once the agent can run (its slot is leased) or it ended before that.
    pub on_placed: Option<Box<dyn FnOnce() + Send>>,
    /// A letter slot reserved for this agent; None = the next free letter.
    pub letter_slot: Option<i32>,
    /// A reserved place in the creation order; None = the next place.
    pub schedule_position: Option<i32>,
    /// Run on this named provider instead of the schedule's (takes no place in the order).
    pub provider: Option<String>,
    pub max_iterations: Option<i32>,
    pub max_output_tokens: Option<i32>,
    pub effort: Option<String>,
    pub timeout_seconds: Option<i32>,
    /// Read-only: only look-and-list tools and read-only bash commands run.
    pub read_only: bool,
    /// Keep the whole result (the planner's plan).
    pub full_result: bool,
}

/// One task of a parallel batch (`ParallelAgentSpec`). `size`: the Lead's size estimate;
/// `after`: indices of tasks that must finish first (their reports are added to this task).
#[derive(Debug, Clone, Default)]
pub struct ParallelAgentSpec {
    pub task: String,
    pub agent_type: String,
    pub label: Option<String>,
    pub size: Option<i32>,
    pub after: Option<Vec<usize>>,
}

impl ParallelAgentSpec {
    pub fn new(task: impl Into<String>, agent_type: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            agent_type: agent_type.into(),
            ..Self::default()
        }
    }
}

/// What happened to a message sent to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageResult {
    NotFound,
    Queued,
    Resumed,
    NotAccepting,
}

/// Builds the sub-agent system prompt (type, the tools it will see, task); None = the type's own.
pub type SystemPromptFactory =
    Arc<dyn Fn(&AgentTypeDefinition, &[ToolDefinition], &str) -> Option<String> + Send + Sync>;

type EventHandler = Arc<dyn Fn(&AgentEvent) + Send + Sync>;
type UserAgentRepliedHandler = Arc<dyn Fn(&AgentContext, &str) + Send + Sync>;

#[derive(Clone, Default)]
struct Ambient {
    depth: i32,
    agent: Option<Arc<AgentContext>>,
}

tokio::task_local! {
    static AMBIENT: Ambient;
}

#[derive(Default)]
struct CreateState {
    next_number: i32,
    /// Places in the creation order handed out so far.
    scheduled: i32,
    /// Every letter slot handed out this session, in order.
    letters_used: Vec<i32>,
}

#[derive(Default)]
struct LeadState {
    progress: Option<f64>,
    note: Option<String>,
    request_mode: Option<String>,
}

pub struct AgentOrchestrator {
    config: OrcaConfig,
    types: RwLock<AgentTypeRegistry>,
    provider: Arc<dyn AgentClientProvider>,
    compactor: RwLock<Option<Arc<dyn ConversationCompactor>>>,
    agents: Mutex<Vec<Arc<AgentContext>>>,
    create: Mutex<CreateState>,
    tools: RwLock<Vec<ToolDefinition>>,
    executor: RwLock<Option<Arc<dyn SubAgentToolExecutor>>>,
    system_prompt_factory: RwLock<Option<SystemPromptFactory>>,
    use_streaming: AtomicBool,
    text_tool_calls: AtomicBool,
    handlers: Mutex<Vec<EventHandler>>,
    user_agent_replied: Mutex<Vec<UserAgentRepliedHandler>>,
    lead: Mutex<LeadState>,
    user_agent_notes: Mutex<VecDeque<String>>,
    cwd: PathBuf,
    platform: String,
    /// A follow-up after at least this pause gets the thinking-aware compaction first.
    pub resume_compaction_idle: Mutex<Duration>,
    /// How long the report after a timeout may take.
    pub timeout_report_grace: Mutex<Duration>,
}

impl AgentOrchestrator {
    /// An orchestrator for the session in `cwd`.
    pub fn new(
        provider: Arc<dyn AgentClientProvider>,
        config: OrcaConfig,
        types: AgentTypeRegistry,
        cwd: impl Into<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            types: RwLock::new(types),
            provider,
            compactor: RwLock::new(None),
            agents: Mutex::new(Vec::new()),
            create: Mutex::new(CreateState::default()),
            tools: RwLock::new(Vec::new()),
            executor: RwLock::new(None),
            system_prompt_factory: RwLock::new(None),
            use_streaming: AtomicBool::new(false),
            text_tool_calls: AtomicBool::new(true),
            handlers: Mutex::new(Vec::new()),
            user_agent_replied: Mutex::new(Vec::new()),
            lead: Mutex::new(LeadState::default()),
            user_agent_notes: Mutex::new(VecDeque::new()),
            cwd: cwd.into(),
            platform: crate::agent::system_prompt::os_version_string(),
            resume_compaction_idle: Mutex::new(Duration::from_secs(120)),
            timeout_report_grace: Mutex::new(Duration::from_secs(180)),
        })
    }

    pub fn config(&self) -> &OrcaConfig {
        &self.config
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// The agent type registry (custom agents are registered through it).
    pub fn type_registry(&self) -> std::sync::RwLockReadGuard<'_, AgentTypeRegistry> {
        self.types.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn type_registry_mut(&self) -> std::sync::RwLockWriteGuard<'_, AgentTypeRegistry> {
        self.types.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn client_provider(&self) -> &Arc<dyn AgentClientProvider> {
        &self.provider
    }

    /// The tools sub-agents may get (`Tools`), before the per-type filter.
    pub fn set_tools(&self, tools: Vec<ToolDefinition>) {
        *self.tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    pub fn tools(&self) -> Vec<ToolDefinition> {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The tool pipeline sub-agents run their calls through (`ToolResultExecutor`).
    pub fn set_executor(&self, executor: Option<Arc<dyn SubAgentToolExecutor>>) {
        *self.executor.write().unwrap_or_else(|e| e.into_inner()) = executor;
    }

    fn executor(&self) -> Option<Arc<dyn SubAgentToolExecutor>> {
        self.executor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_system_prompt_factory(&self, factory: Option<SystemPromptFactory>) {
        *self
            .system_prompt_factory
            .write()
            .unwrap_or_else(|e| e.into_inner()) = factory;
    }

    pub fn set_compactor(&self, compactor: Option<Arc<dyn ConversationCompactor>>) {
        *self.compactor.write().unwrap_or_else(|e| e.into_inner()) = compactor;
    }

    /// Stream sub-agent responses (the CLI turns it on).
    pub fn set_use_streaming(&self, on: bool) {
        self.use_streaming.store(on, Ordering::SeqCst);
    }

    pub fn use_streaming(&self) -> bool {
        self.use_streaming.load(Ordering::SeqCst)
    }

    /// Parse tool calls out of reply text (`TextToolCallParser` wired; on by default).
    pub fn set_text_tool_calls(&self, on: bool) {
        self.text_tool_calls.store(on, Ordering::SeqCst);
    }

    /// Subscribe to agent events (`AgentEventRaised`).
    pub fn subscribe(&self, handler: impl Fn(&AgentEvent) + Send + Sync + 'static) {
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(handler));
    }

    /// `UserAgentReplied`: an agent the user continued is done again, with the note for the Lead.
    pub fn on_user_agent_replied(
        &self,
        handler: impl Fn(&AgentContext, &str) + Send + Sync + 'static,
    ) {
        self.user_agent_replied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(handler));
    }

    fn raise(&self, event: AgentEvent) {
        let handlers: Vec<EventHandler> = self
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for h in handlers {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&event)));
            if r.is_err() {
                tracing::warn!("Agent event handler threw for {}", event.kind());
            }
        }
    }

    // ── depth, lead progress ─────────────────────────────────────────────────

    /// The spawn depth of a spawn issued from the current async flow: 0 from the Lead or the
    /// user, the calling agent's depth + 1 from inside a sub-agent's tool call.
    pub fn current_depth() -> i32 {
        AMBIENT.try_with(|a| a.depth).unwrap_or(0)
    }

    /// The sub-agent whose tool call is running in this async flow, if any.
    pub fn current_agent() -> Option<Arc<AgentContext>> {
        AMBIENT.try_with(|a| a.agent.clone()).ok().flatten()
    }

    /// Progress the Lead reported for the current task (0..1).
    pub fn lead_progress(&self) -> Option<f64> {
        self.lead.lock().unwrap_or_else(|e| e.into_inner()).progress
    }

    pub fn lead_note(&self) -> Option<String> {
        self.lead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note
            .clone()
    }

    /// agent.leadMode "auto": the mode the Lead chose for the current request.
    pub fn lead_request_mode(&self) -> Option<String> {
        self.lead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_mode
            .clone()
    }

    /// Record the Lead's mode for this request and repaint its bar.
    pub fn set_lead_request_mode(&self, mode: Option<&str>) {
        let (progress, note) = {
            let mut l = self.lead.lock().unwrap_or_else(|e| e.into_inner());
            l.request_mode = mode.map(str::to_string);
            (l.progress, l.note.clone())
        };
        self.raise(AgentEvent::ProgressReported {
            agent_id: LEAD_ID.into(),
            fraction: progress.unwrap_or(0.0),
            note,
        });
    }

    /// report_progress: move the bar of whoever is calling (the sub-agent in this flow, or the
    /// Lead). Returns the tool's reply.
    pub fn report_progress(&self, percent: i32, label: Option<&str>) -> String {
        let percent = percent.clamp(0, 100);
        let fraction = f64::from(percent) / 100.0;
        let note = labels::normalize(label);
        let note = (!note.is_empty()).then_some(note);
        if let Some(agent) = Self::current_agent() {
            {
                let mut s = agent.state();
                s.reported_progress = Some(fraction);
                s.progress_note = note.clone();
            }
            self.raise(AgentEvent::ProgressReported {
                agent_id: agent.id.clone(),
                fraction,
                note,
            });
            return format!("Progress recorded for {}: {percent}%.", agent.tag());
        }
        {
            let mut l = self.lead.lock().unwrap_or_else(|e| e.into_inner());
            l.progress = Some(fraction);
            l.note = note.clone();
        }
        self.raise(AgentEvent::ProgressReported {
            agent_id: LEAD_ID.into(),
            fraction,
            note,
        });
        format!("Progress recorded: {percent}%.")
    }

    /// A new user turn: the Lead's bar starts again at 0%.
    pub fn reset_lead_progress(&self) {
        {
            let mut l = self.lead.lock().unwrap_or_else(|e| e.into_inner());
            if l.progress.is_none() && l.note.is_none() && l.request_mode.is_none() {
                return;
            }
            *l = LeadState::default();
        }
        self.raise(AgentEvent::ProgressReported {
            agent_id: LEAD_ID.into(),
            fraction: 0.0,
            note: None,
        });
    }

    // ── caps ─────────────────────────────────────────────────────────────────

    /// Sub-agents created this session (each counts toward agent.maxAgents).
    pub fn agents_created(&self) -> i32 {
        self.create
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scheduled
    }

    /// How many more agents may be created, or None without a cap.
    pub fn agents_left(&self) -> Option<i32> {
        let scheduled = self.agents_created();
        (self.config.agent.max_agents > 0)
            .then(|| (self.config.agent.max_agents - scheduled).max(0))
    }

    /// The remaining agent budget for the Lead (swarm mode), or None.
    pub fn budget_note(&self) -> Option<String> {
        let max = self.config.agent.max_agents;
        if !is_swarm(&self.config) || max <= 0 {
            return None;
        }
        let left = self.agents_left().unwrap_or(0);
        Some(if left > 0 {
            format!(
                "Agent budget: {left} of {max} new agents left (follow-ups with to do not count). A piece that is still large can go to several new agents, one file group each."
            )
        } else {
            format!("Agent budget: all {max} agents used; send new work as follow-ups with to.")
        })
    }

    fn try_reserve_positions(state: &mut CreateState, max: i32, count: i32) -> Option<i32> {
        let first = state.scheduled;
        if max > 0 && state.scheduled + count > max {
            return None;
        }
        state.scheduled += count;
        Some(first)
    }

    /// The error for a call that would create `new_agents` agents past agent.maxAgents.
    pub fn cap_error(&self, new_agents: i32) -> Option<String> {
        let max = self.config.agent.max_agents;
        let used = {
            let c = self.create.lock().unwrap_or_else(|e| e.into_inner());
            if max <= 0 || c.scheduled + new_agents <= max {
                return None;
            }
            c.scheduled
        };
        let left = (max - used).max(0);
        let agents: Vec<Arc<AgentContext>> = self
            .get_all_agents()
            .into_iter()
            .filter(|a| a.schedule_position.is_some())
            .collect();
        let list = if agents.is_empty() {
            String::new()
        } else {
            format!(
                " Existing agents: {}.",
                agents
                    .iter()
                    .map(|a| format!(
                        "{} ({}{}, {})",
                        a.letter(),
                        a.label(),
                        a.model_tag().map(|m| format!(", {m}")).unwrap_or_default(),
                        state_word(a)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Some(format!(
            "Agent cap reached: {used} of the session's {max} agents are already created (finished ones count), so {}Send the remaining work as follow-ups to existing agents, best to the agent that wrote the code involved: {{\"to\": \"A\", \"task\": \"<the new work>\"}}; it keeps its context, so say only what is new.{list}",
            if left == 0 {
                format!("no new agent can start ({new_agents} asked for). ")
            } else {
                format!(
                    "only {left} more can start, and this call asks for {new_agents}: send at most {left} new task(s), or fewer. "
                )
            }
        ))
    }

    // ── spawning ─────────────────────────────────────────────────────────────

    /// Spawn a typed agent at the ambient depth and wait for it.
    pub async fn spawn_agent(self: &Arc<Self>, task: &str, agent_type: &str) -> Arc<AgentContext> {
        self.spawn_agent_with(
            task,
            agent_type,
            Self::current_depth(),
            &CancelToken::new(),
            SpawnOptions::default(),
        )
        .await
    }

    /// Spawn a typed agent at `depth` and wait for it. Never fails for agent-level errors.
    pub async fn spawn_agent_with(
        self: &Arc<Self>,
        task: &str,
        agent_type: &str,
        depth: i32,
        ct: &CancelToken,
        options: SpawnOptions,
    ) -> Arc<AgentContext> {
        let (context, type_def) = self.create_context(task, agent_type, depth, options);
        if let Some(type_def) = type_def {
            self.run_context(&context, &type_def, depth, None, ct).await;
        }
        context
    }

    /// Start an agent in the background and return at once (the user's /agent). It keeps its
    /// conversation; [`AgentContext::wait`] waits for it.
    pub fn start_background_agent(
        self: &Arc<Self>,
        task: &str,
        agent_type: &str,
        options: Option<SpawnOptions>,
    ) -> Arc<AgentContext> {
        let options = options.unwrap_or(SpawnOptions {
            origin: AgentOrigin::User,
            retain_conversation: true,
            ..SpawnOptions::default()
        });
        let depth = Self::current_depth();
        let (context, type_def) = self.create_context(task, agent_type, depth, options);
        let Some(type_def) = type_def else {
            return context;
        };
        context.set_status(AgentStatus::Running);
        let me = self.clone();
        let c = context.clone();
        tokio::spawn(async move {
            me.run_context(&c, &type_def, depth, None, &CancelToken::new())
                .await;
        });
        context
    }

    /// Send a message to an agent: a running agent gets it at its next turn; a finished one that
    /// kept its conversation is resumed in the background with it.
    pub fn send_message(self: &Arc<Self>, id_or_number: &str, message: &str) -> AgentMessageResult {
        let Some(context) = self.find_agent(id_or_number) else {
            return AgentMessageResult::NotFound;
        };
        if context.is_active() {
            context.enqueue_message(message);
            context.record_user_message(message);
            context.transcript.add(
                super::transcript::AgentTranscriptKind::Status,
                "message queued \u{2014} delivered at the agent's next turn",
                false,
            );
            return AgentMessageResult::Queued;
        }
        if !can_resume(&context) {
            return AgentMessageResult::NotAccepting;
        }
        context.record_user_message(message);
        let type_def = self.resolve_type(&context.agent_type);
        {
            let mut s = context.state();
            s.resumed_by_user = true;
            s.result = None;
            s.status = AgentStatus::Running;
            s.completed_at = None;
            s.error = None;
        }
        let me = self.clone();
        let c = context.clone();
        let message = message.to_string();
        tokio::spawn(async move {
            me.run_context(&c, &type_def, c.depth, Some(&message), &CancelToken::new())
                .await;
        });
        AgentMessageResult::Resumed
    }

    fn resolve_type(&self, name: &str) -> AgentTypeDefinition {
        let types = self.type_registry();
        types.resolve(name).unwrap_or_else(|| types.get_default())
    }

    /// Notes for the Lead about agents the user messaged after they finished (each once).
    pub fn drain_user_agent_notes(&self) -> Option<String> {
        let mut q = self
            .user_agent_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return None;
        }
        let mut sb = String::new();
        while let Some(note) = q.pop_front() {
            sb.push_str(&note);
            sb.push_str(NEWLINE);
            sb.push_str(NEWLINE);
        }
        Some(sb.trim_end().to_string())
    }

    /// Cancel one running agent without touching its parent or siblings.
    pub fn cancel_agent(&self, id_or_number: &str) -> bool {
        match self.find_agent(id_or_number) {
            Some(c) if c.is_active() => c.cancel(),
            _ => false,
        }
    }

    /// Find an agent by number ("2", "#2"), id / id prefix (3+ chars) or letter ("A", "Agent A").
    pub fn find_agent(&self, id_or_number: &str) -> Option<Arc<AgentContext>> {
        if id_or_number.trim().is_empty() {
            return None;
        }
        let key = id_or_number.trim().trim_start_matches('#');
        let agents = self.get_all_agents();
        if let Ok(number) = key.parse::<i32>()
            && let Some(a) = agents.iter().find(|a| a.number == number)
        {
            return Some(a.clone());
        }
        agents
            .iter()
            .find(|a| a.id.eq_ignore_ascii_case(key))
            .or_else(|| {
                agents.iter().find(|a| {
                    key.len() >= 3
                        && a.id.len() >= key.len()
                        && a.id[..key.len()].eq_ignore_ascii_case(key)
                })
            })
            .cloned()
            .or_else(|| find_by_letter(&agents, key))
    }

    /// Send a follow-up task to a finished agent and wait for it. Err = the message for the
    /// caller (no such agent, still running, no conversation kept).
    pub async fn follow_up(
        self: &Arc<Self>,
        id_or_letter: &str,
        message: &str,
        ct: &CancelToken,
    ) -> Result<Arc<AgentContext>, String> {
        let Some(context) = self.find_agent(id_or_letter) else {
            return Err(unknown_agent_message(id_or_letter, &self.get_all_agents()));
        };
        let type_def;
        {
            let mut s = context.state();
            if matches!(s.status, AgentStatus::Running | AgentStatus::Pending) {
                return Err(format!(
                    "{} is still running; wait for its result first.",
                    context.tag()
                ));
            }
            if !context.retain_conversation || s.conversation.messages().is_empty() {
                return Err(format!(
                    "{} kept no conversation, so it can't take a follow-up; spawn a new agent with the full context.",
                    context.tag()
                ));
            }
            s.status = AgentStatus::Running;
            s.completed_at = None;
            s.error = None;
            s.result = None;
            drop(s);
            type_def = self.resolve_type(&context.agent_type);
        }
        self.run_context(&context, &type_def, context.depth, Some(message), ct)
            .await;
        Ok(context)
    }

    fn create_context(
        &self,
        task: &str,
        agent_type: &str,
        depth: i32,
        options: SpawnOptions,
    ) -> (Arc<AgentContext>, Option<AgentTypeDefinition>) {
        let mut create = self.create.lock().unwrap_or_else(|e| e.into_inner());
        let max_depth = self.config.agent.max_spawn_depth;
        create.next_number += 1;
        let number = create.next_number;
        let label = labels::derive(options.label.as_deref(), Some(task), Some(agent_type));
        let caller_depth = Self::current_depth();
        let failed = |create: &mut CreateState, error: String| {
            let color_slot = options
                .letter_slot
                .unwrap_or_else(|| self.next_color_slot(create, &[]));
            let ctx = Arc::new(AgentContext::new(AgentSpec {
                number,
                task: task.to_string(),
                label: label.clone(),
                color_slot,
                agent_type: agent_type.to_string(),
                depth,
                caller_depth,
                origin: options.origin,
                ..AgentSpec::default()
            }));
            {
                let mut s = ctx.state();
                let now = SystemTime::now();
                s.started_at = Some(now);
                s.completed_at = Some(now);
                s.status = AgentStatus::Failed;
                s.error = Some(error.clone());
                s.result = Some(error);
            }
            self.track_agent(ctx.clone());
            ctx
        };
        if depth >= max_depth {
            tracing::warn!("Blocked spawn at depth {depth} (max {max_depth}): {task}");
            let ctx = failed(
                &mut create,
                format!(
                    "Maximum agent spawn depth ({max_depth}) reached. Cannot spawn nested agents beyond this depth."
                ),
            );
            return (ctx, None);
        }
        let position = if options.provider.is_some() {
            -1
        } else if let Some(reserved) = options.schedule_position {
            reserved
        } else {
            match Self::try_reserve_positions(&mut create, self.config.agent.max_agents, 1) {
                Some(p) => p,
                None => {
                    drop(create);
                    let error = self.cap_error(1).unwrap_or_else(|| {
                        format!("Agent cap ({}) reached.", self.config.agent.max_agents)
                    });
                    let mut create = self.create.lock().unwrap_or_else(|e| e.into_inner());
                    tracing::warn!(
                        "Blocked spawn: agent cap {} reached: {task}",
                        self.config.agent.max_agents
                    );
                    let ctx = failed(&mut create, error);
                    return (ctx, None);
                }
            }
        };

        let type_def = self.resolve_type(agent_type);
        let scheduled_provider = options.provider.clone().or_else(|| {
            if type_def.model.is_none() {
                self.config.agent.provider_at(position.max(0) as usize)
            } else {
                None
            }
        });
        let has_schedule = self
            .config
            .agent
            .provider_schedule
            .as_ref()
            .is_some_and(|s| !s.is_empty());
        let model_tag = options.provider.clone().or_else(|| {
            has_schedule.then(|| {
                scheduled_provider
                    .clone()
                    .or_else(|| type_def.model.clone())
                    .unwrap_or_else(|| crate::config::AgentConfig::LOCAL_PROVIDER.to_string())
            })
        });
        let lead_delegates = (is_orchestrator_lead(&self.config)
            || is_auto_lead(&self.config)
            || self.config.agent.peer_mode)
            && Self::current_agent().is_none();
        let color_slot = options
            .letter_slot
            .unwrap_or_else(|| self.next_color_slot(&mut create, &[]));
        let max_iterations = options.max_iterations.unwrap_or_else(|| {
            iterations_for(&self.config, &type_def.name, type_def.max_iterations)
        });
        let ctx = Arc::new(AgentContext::new(AgentSpec {
            number,
            task: task.to_string(),
            label,
            color_slot,
            agent_type: type_def.name.clone(),
            depth,
            caller_depth,
            origin: options.origin,
            read_only: options.read_only,
            max_output_tokens_override: options.max_output_tokens,
            effort: options.effort.clone(),
            timeout_seconds_override: options.timeout_seconds,
            max_iterations_override: options.max_iterations,
            full_result: options.full_result,
            schedule_position: (position >= 0).then_some(position),
            heavy: options.heavy,
            retain_conversation: options.retain_conversation || lead_delegates,
        }));
        {
            let mut s = ctx.state();
            s.provider = scheduled_provider;
            s.model_tag = model_tag;
            s.started_at = Some(SystemTime::now());
            s.max_iterations = max_iterations;
            s.current_activity = Some("starting".into());
        }
        ctx.arm_placed(options.on_placed);
        self.track_agent(ctx.clone());
        (ctx, Some(type_def))
    }

    /// The next letter (slot): never one an earlier agent of this session had, preferring a
    /// colour no agent on the footer holds. `pending`: slots reserved but not yet on the footer.
    fn next_color_slot(&self, create: &mut CreateState, pending: &[i32]) -> i32 {
        let now = SystemTime::now();
        let held: Vec<i32> = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|a| holds(a, now))
            .map(|a| a.color_slot)
            .chain(pending.iter().copied())
            .collect();
        let slot = palette::assign_letter(&create.letters_used, &held);
        create.letters_used.push(slot);
        slot
    }

    /// Reserve `count` letters at once, in listed order.
    pub fn reserve_letters(&self, count: usize) -> Vec<i32> {
        let mut create = self.create.lock().unwrap_or_else(|e| e.into_inner());
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let s = self.next_color_slot(&mut create, &slots);
            slots.push(s);
        }
        slots
    }

    fn track_agent(&self, context: Arc<AgentContext>) {
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        agents.push(context);
        // Evict the oldest finished agents only: a running agent must stay findable.
        let mut i = 0;
        while agents.len() > MAX_RETAINED_AGENTS && i < agents.len() {
            if agents[i].is_active() {
                i += 1;
            } else {
                agents.remove(i);
            }
        }
    }

    pub fn get_all_agents(&self) -> Vec<Arc<AgentContext>> {
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Agents that are running or pending.
    pub fn get_running_agents(&self) -> Vec<Arc<AgentContext>> {
        self.get_all_agents()
            .into_iter()
            .filter(|a| a.is_active())
            .collect()
    }

    // ── parallel batches ─────────────────────────────────────────────────────

    /// Run a batch concurrently (bounded by agent.maxConcurrent); results in batch order. Err:
    /// a bad `after` list, or the batch does not fit under agent.maxAgents (nothing ran).
    pub async fn spawn_parallel(
        self: &Arc<Self>,
        specs: &[ParallelAgentSpec],
        depth: i32,
        ct: &CancelToken,
    ) -> Result<Vec<Arc<AgentContext>>, String> {
        if let Some(error) = dependency_error(specs) {
            return Err(error);
        }
        let order = start_order(specs);
        let first_position = {
            let mut create = self.create.lock().unwrap_or_else(|e| e.into_inner());
            Self::try_reserve_positions(
                &mut create,
                self.config.agent.max_agents,
                specs.len() as i32,
            )
        };
        let Some(first_position) = first_position else {
            return Err(self.cap_error(specs.len() as i32).unwrap_or_else(|| {
                format!("Agent cap ({}) reached.", self.config.agent.max_agents)
            }));
        };
        let letters = self.reserve_letters(specs.len());
        let gate = Arc::new(tokio::sync::Semaphore::new(
            self.config.agent.max_concurrent.max(1) as usize,
        ));
        let done: Vec<tokio::sync::watch::Sender<Option<Arc<AgentContext>>>> = specs
            .iter()
            .map(|_| tokio::sync::watch::channel(None).0)
            .collect();

        let run = |i: usize| {
            let me = self.clone();
            let gate = gate.clone();
            let done_tx = done[i].clone();
            let deps: Vec<tokio::sync::watch::Receiver<Option<Arc<AgentContext>>>> = specs[i]
                .after
                .iter()
                .flatten()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|d| done[d].subscribe())
                .collect();
            let mut spec = specs[i].clone();
            let letter = letters[i];
            let position = first_position + i as i32;
            let ct = ct.clone();
            async move {
                let mut finished = Vec::new();
                for mut rx in deps {
                    let dep = loop {
                        if let Some(d) = rx.borrow().clone() {
                            break Some(d);
                        }
                        if rx.changed().await.is_err() {
                            break rx.borrow().clone();
                        }
                    };
                    if let Some(d) = dep {
                        finished.push(d);
                    }
                }
                let result = if let Some(failed) = finished.iter().find(|d| {
                    !matches!(
                        d.status(),
                        AgentStatus::Completed | AgentStatus::CompletedPartial
                    )
                }) {
                    let reason = format!(
                        "Skipped: it waits for {} ({}), which did not finish ({}).",
                        failed.tag(),
                        failed.label(),
                        failed.status().as_str()
                    );
                    me.skipped(&spec, depth, reason, Some(letter), Some(position))
                } else {
                    if !finished.is_empty() {
                        spec.task = with_dependency_reports(&spec.task, &finished);
                    }
                    let _permit = gate.acquire().await;
                    me.spawn_agent_with(
                        &spec.task,
                        &spec.agent_type,
                        depth,
                        &ct,
                        SpawnOptions {
                            label: spec.label.clone(),
                            letter_slot: Some(letter),
                            schedule_position: Some(position),
                            ..SpawnOptions::default()
                        },
                    )
                    .await
                };
                let _ = done_tx.send(Some(result.clone()));
                (i, result)
            }
        };
        let futures: Vec<_> = order.iter().map(|&i| run(i)).collect();
        let mut results: Vec<Option<Arc<AgentContext>>> = vec![None; specs.len()];
        for (i, ctx) in futures_util::future::join_all(futures).await {
            results[i] = Some(ctx);
        }
        Ok(results
            .into_iter()
            .map(|r| r.expect("every task ran"))
            .collect())
    }

    /// A batch task that never ran (its dependency failed).
    fn skipped(
        &self,
        spec: &ParallelAgentSpec,
        depth: i32,
        reason: String,
        letter: Option<i32>,
        position: Option<i32>,
    ) -> Arc<AgentContext> {
        let ctx = {
            let mut create = self.create.lock().unwrap_or_else(|e| e.into_inner());
            create.next_number += 1;
            let number = create.next_number;
            let color_slot = letter.unwrap_or_else(|| self.next_color_slot(&mut create, &[]));
            let ctx = Arc::new(AgentContext::new(AgentSpec {
                number,
                task: spec.task.clone(),
                label: labels::derive(
                    spec.label.as_deref(),
                    Some(&spec.task),
                    Some(&spec.agent_type),
                ),
                color_slot,
                agent_type: spec.agent_type.clone(),
                depth,
                caller_depth: Self::current_depth(),
                schedule_position: position,
                ..AgentSpec::default()
            }));
            {
                let mut s = ctx.state();
                let now = SystemTime::now();
                s.started_at = Some(now);
                s.completed_at = Some(now);
                s.status = AgentStatus::Failed;
                s.error = Some(reason.clone());
                s.result = Some(reason.clone());
            }
            self.track_agent(ctx.clone());
            ctx
        };
        tracing::info!("Sub-agent #{} skipped: {reason}", ctx.number);
        self.raise(AgentEvent::Completed {
            agent_id: ctx.id.clone(),
            status: ctx.status(),
            iterations: 0,
        });
        ctx
    }

    // ── one agent's run ──────────────────────────────────────────────────────

    /// Run (or resume) an agent to a final state: status, timeout, cancellation and failure
    /// handling, completion event. Never fails for agent-level errors.
    async fn run_context(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        depth: i32,
        resume_message: Option<&str>,
        ct: &CancelToken,
    ) {
        let timeout_seconds = context
            .timeout_seconds_override
            .or(type_def.timeout_seconds)
            .unwrap_or(self.config.agent.timeout_seconds);
        let mut client = self.client_for_type(type_def);
        let mut route: Option<ProviderRoute> = None;
        if type_def.model.is_none()
            && let Some(provider_name) = context.provider()
        {
            match self.provider.provider_route(&provider_name) {
                Some(r) => {
                    client = Arc::new(super::client::PinnedModelClient::new(
                        r.client.clone(),
                        r.model.clone(),
                    ));
                    context.state().provider_model = Some(r.model.clone());
                    route = Some(r);
                }
                None => {
                    context.transcript.add(
                        super::transcript::AgentTranscriptKind::Status,
                        &format!(
                            "provider {provider_name} unavailable; running on the local model"
                        ),
                        true,
                    );
                    let mut s = context.state();
                    s.provider = None;
                    s.model_tag = Some(crate::config::AgentConfig::LOCAL_PROVIDER.into());
                }
            }
        }
        self.provider.note_type(context, type_def);

        let agent_ct = ct.child();
        {
            let mut s = context.state();
            s.cancellation = Some(agent_ct.clone());
            s.status = AgentStatus::Running;
            s.completed_at = None;
            s.time_limit_seconds = None;
        }
        tracing::info!(
            "{} {} sub-agent #{} {} (depth {depth}): {}",
            if resume_message.is_none() {
                "Spawning"
            } else {
                "Resuming"
            },
            type_def.name,
            context.number,
            context.id,
            context.task
        );
        let spawner = Self::current_agent();
        self.raise(AgentEvent::Started {
            agent_id: context.id.clone(),
            context: context.clone(),
        });

        // A slot first (may queue): the agent's timeout starts once it can run.
        if type_def.model.is_none() && context.provider().is_none() {
            let waiting_ctx = context.clone();
            let me = self.clone();
            let waiting = move || {
                {
                    let mut s = waiting_ctx.state();
                    s.status = AgentStatus::Pending;
                    s.current_activity = Some(WAITING_FOR_SLOT.into());
                }
                waiting_ctx.transcript.add(
                    super::transcript::AgentTranscriptKind::Status,
                    "waiting for a free inference slot",
                    false,
                );
                me.raise(AgentEvent::ProgressReported {
                    agent_id: waiting_ctx.id.clone(),
                    fraction: waiting_ctx.reported_progress().unwrap_or(0.0),
                    note: waiting_ctx.progress_note(),
                });
            };
            let lease = tokio::select! {
                l = self.provider.acquire(context, spawner.as_deref(), &waiting) => Some(l),
                _ = agent_ct.cancelled() => None,
            };
            match lease {
                Some(Some(lease)) => {
                    client = lease.client;
                    let mut s = context.state();
                    if s.status == AgentStatus::Pending {
                        s.status = AgentStatus::Running;
                    }
                    if s.current_activity.as_deref() == Some(WAITING_FOR_SLOT) {
                        s.current_activity = Some("starting".into());
                    }
                    if lease.slot.is_some() {
                        s.last_slot = lease.slot.clone();
                    }
                    s.slot = lease.slot;
                    drop(s);
                    if let Some(note) = lease.note {
                        context.transcript.add(
                            super::transcript::AgentTranscriptKind::Status,
                            &note,
                            false,
                        );
                    }
                }
                Some(None) => {}
                None => {
                    self.finish_run(
                        context,
                        type_def,
                        RunEnd::Cancelled(ct.is_cancelled()),
                        &client,
                        &agent_ct,
                    )
                    .await;
                    return;
                }
            }
        }
        context.signal_placed();

        let run = self.run_agent(
            context,
            type_def,
            depth,
            client.clone(),
            route.as_ref(),
            resume_message,
        );
        let end = tokio::select! {
            r = run => match r {
                Ok(()) => RunEnd::Done,
                Err(e) => RunEnd::Failed(e),
            },
            _ = agent_ct.cancelled() => RunEnd::Cancelled(ct.is_cancelled()),
            _ = tokio::time::sleep(Duration::from_secs(timeout_seconds.max(0) as u64)) => RunEnd::TimedOut(timeout_seconds),
        };
        self.finish_run(context, type_def, end, &client, &agent_ct)
            .await;
    }

    async fn finish_run(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        end: RunEnd,
        client: &Arc<dyn AgentChatClient>,
        agent_ct: &CancelToken,
    ) {
        match end {
            RunEnd::Done => {
                let mut s = context.state();
                // run_agent sets CompletedPartial itself on iteration exhaustion.
                if s.status == AgentStatus::Running {
                    s.status = AgentStatus::Completed;
                }
            }
            RunEnd::TimedOut(seconds) => {
                // Out of time is like out of steps: the agent comes back INCOMPLETE with a
                // Completed / Remaining report (one short extra request).
                tracing::warn!("Sub-agent {} timed out after {seconds}s", context.id);
                {
                    let mut s = context.state();
                    s.status = AgentStatus::CompletedPartial;
                    s.time_limit_seconds = Some(seconds);
                    s.current_activity = Some("summarizing".into());
                }
                close_open_tool_calls(
                    context,
                    &format!("Not run: the agent's time limit ({seconds} s) was reached."),
                );
                let tools = context.state().request_tools.clone().unwrap_or_default();
                let grace = *self
                    .timeout_report_grace
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let request = time_limit_report_request(seconds);
                let report = tokio::select! {
                    r = self.request_final_summary(context, type_def, client, &tools, Some(&request)) => r,
                    _ = tokio::time::sleep(grace) => None,
                    _ = agent_ct.cancelled() => None,
                };
                let header = format!(
                    "Agent timed out after {seconds} seconds ({} iterations).",
                    context.iteration_count()
                );
                let report = report.unwrap_or_else(|| timeout_report(context, Some(&header)));
                context.state().result = Some(report);
            }
            RunEnd::Cancelled(by_parent) => {
                let mut s = context.state();
                s.status = AgentStatus::Cancelled;
                s.error = Some(if by_parent {
                    "Agent was cancelled.".into()
                } else {
                    "Agent was cancelled by the user.".into()
                });
            }
            RunEnd::Failed(message) => {
                tracing::error!("Sub-agent {} failed: {message}", context.id);
                let mut s = context.state();
                s.status = AgentStatus::Failed;
                s.error = Some(message);
            }
        }

        context.signal_placed();
        self.provider.release(context).await;
        {
            let mut s = context.state();
            s.slot = None;
        }
        tracing::info!(
            "Sub-agent #{} {} finished: {} after {} iterations",
            context.number,
            type_def.name,
            context.status().as_str(),
            context.iteration_count()
        );
        let mut lead_note = None;
        {
            let mut s = context.state();
            s.completed_at = Some(SystemTime::now());
            s.cancellation = None;
            s.current_activity = None;
            if !context.full_result
                && let Some(long) = s.result.as_ref().filter(|r| len16(r) > MAX_RESULT_CHARS)
            {
                let total = len16(long);
                s.result = Some(format!(
                    "{}\n\n[Result truncated \u{2014} {} more chars omitted]",
                    take16(long, MAX_RESULT_CHARS),
                    crate::chat::n0((total - MAX_RESULT_CHARS) as i64)
                ));
            }
        }
        // Whoever reads the result (the Lead) learns what the user told the agent meanwhile.
        if let Some(told) = user_messages_note(&context.take_user_messages()) {
            let mut s = context.state();
            s.result = Some(match s.result.as_deref() {
                Some(r) if !r.is_empty() => format!("{r}\n\n{told}"),
                _ => told,
            });
        }
        if context.state().resumed_by_user {
            context.state().resumed_by_user = false;
            if context.origin != AgentOrigin::User {
                let reply = {
                    let s = context.state();
                    s.result
                        .clone()
                        .or(s.error.clone())
                        .unwrap_or_else(|| "(no reply)".into())
                };
                let note = format!(
                    "[The user messaged {} ({}) directly after it finished. Its reply ({}):\n{}]",
                    context.tag(),
                    context.label(),
                    state_word(context),
                    truncate3(&reply, 3000)
                );
                self.user_agent_notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(note.clone());
                lead_note = Some(note);
            }
        }
        // The result string is what the parent consumes; drop the transcript unless the agent
        // may be resumed.
        if !context.retain_conversation {
            context.state().conversation.clear();
        }
        let (status, error, iterations) = {
            let s = context.state();
            (s.status, s.error.clone(), s.iteration_count)
        };
        let line = match status {
            AgentStatus::Completed => format!("completed ({iterations} iterations)"),
            AgentStatus::CompletedPartial => format!(
                "stopped at its {} \u{2014} partial result",
                context.limit_description()
            ),
            AgentStatus::Cancelled => error.clone().unwrap_or_else(|| "cancelled".into()),
            _ => format!("failed: {}", error.clone().unwrap_or_default()),
        };
        context.transcript.add(
            super::transcript::AgentTranscriptKind::Status,
            &line,
            status == AgentStatus::Failed,
        );
        context.notify_finished();
        self.raise(AgentEvent::Completed {
            agent_id: context.id.clone(),
            status,
            iterations,
        });
        if let Some(note) = lead_note {
            let handlers: Vec<_> = self
                .user_agent_replied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for h in handlers {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(context, &note)))
                    .is_err()
                {
                    tracing::warn!("UserAgentReplied handler threw");
                }
            }
        }
    }

    /// The client for an agent type: its model reference when set and resolvable, else the
    /// default client.
    fn client_for_type(&self, type_def: &AgentTypeDefinition) -> Arc<dyn AgentChatClient> {
        if let Some(model) = &type_def.model {
            match self.provider.client_for_model(model) {
                Some(c) => return c,
                None => tracing::warn!(
                    "Agent type {} requests model '{model}' but it could not be resolved; using default client.",
                    type_def.name
                ),
            }
        }
        self.provider.default_client()
    }

    /// The tools of an agent type: all when it is unrestricted, else the allowed ones plus the
    /// always-allowed progress tools.
    pub fn filter_tools_for_type(&self, type_def: &AgentTypeDefinition) -> Vec<ToolDefinition> {
        let tools = self.tools();
        match &type_def.allowed_tools {
            None => tools,
            Some(allowed) => tools
                .into_iter()
                .filter(|t| {
                    allowed.iter().any(|a| a.eq_ignore_ascii_case(&t.name))
                        || is_progress_tool(&t.name)
                })
                .collect(),
        }
    }

    /// The model-facing loop of one agent (`RunAgentAsync`). Err = the agent failed.
    async fn run_agent(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        depth: i32,
        client: Arc<dyn AgentChatClient>,
        route: Option<&ProviderRoute>,
        resume_message: Option<&str>,
    ) -> Result<(), String> {
        let ambient = Ambient {
            depth: depth + 1,
            agent: Some(context.clone()),
        };
        AMBIENT
            .scope(
                ambient,
                self.run_agent_inner(context, type_def, depth, client, route, resume_message),
            )
            .await
    }

    async fn run_agent_inner(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        depth: i32,
        client: Arc<dyn AgentChatClient>,
        route: Option<&ProviderRoute>,
        resume_message: Option<&str>,
    ) -> Result<(), String> {
        let default_model = type_def.model.is_none() && context.provider().is_none();
        let max_iterations = context.max_iterations_override.unwrap_or_else(|| {
            iterations_for(&self.config, &type_def.name, type_def.max_iterations)
        });
        {
            let mut s = context.state();
            s.max_iterations = max_iterations;
            s.iteration_count = 0;
        }
        let mut reasoning = if default_model {
            AdaptiveReasoning::for_config(&self.config.agent.reasoning, false)
        } else {
            None
        };

        let mut filtered = self.filter_tools_for_type(type_def);
        let next_depth = depth + 1;
        if next_depth >= self.config.agent.max_spawn_depth {
            filtered.retain(|t| !activity::is_agent_tool(&t.name));
        }
        let native = self.config.lm_studio.native_tool_calling;
        // Every request (the final summary included) sends the same tools.
        let request_tools: Vec<ToolDefinition> = if native { filtered.clone() } else { Vec::new() };
        context.state().request_tools = Some(request_tools.clone());
        let context_config = self.context_for(route);

        match resume_message {
            None => {
                let mut system_prompt = self.build_system_prompt(context, type_def, &filtered);
                if !native {
                    system_prompt = append_text_tool_instructions(&system_prompt, &filtered);
                }
                let user = if self.config.agent.preload_brief_files {
                    brief_files::append(&context.task, &self.cwd)
                } else {
                    context.task.clone()
                };
                let mut s = context.state();
                s.conversation.add_system_message(system_prompt);
                s.conversation.add_user_message(user);
                drop(s);
                context.transcript.add(
                    super::transcript::AgentTranscriptKind::Task,
                    &context.task,
                    false,
                );
            }
            Some(message) => {
                // A follow-up after a long pause: make the re-read short first (no model call).
                let idle = context
                    .state()
                    .last_request_at
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::MAX);
                let resume_idle = *self
                    .resume_compaction_idle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if context_config.auto_compact_enabled
                    && context_config.thinking_aware_compaction
                    && idle >= resume_idle
                {
                    let settings = CompactionSettings::from_config(&context_config);
                    let mut s = context.state();
                    let before = s
                        .conversation
                        .estimate_context_tokens(context_config.chars_per_token);
                    if before > settings.keep_recent_tokens * 3 / 2 {
                        let reclaimed = planner::compact_for_resume(
                            &mut s.conversation,
                            &settings,
                            &context_config,
                        );
                        if reclaimed > 0 {
                            drop(s);
                            tracing::info!(
                                "Compaction (resume) for agent {}: ~{before} tokens before its follow-up",
                                context.id
                            );
                            context.transcript.add(
                                super::transcript::AgentTranscriptKind::Status,
                                &format!(
                                    "compacted context (~{} tokens of old reasoning and output dropped)",
                                    crate::chat::n0(i64::from(reclaimed))
                                ),
                                false,
                            );
                        }
                    }
                }
                context.state().conversation.add_user_message(message);
                let from = if context.state().resumed_by_user {
                    "user"
                } else {
                    "lead"
                };
                context.transcript.add_message(message, from);
                self.raise(AgentEvent::MessageDelivered {
                    agent_id: context.id.clone(),
                    message: message.to_string(),
                });
            }
        }

        let tool_timeout =
            Duration::from_secs(self.config.agent.tool_timeout_seconds.max(1) as u64);
        let mut narration = String::new();
        // A read-only agent (the planner) has a hard limit: no extensions.
        let mut budget = StepBudget::new(
            max_iterations,
            if context.read_only {
                0
            } else {
                self.config.agent.iteration_extensions
            },
        );
        let mut budget_notice: Option<String> = None;
        let mut cut_offs_in_a_row = 0;
        let mut reasoning_cap = if self.use_streaming() && default_model {
            ReasoningCap::for_config(&self.config, false)
        } else {
            None
        };
        let peer_guard_on = self.config.agent.peer_mode && self.config.agent.peer_repetition_guard;

        let mut iteration: i32 = 0;
        while iteration < budget.max() {
            {
                let mut s = context.state();
                s.iteration_count = iteration + 1;
                s.current_activity = Some("thinking".into());
            }
            let step = iteration + 1;
            self.raise(AgentEvent::IterationStarted {
                agent_id: context.id.clone(),
                iteration: step,
                max_iterations: budget.max(),
            });
            self.deliver_queued_messages(context);
            if budget_notice.is_none()
                && step > 1
                && let Some(warning) = budget.warning_before(step)
            {
                budget_notice = Some(if context.read_only {
                    read_only_wrap_up_notice(step_budget::WARN_STEPS_LEFT, step, budget.max())
                } else {
                    warning
                });
            }
            if let Some(notice) = budget_notice.take() {
                context
                    .state()
                    .conversation
                    .add_user_message(notice.clone());
                context.transcript.add(
                    super::transcript::AgentTranscriptKind::Status,
                    &notice,
                    false,
                );
            }
            self.maybe_compact(context, &context_config, Some(budget.max() - iteration))
                .await;

            let mut options = ChatOptions {
                temperature: Some(self.config.lm_studio.temperature),
                max_output_tokens: self.max_output_tokens_for(context, route),
                tools: request_tools.clone(),
                model_from_client: true,
                ..ChatOptions::default()
            };
            self.apply_request_parameters(&mut options, type_def, context);
            if let Some(r) = reasoning.as_mut()
                && r.should_skip(context.state().conversation.messages())
            {
                request_parameters::skip_thinking(&mut options);
            }

            let mut guard = peer_guard_on.then(RepetitionDetector::new);
            if let Some(cap) = reasoning_cap.as_mut() {
                cap.begin_request();
            }
            let mut mark = context.state().conversation.mark();
            let mut response = self
                .get_response(
                    context,
                    &client,
                    &options,
                    guard.as_mut(),
                    reasoning_cap.as_mut(),
                )
                .await?;
            // The cap stopped a request that was still only thinking: ask again (append only).
            while let Some(cap) = reasoning_cap.as_mut().filter(|c| c.fired()) {
                tracing::info!(
                    "Reasoning cap: agent #{} step {step} cut at {} reasoning tokens (limit {}, fire {}/{})",
                    context.number,
                    cap.reasoning_tokens(),
                    cap.max_step_tokens(),
                    cap.fires_this_step(),
                    cap.retries()
                );
                context.transcript.add(
                    super::transcript::AgentTranscriptKind::Status,
                    &cap.describe(),
                    false,
                );
                cap.reask(context.state().conversation.messages_mut());
                mark = context.state().conversation.mark();
                guard = peer_guard_on.then(RepetitionDetector::new);
                cap.begin_request();
                response = self
                    .get_response(
                        context,
                        &client,
                        &options,
                        guard.as_mut(),
                        reasoning_cap.as_mut(),
                    )
                    .await?;
            }
            {
                let mut s = context.state();
                s.last_request_at = Some(Instant::now());
                if let Some(tokens) = Conversation::prompt_tokens_of(&response.usage) {
                    s.conversation.record_prompt_usage_at(tokens, mark);
                }
            }

            let has_native_calls = !response.tool_calls.is_empty();
            let found = guard.as_ref().and_then(|g| g.found().map(str::to_string));
            if guard.is_some()
                && (found.is_some()
                    || (was_cut_off(&response, &options)
                        && !has_native_calls
                        && !(self.text_tool_calls.load(Ordering::SeqCst)
                            && !tool_call_parser::parse_tool_calls_from_text(&response.text)
                                .is_empty())))
            {
                cut_offs_in_a_row += 1;
                context
                    .state()
                    .conversation
                    .add_assistant_message(cut_off_stub(&response.text, found.as_deref()));
                let limit = self.config.agent.peer_max_cut_offs_in_a_row;
                if (limit > 0 && cut_offs_in_a_row >= limit) || iteration >= budget.max() - 1 {
                    tracing::warn!(
                        "Sub-agent #{}: {cut_offs_in_a_row} replies in a row stopped or cut off; ending with a partial report",
                        context.number
                    );
                    context.transcript.add(
                        super::transcript::AgentTranscriptKind::Status,
                        &format!(
                            "{cut_offs_in_a_row} replies in a row stopped or cut off; stopping"
                        ),
                        false,
                    );
                    break;
                }
                let notice = match &found {
                    Some(unit) => repetition_notice(&truncate3(unit.trim(), 60)),
                    None => format!(
                        "{} {CUT_OFF_NO_REPEAT_HINT}",
                        output_cut_off_notice(options.max_output_tokens)
                    ),
                };
                context
                    .state()
                    .conversation
                    .add_user_message(notice.clone());
                context.transcript.add(
                    super::transcript::AgentTranscriptKind::Status,
                    &notice,
                    false,
                );
                iteration += 1;
                continue;
            }
            cut_offs_in_a_row = 0;

            let mut message = response.to_message();
            // `ToChatResponse` coalesces the streamed text parts into one.
            message.leading_empty_part = false;
            context.state().conversation.add_message(message);
            let mut response_text = String::new();
            if !response.text.trim().is_empty() {
                response_text.push_str(&response.text);
                response_text.push_str(NEWLINE);
            }
            if !response_text.is_empty() {
                narration.push_str(&response_text);
                narration.push_str(NEWLINE);
                context.transcript.add(
                    super::transcript::AgentTranscriptKind::AssistantText,
                    &response_text,
                    false,
                );
                self.record_progress(context, &response_text);
            }

            if !has_native_calls {
                if self.text_tool_calls.load(Ordering::SeqCst) && !response_text.is_empty() {
                    let parsed = tool_call_parser::parse_tool_calls_from_text(&response_text);
                    if !parsed.is_empty() {
                        let calls: Vec<ToolCall> = parsed
                            .iter()
                            .map(|p| ToolCall {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                arguments: Value::Object(p.arguments.clone()).to_string(),
                            })
                            .collect();
                        budget.record_calls(&calls);
                        self.execute_text_tool_calls(
                            context,
                            type_def,
                            &calls,
                            next_depth,
                            tool_timeout,
                        )
                        .await;
                        budget_notice = self.extend_at_limit(context, &mut budget, iteration);
                        iteration += 1;
                        continue;
                    }
                }
                // Cut off at the output limit: nothing ran; tell the agent to split the work.
                if was_cut_off(&response, &options) && iteration < budget.max() - 1 {
                    let notice = output_cut_off_notice(options.max_output_tokens);
                    tracing::info!(
                        "Sub-agent #{} reply was cut off at the output limit; asking it to continue in smaller pieces",
                        context.number
                    );
                    context
                        .state()
                        .conversation
                        .add_user_message(notice.clone());
                    context.transcript.add(
                        super::transcript::AgentTranscriptKind::Status,
                        &notice,
                        false,
                    );
                    iteration += 1;
                    continue;
                }
                // A message arrived while it wrote its answer: let it react.
                if context.pending_message_count() > 0 && iteration < budget.max() - 1 {
                    iteration += 1;
                    continue;
                }
                let final_text = activity::strip_progress_markers(Some(&response_text));
                context.state().result = Some(if final_text.is_empty() {
                    activity::strip_progress_markers(Some(&narration))
                } else {
                    final_text
                });
                return Ok(());
            }

            budget.record_calls(&response.tool_calls);
            let mut round: Vec<(String, bool)> = Vec::with_capacity(response.tool_calls.len());
            for call in &response.tool_calls {
                let args = native_args_json(&call.arguments);
                let mut result = self
                    .execute_tool_call(context, type_def, call, &args, next_depth, tool_timeout)
                    .await;
                round.push((call.name.clone(), context.state().last_tool_call_failed));
                if self.config.agent.repeat_call_notes {
                    let note = {
                        let s = context.state();
                        crate::chat::repeat_call_note::note_for(
                            s.conversation.messages(),
                            call,
                            &result,
                        )
                    };
                    if let Some(note) = note {
                        tracing::info!(
                            "Agent {} repeated a {} call with no change",
                            context.id,
                            call.name
                        );
                        result.push_str(&note);
                    }
                }
                context
                    .state()
                    .conversation
                    .add_message(ChatMessage::tool_result(call.id.clone(), result));
            }
            if let Some(r) = reasoning.as_mut() {
                let s = context.state();
                r.note_round(&round, s.conversation.messages());
            }
            budget_notice = self.extend_at_limit(context, &mut budget, iteration);
            iteration += 1;
        }

        // Out of steps: salvage the work instead of discarding it.
        {
            let mut s = context.state();
            s.status = AgentStatus::CompletedPartial;
            s.current_activity = Some("summarizing".into());
        }
        let request = if context.read_only {
            Some(READ_ONLY_FINAL_REQUEST.to_string())
        } else if cut_offs_in_a_row > 0 {
            Some(cut_off_final_request())
        } else {
            None
        };
        let summary = self
            .request_final_summary(
                context,
                type_def,
                &client,
                &request_tools,
                request.as_deref(),
            )
            .await;
        let narration = narration.trim().to_string();
        context.state().result = Some(summary.unwrap_or_else(|| {
            if narration.is_empty() {
                "Agent reached maximum iterations without completing.".into()
            } else {
                format!("[Partial result \u{2014} iteration limit reached]\n{narration}")
            }
        }));
        Ok(())
    }

    /// After the last step of the budget: extend it while the agent is still making progress.
    fn extend_at_limit(
        &self,
        context: &AgentContext,
        budget: &mut StepBudget,
        iteration: i32,
    ) -> Option<String> {
        if iteration != budget.max() - 1 {
            return None;
        }
        let notice = budget.try_extend()?;
        context.state().max_iterations = budget.max();
        tracing::info!(
            "Sub-agent #{} reached its step limit while making progress; extended to {} steps",
            context.number,
            budget.max()
        );
        Some(notice)
    }

    fn build_system_prompt(
        &self,
        context: &AgentContext,
        type_def: &AgentTypeDefinition,
        tools: &[ToolDefinition],
    ) -> String {
        let cwd = self.cwd.display().to_string();
        let factory = self
            .system_prompt_factory
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut prompt = factory.and_then(|f| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(type_def, tools, &context.task)
            }))
            .unwrap_or_else(|_| {
                tracing::warn!("Sub-agent system prompt factory failed; using the type's prompt");
                None
            })
        });
        if prompt.is_none() {
            prompt = AgentPromptLoader::load_prompt(type_def, &context.task, &cwd, &self.platform);
        }
        let mut prompt = prompt.unwrap_or_else(|| {
            crate::agent::prompt_constants::platform_lines(&format!(
                "You are a focused sub-agent. Complete the following task and return a concise result.\nTask: {}\n\nCurrent working directory: {cwd}\nPlatform: {}\n\nUse tools to accomplish the task. Be efficient and focused.\nWhen done, provide a clear summary of what you found or accomplished.",
                context.task, self.platform
            ))
        });
        if context.origin == AgentOrigin::User {
            prompt.push_str("\n\nYou run in the background while the user keeps working. Tools that need the user's approval are denied \u{2014} work with what is allowed and say what you couldn't do.");
        }
        let progress_tool = tools
            .iter()
            .map(|t| t.name.as_str())
            .find(|n| is_progress_tool(n));
        if progress_tool == Some(REPORT_PROGRESS_TOOL_NAME)
            && !prompt.contains(REPORT_PROGRESS_TOOL_NAME)
        {
            prompt.push_str("\n\n");
            prompt.push_str(PROGRESS_TOOL_HINT);
        } else if progress_tool.is_none() && !prompt.to_lowercase().contains("[progress") {
            prompt.push_str("\n\n");
            prompt.push_str(PROGRESS_HINT);
        }
        if is_swarm(&self.config) {
            prompt.push_str("\n\n");
            prompt.push_str(SWARM_WORKER_HINT);
        }
        prompt
    }

    /// Hand queued user messages to the agent as user turns.
    fn deliver_queued_messages(&self, context: &AgentContext) {
        while let Some(message) = context.try_dequeue_message() {
            context
                .state()
                .conversation
                .add_user_message(format!("[Message from the user while you work]\n{message}"));
            context.transcript.add_message(&message, "user");
            self.raise(AgentEvent::MessageDelivered {
                agent_id: context.id.clone(),
                message,
            });
        }
    }

    fn record_progress(&self, context: &AgentContext, text: &str) {
        let Some((fraction, note)) = activity::try_parse_progress(Some(text)) else {
            return;
        };
        {
            let mut s = context.state();
            if s.reported_progress == Some(fraction) && s.progress_note == note {
                return;
            }
            s.reported_progress = Some(fraction);
            s.progress_note = note.clone();
        }
        self.raise(AgentEvent::ProgressReported {
            agent_id: context.id.clone(),
            fraction,
            note,
        });
    }

    /// One request: streamed when [`Self::use_streaming`] (text deltas reach the transcript, the
    /// repetition guard and the reasoning cap can stop it), else a plain request.
    async fn get_response(
        &self,
        context: &AgentContext,
        client: &Arc<dyn AgentChatClient>,
        options: &ChatOptions,
        mut guard: Option<&mut RepetitionDetector>,
        mut cap: Option<&mut ReasoningCap>,
    ) -> Result<ChatResponse, String> {
        let messages = context.state().conversation.get_messages_for_api();
        if !self.use_streaming() {
            return client
                .complete(&context.id, &messages, options)
                .await
                .map_err(|e| chat_error_message(&e));
        }
        let mut streamed = String::new();
        let mut update_text = String::new();
        let mut update_reasoning = String::new();
        let mut on_event = |e: StreamEvent| -> StreamControl {
            match e {
                StreamEvent::Text(t) => update_text.push_str(&t),
                StreamEvent::Reasoning(r) => update_reasoning.push_str(&r),
                StreamEvent::ToolCallStarted { .. } | StreamEvent::Usage(_) => {}
                StreamEvent::UpdateEnd { tool_call_delta } => {
                    let text = std::mem::take(&mut update_text);
                    let reasoning = std::mem::take(&mut update_reasoning);
                    if let Some(g) = guard.as_deref_mut()
                        && (g.add_content(Some(&text)) || g.add_reasoning(Some(&reasoning)))
                    {
                        tracing::warn!(
                            "Sub-agent #{} reply stopped: stuck repeating {}",
                            context.number,
                            truncate3(g.found().unwrap_or_default(), 80)
                        );
                        return StreamControl::Stop;
                    }
                    if let Some(c) = cap.as_deref_mut()
                        && c.observe_update(
                            (!reasoning.is_empty()).then_some(reasoning.as_str()),
                            Some(text.as_str()),
                            tool_call_delta,
                        )
                    {
                        return StreamControl::Stop;
                    }
                    if !text.is_empty() {
                        {
                            let mut s = context.state();
                            if s.current_activity.as_deref() == Some("thinking") {
                                s.current_activity = Some("writing".into());
                            }
                        }
                        context.transcript.raise_delta(&text);
                        streamed.push_str(&text);
                        if text.contains('\n') {
                            self.record_progress(context, &streamed);
                        }
                    }
                }
            }
            StreamControl::Continue
        };
        client
            .stream(&context.id, &messages, options, &mut on_event)
            .await
            .map_err(|e| chat_error_message(&e))
    }

    /// Text-parsed calls: run them and feed the results back as one user message.
    async fn execute_text_tool_calls(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        calls: &[ToolCall],
        next_depth: i32,
        tool_timeout: Duration,
    ) {
        let mut parts = Vec::new();
        for call in calls {
            let args = native_args_json(&call.arguments);
            let result = self
                .execute_tool_call(context, type_def, call, &args, next_depth, tool_timeout)
                .await;
            parts.push(format!("[Tool result for {}]\n{result}", call.name));
        }
        context.state().conversation.add_user_message(format!(
            "Here are the results of the tool calls you made:\n\n{}\n\nContinue with the task based on these results. If you need to use more tools, use <tool_call> tags.",
            parts.join("\n\n")
        ));
    }

    /// One tool call: allowed-tool, read-only and spawn-depth checks, the timeout, events.
    async fn execute_tool_call(
        self: &Arc<Self>,
        context: &Arc<AgentContext>,
        type_def: &AgentTypeDefinition,
        call: &ToolCall,
        args: &str,
        next_depth: i32,
        tool_timeout: Duration,
    ) -> String {
        let tool_name = call.name.as_str();
        let described = activity::describe(tool_name, Some(args), Some(&self.cwd));
        context.state().current_activity = Some(described.clone());
        context
            .transcript
            .add_tool_call(&described, tool_name, args);
        self.raise(AgentEvent::ToolCallStarted {
            agent_id: context.id.clone(),
            tool_name: tool_name.to_string(),
            args_preview: truncate3(args, ARGS_PREVIEW_MAX_CHARS),
            activity: Some(described),
        });
        let started = Instant::now();
        let max_depth = self.config.agent.max_spawn_depth;
        let (result, is_error) = if !is_tool_allowed(type_def, tool_name) {
            tracing::warn!(
                "Agent {} ({}) attempted disallowed tool: {tool_name}",
                context.id,
                type_def.name
            );
            let allowed = type_def
                .allowed_tools
                .clone()
                .unwrap_or_else(|| vec!["all".into()]);
            (
                format!(
                    "Tool '{tool_name}' is not available to {} agents. Allowed tools: {}.",
                    type_def.name,
                    allowed.join(", ")
                ),
                true,
            )
        } else if let Some(refusal) = context
            .read_only
            .then(|| read_only_command::refusal(tool_name, args))
            .flatten()
        {
            tracing::info!(
                "Read-only agent {}: refused {tool_name}: {refusal}",
                context.id
            );
            (refusal, true)
        } else if activity::is_agent_tool(tool_name) && next_depth >= max_depth {
            (
                format!("Cannot spawn nested agents: maximum depth ({max_depth}) reached."),
                true,
            )
        } else if let Some(executor) = self.executor() {
            let exec = executor.execute(tool_name, args);
            if activity::is_agent_tool(tool_name) {
                // Spawn tools manage their own (per-agent) time budget.
                let r = exec.await;
                (r.content, r.is_error)
            } else {
                match tokio::time::timeout(tool_timeout, exec).await {
                    Ok(r) => (r.content, r.is_error),
                    Err(_) => {
                        tracing::warn!("Tool {tool_name} timed out in agent {}", context.id);
                        (
                            format!(
                                "Error: Tool '{tool_name}' timed out after {} seconds.",
                                tool_timeout.as_secs()
                            ),
                            true,
                        )
                    }
                }
            }
        } else {
            ("Tool execution not available.".to_string(), true)
        };
        if !is_error && let Some(written) = written_path(tool_name, args) {
            context.record_file_written(&written);
        }
        {
            let mut s = context.state();
            s.last_tool_call_failed = is_error;
            s.current_activity = Some("thinking".into());
        }
        context.transcript.add(
            super::transcript::AgentTranscriptKind::ToolResult,
            &preview_lines(&result, TOOL_RESULT_PREVIEW_LINES),
            is_error,
        );
        self.raise(AgentEvent::ToolCallCompleted {
            agent_id: context.id.clone(),
            tool_name: tool_name.to_string(),
            is_error,
            elapsed_ms: started.elapsed().as_millis() as i64,
        });
        result
    }

    /// Keep the agent's conversation inside its context budget (the main loop's policy).
    async fn maybe_compact(
        &self,
        context: &AgentContext,
        context_config: &crate::config::ContextConfig,
        remaining_steps: Option<i32>,
    ) {
        if !context_config.auto_compact_enabled {
            return;
        }
        let settings = CompactionSettings::from_config(context_config);
        let mut conversation = context.state().conversation.clone();
        let plan = planner::plan(
            &conversation,
            &settings,
            context_config,
            remaining_steps,
            None,
        );
        if plan.action == CompactionAction::None {
            if plan.tokens_before > settings.planned_trigger_tokens {
                tracing::debug!("Compaction check for agent {}: {plan}", context.id);
            }
            return;
        }
        tracing::info!("Compaction plan for agent {}: {plan}", context.id);
        context.state().current_activity = Some("compacting".into());
        let compactor = self
            .compactor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (rewritten, _) = planner::apply(
            &mut conversation,
            &plan,
            context_config,
            compactor.as_deref(),
        )
        .await;
        tracing::info!(
            "Compaction done for agent {}: ~{} -> ~{} tokens ({})",
            context.id,
            plan.tokens_before,
            conversation.estimate_context_tokens(context_config.chars_per_token),
            plan.action
        );
        context.state().conversation = conversation;
        if rewritten {
            context.transcript.add(
                super::transcript::AgentTranscriptKind::Status,
                "compacted context",
                false,
            );
            self.raise(AgentEvent::Compacted {
                agent_id: context.id.clone(),
            });
        }
    }

    /// The context settings an agent's compaction uses: its provider's window when it has one.
    fn context_for(&self, route: Option<&ProviderRoute>) -> crate::config::ContextConfig {
        let mut c = self.config.context.clone();
        if let Some(window) = route.and_then(|r| r.context_window).filter(|w| *w > 0) {
            c.context_window_size = window;
            c.effective_keep_first_tokens = c.compute_effective_keep_first_tokens();
        }
        c
    }

    /// Sampler and thinking parameters apply to the default client only.
    fn apply_request_parameters(
        &self,
        options: &mut ChatOptions,
        type_def: &AgentTypeDefinition,
        context: &AgentContext,
    ) {
        if type_def.model.is_none() && context.provider().is_none() {
            request_parameters::apply(
                options,
                &self.config,
                self.config.lm_studio.primary_model(),
                false,
            );
            request_parameters::apply_effort(
                options,
                self.config.agent.reasoning.effort_for(false).as_deref(),
            );
        } else if context.provider().is_some()
            && let Some(effort) = context.effort.as_deref().filter(|e| !e.trim().is_empty())
        {
            options.set("reasoning_effort", Value::String(effort.to_string()));
        }
    }

    /// The output cap of an agent's requests (peer mode caps it further).
    fn max_output_tokens_for(
        &self,
        context: &AgentContext,
        route: Option<&ProviderRoute>,
    ) -> Option<i32> {
        peer_cap(
            context
                .max_output_tokens_override
                .or(route.and_then(|r| r.max_tokens))
                .or(self.config.lm_studio.max_tokens),
            if self.config.agent.peer_mode {
                self.config.agent.peer_max_tokens
            } else {
                0
            },
        )
    }

    /// One last request for the agent's report (non-streaming, with the agent's own tools so
    /// the prompt prefix stays cached). None on any failure.
    async fn request_final_summary(
        &self,
        context: &AgentContext,
        type_def: &AgentTypeDefinition,
        client: &Arc<dyn AgentChatClient>,
        tools: &[ToolDefinition],
        request: Option<&str>,
    ) -> Option<String> {
        let request = request
            .map(str::to_string)
            .unwrap_or_else(partial_report_request);
        context.state().conversation.add_user_message(request);
        let mut options = ChatOptions {
            temperature: Some(self.config.lm_studio.temperature),
            max_output_tokens: self.max_output_tokens_for(context, None),
            tools: tools.to_vec(),
            model_from_client: true,
            ..ChatOptions::default()
        };
        self.apply_request_parameters(&mut options, type_def, context);
        let messages = context.state().conversation.get_messages_for_api();
        match client.complete(&context.id, &messages, &options).await {
            Ok(r) => {
                let text = r.text.trim().to_string();
                let why = if context.stopped_at_time_limit() {
                    "time limit"
                } else {
                    "iteration limit"
                };
                (!text.is_empty())
                    .then(|| format!("[Partial result \u{2014} {why} reached]\n{text}"))
            }
            Err(e) => {
                tracing::warn!("Final summary request failed for agent {}: {e}", context.id);
                None
            }
        }
    }
}

enum RunEnd {
    Done,
    TimedOut(i32),
    /// true = the caller's token was cancelled (not the user cancelling this agent).
    Cancelled(bool),
    Failed(String),
}

/// The text a failed request leaves as the agent's error (`ex.Message`).
fn chat_error_message(e: &ChatError) -> String {
    e.to_string()
}

/// Arguments as the C# orchestrator passes them on: the parsed object re-serialized indented
/// (`JsonDefaults.Options`), or `{}` when they are not a JSON object.
pub fn native_args_json(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => {
            crate::config::dotnet_json::to_string_dotnet(&v).unwrap_or_else(|_| "{}".into())
        }
        _ => "{}".into(),
    }
}

/// Text-mode results: the tool list (compact signatures) and the shared text tool-calling
/// instructions appended to a sub-agent prompt.
fn append_text_tool_instructions(prompt: &str, tools: &[ToolDefinition]) -> String {
    let mut sb = prompt.to_string();
    let list: Vec<String> = tools
        .iter()
        .map(|t| {
            format!(
                "  - {}",
                crate::agent::prompts::format_signature(
                    &t.name,
                    Some(&t.description),
                    &t.parameters
                )
            )
        })
        .collect();
    let list = list.join("\n");
    if !list.trim().is_empty() {
        sb.push_str(NEWLINE);
        sb.push_str(NEWLINE);
        sb.push_str("TOOLS AVAILABLE:");
        sb.push_str(NEWLINE);
        sb.push_str(&list);
        sb.push_str(NEWLINE);
    }
    sb.push_str(NEWLINE);
    sb.push_str(&crate::agent::prompt_constants::platform_lines(
        crate::config::prompt_manager::TEXT_TOOL_CALL_INSTRUCTIONS,
    ));
    sb
}

/// Give every tool call of the last assistant message a result, so the conversation is valid
/// for one more request after a step was cut off.
fn close_open_tool_calls(context: &AgentContext, text: &str) {
    let mut s = context.state();
    let messages = s.conversation.messages();
    let Some(last) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return;
    };
    let answered: std::collections::HashSet<String> = messages[last + 1..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    let open: Vec<String> = messages[last]
        .tool_calls
        .iter()
        .filter(|c| !c.id.is_empty() && !answered.contains(&c.id))
        .map(|c| c.id.clone())
        .collect();
    for id in open {
        s.conversation
            .add_message(ChatMessage::tool_result(id, text.to_string()));
    }
}

/// What a timed-out agent hands back: the error plus its last notes and tool calls.
pub fn timeout_report(context: &AgentContext, header: Option<&str>) -> String {
    use super::transcript::AgentTranscriptKind;
    let entries = context.transcript.snapshot();
    let mut sb = header
        .map(str::to_string)
        .or_else(|| context.error())
        .unwrap_or_default();
    sb.push_str(" Its work so far is on disk; check it and finish the task.");
    if let Some(last) = entries
        .iter()
        .rev()
        .find(|e| e.kind == AgentTranscriptKind::AssistantText)
        .map(|e| e.text.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        sb.push_str("\n\nLast notes:\n");
        if len16(&last) > 1500 {
            sb.push_str(&take16(&last, 1500));
            sb.push_str(" \u{2026}");
        } else {
            sb.push_str(&last);
        }
    }
    let calls: Vec<String> = entries
        .iter()
        .filter(|e| e.kind == AgentTranscriptKind::ToolCall)
        .map(|e| format!("- {}", e.text))
        .collect();
    let calls = &calls[calls.len().saturating_sub(12)..];
    if !calls.is_empty() {
        sb.push_str("\n\nLast tool calls:\n");
        sb.push_str(&calls.join("\n"));
    }
    sb
}

/// "User told this agent: ..." for the user's messages since its last report.
pub fn user_messages_note(messages: &[String]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let lines: Vec<String> = messages
        .iter()
        .map(|m| {
            format!(
                "- {}",
                truncate3(
                    m.replace("\r\n", " ").replace(['\n', '\r'], " ").trim(),
                    600
                )
            )
        })
        .collect();
    Some(format!(
        "[User told this agent directly (from its tab):\n{}]",
        lines.join("\n")
    ))
}

/// The error for a follow-up to a name no agent has.
pub fn unknown_agent_message(name: &str, agents: &[Arc<AgentContext>]) -> String {
    if agents.is_empty() {
        return format!("No agent \"{name}\": no agents have run in this session yet.");
    }
    let list: Vec<String> = agents
        .iter()
        .map(|a| format!("{} ({}, {})", a.letter(), a.label(), state_word(a)))
        .collect();
    format!(
        "No agent \"{name}\". Valid names: {}. Use one of these letters in \"to\".",
        list.join(", ")
    )
}

/// A finished agent that kept its conversation: a message resumes it.
pub fn can_resume(context: &AgentContext) -> bool {
    !context.is_active()
        && context.retain_conversation
        && !context.state().conversation.messages().is_empty()
}

fn state_word(a: &AgentContext) -> String {
    match a.status() {
        AgentStatus::Completed => "finished".into(),
        AgentStatus::CompletedPartial => {
            if a.stopped_at_time_limit() {
                "stopped at its time limit".into()
            } else {
                "stopped at its step limit".into()
            }
        }
        AgentStatus::Running | AgentStatus::Pending => "running".into(),
        AgentStatus::Cancelled => "cancelled".into(),
        AgentStatus::Failed => "failed".into(),
    }
}

/// "A", "Agent A" or "agent a": the newest agent with that bar letter.
fn find_by_letter(agents: &[Arc<AgentContext>], key: &str) -> Option<Arc<AgentContext>> {
    let k = if key.len() >= 6 && key[..6].eq_ignore_ascii_case("agent ") {
        key[6..].trim()
    } else {
        key
    };
    let mut chars = k.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if !c.is_alphabetic() {
        return None;
    }
    agents
        .iter()
        .rev()
        .find(|a| a.letter().to_uppercase().eq(c.to_uppercase()))
        .cloned()
}

/// True while a finished agent still holds its colour.
pub fn holds(c: &AgentContext, now: SystemTime) -> bool {
    c.is_active()
        || c.completed_at().is_some_and(|done| {
            now.duration_since(done).unwrap_or_default() < palette::hold_after_finish(c.origin)
        })
}

/// A dependent task's brief: its own task, then the reports of the tasks it waited for.
pub fn with_dependency_reports(task: &str, deps: &[Arc<AgentContext>]) -> String {
    let mut sb = task.trim_end().to_string();
    sb.push_str("\n\nThese tasks finished before you started; their reports:");
    for d in deps {
        let mut report = d
            .result()
            .unwrap_or_else(|| "(no report)".into())
            .trim()
            .to_string();
        if len16(&report) > MAX_DEPENDENCY_REPORT_CHARS {
            report = format!(
                "{} [...]",
                take16(&report, MAX_DEPENDENCY_REPORT_CHARS).trim_end()
            );
        }
        sb.push_str(&format!("\n\n## {} ({})\n", d.tag(), d.label()));
        sb.push_str(&report);
    }
    sb
}

/// Why a batch's `after` lists can't run (bad index, self, cycle), or None.
pub fn dependency_error(specs: &[ParallelAgentSpec]) -> Option<String> {
    for (i, s) in specs.iter().enumerate() {
        for &d in s.after.iter().flatten() {
            if d >= specs.len() || d == i {
                return Some(format!(
                    "Task {} waits for task {}, which is {}.",
                    i + 1,
                    d + 1,
                    if d == i { "itself" } else { "not in this call" }
                ));
            }
        }
    }
    // Kahn: every task must become ready.
    let mut indeg: Vec<usize> = specs
        .iter()
        .map(|s| {
            s.after
                .iter()
                .flatten()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        })
        .collect();
    let mut queue: VecDeque<usize> = (0..specs.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(n) = queue.pop_front() {
        seen += 1;
        for (j, s) in specs.iter().enumerate() {
            if s.after.as_ref().is_some_and(|a| a.contains(&n)) {
                indeg[j] -= 1;
                if indeg[j] == 0 {
                    queue.push_back(j);
                }
            }
        }
    }
    (seen != specs.len())
        .then(|| "The tasks' after lists form a cycle, so no task in it could ever start.".into())
}

/// Start order of a batch: by size, biggest first (stable; unsized last), or as listed.
pub fn start_order(specs: &[ParallelAgentSpec]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..specs.len()).collect();
    if specs.iter().any(|s| s.size.is_some_and(|n| n > 0)) {
        idx.sort_by_key(|&i| std::cmp::Reverse(specs[i].size.unwrap_or(-1)));
    }
    idx
}

/// What stays in a helper's history of a reply stopped for repetition (`unit`) or cut off.
pub fn cut_off_stub(reply: &str, unit: Option<&str>) -> String {
    let mut head = reply.trim().to_string();
    if len16(&head) > CUT_OFF_STUB_CHARS {
        head = format!("{} \u{2026}", take16(&head, CUT_OFF_STUB_CHARS));
    }
    let why = match unit {
        Some(u) => format!(
            "[reply stopped: it kept repeating \"{}\"; nothing ran]",
            truncate3(u.trim(), 60)
        ),
        None => "[reply cut off at the output limit; nothing ran]".into(),
    };
    if head.is_empty() {
        why
    } else {
        format!("{head}\n{why}")
    }
}

/// True when the response stopped at the output token limit rather than finishing.
pub fn was_cut_off(response: &ChatResponse, options: &ChatOptions) -> bool {
    response.finish_reason.as_deref() == Some("length")
        || options.max_output_tokens.is_some_and(|max| {
            max > 0
                && response
                    .usage
                    .completion_tokens
                    .is_some_and(|out| out >= i64::from(max))
        })
}

/// The lower of an output cap and a peer cap (`peer_max` 0 or less = no peer cap).
pub fn peer_cap(max: Option<i32>, peer_max: i32) -> Option<i32> {
    if peer_max <= 0 {
        return max;
    }
    match max {
        Some(m) if m > 0 => Some(m.min(peer_max)),
        _ => Some(peer_max),
    }
}

/// Whether a tool name is allowed for the type (unrestricted types allow everything).
pub fn is_tool_allowed(type_def: &AgentTypeDefinition, tool_name: &str) -> bool {
    match &type_def.allowed_tools {
        None => true,
        Some(_) if is_progress_tool(tool_name) => true,
        Some(allowed) => allowed.iter().any(|a| a.eq_ignore_ascii_case(tool_name)),
    }
}

/// report_progress, or the Pi profile's "progress".
pub fn is_progress_tool(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case(REPORT_PROGRESS_TOOL_NAME)
        || tool_name.eq_ignore_ascii_case(PI_PROGRESS_TOOL_NAME)
}

const WRITE_TOOL_NAMES: &[&str] = &[
    "write",
    "edit",
    "write_file",
    "edit_file",
    "multi_edit",
    "create_file",
];

/// The file a write/edit call changed (its path argument), or None for other tools.
pub fn written_path(tool_name: &str, args_json: &str) -> Option<String> {
    if !WRITE_TOOL_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(tool_name))
    {
        return None;
    }
    let args: serde_json::Map<String, Value> = serde_json::from_str(args_json).ok()?;
    for key in ["path", "file_path", "filePath"] {
        let text = match args.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => continue,
            Some(other) => other.to_string(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn preview_lines(text: &str, max_lines: usize) -> String {
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    format!(
        "{}\n\u{2026} ({} more lines)",
        lines[..max_lines].join("\n"),
        lines.len() - max_lines
    )
}

/// `Truncate(value, max)`: over the limit, the first `max - 3` units and "...".
pub fn truncate3(value: &str, max_chars: usize) -> String {
    if len16(value) <= max_chars {
        value.to_string()
    } else {
        format!("{}...", take16(value, max_chars.saturating_sub(3)))
    }
}

/// Length in UTF-16 units (C# `string.Length`).
pub(crate) fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The first `n` UTF-16 units of `s` (C# `s[..n]`), cut back to a char boundary.
pub(crate) fn take16(s: &str, n: usize) -> String {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

// ── AgentConfig helpers (computed properties in C#) ──────────────────────────

/// `AgentConfig.IterationsFor`: the type's override, the reviewer budget, else maxIterations.
pub fn iterations_for(config: &OrcaConfig, type_name: &str, type_override: Option<i32>) -> i32 {
    let a = &config.agent;
    type_override.unwrap_or(
        if a.reviewer_max_iterations > 0
            && (type_name.eq_ignore_ascii_case("reviewer")
                || type_name.eq_ignore_ascii_case("review"))
        {
            a.reviewer_max_iterations
        } else {
            a.max_iterations
        },
    )
}

/// `AgentConfig.IsOrchestratorLead`.
pub fn is_orchestrator_lead(config: &OrcaConfig) -> bool {
    !config.agent.peer_mode
        && config
            .agent
            .lead_mode
            .trim()
            .eq_ignore_ascii_case("orchestrator")
}

/// `AgentConfig.IsAutoLead`.
pub fn is_auto_lead(config: &OrcaConfig) -> bool {
    !config.agent.peer_mode && config.agent.lead_mode.trim().eq_ignore_ascii_case("auto")
}

/// `AgentConfig.IsSwarm`.
pub fn is_swarm(config: &OrcaConfig) -> bool {
    config
        .agent
        .provider_schedule
        .as_ref()
        .is_some_and(|s| !s.is_empty())
        || config.agent.max_agents > 0
}
