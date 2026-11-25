//! The part of C# `EngineEventRenderer` every front end shares: a bridge turn's events kept in
//! the agent session's state (last answer, tool history, files modified, output tokens),
//! approvals through the session's permission rules and prompter, and host tools run from the
//! session's registry. What the user sees is a [`BridgeView`] (the REPL renders, the web UI
//! publishes on its bus).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use orca_abstractions::ToolRiskLevel;
use serde_json::Value;

use super::{AgentEngineEvent, AgentEventSink, EngineToolRequest, HostToolResult};
use crate::agent::session::TOOL_RESULT_HISTORY_MAX_CHARS;
use crate::agent::{AgentSession, ToolCallRecord};

/// How a front end shows a bridge turn. Every method has a no-op default.
pub trait BridgeView: Send {
    /// An event, after the session state took it. For a tool result, `elapsed` is the time
    /// since its call and `args` the call's arguments (the result's own, else the call's).
    fn event(
        &mut self,
        _event: &AgentEngineEvent,
        _elapsed: Option<Duration>,
        _args: Option<&str>,
    ) {
    }

    /// An approval prompt or a host tool (ask_user) is about to take the screen.
    fn before_prompt(&mut self) {}

    /// The CLI's tool call was refused (`⊘ Write denied`).
    fn denied(&mut self, _original_name: &str) {}
}

/// A view that shows nothing.
pub struct NoView;

impl BridgeView for NoView {}

/// [`AgentEventSink`] over an [`AgentSession`] and a [`BridgeView`].
pub struct SessionBridgeSink<'a> {
    session: &'a mut AgentSession,
    view: &'a mut dyn BridgeView,
    open_calls: HashMap<String, (String, Instant)>,
    answer: String,
}

impl<'a> SessionBridgeSink<'a> {
    pub fn new(session: &'a mut AgentSession, view: &'a mut dyn BridgeView) -> Self {
        Self {
            session,
            view,
            open_calls: HashMap::new(),
            answer: String::new(),
        }
    }
}

/// "ReadOnly" / "Moderate" / anything else (Dangerous).
pub fn risk_from_name(risk: &str) -> ToolRiskLevel {
    match risk {
        "ReadOnly" => ToolRiskLevel::ReadOnly,
        "Moderate" => ToolRiskLevel::Moderate,
        _ => ToolRiskLevel::Dangerous,
    }
}

#[async_trait]
impl AgentEventSink for SessionBridgeSink<'_> {
    fn on_event(&mut self, event: AgentEngineEvent) {
        let mut elapsed = None;
        let mut args = None;
        match &event {
            AgentEngineEvent::TextDelta(text) => self.answer.push_str(text),
            AgentEngineEvent::ToolCall { id, args_json, .. } => {
                self.open_calls
                    .insert(id.clone(), (args_json.clone(), Instant::now()));
            }
            AgentEngineEvent::ToolResult {
                id,
                name,
                result,
                is_error,
                args_json,
            } => {
                args = args_json.clone();
                if let Some((open_args, started)) = self.open_calls.remove(id) {
                    elapsed = Some(started.elapsed());
                    if args.is_none() {
                        args = Some(open_args);
                    }
                }
                self.session.tool_call_history.push(ToolCallRecord {
                    name: name.clone(),
                    arguments: args.clone(),
                    result: Some(orca_tools_common::keep_head_and_tail(
                        result,
                        TOOL_RESULT_HISTORY_MAX_CHARS,
                        None,
                    )),
                    is_error: *is_error,
                    duration_ms: elapsed.map_or(0, |e| e.as_millis() as i64),
                });
            }
            AgentEngineEvent::FileEdited { path, .. } => {
                let lower = path.to_lowercase();
                if !self
                    .session
                    .files_modified
                    .iter()
                    .any(|f| f.to_lowercase() == lower)
                {
                    self.session.files_modified.push(path.clone());
                }
            }
            AgentEngineEvent::TurnCompleted(done) => {
                if let Some(out) = done.output_tokens {
                    self.session.total_output_tokens += i64::from(out);
                }
                let final_text = done
                    .final_text
                    .clone()
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| self.answer.clone());
                if !final_text.is_empty() {
                    self.session.last_assistant_response = Some(final_text);
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
            _ => {}
        }
        self.view.event(&event, elapsed, args.as_deref());
    }

    async fn approve(&mut self, request: EngineToolRequest) -> bool {
        self.view.before_prompt();
        let arg_name = self.session.registry.resolve(&request.name).map_or_else(
            || "path".to_string(),
            |t| t.permission_arg_name().to_string(),
        );
        let args: Value = serde_json::from_str(&request.args_json).unwrap_or(Value::Null);
        let allowed = self.session.check_permission_for(
            &request.name,
            risk_from_name(&request.risk_level),
            &arg_name,
            &args,
        );
        if !allowed {
            self.view.denied(&request.original_name);
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
        self.view.before_prompt();
        let Some(tool) = self.session.registry.resolve(name) else {
            return HostToolResult::error(format!(
                "{} tool '{name}' is not available.",
                crate::brand::PRODUCT_NAME
            ));
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return HostToolResult::error(format!("Invalid arguments for {name}: {e}")),
        };
        let result = tool.execute(&args, &self.session.ctx).await;
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
