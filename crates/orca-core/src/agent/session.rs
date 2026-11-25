//! One agent and its conversation: the model loop (`AgentLoopRunner`, simplified) and the tool
//! pipeline (`ToolExecutionPipeline`).
//!
//! Phase 1 covers one agent, native tool calls and the summary wind-down. Not yet ported (see
//! `PORTING.md`): text-mode tool calls, parameter alias repair, the verification request, the
//! runaway-reasoning cap, compaction, retries/failover, hooks, checkpoints, read-before-edit
//! and sub-agents.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use orca_abstractions::{Tool, ToolCapabilities, ToolContext, ToolResult};
use orca_tools_common::keep_head_and_tail;

use super::permissions;
use super::prompts;
use super::registry::ToolRegistry;
use super::system_prompt::{PromptTool, SystemPromptBuilder};
use crate::client::{
    ChatError, ChatMessage, ChatOptions, OpenAiClient, ToolCall, request_parameters,
};
use crate::config::OrcaConfig;

/// `CliConstants.ToolResultMaxChars`: model-visible tool output cap.
pub const TOOL_RESULT_MAX_CHARS: usize = 30_000;
/// `CliConstants.ToolResultHistoryMaxChars`: cap for the recorded history copy.
pub const TOOL_RESULT_HISTORY_MAX_CHARS: usize = 10_000;
/// `CliConstants.HttpProbeTimeoutSeconds`.
pub const HTTP_PROBE_TIMEOUT_SECONDS: u64 = 15;
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
    /// A one-line status message (retries, nudges, loop guards) the console shows in yellow.
    fn on_notice(&mut self, _message: &str) {}
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
    /// Advertise tools via native function calling (resolved at the start of every turn).
    pub native_tools: bool,
    /// Plan mode: only read-only tools are advertised and run.
    pub plan_mode: bool,
    /// Asks the user about calls that need approval (none: they are refused).
    pub prompter: Option<Box<dyn permissions::PermissionPrompter>>,
    /// Scopes approved with "yes to all" this session (lowercase keys).
    pub approved_scopes: std::collections::HashSet<String>,
    /// Builds the system prompt (`SystemPromptBuilder`); also remembers which prompts it built
    /// for which tool-calling mode.
    pub prompt_builder: Arc<SystemPromptBuilder>,

    pub tool_call_history: Vec<ToolCallRecord>,
    /// Paths changed by successful file-modifying calls, as the model passed them
    /// (case-insensitive set in C#; insertion order kept here).
    pub files_modified: Vec<String>,
    pub total_output_tokens: i64,
    /// Text chunks of the last streamed reply (token fallback when the server sends no usage).
    pub(super) last_chunk_count: i64,
    pub last_assistant_response: Option<String>,
    /// Files read (or written) this session with their mtime then: read-before-edit.
    pub files_read: std::collections::HashMap<std::path::PathBuf, Option<std::time::SystemTime>>,
    /// The current call is the first of several in one response: C# records its argument
    /// text compact (the rest indented).
    pub(super) record_raw_arguments: bool,
    /// Image parts of the last tool result, for its tool message (`ToolResultImages`).
    pub(super) last_tool_images: Vec<crate::client::ImagePart>,
    /// Successful file modifications so far (`MutationEpoch`).
    pub mutation_epoch: u64,
    pub(super) turn_history_start: usize,
    /// Identical-failure tracking (`_recentToolErrors`): `tool::args` -> (error, count, epoch).
    pub(super) recent_tool_errors: std::collections::HashMap<String, (String, u32, u64)>,
    /// Calls this turn for doom-loop detection: (name, args, mutation epoch).
    pub(super) recent_call_history: Vec<(String, String, u64)>,
    /// Index into `lmStudio.models` of the model in use (failover moves it on).
    pub model_index: usize,
    /// Models that proved they can't do native tool calls (`NativeToolsSupport`).
    pub native_tools_support: std::collections::HashMap<String, bool>,
    /// Base delay of the same-model retry backoff (2 s; tests shorten it).
    pub retry_base_delay: Duration,
    /// Pause before a failover request (2 s; tests shorten it).
    pub failover_delay: Duration,
    /// The wrap-up a turn ended on when it is not the last response text (verification
    /// changed nothing and the model then answered "Verified.").
    pub final_prose: Option<String>,
    /// Session id, checkpoints, hooks and formatter around each tool call (none by default).
    pub tool_hooks: super::tool_hooks::ToolHooks,

    // ---- context management (agent/context.rs, chat::*) ----
    /// Usage bookkeeping for the compaction estimates (server prompt size, rewrite count).
    pub context_usage: crate::chat::ContextUsage,
    /// Overflow recovery already ran this turn (`AgentLoopState.OverflowRecoveryUsed`).
    pub overflow_recovery_used: bool,
    /// What compaction told the user (console lines in C#); a UI drains it.
    pub compaction_notices: Vec<String>,
    // ---- end context management ----
    /// Run the startup probes (`MaxTokensNegotiator`, context-window detection) before the
    /// first turn. Off by default for library use; the CLI turns it on, as C# always runs them.
    pub startup_probes: bool,
    /// The startup probes already ran this session.
    pub max_tokens_negotiated: bool,
    /// Identifies servers for the context-window probe (the process-wide one by default).
    pub server_identity: crate::client::ServerIdentity,
    /// HTTP client for the startup probes and the server error probe (15 s timeout).
    pub probe_http: reqwest::Client,
    /// Sub-agents (spawn tools, lead mode, delegated checks); None = no agent tools wired.
    pub orchestrator: Option<Arc<crate::orchestration::AgentOrchestrator>>,
    /// The pipeline state shared with sub-agents' tool calls (see `sub_agent_tools`).
    pub sub_agent_pipeline: Option<super::sub_agent_tools::SharedPipeline>,
    /// Called when the Lead would end its turn (`CollectBackgroundWork`): a message to continue
    /// the turn with (peer mode: background helpers' reports it never collected), or None.
    pub collect_background: Option<BackgroundCollector>,
    /// Read-only calls started while the current response streams (agent.earlyReadOnlyTools).
    pub(crate) early_runs: Option<super::early_runs::EarlyRuns>,
    /// Calls whose early run was used this session.
    pub early_tool_runs_used: usize,
    /// Pair model programming vote tallies for the session (`ReplState.Pair*`).
    pub pair_stats: super::pair_vote::PairStats,
    /// This turn's tallies (`AgentLoopState.Pair*`).
    pub(super) turn_pair_stats: super::pair_vote::PairStats,
}

/// See [`AgentSession::collect_background`].
pub type BackgroundCollector =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<String>> + Send + Sync>;

impl AgentSession {
    pub fn new(
        config: OrcaConfig,
        client: OpenAiClient,
        registry: ToolRegistry,
        ctx: ToolContext,
    ) -> Self {
        let prompt_builder = Arc::new(SystemPromptBuilder::with_defaults(&config, &ctx.cwd));
        Self {
            config,
            client,
            registry,
            ctx,
            prompt_builder,
            messages: Vec::new(),
            actor: "lead".into(),
            native_tools: true,
            plan_mode: false,
            prompter: None,
            approved_scopes: Default::default(),
            tool_call_history: Vec::new(),
            files_modified: Vec::new(),
            total_output_tokens: 0,
            last_chunk_count: 0,
            last_assistant_response: None,
            files_read: Default::default(),
            record_raw_arguments: false,
            last_tool_images: Vec::new(),
            mutation_epoch: 0,
            turn_history_start: 0,
            recent_tool_errors: Default::default(),
            recent_call_history: Vec::new(),
            model_index: 0,
            native_tools_support: Default::default(),
            retry_base_delay: Duration::from_secs(2),
            failover_delay: Duration::from_secs(2),
            final_prose: None,
            tool_hooks: Default::default(),
            context_usage: Default::default(),
            overflow_recovery_used: false,
            compaction_notices: Vec::new(),
            startup_probes: false,
            max_tokens_negotiated: false,
            server_identity: crate::client::ServerIdentity::shared(),
            probe_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(HTTP_PROBE_TIMEOUT_SECONDS))
                .build()
                .expect("reqwest client builds with static settings"),
            orchestrator: None,
            sub_agent_pipeline: None,
            collect_background: None,
            early_runs: None,
            early_tool_runs_used: 0,
            pair_stats: Default::default(),
            turn_pair_stats: Default::default(),
        }
    }

    /// Put the system prompt first (replacing an existing one).
    pub fn set_system_prompt(&mut self, prompt: String) {
        match self.messages.first_mut() {
            Some(m) if m.role == crate::client::Role::System => m.content = Some(prompt),
            _ => self.messages.insert(0, ChatMessage::system(prompt)),
        }
    }

    /// The system prompt for this session's tools, directory and config
    /// (`SystemPromptBuilder.GetSystemPromptAsync`); `native_tools` decides the tool-calling
    /// protocol section (text when no tools are registered).
    pub fn default_system_prompt(&self) -> String {
        let mut tools = PromptTool::from_registry(&self.registry);
        tools.retain(|t| super::tool_exposure::is_advertised(&t.name, &self.config));
        let native = self.native_tools && !tools.is_empty();
        self.prompt_builder.get_system_prompt_for_mode(
            &self.config,
            Some(&tools),
            self.plan_mode,
            native,
        )
    }

    /// `SyncSystemPromptToolModeAsync`: rebuild the system prompt when it was built by this
    /// session's builder for the other tool-calling mode (custom prompts are left alone).
    pub fn sync_system_prompt_tool_mode(&mut self) {
        let current = self
            .messages
            .first()
            .filter(|m| m.role == crate::client::Role::System)
            .and_then(|m| m.content.as_deref());
        let built_for = self.prompt_builder.get_prompt_tool_mode(current);
        if built_for.is_none_or(|b| b == self.native_tools) {
            return;
        }
        tracing::info!(
            "Rebuilding system prompt for {} tool calling",
            if self.native_tools { "native" } else { "text" }
        );
        let prompt = self.default_system_prompt();
        self.set_system_prompt(prompt);
    }

    /// The model requests go to: the failover position in `lmStudio.models`, else the
    /// client's default.
    pub fn active_model(&self) -> Option<String> {
        if self.model_index > 0
            && let Some(m) = self.config.lm_studio.models.get(self.model_index)
        {
            return Some(m.clone());
        }
        self.client.settings().default_model.clone()
    }

    pub(super) fn build_options(&self) -> ChatOptions {
        self.build_options_for(false, false)
    }

    /// `BuildChatOptions(useNativeTools, runawayRecovery, skipThinking)`: the cold
    /// (temperature 0.2) recovery attempt after runaway thinking keeps its temperature, and a
    /// step adaptive reasoning skips sends thinking off.
    pub(super) fn build_options_for(
        &self,
        runaway_recovery: bool,
        skip_thinking: bool,
    ) -> ChatOptions {
        let mut options = ChatOptions::default();
        let model = self.active_model();
        if self.model_index > 0 {
            options.model = model.clone();
        }
        if runaway_recovery {
            options.temperature = Some(0.2);
        }
        request_parameters::apply(
            &mut options,
            &self.config,
            model.as_deref(),
            runaway_recovery,
        );
        request_parameters::apply_effort(
            &mut options,
            self.config.agent.reasoning.effort_for(true).as_deref(),
        );
        if skip_thinking {
            request_parameters::skip_thinking(&mut options);
            tracing::debug!("adaptive reasoning: no thinking for this step");
        }
        options.max_output_tokens = self.config.lm_studio.max_tokens;
        if self.native_tools {
            options.tools = self.tools_for_mode();
        }
        options
    }

    /// `GetToolsForMode` + `LeadModePolicy.FilterForLead`: the advertised tools. Plan and
    /// sandbox mode advertise only read-only tools; the orchestrator Lead loses the tools it
    /// may not use.
    pub fn tools_for_mode(&self) -> Vec<crate::client::ToolDefinition> {
        let read_only = self.plan_mode || self.config.sandbox_mode;
        self.registry
            .definitions()
            .into_iter()
            .filter(|d| super::tool_exposure::is_advertised(&d.name, &self.config))
            .filter(|d| !super::policies::is_denied_for_lead(&self.config, &d.name))
            .filter(|d| {
                !read_only
                    || self.registry.resolve(&d.name).is_some_and(|t| {
                        t.risk_level() == orca_abstractions::ToolRiskLevel::ReadOnly
                    })
            })
            .collect()
    }

    /// `ToolCallExecutor.HasMissingRequiredArgs`.
    pub(super) fn has_missing_required_args(&self, calls: &[ToolCall]) -> bool {
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
    pub(super) fn should_request_verification(&self) -> bool {
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

    pub(super) fn turn_history(&self) -> &[ToolCallRecord] {
        &self.tool_call_history[self.turn_history_start.min(self.tool_call_history.len())..]
    }

    pub(super) fn is_successful_mutation(&self, r: &ToolCallRecord) -> bool {
        !r.is_error
            && self
                .registry
                .resolve(&r.name)
                .is_some_and(|t| t.capabilities().contains(ToolCapabilities::MODIFIES_FILES))
    }

    pub fn turn_modified_files(&self) -> bool {
        self.turn_history()
            .iter()
            .any(|r| self.is_successful_mutation(r))
    }

    /// The latest command run this turn passed and nothing was modified after it. (C# also
    /// keeps the pass when the turn deletes a scratch file it created; no delete tool yet.)
    pub fn has_recent_test_pass_evidence(&self) -> bool {
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
        let pi_profile = orca_tools_pi::is_pi_profile(&self.config.tool_profile);
        let Some(tool) = self.registry.resolve(&call.name) else {
            if pi_profile {
                // Pi's agent-loop text.
                return ToolResult::error_with(
                    format!("Tool {} not found", call.name),
                    orca_abstractions::ToolErrorCode::NotFound,
                );
            }
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

        // `IArgumentPreparingTool`: the tool owns repair, coercion and validation (and its
        // error text); hooks and the record see the prepared arguments.
        // Arguments that are not a JSON object reach the tool as `{}` (as the C# client parses them).
        let raw_arguments = match serde_json::from_str::<Value>(&call.arguments) {
            Ok(Value::Object(_)) => call.arguments.as_str(),
            _ => "{}",
        };
        let prepared = match tool.prepare_arguments(raw_arguments) {
            Some(Err(error)) => {
                return ToolResult::error_with(
                    error,
                    orca_abstractions::ToolErrorCode::InvalidArguments,
                );
            }
            Some(Ok(json)) => Some(json),
            None => None,
        };
        let prepares = prepared.is_some();
        let (args, args_json, hook_args) = match prepared {
            Some(json) => (
                serde_json::from_str::<Value>(&json)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                json.clone(),
                json,
            ),
            None => {
                // Invalid JSON arguments run as `{}` (the required-argument check then explains).
                let args: Value = match serde_json::from_str::<Value>(&call.arguments) {
                    Ok(v @ Value::Object(_)) => v,
                    _ => Value::Object(serde_json::Map::new()),
                };
                // The record shows the arguments re-serialized (indented, JavaScriptEncoder
                // escaping), except the first of several calls in one response, which C#
                // records compact.
                let args_json = if self.record_raw_arguments {
                    call.arguments.clone()
                } else {
                    // Numbers keep the model's digits (C# re-serializes JsonElements).
                    match crate::client::types::RawArguments::parse(&call.arguments) {
                        Some(raw) => crate::config::dotnet_json::to_string_dotnet(&raw),
                        None => crate::config::dotnet_json::to_string_dotnet(&args),
                    }
                    .unwrap_or_default()
                };
                (args, args_json, call.arguments.clone())
            }
        };

        if !self.check_permission(tool.as_ref(), &args) {
            return ToolResult::denied("Permission denied by user.");
        }
        // Already started while the model was still writing (agent.earlyReadOnlyTools): its
        // pre-hook ran with it.
        let early = self.take_early_run(call).await;
        // Hooks see the tool name the model used and its argument text.
        let hook_allowed = match &early {
            Some(e) => e.hook_allowed,
            None => self.tool_hooks.pre_hook(&call.name, &hook_args).await,
        };
        if !hook_allowed {
            return ToolResult::blocked("Tool blocked by hook.");
        }
        if !prepares && let Some(err) = validate_required_args(tool.as_ref(), &args) {
            return ToolResult::error_with(err, orca_abstractions::ToolErrorCode::InvalidArguments);
        }
        self.tool_hooks.snapshot(tool.as_ref(), &args, &self.ctx);
        if let Some(err) = self.check_directory_restriction(tool.as_ref(), &args) {
            return ToolResult::blocked(err);
        }
        if let Some(err) = self.check_read_before_edit(tool.as_ref(), &args) {
            return ToolResult::blocked(err);
        }

        // A spawn tool's sub-agents share this pipeline's state; their calls are recorded
        // before the spawn call itself.
        let shared = self
            .sub_agent_pipeline
            .clone()
            .filter(|_| crate::orchestration::activity::is_agent_tool(tool.name()));
        if let Some(shared) = &shared {
            shared.seed_from(self);
        }
        let started = Instant::now();
        let (result, elapsed) = match early {
            Some(e) => (e.result, e.elapsed_ms),
            None => {
                let result = self.run_tool(&tool, &args, observer).await;
                (result, started.elapsed().as_millis() as i64)
            }
        };
        if let Some(shared) = &shared {
            shared.drain_into(self);
        }
        // `cd`: later tool calls resolve paths against the new directory (C# changes the
        // process-wide current directory).
        if let Some(dir) = result.cwd_change().filter(|_| !result.is_error) {
            self.ctx.cwd = dir;
        }
        self.tool_hooks
            .post_hook(&call.name, &hook_args, &result.content, result.is_error);

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
            for path in tool.affected_file_paths(&args, &self.ctx) {
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
            let truncated = truncate_tool_output(tool.name(), &result.content);
            self.tool_hooks
                .append_auto_format(tool.as_ref(), &args, &self.ctx, truncated)
                .await
        };
        ToolResult { content, ..result }
    }

    async fn run_tool(
        &self,
        tool: &Arc<dyn Tool>,
        args: &Value,
        observer: &mut dyn AgentObserver,
    ) -> ToolResult {
        // Interactive tools (ask_user) wait on a human and long-running ones keep their own
        // time budget: neither gets the pipeline timeout.
        let long_running = tool.capabilities().contains(ToolCapabilities::LONG_RUNNING)
            || tool.capabilities().contains(ToolCapabilities::INTERACTIVE);
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

    /// `PermissionManager.CheckPermissionAsync`: the configured rules, then the prompter.
    fn check_permission(&mut self, tool: &dyn Tool, args: &Value) -> bool {
        match permissions::evaluate_permission(
            &self.config.permissions,
            tool.name(),
            tool.risk_level(),
            tool.permission_arg_name(),
            args,
            &self.approved_scopes,
        ) {
            permissions::PermissionVerdict::Allow => true,
            permissions::PermissionVerdict::Deny => false,
            permissions::PermissionVerdict::Ask { scope_key } => {
                // Background agents can't prompt: refused, as without a prompter.
                if permissions::is_non_interactive() {
                    return false;
                }
                let Some(prompter) = self.prompter.as_mut() else {
                    return false;
                };
                match prompter.prompt(tool.name(), permissions::risk_name(tool.risk_level())) {
                    permissions::PermissionDecision::Approved => true,
                    permissions::PermissionDecision::ApproveAll => {
                        self.approved_scopes.insert(scope_key.to_lowercase());
                        true
                    }
                    permissions::PermissionDecision::Denied => false,
                }
            }
        }
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
        let pi_profile = orca_tools_pi::is_pi_profile(&self.config.tool_profile);
        if pi_profile && !self.config.pi.read_before_edit {
            return None;
        }
        let read_tool = if pi_profile { "read" } else { "read_file" };
        for path in tool.affected_file_paths(args, &self.ctx) {
            let full = self.ctx.full_path(&path);
            if !full.is_file() {
                continue;
            }
            let Some(read_mtime) = self.files_read.get(&full) else {
                return Some(format!(
                    "ERROR: '{path}' exists but has not been read this session. Use {read_tool} first so the edit is based on the file's actual content."
                ));
            };
            let now = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
            if now != *read_mtime {
                return Some(format!(
                    "ERROR: '{path}' has changed on disk since it was read. Use {read_tool} again before editing to avoid clobbering the newer content."
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
        for path in tool.affected_file_paths(args, &self.ctx) {
            let full = self.ctx.full_path(&path);
            if full.is_file() {
                let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
                self.files_read.insert(full, mtime);
            }
        }
    }

    fn check_directory_restriction(&self, tool: &dyn Tool, args: &Value) -> Option<String> {
        directory_restriction(
            self.config.allowed_directory.as_deref(),
            &self.ctx,
            tool,
            args,
        )
    }
}

/// `allowedDirectory`: a directory-restricted tool's path must lie inside it.
pub(crate) fn directory_restriction(
    allowed: Option<&str>,
    ctx: &ToolContext,
    tool: &dyn Tool,
    args: &Value,
) -> Option<String> {
    let allowed = allowed?;
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
    let full = ctx.full_path(path);
    let allowed_full = ctx.full_path(allowed);
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
