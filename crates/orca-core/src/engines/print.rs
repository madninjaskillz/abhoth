//! `--print` / `--prompt` with a bridge engine (`agent.engine` = claude / codex): the parts of
//! `ReplLoop.PrepareEngineAsync`, `EngineHost.RunTurnAsync` and `EngineEventRenderer` that
//! decide what C# reports in single-prompt mode.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use orca_abstractions::{ToolContext, ToolRiskLevel};
use serde_json::Value;

use super::claude::{ClaudeBridgeEngine, ClaudeBridgeOptions};
use super::codex::{CodexBridgeEngine, CodexBridgeOptions};
use super::transport::BridgeTransportFactory;
use super::{
    AgentEngine, AgentEngineEvent, AgentEngines, AgentEventSink, AgentTurn, CancelToken,
    EngineError, EngineReadiness, EngineToolRequest, HostToolResult, HostToolSpec, readiness,
};
use crate::agent::permissions::{PermissionVerdict, evaluate_permission};
use crate::agent::session::TOOL_RESULT_HISTORY_MAX_CHARS;
use crate::agent::{ToolCallRecord, ToolRegistry};
use crate::chat::Conversation;
use crate::client::Role;
use crate::config::{OrcaConfig, PermissionsConfig};

/// OpenOrca tools offered to bridged CLIs (`EngineHost.HostToolNames`).
pub const HOST_TOOL_NAMES: [&str; 1] = ["ask_user"];

/// The selected bridge engine ("claude" / "codex"), `None` for native (or an unknown name,
/// which C# also runs natively).
pub fn selected_bridge(config: &OrcaConfig) -> Option<&'static str> {
    match AgentEngines::normalize(Some(&config.agent.engine)) {
        Some(id @ (AgentEngines::CLAUDE | AgentEngines::CODEX)) => Some(id),
        _ => None,
    }
}

/// `EngineReadinessChecker.CheckAsync(engine, config)`.
pub async fn check_readiness(
    engine: &str,
    config: &OrcaConfig,
    cancel: &CancelToken,
) -> Result<EngineReadiness, EngineError> {
    readiness::check(engine, config, cancel).await
}

/// `ReplLoop.PrepareEngineAsync(interactive: false)`: `Ok(None)` when the turn may run (native,
/// or a ready bridge); `Ok(Some(message))` when the bridge can't run: C# writes the message to
/// stderr and exits with code 2. `Err` is a failure C# lets escape (the check itself failing).
pub async fn prepare_engine_non_interactive(
    config: &OrcaConfig,
    cancel: &CancelToken,
) -> Result<Option<String>, EngineError> {
    let Some(id) = selected_bridge(config) else {
        return Ok(None);
    };
    let r = readiness::check(id, config, cancel).await?;
    Ok((!r.is_ready()).then_some(r.message))
}

/// What `--print` reports after a bridge turn (`SinglePromptResult` fields from `ReplState`).
#[derive(Debug, Clone, PartialEq)]
pub struct PrintTurnOutcome {
    /// `LastAssistantResponse ?? ""`.
    pub response: String,
    /// `TotalOutputTokens`: the turn's reported output tokens.
    pub tokens: i64,
    /// Time spent in the turn (C# measures around `EngineHost.RunTurnAsync`).
    pub duration_ms: i64,
    /// `ToolCallHistory` (null when empty).
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    /// `FilesModified` (null when empty): full paths, case-insensitive set, first spelling kept.
    pub files_modified: Option<Vec<String>>,
    /// `!lastToolError`.
    pub success: bool,
    /// The bridge was unavailable (`AgentEngineUnavailableException`): C# prints
    /// `✗ {message}` (to stderr in JSON mode) and still writes the JSON result.
    pub unavailable: Option<String>,
    /// The CLI's session id after the turn (`conversation.EngineSessionId`).
    pub engine_session_id: Option<String>,
}

/// Inputs for [`run_print_turn`].
pub struct PrintTurnRequest<'a> {
    pub config: &'a OrcaConfig,
    /// The working directory the CLI runs in (C#: the process's current directory).
    pub cwd: &'a Path,
    /// The session's conversation (a restored one for `-c` / `-r`: when its `engine_id` is
    /// the selected engine, the CLI resumes `engine_session_id`).
    pub conversation: &'a mut Conversation,
    /// Added before the prompt as C# does (bridges don't send it).
    pub system_prompt: Option<String>,
    pub prompt: &'a str,
    /// The tool registry: host tools offered to the CLI (ask_user, when registered) and each
    /// tool's permission argument name.
    pub registry: Option<&'a ToolRegistry>,
    /// The OpenOrca session id (the progress bucket name); `None`: a random 8-hex id.
    pub session_id: Option<&'a str>,
    /// JSON mode: nothing is printed while the turn runs.
    pub quiet: bool,
}

/// Run the prompt on the selected bridge (`RunSinglePromptJsonAsync` / `RunSinglePromptAsync`
/// with a bridge engine). `Err` is what C# lets escape (no JSON is written): the turn failed
/// (`Claude Code: …`, `… exited mid-turn`) or was cancelled. Call only when
/// [`selected_bridge`] is `Some`.
pub async fn run_print_turn(
    req: PrintTurnRequest<'_>,
    cancel: &CancelToken,
) -> Result<PrintTurnOutcome, EngineError> {
    run_print_turn_with(req, cancel, None).await
}

pub(crate) async fn run_print_turn_with(
    req: PrintTurnRequest<'_>,
    cancel: &CancelToken,
    test_factory: Option<BridgeTransportFactory>,
) -> Result<PrintTurnOutcome, EngineError> {
    let PrintTurnRequest {
        config,
        cwd,
        conversation,
        system_prompt,
        prompt,
        registry,
        session_id,
        quiet,
    } = req;
    let Some(id) = selected_bridge(config) else {
        return Err(EngineError::Failed(format!(
            "Unknown engine '{}'.",
            config.agent.engine
        )));
    };
    if let Some(system) = system_prompt {
        conversation.add_system_message(system);
    }
    conversation.add_user_message(prompt);
    let user_text = last_user_text(conversation);

    let started = Instant::now();
    let resume = (conversation.engine_id.as_deref() == Some(id))
        .then(|| conversation.engine_session_id.clone())
        .flatten();
    let bucket = session_id.map_or_else(random_bucket_id, str::to_string);
    let progress_session = format!("openorca-{bucket}");
    let cwd_text = cwd.to_string_lossy().into_owned();
    let host_tools = host_tools(registry);
    let skip_version_check = test_factory.is_some();
    let mut engine: Box<dyn AgentEngine> = if id == AgentEngines::CLAUDE {
        let mut o = ClaudeBridgeOptions::from_config(
            &config.bridges.claude,
            &cwd_text,
            resume.as_deref(),
            Some(&progress_session),
        );
        o.host_tools = host_tools;
        o.skip_version_check = skip_version_check;
        Box::new(ClaudeBridgeEngine::new(o, test_factory))
    } else {
        let mut o = CodexBridgeOptions::from_config(
            &config.bridges.codex,
            &cwd_text,
            resume.as_deref(),
            Some(&progress_session),
        );
        o.host_tools = host_tools;
        o.skip_version_check = skip_version_check;
        Box::new(CodexBridgeEngine::new(o, test_factory))
    };

    let mut sink = PrintSink {
        permissions: &config.permissions,
        registry,
        ctx: ToolContext::new(cwd.to_path_buf()),
        quiet,
        open_calls: HashMap::new(),
        answer: String::new(),
        records: Vec::new(),
        files: Vec::new(),
        tokens: 0,
        last_response: None,
    };
    let result = engine
        .run_turn(AgentTurn::new(conversation, user_text, &mut sink), cancel)
        .await;
    engine.dispose().await;
    let unavailable = match result {
        Ok(()) => None,
        Err(EngineError::Unavailable(m)) => Some(m),
        Err(e) => return Err(e),
    };
    let duration_ms = started.elapsed().as_millis() as i64;

    let success = !sink.records.last().is_some_and(|r| r.is_error);
    Ok(PrintTurnOutcome {
        response: sink.last_response.unwrap_or_default(),
        tokens: sink.tokens,
        duration_ms,
        tool_calls: (!sink.records.is_empty()).then_some(sink.records),
        files_modified: (!sink.files.is_empty()).then_some(sink.files),
        success,
        unavailable,
        engine_session_id: conversation.engine_session_id.clone(),
    })
}

fn last_user_text(conversation: &Conversation) -> String {
    conversation
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

fn random_bucket_id() -> String {
    // C#: the first 8 hex digits of a new GUID.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as u64 ^ u64::from(std::process::id())).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", mixed >> 32)
}

/// `EngineHost.HostTools()`: the host tools this OpenOrca has (from the tool registry).
pub fn host_tools(registry: Option<&ToolRegistry>) -> Vec<HostToolSpec> {
    let Some(registry) = registry else {
        return Vec::new();
    };
    HOST_TOOL_NAMES
        .iter()
        .filter_map(|n| registry.resolve(n))
        .map(|t| HostToolSpec {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.parameter_schema(),
        })
        .collect()
}

fn risk_from_name(risk: &str) -> ToolRiskLevel {
    match risk {
        "ReadOnly" => ToolRiskLevel::ReadOnly,
        "Moderate" => ToolRiskLevel::Moderate,
        _ => ToolRiskLevel::Dangerous,
    }
}

/// `EngineEventRenderer` in single-prompt mode: records what `ReplState` keeps, approves through
/// the permission rules (no prompter: calls needing approval are refused, as C# `--print`).
struct PrintSink<'a> {
    permissions: &'a PermissionsConfig,
    registry: Option<&'a ToolRegistry>,
    ctx: ToolContext,
    quiet: bool,
    open_calls: HashMap<String, (String, String, Instant)>,
    answer: String,
    records: Vec<ToolCallRecord>,
    files: Vec<String>,
    tokens: i64,
    last_response: Option<String>,
}

#[async_trait]
impl AgentEventSink for PrintSink<'_> {
    fn on_event(&mut self, event: AgentEngineEvent) {
        match event {
            AgentEngineEvent::TextDelta(text) => {
                if !self.quiet {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                self.answer.push_str(&text);
            }
            AgentEngineEvent::ToolCall {
                id,
                name,
                args_json,
                ..
            } => {
                if !self.quiet {
                    eprintln!("\n> {name} {args_json}");
                }
                self.open_calls
                    .insert(id, (name, args_json, Instant::now()));
            }
            AgentEngineEvent::ToolResult {
                id,
                name,
                result,
                is_error,
                args_json,
            } => {
                let mut args = args_json;
                let mut elapsed = None;
                if let Some((_, open_args, started)) = self.open_calls.remove(&id) {
                    elapsed = Some(started.elapsed());
                    if args.is_none() {
                        args = Some(open_args);
                    }
                }
                if !self.quiet && is_error {
                    eprintln!("  (error)");
                }
                self.records.push(ToolCallRecord {
                    name,
                    arguments: args,
                    result: Some(orca_tools_common::keep_head_and_tail(
                        &result,
                        TOOL_RESULT_HISTORY_MAX_CHARS,
                        None,
                    )),
                    is_error,
                    duration_ms: elapsed.map_or(0, |e| e.as_millis() as i64),
                });
            }
            AgentEngineEvent::FileEdited { path, .. } => {
                let lower = path.to_lowercase();
                if !self.files.iter().any(|f| f.to_lowercase() == lower) {
                    self.files.push(path);
                }
            }
            AgentEngineEvent::Notice { text, .. } => {
                if !self.quiet {
                    eprintln!("{text}");
                }
            }
            AgentEngineEvent::TurnCompleted(done) => {
                if let Some(out) = done.output_tokens {
                    self.tokens += i64::from(out);
                }
                let final_text = done
                    .final_text
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| self.answer.clone());
                if !final_text.is_empty() {
                    self.last_response = Some(final_text);
                }
                tracing::info!(
                    "Bridge turn {}: cost {:?} USD, in {:?} / out {:?} tokens, {:?} model turns, {:?} ms",
                    done.outcome,
                    done.cost_usd,
                    done.input_tokens,
                    done.output_tokens,
                    done.num_turns,
                    done.duration_ms
                );
            }
            AgentEngineEvent::SessionStarted { .. }
            | AgentEngineEvent::ThinkingDelta(_)
            | AgentEngineEvent::ThinkingProgress(_) => {}
        }
    }

    async fn approve(&mut self, request: EngineToolRequest) -> bool {
        let arg_name = self
            .registry
            .and_then(|r| r.resolve(&request.name))
            .map_or_else(
                || "path".to_string(),
                |t| t.permission_arg_name().to_string(),
            );
        let args: Value = serde_json::from_str(&request.args_json).unwrap_or(Value::Null);
        let allowed = evaluate_permission(
            self.permissions,
            &request.name,
            risk_from_name(&request.risk_level),
            &arg_name,
            &args,
            &Default::default(),
        ) == PermissionVerdict::Allow;
        if !allowed && !self.quiet {
            eprintln!("  \u{2298} {} denied", request.original_name);
        }
        tracing::info!(
            "Bridge approval {} ({}, {}): {}",
            request.name,
            request.original_name,
            request.risk_level,
            if allowed { "allowed" } else { "denied" }
        );
        allowed
    }

    async fn run_host_tool(&mut self, name: &str, args_json: &str) -> HostToolResult {
        let Some(tool) = self.registry.and_then(|r| r.resolve(name)) else {
            return HostToolResult::error(format!("Abhoth tool '{name}' is not available."));
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return HostToolResult::error(format!("Invalid arguments for {name}: {e}")),
        };
        let result = tool.execute(&args, &self.ctx).await;
        tracing::info!(
            "Bridge host tool {name}: {}",
            if result.is_error { "error" } else { "ok" }
        );
        HostToolResult {
            text: result.content,
            is_error: result.is_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::test_support::{FakeClaude, claude_factory, load_claude_turns};

    #[tokio::test]
    async fn print_turn_reports_like_csharp() {
        let mut config = OrcaConfig::default();
        config.agent.engine = "claude-code".into();
        config.permissions.auto_approve_moderate = true;
        let turns = load_claude_turns(crate::engines::test_support::CLAUDE_MULTI_TURN_EDIT);
        let fakes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<FakeClaude>::new()));
        let starts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory = claude_factory(turns, fakes.clone(), starts.clone(), |_| {});
        let mut conversation = Conversation::new();
        let cwd = Path::new(r"C:\work\proj");
        let outcome = run_print_turn_with(
            PrintTurnRequest {
                config: &config,
                cwd,
                conversation: &mut conversation,
                system_prompt: Some("sys".into()),
                prompt: "Create hello.txt",
                registry: None,
                session_id: Some("abc"),
                quiet: true,
            },
            &CancelToken::new(),
            Some(factory),
        )
        .await
        .unwrap();

        assert!(outcome.success);
        assert!(outcome.unavailable.is_none());
        let calls = outcome.tool_calls.unwrap();
        assert_eq!(
            calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["write_file"]
        );
        assert!(
            calls[0]
                .result
                .as_deref()
                .unwrap()
                .starts_with("Wrote 1 line to ")
        );
        assert!(calls[0].arguments.as_deref().unwrap().contains("\"path\":"));
        assert_eq!(outcome.files_modified.unwrap().len(), 1);
        assert!(outcome.tokens > 0);
        assert!(!outcome.response.is_empty());
        assert_eq!(
            outcome.engine_session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(conversation.engine_id.as_deref(), Some("claude"));
        // The Write was auto-approved (Moderate), the user message went out.
        let fake = fakes.lock().unwrap()[0].clone();
        assert_eq!(fake.written_of_type("user", None).len(), 1);
        let answers = fake.written_of_type("control_response", None);
        assert_eq!(answers[0]["response"]["response"]["behavior"], "allow");
        // PROGRESS_SESSION is the bucket of the session id.
        let env = starts.lock().unwrap()[0].environment.clone().unwrap();
        assert_eq!(
            crate::engines::transport::env::get(&env, "PROGRESS_SESSION"),
            Some("openorca-abc")
        );
    }

    #[tokio::test]
    async fn print_turn_denies_without_rules_like_csharp_print() {
        let mut config = OrcaConfig::default();
        config.agent.engine = "claude".into();
        let turns = load_claude_turns(crate::engines::test_support::CLAUDE_DENIED_WRITE);
        let fakes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<FakeClaude>::new()));
        let starts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory = claude_factory(turns, fakes.clone(), starts, |_| {});
        let mut conversation = Conversation::new();
        let outcome = run_print_turn_with(
            PrintTurnRequest {
                config: &config,
                cwd: Path::new(r"C:\work\proj"),
                conversation: &mut conversation,
                system_prompt: None,
                prompt: "make a.txt",
                registry: None,
                session_id: None,
                quiet: true,
            },
            &CancelToken::new(),
            Some(factory),
        )
        .await
        .unwrap();
        let fake = fakes.lock().unwrap()[0].clone();
        let answer = &fake.written_of_type("control_response", None)[0];
        assert_eq!(answer["response"]["response"]["behavior"], "deny");
        assert!(!outcome.success);
        assert!(outcome.files_modified.is_none());
    }

    #[test]
    fn selected_bridge_normalizes() {
        let mut c = OrcaConfig::default();
        assert_eq!(selected_bridge(&c), None);
        c.agent.engine = "cc".into();
        assert_eq!(selected_bridge(&c), Some("claude"));
        c.agent.engine = "gpt".into();
        assert_eq!(selected_bridge(&c), None);
    }
}
