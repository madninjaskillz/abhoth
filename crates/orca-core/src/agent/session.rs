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
use super::prompts::{
    self, MAX_SUMMARY_CONTINUATIONS, SUMMARY_CONTINUE_MESSAGE, SUMMARY_REQUEST_MESSAGE,
};
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
        let max_iterations = self.config.agent.main_loop_max_iterations.max(1);
        let mut tools_executed = false;
        let mut summary_requested = false;
        let mut summary_continuations = 0;

        for _ in 0..max_iterations {
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
            self.total_output_tokens += response.usage.completion_tokens.unwrap_or(chunks);
            self.last_assistant_response = Some(prompts::strip_next_prompt_tag(&response.text));
            self.messages.push(response.to_message());

            if !response.tool_calls.is_empty() {
                for call in &response.tool_calls {
                    observer.on_tool_start(call);
                    let result = self.execute_tool_call(call, observer).await;
                    observer.on_tool_end(call, &result);
                    self.messages
                        .push(ChatMessage::tool_result(call.id.clone(), result.content));
                }
                tools_executed = true;
                continue;
            }

            // No tool calls: wind down (ToolCallDispatcher.HandleNoToolCallResponse).
            if tools_executed && !summary_requested {
                let prose = prompts::extract_final_prose(&response.text);
                if prompts::is_substantive_final_message(&prose) {
                    return Ok(TurnEnd::Done);
                }
                summary_requested = true;
                self.messages
                    .push(ChatMessage::user(SUMMARY_REQUEST_MESSAGE));
                continue;
            }
            if summary_requested
                && response.finish_reason.as_deref() == Some("length")
                && summary_continuations < MAX_SUMMARY_CONTINUATIONS
            {
                summary_continuations += 1;
                self.messages
                    .push(ChatMessage::user(SUMMARY_CONTINUE_MESSAGE));
                continue;
            }
            return Ok(TurnEnd::Done);
        }
        Ok(TurnEnd::IterationLimit)
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
        let args_json = pretty_args(&args);

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

        let started = Instant::now();
        let result = self.run_tool(&tool, &args, observer).await;
        let elapsed = started.elapsed().as_millis() as i64;

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

/// The arguments as the C# record shows them (indented JSON).
fn pretty_args(args: &Value) -> String {
    let s = serde_json::to_string_pretty(args).unwrap_or_default();
    if cfg!(windows) {
        s.replace('\n', "\r\n")
    } else {
        s
    }
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
