//! The Codex bridge (`CodexBridgeEngine`): the CLI's app server (`codex app-server`, JSON-RPC
//! over stdio), one process and one thread per session, `turn/start` per turn, notifications
//! mapped by [`CodexStreamMapper`]. Command and patch approvals arrive as server requests and
//! are answered by the sink; OpenOrca's host tools (ask_user) are Codex dynamic tools, and
//! Codex's own request_user_input goes to ask_user too. Cancel sends `turn/interrupt`; if the
//! turn doesn't end within the grace period the tree is killed and the next turn resumes the
//! thread in a new process.

mod mapper;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

pub use mapper::{CodexStreamMapper, display_command, file_change_call};

use super::bridge_tools;
use super::cli_locator::{self, Version};
use super::json::{prop, raw_text, str_of, to_compact_dotnet};
use super::readiness::CODEX_MINIMUM_VERSION;
use super::transport::{
    BridgeTransport, BridgeTransportFactory, ProcessSpec, current_environment, env, process_factory,
};
use super::{
    AgentEngine, AgentEngineEvent, AgentEngines, AgentEventSink, AgentTurn, AgentTurnMode,
    CancelToken, EngineError, EngineToolRequest, EngineTurnCompleted, HostToolResult, HostToolSpec,
    approve_with, exit_message, host_tool_with,
};
use crate::config::model::BridgeCliConfig;

pub const MINIMUM_VERSION: Version = CODEX_MINIMUM_VERSION;

/// Environment variables never passed on: a Codex that launched OpenOrca marks its shells.
pub const STRIPPED_ENV: [&str; 3] = [
    "CODEX_THREAD_ID",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

/// Developer instructions added to the thread when ask_user is offered.
pub const ASK_USER_INSTRUCTIONS: &str = "When you need the user to decide or clarify something, call the ask_user tool (question plus 2-10 options) and wait for the answer.";

/// Prefixed to an /ask turn's message (Codex has no per-turn switch for tools).
pub const ASK_MODE_PREFIX: &str = "[Answer directly from the conversation and what you know. Do not run commands, read files or change anything in this turn.]\n\n";

/// Settings for one Codex bridge session (`CodexBridgeOptions`).
#[derive(Debug, Clone)]
pub struct CodexBridgeOptions {
    pub executable_path: Option<String>,
    pub working_directory: String,
    pub model: Option<String>,
    /// A Codex thread to continue (thread/resume).
    pub resume_thread_id: Option<String>,
    /// Extra arguments after `codex app-server`.
    pub extra_args: Vec<String>,
    pub pass_api_key: bool,
    /// approvalPolicy "untrusted" and the workspace-write sandbox, so OpenOrca's rules decide.
    pub isolate_settings: bool,
    /// OpenOrca tools offered as Codex dynamic tools.
    pub host_tools: Vec<HostToolSpec>,
    pub progress_session: Option<String>,
    pub interrupt_grace: Duration,
    pub startup_timeout: Duration,
    pub skip_version_check: bool,
}

impl Default for CodexBridgeOptions {
    fn default() -> Self {
        Self {
            executable_path: None,
            working_directory: std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default(),
            model: None,
            resume_thread_id: None,
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

impl CodexBridgeOptions {
    pub fn from_config(
        config: &BridgeCliConfig,
        working_directory: &str,
        resume_thread_id: Option<&str>,
        progress_session: Option<&str>,
    ) -> Self {
        Self {
            executable_path: config.path.clone(),
            working_directory: working_directory.to_string(),
            model: config.model.clone(),
            resume_thread_id: resume_thread_id.map(str::to_string),
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

/// The CLI's environment: OpenOrca's, without API keys (unless configured) and nested-session
/// markers.
pub fn apply_environment(
    environment: &mut HashMap<String, String>,
    pass_api_key: bool,
    progress_session: Option<&str>,
) {
    for name in STRIPPED_ENV {
        env::remove(environment, name);
    }
    if !pass_api_key {
        env::remove(environment, "OPENAI_API_KEY");
        env::remove(environment, "CODEX_API_KEY");
    }
    env::remove(environment, "OPENORCA_ANTHROPIC_KEY");
    if let Some(p) = progress_session.filter(|p| !p.is_empty()) {
        env::set(environment, "PROGRESS_SESSION", p);
    }
}

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
    let Some(exe) = cli_locator::find("codex", configured_path, None) else {
        return Err(EngineError::Unavailable(
            match configured_path.filter(|p| !p.is_empty()) {
                Some(p) => format!("Codex CLI not found at bridges.codex.path = {p}."),
                None => "Codex CLI (`codex`) not found on PATH. Install it (npm install -g @openai/codex) and log in, or set bridges.codex.path; /engine native switches back.".into(),
            },
        ));
    };
    cli_locator::check_version_async(
        &exe,
        "Codex",
        MINIMUM_VERSION,
        "Update it with `npm install -g @openai/codex`.",
        cancel,
    )
    .await?;
    Ok(exe)
}

enum Step {
    Line(Option<String>),
    Cancelled,
    TimedOut,
}

/// Why starting failed inside the startup sequence.
enum StartError {
    /// `CodexRpcException`: the server answered a request with an error.
    Rpc(String),
    /// The startup time limit passed.
    TimedOut,
    Engine(EngineError),
}

impl From<EngineError> for StartError {
    fn from(e: EngineError) -> Self {
        StartError::Engine(e)
    }
}

pub struct CodexBridgeEngine {
    options: CodexBridgeOptions,
    factory: BridgeTransportFactory,
    mapper: CodexStreamMapper,
    transport: Option<Box<dyn BridgeTransport>>,
    executable: Option<String>,
    ids: i64,
    thread_id: Option<String>,
    model: Option<String>,
}

impl CodexBridgeEngine {
    pub fn new(options: CodexBridgeOptions, factory: Option<BridgeTransportFactory>) -> Self {
        let mapper = CodexStreamMapper::new(Some(&options.working_directory));
        let thread_id = options.resume_thread_id.clone();
        Self {
            options,
            factory: factory.unwrap_or_else(process_factory),
            mapper,
            transport: None,
            executable: None,
            ids: 0,
            thread_id,
            model: None,
        }
    }

    pub fn build_arguments(&self) -> Vec<String> {
        let mut args = vec!["app-server".to_string()];
        args.extend(self.options.extra_args.iter().cloned());
        args
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

    async fn write(&mut self, message: Value) -> Result<(), EngineError> {
        let Some(t) = self.transport.as_mut() else {
            return Err(EngineError::Failed(
                "The bridge process is not running.".into(),
            ));
        };
        t.write_line(&to_compact_dotnet(&message))
            .await
            .map_err(|e| EngineError::Failed(e.to_string()))
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<i64, EngineError> {
        self.ids += 1;
        let id = self.ids;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(id)
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

    /// A request whose answer is awaited here (startup only: nothing else is in flight).
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        cancel: &CancelToken,
        deadline: tokio::time::Instant,
    ) -> Result<Value, StartError> {
        let id = self.send_request(method, params).await?;
        loop {
            let line = match self.read_step(Some(cancel), Some(deadline)).await {
                Step::Cancelled => return Err(EngineError::Cancelled.into()),
                Step::TimedOut => return Err(StartError::TimedOut),
                Step::Line(None) => {
                    return Err(EngineError::Unavailable(
                        self.exit_message("Codex exited while starting"),
                    )
                    .into());
                }
                Step::Line(Some(line)) => line,
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if str_of(&msg, "method").is_none()
                && prop(&msg, "id").and_then(Value::as_i64) == Some(id)
            {
                if let Some(err) = prop(&msg, "error") {
                    return Err(StartError::Rpc(
                        str_of(err, "message").map_or_else(|| raw_text(err), str::to_string),
                    ));
                }
                return Ok(prop(&msg, "result").cloned().unwrap_or(Value::Null));
            }
            // Startup notifications (MCP servers starting, account, ...): nothing to show.
        }
    }

    fn thread_params(&self) -> Map<String, Value> {
        let mut p = Map::new();
        p.insert("cwd".into(), self.options.working_directory.clone().into());
        if let Some(m) = self.options.model.as_ref().filter(|m| !m.trim().is_empty()) {
            p.insert("model".into(), m.clone().into());
        }
        if self.options.isolate_settings {
            p.insert("approvalPolicy".into(), "untrusted".into());
            p.insert("sandbox".into(), "workspace-write".into());
        }
        if !self.options.host_tools.is_empty() {
            let tools: Vec<Value> = self
                .options
                .host_tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            p.insert("dynamicTools".into(), Value::Array(tools));
            if self.options.host_tools.iter().any(|t| t.name == "ask_user") {
                p.insert("developerInstructions".into(), ASK_USER_INSTRUCTIONS.into());
            }
        }
        p
    }

    async fn start_sequence(
        &mut self,
        cancel: &CancelToken,
        deadline: tokio::time::Instant,
    ) -> Result<(), StartError> {
        self.call(
            "initialize",
            json!({
                "clientInfo": {"name": "openorca", "title": "OpenOrca", "version": "1"},
                // Dynamic tools and collaboration modes are experimental API.
                "capabilities": {"experimentalApi": true, "requestAttestation": false},
            }),
            cancel,
            deadline,
        )
        .await?;
        self.write(json!({"jsonrpc": "2.0", "method": "initialized"}))
            .await?;

        let mut thread = self.thread_params();
        let result = match self.thread_id.clone().filter(|t| !t.is_empty()) {
            Some(resume) => {
                thread.insert("threadId".into(), resume.into());
                thread.shift_remove("dynamicTools"); // kept with the thread; resume can't set them
                thread.insert("excludeTurns".into(), true.into());
                self.call("thread/resume", Value::Object(thread), cancel, deadline)
                    .await?
            }
            None => {
                self.call("thread/start", Value::Object(thread), cancel, deadline)
                    .await?
            }
        };
        if let Some(t) = prop(&result, "thread")
            && let Some(id) = str_of(t, "id")
        {
            self.thread_id = Some(id.to_string());
        }
        if let Some(m) = str_of(&result, "model") {
            self.model = Some(m.to_string());
        }
        if self.thread_id.is_none() {
            return Err(EngineError::Unavailable("Codex did not start a thread.".into()).into());
        }
        Ok(())
    }

    async fn ensure_started(&mut self, cancel: &CancelToken) -> Result<(), EngineError> {
        let alive = match self.transport.as_mut() {
            Some(t) => !t.has_exited(),
            None => false,
        };
        if alive && self.thread_id.is_some() {
            return Ok(());
        }
        self.dispose_transport().await;

        let mut spec = if self.options.skip_version_check {
            ProcessSpec::new(
                self.options
                    .executable_path
                    .clone()
                    .unwrap_or_else(|| "codex".into()),
                self.build_arguments(),
            )
        } else {
            if self.executable.is_none() {
                self.executable =
                    Some(locate(self.options.executable_path.as_deref(), cancel).await?);
            }
            let exe = self.executable.clone().unwrap_or_default();
            cli_locator::start_spec(&exe, &self.build_arguments())
        };
        spec.working_directory = Some(self.options.working_directory.clone());
        spec.environment = Some(effective_environment(
            self.options.pass_api_key,
            self.options.progress_session.as_deref(),
        ));
        tracing::info!(
            "Starting Codex bridge: {} {}",
            spec.file_name,
            spec.arguments.join(" ")
        );
        match (self.factory)(&spec) {
            Ok(t) => self.transport = Some(t),
            Err(e) => {
                return Err(EngineError::Unavailable(format!(
                    "Could not start Codex ({}): {e}",
                    spec.file_name
                )));
            }
        }

        let deadline = tokio::time::Instant::now() + self.options.startup_timeout;
        match self.start_sequence(cancel, deadline).await {
            Ok(()) => Ok(()),
            Err(StartError::TimedOut) => {
                self.kill_transport();
                Err(EngineError::Unavailable(format!(
                    "Codex did not answer within {:.0} s. Is it logged in? Run `codex login status`.",
                    self.options.startup_timeout.as_secs_f64()
                )))
            }
            Err(StartError::Engine(e)) => Err(e),
            Err(StartError::Rpc(m)) => {
                self.dispose_transport().await;
                Err(EngineError::Unavailable(format!(
                    "Codex refused the bridge: {m}"
                )))
            }
        }
    }

    async fn answer_server_request(
        &mut self,
        method: &str,
        id: &Value,
        msg: &Value,
        sink: &mut dyn AgentEventSink,
        mode: AgentTurnMode,
        cancel: Option<&CancelToken>,
    ) -> Result<(), EngineError> {
        let null = Value::Null;
        let p = prop(msg, "params").unwrap_or(&null);
        let mut result: Option<Value> = None;
        let mut error: Option<String> = None;
        match method {
            "item/commandExecution/requestApproval" => {
                let item_id = str_of(p, "itemId").map(str::to_string);
                let args = match item_id.as_deref().and_then(|i| self.mapper.call_of(i)) {
                    Some((_, args)) => args.clone(),
                    None => {
                        let mut o = Map::new();
                        o.insert("command".into(), display_command(p).into());
                        to_compact_dotnet(&Value::Object(o))
                    }
                };
                let allowed = mode != AgentTurnMode::Ask
                    && approve_with(
                        sink,
                        EngineToolRequest {
                            name: "bash".into(),
                            original_name: "shell".into(),
                            args_json: with_reason(&args, p),
                            risk_level: "Dangerous".into(),
                            tool_call_id: item_id,
                        },
                        cancel,
                    )
                    .await;
                result = Some(json!({"decision": if allowed { "accept" } else { "decline" }}));
            }
            "item/fileChange/requestApproval" => {
                let item_id = str_of(p, "itemId").map(str::to_string);
                let (name, original, args) = match item_id
                    .as_deref()
                    .and_then(|i| self.mapper.file_changes_of(i))
                {
                    Some(c) => file_change_call(c, Some(&self.options.working_directory)),
                    None => ("edit_file".into(), "apply_patch".into(), "{}".into()),
                };
                let allowed = mode == AgentTurnMode::Normal
                    && approve_with(
                        sink,
                        EngineToolRequest {
                            risk_level: bridge_tools::risk_of(&name).into(),
                            name,
                            original_name: original,
                            args_json: with_reason(&args, p),
                            tool_call_id: item_id,
                        },
                        cancel,
                    )
                    .await;
                result = Some(json!({"decision": if allowed { "accept" } else { "decline" }}));
            }
            "item/tool/call" => {
                let tool = str_of(p, "tool").unwrap_or("").to_string();
                let mut args = prop(p, "arguments").map_or_else(|| "{}".to_string(), raw_text);
                if tool == "ask_user" {
                    args = bridge_tools::normalize_ask_user_args(&args);
                }
                let outcome = if self.options.host_tools.iter().any(|t| t.name == tool) {
                    host_tool_with(sink, &tool, &args, cancel).await
                } else {
                    HostToolResult::error(format!("Unknown tool: {tool}"))
                };
                result = Some(json!({
                    "contentItems": [{"type": "inputText", "text": outcome.text}],
                    "success": !outcome.is_error,
                }));
            }
            "item/tool/requestUserInput" => {
                // Codex's own question tool: each question goes through OpenOrca's ask_user.
                let questions = prop(p, "questions").and_then(Value::as_array);
                match questions {
                    Some(qs) if self.options.host_tools.iter().any(|t| t.name == "ask_user") => {
                        let mut answers = Map::new();
                        for q in qs {
                            let options: Vec<Value> = prop(q, "options")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|o| str_of(o, "label").filter(|l| !l.is_empty()))
                                .map(|l| Value::String(l.to_string()))
                                .collect();
                            let question = str_of(q, "question")
                                .or_else(|| str_of(q, "header"))
                                .unwrap_or("")
                                .to_string();
                            let mut a = Map::new();
                            a.insert("question".into(), question.clone().into());
                            a.insert("options".into(), Value::Array(options));
                            let outcome = host_tool_with(
                                sink,
                                "ask_user",
                                &to_compact_dotnet(&Value::Object(a)),
                                cancel,
                            )
                            .await;
                            let key = str_of(q, "id").map_or(question, str::to_string);
                            answers.insert(key, json!({"answers": [outcome.text]}));
                        }
                        result = Some(json!({"answers": answers}));
                    }
                    _ => error = Some("OpenOrca has no ask_user here".into()),
                }
            }
            // Permission-profile grants, MCP elicitations, legacy v1 approvals: not offered.
            _ => error = Some(format!("OpenOrca's bridge does not handle '{method}'")),
        }

        let mut response = Map::new();
        response.insert("jsonrpc".into(), "2.0".into());
        response.insert("id".into(), id.clone());
        match result {
            Some(r) => {
                response.insert("result".into(), r);
            }
            None => {
                response.insert(
                    "error".into(),
                    json!({"code": -32601, "message": error.unwrap_or_else(|| "unsupported".into())}),
                );
            }
        }
        self.write(Value::Object(response)).await
    }

    async fn send_interrupt(&mut self) {
        let Some(turn_id) = self.mapper.turn_id.clone() else {
            self.kill_transport();
            return;
        };
        let params = json!({"threadId": self.thread_id, "turnId": turn_id});
        if self.send_request("turn/interrupt", params).await.is_err() {
            self.kill_transport();
        }
    }
}

fn with_reason(args_json: &str, p: &Value) -> String {
    let Some(reason) = str_of(p, "reason").filter(|r| !r.is_empty()) else {
        return args_json.to_string();
    };
    let mut obj = match serde_json::from_str::<Value>(args_json) {
        Ok(Value::Object(o)) => o,
        _ => Map::new(),
    };
    obj.insert("_reason".into(), reason.into());
    to_compact_dotnet(&Value::Object(obj))
}

#[async_trait]
impl AgentEngine for CodexBridgeEngine {
    fn id(&self) -> &'static str {
        AgentEngines::CODEX
    }

    fn display_name(&self) -> &'static str {
        AgentEngines::display_name_of(AgentEngines::CODEX)
    }

    fn uses_slots(&self) -> bool {
        false
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    async fn run_turn(
        &mut self,
        turn: AgentTurn<'_>,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        self.ensure_started(cancel).await?;
        let AgentTurn {
            conversation,
            user_text,
            events: sink,
            mode,
        } = turn;
        self.mapper.begin_turn();
        sink.on_event(AgentEngineEvent::SessionStarted {
            session_id: self.thread_id.clone(),
            model: self.model.clone(),
            working_directory: Some(self.options.working_directory.clone()),
        });

        let text = if mode == AgentTurnMode::Ask {
            format!("{ASK_MODE_PREFIX}{user_text}")
        } else {
            user_text
        };
        let mut start = Map::new();
        start.insert("threadId".into(), json!(self.thread_id));
        start.insert(
            "input".into(),
            json!([{"type": "text", "text": text, "text_elements": []}]),
        );
        if let Some(model) = self.model.clone().filter(|m| !m.is_empty()) {
            // Plan mode is Codex's "plan" collaboration mode; every other turn sets "default" back.
            start.insert(
                "collaborationMode".into(),
                json!({
                    "mode": if mode == AgentTurnMode::Plan { "plan" } else { "default" },
                    "settings": {"model": model, "reasoning_effort": null, "developer_instructions": null},
                }),
            );
        }
        let start_id = self
            .send_request("turn/start", Value::Object(start))
            .await?;

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
                        "Codex did not stop within {:?}; killing its process tree",
                        self.options.interrupt_grace
                    );
                    self.kill_transport();
                    break;
                }
                Step::Line(None) => {
                    let message = self.exit_message("Codex exited mid-turn");
                    self.dispose_transport().await;
                    if interrupted {
                        break;
                    }
                    return Err(EngineError::Failed(message));
                }
                Step::Line(Some(line)) => line,
            };

            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                tracing::debug!("Codex non-JSON line: {line}");
                continue;
            };
            let method = str_of(&msg, "method").map(str::to_string);
            let id = prop(&msg, "id").cloned();
            let Some(method) = method else {
                // A response to one of our requests.
                if id.as_ref().and_then(Value::as_i64) == Some(start_id) {
                    if let Some(err) = prop(&msg, "error") {
                        return Err(EngineError::Failed(format!(
                            "Codex refused the turn: {}",
                            str_of(err, "message").unwrap_or("error")
                        )));
                    }
                    if let Some(t) = prop(&msg, "result").and_then(|r| prop(r, "turn"))
                        && self.mapper.turn_id.is_none()
                    {
                        self.mapper.turn_id = str_of(t, "id").map(str::to_string);
                    }
                }
                continue;
            };
            if let Some(id) = id {
                let ct = if interrupted { None } else { Some(cancel) };
                self.answer_server_request(&method, &id, &msg, &mut *sink, mode, ct)
                    .await?;
                continue;
            }
            for e in self.mapper.map(&msg) {
                if let AgentEngineEvent::TurnCompleted(mut done) = e {
                    if interrupted && done.outcome != "cancelled" {
                        done.outcome = "cancelled".into();
                    }
                    sink.on_event(AgentEngineEvent::TurnCompleted(done.clone()));
                    completed = Some(done);
                } else {
                    sink.on_event(e);
                }
            }
        }

        let answer = self.mapper.turn_text().to_string();
        if !answer.is_empty() {
            conversation.add_assistant_message(answer);
        }
        conversation.engine_id = Some(AgentEngines::CODEX.into());
        conversation.engine_session_id = self.thread_id.clone();

        if interrupted {
            if completed.is_none() {
                sink.on_event(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
                    outcome: "cancelled".into(),
                    detail: Some("killed".into()),
                    session_id: self.thread_id.clone(),
                    ..Default::default()
                }));
            }
            return Err(EngineError::Cancelled);
        }
        if let Some(c) = completed.filter(|c| c.outcome == "error") {
            return Err(EngineError::Failed(format!(
                "Codex: {}",
                c.detail.as_deref().unwrap_or("the turn failed")
            )));
        }
        Ok(())
    }

    async fn dispose(&mut self) {
        self.dispose_transport().await;
    }
}
