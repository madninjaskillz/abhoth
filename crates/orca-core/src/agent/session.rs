//! One agent and its conversation: the model loop (`AgentLoopRunner`, simplified) and the tool
//! pipeline (`ToolExecutionPipeline`).
//!
//! Phase 1 covers one agent, native tool calls and the summary wind-down. Not yet ported (see
//! `PORTING.md`): text-mode tool calls, parameter alias repair, the verification request, the
//! runaway-reasoning cap, compaction, retries/failover, hooks, checkpoints, read-before-edit,
//! early read-only tools and sub-agents.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use orca_abstractions::{Tool, ToolCapabilities, ToolContext, ToolResult};
use orca_tools_common::keep_head_and_tail;

use super::permissions;
use super::prompts::{self, MAX_SUMMARY_CONTINUATIONS, SUMMARY_CONTINUE_MESSAGE};
use super::registry::ToolRegistry;
use crate::client::{
    ChatError, ChatMessage, ChatOptions, OpenAiClient, StreamEvent, ToolCall, request_parameters,
};
use crate::config::OrcaConfig;

/// `CliConstants.ToolResultMaxChars`: model-visible tool output cap.
pub const TOOL_RESULT_MAX_CHARS: usize = 30_000;
/// `CliConstants.ToolResultHistoryMaxChars`: cap for the recorded history copy.
pub const TOOL_RESULT_HISTORY_MAX_CHARS: usize = 10_000;
/// `CliConstants.ToolExecutionTimeoutSeconds`.
pub const TOOL_EXECUTION_TIMEOUT_SECONDS: u64 = 120;

/// One executed tool call, as `--print` reports it (`ToolCallRecord`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub is_error: bool,
    pub duration_ms: i64,
}

/// What a UI wants to see while a turn runs. Every method has a no-op default.
pub trait AgentObserver: Send {
    fn on_text(&mut self, _text: &str) {}
    fn on_reasoning(&mut self, _text: &str) {}
    fn on_tool_start(&mut self, _call: &ToolCall) {}
    fn on_tool_output(&mut self, _line: &str) {}
    fn on_tool_end(&mut self, _call: &ToolCall, _result: &ToolResult) {}
}

pub struct NullObserver;
impl AgentObserver for NullObserver {}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Chat(#[from] ChatError),
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// The model answered without (further) tool calls.
    Done,
    /// The iteration budget ran out.
    IterationLimit,
}

pub struct AgentSession {
    pub config: OrcaConfig,
    pub client: OpenAiClient,
    pub registry: ToolRegistry,
    pub ctx: ToolContext,
    /// The conversation, system prompt first.
    pub messages: Vec<ChatMessage>,
    /// Who this agent is in served-model tracking ("lead" or a sub-agent id).
    pub actor: String,
    /// Advertise tools via native function calling (phase 1 always does).
    pub native_tools: bool,

    pub tool_call_history: Vec<ToolCallRecord>,
    /// Paths changed by successful file-modifying calls, as the model passed them
    /// (case-insensitive set in C#; insertion order kept here).
    pub files_modified: Vec<String>,
    pub total_output_tokens: i64,
    pub last_assistant_response: Option<String>,
    /// Files read (or written) this session with their mtime then: read-before-edit.
    pub files_read: std::collections::HashMap<std::path::PathBuf, Option<std::time::SystemTime>>,
    /// The current call is the first of several in one response: C# records its argument
    /// text compact (the rest indented).
    record_raw_arguments: bool,
    /// Successful file modifications so far (`MutationEpoch`).
    pub mutation_epoch: u64,
    turn_history_start: usize,
    /// The wrap-up a turn ended on when it is not the last response text (verification
    /// changed nothing and the model then answered "Verified.").
    pub final_prose: Option<String>,

    // ---- context management (agent/context.rs, chat::*) ----
    /// Usage bookkeeping for the compaction estimates (server prompt size, rewrite count).
    pub context_usage: crate::chat::ContextUsage,
    /// Overflow recovery already ran this turn (`AgentLoopState.OverflowRecoveryUsed`).
    pub overflow_recovery_used: bool,
    /// What compaction told the user (console lines in C#); a UI drains it.
    pub compaction_notices: Vec<String>,
    // ---- end context management ----
}

/// Per-turn wind-down state (`AgentLoopState`).
#[derive(Default)]
struct WindDown {
    tools_executed: bool,
    verification_requested: bool,
    verification_tools_used: bool,
    summary_requested: bool,
    summary_continuations: u32,
    pre_verification_wrap_up: Option<String>,
    pre_verification_epoch: u64,
}

impl AgentSession {
    pub fn new(
        config: OrcaConfig,
        client: OpenAiClient,
        registry: ToolRegistry,
        ctx: ToolContext,
    ) -> Self {
        Self {
            config,
            client,
            registry,
            ctx,
            messages: Vec::new(),
            actor: "lead".into(),
            native_tools: true,
            tool_call_history: Vec::new(),
            files_modified: Vec::new(),
            total_output_tokens: 0,
            last_assistant_response: None,
            files_read: Default::default(),
            record_raw_arguments: false,
            mutation_epoch: 0,
            turn_history_start: 0,
            final_prose: None,
            context_usage: Default::default(),
            overflow_recovery_used: false,
            compaction_notices: Vec::new(),
        }
    }

    /// Put the system prompt first (replacing an existing one).
    pub fn set_system_prompt(&mut self, prompt: String) {
        match self.messages.first_mut() {
            Some(m) if m.role == crate::client::Role::System => m.content = Some(prompt),
            _ => self.messages.insert(0, ChatMessage::system(prompt)),
        }
    }

    /// The built-in system prompt for this session's tools and directory.
    pub fn default_system_prompt(&self) -> String {
        prompts::system_prompt(
            &self.ctx.cwd.display().to_string(),
            &self.registry,
            self.config.agent.main_loop_max_iterations,
        )
    }

    fn build_options(&self) -> ChatOptions {
        let mut options = ChatOptions::default();
        let model = self.client.settings().default_model.clone();
        request_parameters::apply(&mut options, &self.config, model.as_deref(), false);
        request_parameters::apply_effort(
            &mut options,
            self.config.agent.reasoning.effort_for(true).as_deref(),
        );
        options.max_output_tokens = self.config.lm_studio.max_tokens;
        if self.native_tools {
            options.tools = self.registry.definitions();
        }
        options
    }

    /// Run one user turn to the end.
    pub async fn run_turn(
        &mut self,
        user_message: &str,
        observer: &mut dyn AgentObserver,
    ) -> Result<TurnEnd, AgentError> {
        self.messages.push(ChatMessage::user(user_message));
        self.turn_history_start = self.tool_call_history.len();
        self.final_prose = None;
        self.overflow_recovery_used = false; // context management: once per turn
        let max_iterations = self.config.agent.main_loop_max_iterations.max(1);
        let mut w = WindDown::default();

        for _ in 0..max_iterations {
            // ---- context management: compact before the request, mark what it covers ----
            self.maybe_compact().await;
            let context_mark = self.context_mark();
            // ---- end context management ----
            let options = self.build_options();
            let mut chunks = 0i64;
            let response = {
                let mut on_event = |e: StreamEvent| match e {
                    StreamEvent::Text(t) => {
                        chunks += 1;
                        observer.on_text(&t);
                    }
                    StreamEvent::Reasoning(r) => observer.on_reasoning(&r),
                    _ => {}
                };
                self.client
                    .stream(&self.actor, &self.messages, &options, &mut on_event)
                    .await?
            };
            self.record_prompt_usage(context_mark, &response.usage); // context management
            self.total_output_tokens += response.usage.completion_tokens.unwrap_or(chunks);
            self.last_assistant_response = Some(prompts::strip_next_prompt_tag(&response.text));

            if !response.tool_calls.is_empty() {
                // A native call without arguments for a tool that needs some: the server lost
                // the payload. Drop the response and fall back to text tool calling.
                if self.native_tools && self.has_missing_required_args(&response.tool_calls) {
                    tracing::warn!(
                        "native tool call(s) have empty args; downgrading to text-based calling"
                    );
                    self.native_tools = false;
                    continue;
                }
                self.messages.push(response.to_message());
                let several = response.tool_calls.len() > 1;
                for (i, call) in response.tool_calls.iter().enumerate() {
                    self.record_raw_arguments = several && i == 0;
                    observer.on_tool_start(call);
                    let result = self.execute_tool_call(call, observer).await;
                    observer.on_tool_end(call, &result);
                    self.messages
                        .push(ChatMessage::tool_result(call.id.clone(), result.content));
                }
                w.tools_executed = true;
                if w.verification_requested {
                    w.verification_tools_used = true;
                }
                continue;
            }

            self.messages.push(response.to_message());
            if self.handle_no_tool_call_response(&response, &mut w) {
                continue;
            }
            return Ok(TurnEnd::Done);
        }
        Ok(TurnEnd::IterationLimit)
    }

    /// `ToolCallDispatcher.HandleNoToolCallResponse` (verification and summary wind-down).
    /// True when a follow-up message was added and the loop goes on.
    fn handle_no_tool_call_response(
        &mut self,
        response: &crate::client::ChatResponse,
        w: &mut WindDown,
    ) -> bool {
        if w.tools_executed && !w.verification_requested {
            w.verification_requested = true;
            if self.should_request_verification() {
                let wrap_up = prompts::extract_final_prose(&response.text);
                w.pre_verification_wrap_up =
                    prompts::is_substantive_final_message(&wrap_up).then_some(wrap_up);
                w.pre_verification_epoch = self.mutation_epoch;
                self.messages
                    .push(ChatMessage::user(prompts::platform_lines(
                        prompts::VERIFICATION_MESSAGE,
                    )));
                return true;
            }
            w.verification_tools_used = true;
        }
        if w.tools_executed && w.verification_requested && !w.summary_requested {
            if !w.verification_tools_used {
                w.verification_tools_used = true;
                self.messages
                    .push(ChatMessage::user(prompts::platform_lines(
                        prompts::VERIFICATION_RETRY_MESSAGE,
                    )));
                return true;
            }
            let mut prose = prompts::extract_final_prose(&response.text);
            if !prompts::is_substantive_final_message(&prose)
                && self.mutation_epoch == w.pre_verification_epoch
                && let Some(wrap_up) = w.pre_verification_wrap_up.take()
            {
                prose = wrap_up;
            }
            w.pre_verification_wrap_up = None;
            if prompts::is_substantive_final_message(&prose) {
                self.final_prose = Some(prose);
                return false;
            }
            w.summary_requested = true;
            self.messages
                .push(ChatMessage::user(prompts::summary_request_message()));
            return true;
        }
        if w.summary_requested
            && response.finish_reason.as_deref() == Some("length")
            && w.summary_continuations < MAX_SUMMARY_CONTINUATIONS
        {
            w.summary_continuations += 1;
            self.messages
                .push(ChatMessage::user(SUMMARY_CONTINUE_MESSAGE));
            return true;
        }
        false
    }

    /// `ToolCallExecutor.HasMissingRequiredArgs`.
    fn has_missing_required_args(&self, calls: &[ToolCall]) -> bool {
        calls.iter().any(|c| {
            let empty = match serde_json::from_str::<Value>(&c.arguments) {
                Ok(Value::Object(o)) => o.is_empty(),
                _ => true,
            };
            empty
                && self.registry.resolve(&c.name).is_some_and(|t| {
                    t.name() == c.name
                        && t.parameter_schema()
                            .get("required")
                            .and_then(Value::as_array)
                            .is_some_and(|r| !r.is_empty())
                })
        })
    }

    /// `ShouldRequestVerification` for `agent.verificationMode`.
    fn should_request_verification(&self) -> bool {
        match self
            .config
            .agent
            .verification_mode
            .trim()
            .to_lowercase()
            .as_str()
        {
            "off" | "never" | "false" => false,
            "always" | "true" => true,
            _ => self.turn_modified_files() && !self.has_recent_test_pass_evidence(),
        }
    }

    fn turn_history(&self) -> &[ToolCallRecord] {
        &self.tool_call_history[self.turn_history_start.min(self.tool_call_history.len())..]
    }

    fn is_successful_mutation(&self, r: &ToolCallRecord) -> bool {
        !r.is_error
            && self
                .registry
                .resolve(&r.name)
                .is_some_and(|t| t.capabilities().contains(ToolCapabilities::MODIFIES_FILES))
    }

    fn turn_modified_files(&self) -> bool {
        self.turn_history()
            .iter()
            .any(|r| self.is_successful_mutation(r))
    }

    /// The latest command run this turn passed and nothing was modified after it. (C# also
    /// keeps the pass when the turn deletes a scratch file it created; no delete tool yet.)
    fn has_recent_test_pass_evidence(&self) -> bool {
        use super::run_evidence::{RunOutcome, classify};
        for r in self.turn_history().iter().rev() {
            if self.is_successful_mutation(r) {
                return false;
            }
            if !matches!(
                r.name.as_str(),
                "bash"
                    | "shell"
                    | "run_command"
                    | "execute_command"
                    | "get_process_output"
                    | "run_tests"
            ) {
                continue;
            }
            match classify(r.result.as_deref(), r.is_error, r.arguments.as_deref()) {
                RunOutcome::Pass => return true,
                RunOutcome::Fail => return false,
                _ => {}
            }
        }
        false
    }

    /// The tool pipeline: resolve → parse → permission → validate → directory restriction →
    /// execute (with timeout) → record → format. Returns the model-visible result.
    pub async fn execute_tool_call(
        &mut self,
        call: &ToolCall,
        observer: &mut dyn AgentObserver,
    ) -> ToolResult {
        let Some(tool) = self.registry.resolve(&call.name) else {
            let hint = self
                .registry
                .find_closest_match(&call.name)
                .map(|s| format!(" Did you mean '{s}'?"))
                .unwrap_or_default();
            return ToolResult::error_with(
                format!(
                    "Unknown tool: {}.{hint} Use only the tools listed in your system prompt.",
                    call.name
                ),
                orca_abstractions::ToolErrorCode::NotFound,
            );
        };

        // Invalid JSON arguments run as `{}` (the required-argument check then explains).
        let args: Value = match serde_json::from_str::<Value>(&call.arguments) {
            Ok(v @ Value::Object(_)) => v,
            _ => Value::Object(serde_json::Map::new()),
        };
        // The record shows the arguments re-serialized (indented, JavaScriptEncoder escaping),
        // except the first of several calls in one response, which C# records compact.
        let args_json = if self.record_raw_arguments {
            call.arguments.clone()
        } else {
            crate::config::dotnet_json::to_string_dotnet(&args).unwrap_or_default()
        };

        if !permissions::check_permission(
            &self.config.permissions,
            tool.name(),
            tool.risk_level(),
            tool.permission_arg_name(),
            &args,
        ) {
            return ToolResult::denied("Permission denied by user.");
        }
        if let Some(err) = validate_required_args(tool.as_ref(), &args) {
            return ToolResult::error_with(err, orca_abstractions::ToolErrorCode::InvalidArguments);
        }
        if let Some(err) = self.check_directory_restriction(tool.as_ref(), &args) {
            return ToolResult::blocked(err);
        }
        if let Some(err) = self.check_read_before_edit(tool.as_ref(), &args) {
            return ToolResult::blocked(err);
        }

        let started = Instant::now();
        let result = self.run_tool(&tool, &args, observer).await;
        let elapsed = started.elapsed().as_millis() as i64;

        if !result.is_error {
            self.track_file_access(tool.as_ref(), &args);
            if tool
                .capabilities()
                .contains(ToolCapabilities::MODIFIES_FILES)
            {
                self.mutation_epoch += 1;
            }
        }
        self.tool_call_history.push(ToolCallRecord {
            name: tool.name().to_string(),
            arguments: Some(args_json),
            result: Some(orca_tools_common_trim(
                &result.content,
                TOOL_RESULT_HISTORY_MAX_CHARS,
            )),
            is_error: result.is_error,
            duration_ms: elapsed,
        });
        if !result.is_error
            && tool
                .capabilities()
                .contains(ToolCapabilities::MODIFIES_FILES)
        {
            for path in tool.affected_file_paths(&args) {
                if !self
                    .files_modified
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&path))
                {
                    self.files_modified.push(path);
                }
            }
        }

        let content = if tool
            .capabilities()
            .contains(ToolCapabilities::VERBATIM_OUTPUT)
        {
            result.content.clone()
        } else if result.is_error {
            format!("ERROR: {}", result.content)
        } else {
            truncate_tool_output(tool.name(), &result.content)
        };
        ToolResult { content, ..result }
    }

    async fn run_tool(
        &self,
        tool: &Arc<dyn Tool>,
        args: &Value,
        observer: &mut dyn AgentObserver,
    ) -> ToolResult {
        let long_running = tool.capabilities().contains(ToolCapabilities::LONG_RUNNING);
        // Lines stream through a channel so the observer (not Sync) stays on this task.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let on_output = move |line: &str| {
            let _ = tx.send(line.to_string());
        };
        let exec = tool.execute_streaming(args, &self.ctx, &on_output);
        let result = if long_running {
            exec.await
        } else {
            match tokio::time::timeout(Duration::from_secs(TOOL_EXECUTION_TIMEOUT_SECONDS), exec)
                .await
            {
                Ok(r) => r,
                Err(_) => ToolResult::error(format!(
                    "Error: Tool '{}' timed out after {TOOL_EXECUTION_TIMEOUT_SECONDS} seconds.",
                    tool.name()
                )),
            }
        };
        while let Ok(line) = rx.try_recv() {
            observer.on_tool_output(&line);
        }
        result
    }

    /// `CheckReadBeforeEdit`: a content-modifying tool may not touch an existing file that was
    /// not read this session, or that changed on disk since it was read.
    fn check_read_before_edit(&self, tool: &dyn Tool, args: &Value) -> Option<String> {
        let caps = tool.capabilities();
        let content_modifying = caps.contains(ToolCapabilities::MODIFIES_FILES)
            && (caps.contains(ToolCapabilities::MODIFIES_FILE_CONTENT)
                || ["write_file", "edit_file", "multi_edit", "apply_patch"]
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(tool.name())));
        if !content_modifying {
            return None;
        }
        for path in tool.affected_file_paths(args) {
            let full = self.ctx.full_path(&path);
            if !full.is_file() {
                continue;
            }
            let Some(read_mtime) = self.files_read.get(&full) else {
                return Some(format!(
                    "ERROR: '{path}' exists but has not been read this session. Use read_file first so the edit is based on the file's actual content."
                ));
            };
            let now = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
            if now != *read_mtime {
                return Some(format!(
                    "ERROR: '{path}' has changed on disk since it was read. Use read_file again before editing to avoid clobbering the newer content."
                ));
            }
        }
        None
    }

    /// `TrackFileAccess`: remember what was read, and refresh the mtime after a write.
    fn track_file_access(&mut self, tool: &dyn Tool, args: &Value) {
        let caps = tool.capabilities();
        let reads = tool.name().eq_ignore_ascii_case("read_file")
            || caps.contains(ToolCapabilities::READS_FILE_CONTENT);
        if !reads && !caps.contains(ToolCapabilities::MODIFIES_FILES) {
            return;
        }
        for path in tool.affected_file_paths(args) {
            let full = self.ctx.full_path(&path);
            if full.is_file() {
                let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
                self.files_read.insert(full, mtime);
            }
        }
    }

    fn check_directory_restriction(&self, tool: &dyn Tool, args: &Value) -> Option<String> {
        let allowed = self.config.allowed_directory.as_deref()?;
        if !tool
            .capabilities()
            .contains(ToolCapabilities::DIRECTORY_RESTRICTED)
        {
            return None;
        }
        let path = args
            .get("path")
            .or_else(|| args.get("directory"))
            .and_then(Value::as_str)?;
        let full = self.ctx.full_path(path);
        let allowed_full = self.ctx.full_path(allowed);
        let full_s = full.to_string_lossy().to_lowercase();
        let allowed_s = allowed_full
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_lowercase();
        if full_s == allowed_s
            || full_s.starts_with(&format!("{allowed_s}/"))
            || full_s.starts_with(&format!("{allowed_s}\\"))
        {
            return None;
        }
        Some(format!(
            "ERROR: Path '{path}' is outside the allowed directory '{allowed}'. File operations are restricted."
        ))
    }
}

fn orca_tools_common_trim(content: &str, max: usize) -> String {
    keep_head_and_tail(content, max, None)
}

/// `ValidateRequiredArgs`: the missing required arguments, with the compact signature.
pub fn validate_required_args(tool: &dyn Tool, args: &Value) -> Option<String> {
    let schema = tool.parameter_schema();
    let required = schema.get("required")?.as_array()?;
    let missing: Vec<&str> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|r| args.get(*r).is_none())
        .collect();
    if missing.is_empty() {
        return None;
    }
    let name = tool.name();
    Some(format!(
        "ERROR: {name} was called with missing required arguments: {}. Signature: {name}({}) You provided: {} Try again with the correct arguments.",
        missing.join(", "),
        prompts::format_parameters(&schema),
        serde_json::to_string(args).unwrap_or_default()
    ))
}

/// `ToolOutputTruncator.Truncate`: over the cap, the full output is spilled to a temp file and
/// the model gets head + tail with a pointer to it.
pub fn truncate_tool_output(tool_name: &str, content: &str) -> String {
    if content.chars().count() <= TOOL_RESULT_MAX_CHARS {
        return content.to_string();
    }
    let dir = std::env::temp_dir().join("openorca_output");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{tool_name}_{stamp}_{:08x}.txt", rand_suffix()));
    let note = match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, content)) {
        Ok(()) => format!(
            "full output: {} (use read_file with offset/limit or grep to see the rest)",
            path.display()
        ),
        Err(_) => "use more specific arguments to reduce output size".to_string(),
    };
    keep_head_and_tail(content, TOOL_RESULT_MAX_CHARS, Some(&note))
}

fn rand_suffix() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    h.finish() as u32
}
