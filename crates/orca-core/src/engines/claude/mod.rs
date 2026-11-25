//! The Claude Code bridge (`ClaudeBridgeEngine`): one long-lived `claude -p --input-format
//! stream-json --output-format stream-json --verbose --include-partial-messages
//! --permission-prompt-tool stdio` process per session, a user message per turn on stdin, the
//! stream mapped by [`ClaudeStreamMapper`]. Tool permission requests arrive as control requests
//! (`can_use_tool`) and are answered by the sink's approval. Cancel sends an `interrupt`
//! control request; if the CLI doesn't end the turn within the grace period its process tree
//! is killed and the next turn starts a new process with `--resume`.

mod mapper;
#[cfg(test)]
mod modes_tests;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

pub use mapper::ClaudeStreamMapper;

use super::bridge_tools::{self, HOST_MCP_PREFIX, HOST_MCP_SERVER};
use super::cli_locator::{self, Version};
use super::json::{prop, raw_text, str_of, to_compact_dotnet};
use super::transport::{
    BridgeTransport, BridgeTransportFactory, ProcessSpec, current_environment, env, process_factory,
};
use super::{
    AgentEngine, AgentEngineEvent, AgentEngines, AgentEventSink, AgentTurn, AgentTurnMode,
    CancelToken, EngineError, EngineToolRequest, EngineTurnCompleted, HostToolResult, HostToolSpec,
    approve_with, exit_message, host_tool_with, paths,
};
use crate::config::model::BridgeCliConfig;

/// Oldest Claude Code with stream-json input and the stdio permission protocol as used here.
pub const MINIMUM_VERSION: Version = Version::new(2, 0, 0);

/// Environment variables never passed to the CLI: nested-session markers of a Claude Code that
/// launched OpenOrca.
pub const STRIPPED_ENV: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
];

/// Ask rules for every built-in tool: Claude Code otherwise runs reads, searches and read-only
/// shell commands without asking, so OpenOrca's deny patterns would never see them.
pub const ISOLATED_SETTINGS_JSON: &str = "{\"permissions\":{\"ask\":[\"Bash\",\"PowerShell\",\"Read\",\"Glob\",\"Grep\",\"LS\",\"WebFetch\",\"WebSearch\",\"Edit\",\"Write\",\"MultiEdit\",\"NotebookEdit\",\"NotebookRead\"]}}";

/// Appended to Claude Code's system prompt when OpenOrca's ask_user is offered.
pub const ASK_USER_PROMPT: &str = "To ask the user a question, use the mcp__openorca__ask_user tool (question plus 2-10 options); AskUserQuestion is not available here.";

/// How to run the Claude Code bridge (`ClaudeBridgeOptions`).
#[derive(Debug, Clone)]
pub struct ClaudeBridgeOptions {
    /// The CLI (`None` = find "claude" on PATH).
    pub executable_path: Option<String>,
    pub working_directory: String,
    pub model: Option<String>,
    /// A Claude Code session to continue (--resume).
    pub resume_session_id: Option<String>,
    pub extra_args: Vec<String>,
    pub pass_api_key: bool,
    /// Load no settings files and add ask rules for every tool, so each tool use reaches
    /// OpenOrca's permission check.
    pub isolate_settings: bool,
    /// OpenOrca tools served to the CLI as the in-process MCP server "openorca".
    pub host_tools: Vec<HostToolSpec>,
    /// PROGRESS_SESSION for the CLI; `None` = leave the env alone.
    pub progress_session: Option<String>,
    /// How long an interrupted turn may take to wind down before the tree is killed.
    pub interrupt_grace: Duration,
    /// How long the CLI may take to answer the initialize request.
    pub startup_timeout: Duration,
    /// Skip the --version check (tests with a fake transport).
    pub skip_version_check: bool,
}

impl Default for ClaudeBridgeOptions {
    fn default() -> Self {
        Self {
            executable_path: None,
            working_directory: std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default(),
            model: None,
            resume_session_id: None,
            extra_args: Vec::new(),
            pass_api_key: false,
            isolate_settings: true,
            host_tools: Vec::new(),
            progress_session: None,
            interrupt_grace: Duration::from_secs(8),
            startup_timeout: Duration::from_secs(60),
            skip_version_check: false,
        }
    }
}

impl ClaudeBridgeOptions {
    pub fn from_config(
        config: &BridgeCliConfig,
        working_directory: &str,
        resume_session_id: Option<&str>,
        progress_session: Option<&str>,
    ) -> Self {
        Self {
            executable_path: config.path.clone(),
            working_directory: working_directory.to_string(),
            model: config.model.clone(),
            resume_session_id: resume_session_id.map(str::to_string),
            extra_args: config.extra_args.clone(),
            pass_api_key: config.pass_api_key,
            isolate_settings: config.isolate_settings,
            progress_session: if config.progress_bars {
                progress_session.map(str::to_string)
            } else {
                None
            },
            ..Default::default()
        }
    }
}

/// Writes [`ISOLATED_SETTINGS_JSON`] to a temp file (once) and returns its path.
pub fn isolated_settings_file() -> String {
    write_temp_once("claude-isolated-settings.json", ISOLATED_SETTINGS_JSON)
}

/// The --mcp-config file that declares OpenOrca's in-process ("sdk") MCP server.
pub fn host_mcp_config_file() -> String {
    write_temp_once(
        "claude-host-mcp.json",
        &format!(
            "{{\"mcpServers\":{{\"{HOST_MCP_SERVER}\":{{\"type\":\"sdk\",\"name\":\"{HOST_MCP_SERVER}\"}}}}}}"
        ),
    )
}

fn write_temp_once(name: &str, content: &str) -> String {
    let dir = std::env::temp_dir().join("openorca-bridge");
    let path = dir.join(name);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(content) {
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            // Unique per call, not just per process: two threads writing one shared temp file
            // could rename it while the other was mid-write and leave `path` empty.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()));
            std::fs::write(&tmp, content)?;
            std::fs::rename(&tmp, &path)
        };
        // Another OpenOrca may write it at the same moment; the content is the same.
        if let Err(e) = write()
            && !path.exists()
        {
            tracing::warn!("could not write {}: {e}", path.display());
        }
    }
    path.to_string_lossy().into_owned()
}

/// The CLI's environment: OpenOrca's, without nested-session markers and (unless
/// `pass_api_key`) without API keys, so the CLI uses its own login.
pub fn apply_environment(
    environment: &mut HashMap<String, String>,
    pass_api_key: bool,
    progress_session: Option<&str>,
) {
    for name in STRIPPED_ENV {
        env::remove(environment, name);
    }
    if pass_api_key {
        if !env::contains(environment, "ANTHROPIC_API_KEY")
            && let Some(key) = env::get(environment, "OPENORCA_ANTHROPIC_KEY")
                .filter(|k| !k.is_empty())
                .map(str::to_string)
        {
            env::set(environment, "ANTHROPIC_API_KEY", &key);
        }
    } else {
        env::remove(environment, "ANTHROPIC_API_KEY");
        env::remove(environment, "ANTHROPIC_AUTH_TOKEN");
    }
    env::remove(environment, "OPENORCA_ANTHROPIC_KEY");
    if let Some(p) = progress_session.filter(|p| !p.is_empty()) {
        env::set(environment, "PROGRESS_SESSION", p);
    }
}

/// The current process's environment, adjusted like a launch.
pub fn effective_environment(
    pass_api_key: bool,
    progress_session: Option<&str>,
) -> HashMap<String, String> {
    let mut e = current_environment();
    apply_environment(&mut e, pass_api_key, progress_session);
    e
}

/// Find and version-check the CLI (`LocateAsync`).
pub async fn locate(
    configured_path: Option<&str>,
    cancel: &CancelToken,
) -> Result<String, EngineError> {
    let Some(exe) = cli_locator::find("claude", configured_path, None) else {
        return Err(EngineError::Unavailable(
            match configured_path.filter(|p| !p.is_empty()) {
                Some(p) => format!("Claude Code CLI not found at bridges.claude.path = {p}."),
                None => "Claude Code CLI (`claude`) not found on PATH. Install Claude Code and log in (run `claude` once), or set bridges.claude.path; /engine native switches back.".into(),
            },
        ));
    };
    cli_locator::check_version_async(
        &exe,
        "Claude Code",
        MINIMUM_VERSION,
        "Update it with `claude update`.",
        cancel,
    )
    .await?;
    Ok(exe)
}

/// Is `path` in Claude Code's plans folder (~/.claude/plans or $CLAUDE_CONFIG_DIR/plans)?
pub fn is_plan_file(path: &str) -> bool {
    let config = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| paths::combine(&user_profile(), ".claude"));
    let plans = format!(
        "{}{}",
        paths::trim_ending_separator(&paths::get_full_path(&paths::combine(&config, "plans"))),
        std::path::MAIN_SEPARATOR
    );
    let full = paths::get_full_path(path);
    if cfg!(windows) {
        full.to_lowercase().starts_with(&plans.to_lowercase())
    } else {
        full.starts_with(&plans)
    }
}

/// `Environment.SpecialFolder.UserProfile`.
pub(crate) fn user_profile() -> String {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).unwrap_or_default()
}

enum Step {
    Line(Option<String>),
    Cancelled,
    TimedOut,
}

pub struct ClaudeBridgeEngine {
    options: ClaudeBridgeOptions,
    factory: BridgeTransportFactory,
    mapper: ClaudeStreamMapper,
    transport: Option<Box<dyn BridgeTransport>>,
    executable: Option<String>,
    request_ids: u64,
    session_id: Option<String>,
    /// The tool profile the running process was started with (Ask = no tools).
    process_profile: AgentTurnMode,
    /// The CLI's permission mode ("default" or "plan").
    permission_mode: &'static str,
    plan_text: Option<String>,
}

impl ClaudeBridgeEngine {
    pub fn new(options: ClaudeBridgeOptions, factory: Option<BridgeTransportFactory>) -> Self {
        let mapper = ClaudeStreamMapper::new(Some(&options.working_directory));
        let session_id = options.resume_session_id.clone();
        Self {
            options,
            factory: factory.unwrap_or_else(process_factory),
            mapper,
            transport: None,
            executable: None,
            request_ids: 0,
            session_id,
            process_profile: AgentTurnMode::Normal,
            permission_mode: "default",
            plan_text: None,
        }
    }

    /// The command line for a new process (without the executable).
    pub fn build_arguments(&self, profile: AgentTurnMode) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(String::from)
        .to_vec();
        if self.options.isolate_settings {
            args.extend([
                "--setting-sources=".into(),
                "--settings".into(),
                isolated_settings_file(),
            ]);
        }
        if profile == AgentTurnMode::Ask {
            // /ask: no built-in tools and no MCP servers; the session carries on via --resume.
            args.extend([
                "--tools".into(),
                String::new(),
                "--strict-mcp-config".into(),
            ]);
        } else if !self.options.host_tools.is_empty() {
            args.extend(["--mcp-config".into(), host_mcp_config_file()]);
            args.push("--allowedTools".into());
            args.push(
                self.options
                    .host_tools
                    .iter()
                    .map(|t| format!("{HOST_MCP_PREFIX}{}", t.name))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            if self.options.host_tools.iter().any(|t| t.name == "ask_user") {
                args.extend([
                    "--disallowedTools".into(),
                    "AskUserQuestion".into(),
                    "--append-system-prompt".into(),
                    ASK_USER_PROMPT.into(),
                ]);
            }
        }
        if let Some(model) = self.options.model.as_ref().filter(|m| !m.trim().is_empty()) {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(sid) = self.session_id.as_ref().filter(|s| !s.trim().is_empty()) {
            args.extend(["--resume".into(), sid.clone()]);
        }
        args.extend(self.options.extra_args.iter().cloned());
        args
    }

    fn next_request_id(&mut self) -> String {
        self.request_ids += 1;
        format!("openorca-{}", self.request_ids)
    }

    async fn send(&mut self, message: Value) -> Result<(), EngineError> {
        let Some(t) = self.transport.as_mut() else {
            return Err(EngineError::Failed(
                "The bridge process is not running.".into(),
            ));
        };
        t.write_line(&to_compact_dotnet(&message))
            .await
            .map_err(|e| EngineError::Failed(e.to_string()))
    }

    /// Sends a control request; its answer arrives in the stream and is skipped there.
    async fn send_control(&mut self, request: Value) -> Result<(), EngineError> {
        let id = self.next_request_id();
        self.send(json!({"type": "control_request", "request_id": id, "request": request}))
            .await
    }

    fn kill_transport(&mut self) {
        if let Some(t) = self.transport.as_mut() {
            t.kill();
        }
    }

    async fn dispose_transport(&mut self) {
        if let Some(mut t) = self.transport.take() {
            t.dispose().await;
        }
    }

    fn exit_message(&mut self, what: &str) -> String {
        exit_message(what, self.transport.as_mut())
    }

    async fn read_step(
        &mut self,
        cancel: Option<&CancelToken>,
        deadline: Option<tokio::time::Instant>,
    ) -> Step {
        let t = self.transport.as_mut().expect("transport is running");
        let never = CancelToken::new();
        let cancel = cancel.unwrap_or(&never);
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Step::Cancelled,
            l = t.read_line() => Step::Line(l),
            _ = sleep => Step::TimedOut,
        }
    }

    async fn ensure_started(
        &mut self,
        profile: AgentTurnMode,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        let alive = match self.transport.as_mut() {
            Some(t) => !t.has_exited(),
            None => false,
        };
        if alive && self.process_profile == profile {
            return Ok(());
        }
        if alive {
            // Another tool profile (/ask turns run without tools): the new one resumes.
            tracing::info!("Restarting Claude Code for {profile:?} turns");
        }
        self.dispose_transport().await;

        let mut spec = if self.options.skip_version_check {
            ProcessSpec::new(
                self.options
                    .executable_path
                    .clone()
                    .unwrap_or_else(|| "claude".into()),
                self.build_arguments(profile),
            )
        } else {
            if self.executable.is_none() {
                self.executable =
                    Some(locate(self.options.executable_path.as_deref(), cancel).await?);
            }
            let exe = self.executable.clone().unwrap_or_default();
            cli_locator::start_spec(&exe, &self.build_arguments(profile))
        };
        spec.working_directory = Some(self.options.working_directory.clone());
        spec.environment = Some(effective_environment(
            self.options.pass_api_key,
            self.options.progress_session.as_deref(),
        ));

        tracing::info!(
            "Starting Claude Code bridge: {} {}",
            spec.file_name,
            spec.arguments.join(" ")
        );
        match (self.factory)(&spec) {
            Ok(t) => self.transport = Some(t),
            Err(e) => {
                return Err(EngineError::Unavailable(format!(
                    "Could not start Claude Code ({}): {e}",
                    spec.file_name
                )));
            }
        }
        self.process_profile = profile;
        self.permission_mode = "default";
        let host_tools = profile != AgentTurnMode::Ask && !self.options.host_tools.is_empty();

        // initialize: the SDK handshake; its answer proves the CLI is up and reading stream-json.
        let init_id = self.next_request_id();
        let request = if host_tools {
            json!({"subtype": "initialize", "sdkMcpServers": [HOST_MCP_SERVER]})
        } else {
            json!({"subtype": "initialize"})
        };
        self.send(json!({"type": "control_request", "request_id": init_id, "request": request}))
            .await?;

        let deadline = tokio::time::Instant::now() + self.options.startup_timeout;
        loop {
            match self.read_step(Some(cancel), Some(deadline)).await {
                Step::Cancelled => return Err(EngineError::Cancelled),
                Step::TimedOut => {
                    self.kill_transport();
                    return Err(EngineError::Unavailable(format!(
                        "Claude Code did not answer within {:.0} s. Is it logged in? Run `claude` once to check.",
                        self.options.startup_timeout.as_secs_f64()
                    )));
                }
                Step::Line(None) => {
                    return Err(EngineError::Unavailable(
                        self.exit_message("Claude Code exited while starting"),
                    ));
                }
                Step::Line(Some(line)) => {
                    let Ok(root) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(error) = control_response_to(&root, &init_id) {
                        if let Some(error) = error {
                            return Err(EngineError::Unavailable(format!(
                                "Claude Code refused the bridge handshake: {error}"
                            )));
                        }
                        return Ok(());
                    }
                    // The MCP handshake with the host server can come first; hook chatter etc. is ignored.
                    if str_of(&root, "type") == Some("control_request") {
                        self.answer_control_request(&root, None, None).await?;
                    }
                }
            }
        }
    }

    async fn answer_control_request(
        &mut self,
        root: &Value,
        mut sink: Option<&mut dyn AgentEventSink>,
        cancel: Option<&CancelToken>,
    ) -> Result<(), EngineError> {
        let request_id = str_of(root, "request_id").unwrap_or("").to_string();
        let null = Value::Null;
        let request = prop(root, "request").unwrap_or(&null);
        let subtype = str_of(request, "subtype");
        let response: Value;
        if subtype == Some("mcp_message")
            && str_of(request, "server_name") == Some(HOST_MCP_SERVER)
            && let Some(mcp_message) = prop(request, "message")
        {
            let mcp = self.handle_mcp(mcp_message, sink, cancel).await;
            response = json!({"mcp_response": mcp});
        } else if subtype == Some("can_use_tool")
            && str_of(request, "tool_name") == Some("ExitPlanMode")
        {
            // Plan mode: the plan is the turn's answer; OpenOrca asks the user to approve it after the turn.
            let plan = prop(request, "input").and_then(|i| str_of(i, "plan"));
            if let Some(plan) = plan.filter(|p| !p.is_empty()) {
                self.plan_text = Some(plan.to_string());
                self.mapper.append_turn_text(plan);
                if let Some(s) = sink.as_deref_mut() {
                    s.on_event(AgentEngineEvent::TextDelta(plan.to_string()));
                }
            }
            response = json!({
                "behavior": "deny",
                "message": "The plan was handed to the user in Abhoth for review. Stop here; do not start implementing.",
            });
        } else if subtype == Some("can_use_tool")
            && let Some(sink) = sink
        {
            let original = str_of(request, "tool_name").unwrap_or("tool").to_string();
            let name = bridge_tools::from_claude(&original);
            let input = prop(request, "input");
            let args = bridge_tools::normalize_args(input, Some(&self.options.working_directory));
            let plan_write = self.permission_mode == "plan"
                && bridge_tools::is_file_write_tool(&name)
                && bridge_tools::file_path_of(input).is_some_and(|f| is_plan_file(&f));
            // Plan mode writes its plan to the CLI's own plans folder: that is the plan, not a change.
            let allowed = plan_write
                || approve_with(
                    sink,
                    EngineToolRequest {
                        risk_level: bridge_tools::risk_of(&name).into(),
                        name,
                        original_name: original,
                        args_json: args,
                        tool_call_id: str_of(request, "tool_use_id").map(str::to_string),
                    },
                    cancel,
                )
                .await;
            response = if allowed {
                json!({
                    "behavior": "allow",
                    "updatedInput": input.filter(|i| i.is_object()).cloned().unwrap_or_else(|| json!({})),
                })
            } else {
                json!({"behavior": "deny", "message": "The user denied this tool call in Abhoth."})
            };
        } else {
            // hook callbacks, other SDK MCP servers: none are registered by the bridge.
            return self
                .send(json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": format!("Abhoth's bridge does not handle '{}'", subtype.unwrap_or("")),
                    },
                }))
                .await;
        }
        self.send(json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }))
        .await
    }

    /// OpenOrca's in-process MCP server: JSON-RPC messages the CLI routes through the control
    /// channel (initialize, tools/list, tools/call). Calls run via the sink.
    async fn handle_mcp(
        &mut self,
        message: &Value,
        sink: Option<&mut dyn AgentEventSink>,
        cancel: Option<&CancelToken>,
    ) -> Value {
        let method = str_of(message, "method");
        let id = match prop(message, "id") {
            Some(v) if !v.is_null() => v.clone(),
            // A notification.
            _ => return json!({"jsonrpc": "2.0", "result": {}, "id": 0}),
        };
        let result = |r: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": r});
        match method {
            Some("initialize") => {
                let version = prop(message, "params").and_then(|p| str_of(p, "protocolVersion"));
                result(json!({
                    "protocolVersion": version.unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": HOST_MCP_SERVER, "version": "1"},
                }))
            }
            Some("tools/list") => {
                let tools: Vec<Value> = self
                    .options
                    .host_tools
                    .iter()
                    .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
                    .collect();
                result(json!({"tools": tools}))
            }
            Some("tools/call") => {
                let null = Value::Null;
                let p = prop(message, "params").unwrap_or(&null);
                let name = str_of(p, "name").unwrap_or("").to_string();
                let mut args = prop(p, "arguments").map_or_else(|| "{}".to_string(), raw_text);
                if name == "ask_user" {
                    args = bridge_tools::normalize_ask_user_args(&args);
                }
                let known = self.options.host_tools.iter().any(|t| t.name == name);
                let outcome = match sink {
                    Some(sink) if known => host_tool_with(sink, &name, &args, cancel).await,
                    _ => HostToolResult::error(format!("Unknown tool: {name}")),
                };
                result(json!({
                    "content": [{"type": "text", "text": outcome.text}],
                    "isError": outcome.is_error,
                }))
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {}", method.unwrap_or(""))},
            }),
        }
    }

    async fn send_interrupt(&mut self) {
        let id = self.next_request_id();
        let sent = self
            .send(json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}}))
            .await;
        if sent.is_err() {
            self.kill_transport();
        }
    }
}

/// `IsControlResponseTo`: `Some(None)` for a success answer to `request_id`, `Some(Some(err))`
/// for an error answer, `None` for anything else.
fn control_response_to(root: &Value, request_id: &str) -> Option<Option<String>> {
    if str_of(root, "type") != Some("control_response") {
        return None;
    }
    let resp = prop(root, "response")?;
    if str_of(resp, "request_id") != Some(request_id) {
        return None;
    }
    if str_of(resp, "subtype") == Some("error") {
        return Some(Some(str_of(resp, "error").unwrap_or("error").to_string()));
    }
    Some(None)
}

#[async_trait]
impl AgentEngine for ClaudeBridgeEngine {
    fn id(&self) -> &'static str {
        AgentEngines::CLAUDE
    }

    fn display_name(&self) -> &'static str {
        AgentEngines::display_name_of(AgentEngines::CLAUDE)
    }

    fn uses_slots(&self) -> bool {
        false
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    async fn run_turn(
        &mut self,
        turn: AgentTurn<'_>,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let profile = if turn.mode == AgentTurnMode::Ask {
            AgentTurnMode::Ask
        } else {
            AgentTurnMode::Normal
        };
        self.ensure_started(profile, cancel).await?;
        let AgentTurn {
            conversation,
            user_text,
            events: sink,
            mode: turn_mode,
        } = turn;
        self.mapper.begin_turn();
        self.plan_text = None;
        let mode = if turn_mode == AgentTurnMode::Plan {
            "plan"
        } else {
            "default"
        };
        if mode != self.permission_mode && turn_mode != AgentTurnMode::Ask {
            self.send_control(json!({"subtype": "set_permission_mode", "mode": mode}))
                .await?;
            self.permission_mode = mode;
        }

        let mut user = Map::new();
        user.insert("type".into(), "user".into());
        user.insert(
            "message".into(),
            json!({"role": "user", "content": user_text}),
        );
        user.insert("parent_tool_use_id".into(), Value::Null);
        user.insert(
            "session_id".into(),
            self.session_id.clone().unwrap_or_default().into(),
        );
        self.send(Value::Object(user)).await?;

        let mut interrupted = false;
        let mut grace: Option<tokio::time::Instant> = None;
        let mut completed: Option<EngineTurnCompleted> = None;
        while completed.is_none() {
            let step = if interrupted {
                self.read_step(None, grace).await
            } else {
                self.read_step(Some(cancel), None).await
            };
            let line = match step {
                Step::Cancelled => {
                    interrupted = true;
                    grace = Some(tokio::time::Instant::now() + self.options.interrupt_grace);
                    self.send_interrupt().await;
                    continue;
                }
                Step::TimedOut => {
                    tracing::warn!(
                        "Claude Code did not stop within {:?}; killing its process tree",
                        self.options.interrupt_grace
                    );
                    self.kill_transport();
                    break;
                }
                Step::Line(None) => {
                    let message = self.exit_message("Claude Code exited mid-turn");
                    self.dispose_transport().await;
                    if interrupted {
                        break;
                    }
                    return Err(EngineError::Failed(message));
                }
                Step::Line(Some(line)) => line,
            };

            let Ok(root) = serde_json::from_str::<Value>(&line) else {
                tracing::debug!("Claude Code non-JSON line: {line}");
                continue;
            };
            let kind = str_of(&root, "type");
            if kind == Some("control_request") {
                let ct = if interrupted { None } else { Some(cancel) };
                self.answer_control_request(&root, Some(&mut *sink), ct)
                    .await?;
                continue;
            }
            if matches!(
                kind,
                Some("control_response" | "control_cancel_request" | "keep_alive")
            ) {
                continue;
            }

            for e in self.mapper.map(&root) {
                if let AgentEngineEvent::SessionStarted {
                    session_id: Some(sid),
                    ..
                } = &e
                    && !sid.is_empty()
                {
                    self.session_id = Some(sid.clone());
                }
                if let AgentEngineEvent::TurnCompleted(done) = e {
                    if let Some(s) = done.session_id.as_ref().filter(|s| !s.is_empty()) {
                        self.session_id = Some(s.clone());
                    }
                    let mut c = done;
                    if interrupted && c.outcome != "cancelled" {
                        c.outcome = "cancelled".into();
                    }
                    if self.plan_text.is_some() {
                        c.final_text = Some(self.mapper.turn_text().to_string());
                    }
                    sink.on_event(AgentEngineEvent::TurnCompleted(c.clone()));
                    completed = Some(c);
                } else {
                    sink.on_event(e);
                }
            }
        }

        // The conversation keeps the answer so the transcript, /export and saved sessions have it.
        let text = self.mapper.turn_text().to_string();
        if !text.is_empty() {
            conversation.add_assistant_message(text);
        }
        conversation.engine_id = Some(AgentEngines::CLAUDE.into());
        conversation.engine_session_id = self.session_id.clone();

        if interrupted {
            if completed.is_none() {
                sink.on_event(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
                    outcome: "cancelled".into(),
                    detail: Some("killed".into()),
                    session_id: self.session_id.clone(),
                    ..Default::default()
                }));
            }
            return Err(EngineError::Cancelled);
        }
        if let Some(c) = completed.filter(|c| c.outcome == "error") {
            return Err(EngineError::Failed(format!(
                "Claude Code: {}",
                c.detail.as_deref().unwrap_or("the turn failed")
            )));
        }
        Ok(())
    }

    async fn dispose(&mut self) {
        self.dispose_transport().await;
    }
}
