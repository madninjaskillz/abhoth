//! Agent engines: the native loop and the bridge CLIs (port of `OpenOrca.Core.Engines`).
//!
//! A bridge engine hands the whole turn to an external coding-agent CLI (Claude Code, Codex)
//! and turns its event stream into [`AgentEngineEvent`]s, which the host renders like a native
//! turn (streamed text, thinking, tool cards, file-edit markers, approvals).

pub mod bridge_tools;
mod cancel;
pub mod claude;
pub mod cli_locator;
pub mod codex;
mod event;
pub mod host;
pub(crate) mod json;
pub mod paths;
pub mod print;
pub mod readiness;
pub mod session_sink;
pub mod transport;

#[cfg(test)]
mod test_support;

use async_trait::async_trait;
use serde_json::Value;

use crate::chat::Conversation;

pub use cancel::CancelToken;
pub use event::{AgentEngineEvent, EngineTurnCompleted};
pub use host::{BridgeStart, BridgeTurnRequest, EngineHost, EngineSwitchResult};
pub use print::{
    PrintTurnOutcome, PrintTurnRequest, check_readiness, prepare_engine_non_interactive,
    run_print_turn, selected_bridge,
};
pub use readiness::{EngineAuthRequired, EngineReadiness, EngineReadinessState};
pub use transport::{BridgeTransport, BridgeTransportFactory, ProcessBridgeTransport, ProcessSpec};

/// Engine ids and the names users may type for them (`AgentEngines`).
pub struct AgentEngines;

impl AgentEngines {
    pub const NATIVE: &'static str = "native";
    pub const CLAUDE: &'static str = "claude";
    pub const CODEX: &'static str = "codex";

    pub const ALL: [&'static str; 3] = [Self::NATIVE, Self::CLAUDE, Self::CODEX];

    /// The canonical id for a typed name ("claude-code" -> "claude"), or `None` when unknown.
    /// Blank (or no) name means native.
    pub fn normalize(name: Option<&str>) -> Option<&'static str> {
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return Some(Self::NATIVE);
        };
        match name.trim().to_lowercase().as_str() {
            "native" | "openorca" | "orca" | "default" => Some(Self::NATIVE),
            "claude" | "claude-code" | "claudecode" | "cc" => Some(Self::CLAUDE),
            "codex" | "openai-codex" | "codex-cli" => Some(Self::CODEX),
            _ => None,
        }
    }

    pub fn display_name_of(id: &str) -> &'static str {
        match id {
            Self::CLAUDE => "claude-code",
            Self::CODEX => "codex",
            _ => Self::NATIVE,
        }
    }

    pub fn is_bridge(id: Option<&str>) -> bool {
        matches!(Self::normalize(id), Some(Self::CLAUDE | Self::CODEX))
    }
}

/// How a turn may use tools (`AgentTurnMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentTurnMode {
    #[default]
    Normal,
    /// /ask and persistent Ask mode: answer from the conversation, no tools.
    Ask,
    /// Plan mode: investigate and propose a plan; no edits or commands that change things.
    Plan,
}

/// An OpenOrca tool offered to a bridged CLI (ask_user, ...): served to Claude Code as an
/// in-process MCP server ("openorca") and to Codex as a dynamic tool.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A host tool's answer for the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolResult {
    pub text: String,
    pub is_error: bool,
}

impl HostToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A tool call a bridge CLI wants to run, named the way OpenOrca's permission rules know it.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineToolRequest {
    /// OpenOrca's name for the tool ("bash", "edit_file", ...).
    pub name: String,
    /// The CLI's own name ("Bash", "Edit", ...).
    pub original_name: String,
    /// Arguments with OpenOrca's key names added ("path" next to "file_path").
    pub args_json: String,
    /// "ReadOnly", "Moderate" or "Dangerous".
    pub risk_level: String,
    pub tool_call_id: Option<String>,
}

/// Receives a bridge turn's events, in order (never concurrently), and answers its
/// tool-approval requests (`IAgentEventSink`). A cancelled turn drops the pending
/// `approve` / `run_host_tool` future (C#: the token cancels it) and treats it as denied /
/// "Cancelled by the user.".
#[async_trait]
pub trait AgentEventSink: Send {
    fn on_event(&mut self, event: AgentEngineEvent);

    /// May the CLI run this tool? (OpenOrca's permission settings and prompt decide.)
    async fn approve(&mut self, request: EngineToolRequest) -> bool;

    /// Run one of OpenOrca's host tools for the CLI, e.g. ask_user.
    async fn run_host_tool(&mut self, name: &str, _args_json: &str) -> HostToolResult {
        HostToolResult::error(format!("Abhoth tool '{name}' is not available here."))
    }
}

/// One turn for an engine (`AgentTurn`). The user's message is already the last message of
/// `conversation`; a bridge sends `user_text` to its CLI and appends the reply.
pub struct AgentTurn<'a> {
    pub conversation: &'a mut Conversation,
    pub user_text: String,
    pub events: &'a mut dyn AgentEventSink,
    pub mode: AgentTurnMode,
}

impl<'a> AgentTurn<'a> {
    pub fn new(
        conversation: &'a mut Conversation,
        user_text: impl Into<String>,
        events: &'a mut dyn AgentEventSink,
    ) -> Self {
        Self {
            conversation,
            user_text: user_text.into(),
            events,
            mode: AgentTurnMode::Normal,
        }
    }

    pub fn with_mode(mut self, mode: AgentTurnMode) -> Self {
        self.mode = mode;
        self
    }
}

/// Why a turn (or starting an engine) failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// `AgentEngineUnavailableException`: the CLI can't be used (not installed, too old,
    /// failed to start). The message says what to do.
    #[error("{0}")]
    Unavailable(String),
    /// Any other failure (C# `InvalidOperationException`, I/O errors): the turn failed.
    #[error("{0}")]
    Failed(String),
    /// The turn was cancelled (C# `OperationCanceledException`).
    #[error("The operation was canceled.")]
    Cancelled,
}

/// What drives a session's turns (`IAgentEngine`).
#[async_trait]
pub trait AgentEngine: Send {
    /// Canonical id: "native", "claude" or "codex".
    fn id(&self) -> &'static str;
    /// Name shown in the footer in place of the slot and model, e.g. "claude-code".
    fn display_name(&self) -> &'static str;
    /// False for bridges: their requests never go through OpenOrca's chat client.
    fn uses_slots(&self) -> bool;
    /// The engine's own session id (a bridge CLI's resumable session).
    fn session_id(&self) -> Option<&str>;
    /// Run one turn. Cancelling `cancel` interrupts it; the result is then
    /// [`EngineError::Cancelled`].
    async fn run_turn(
        &mut self,
        turn: AgentTurn<'_>,
        cancel: &CancelToken,
    ) -> Result<(), EngineError>;
    /// End the CLI process (`DisposeAsync`).
    async fn dispose(&mut self);
}

/// `sink.ApproveAsync(request, ct)` with cancellation counted as a denial (C# catches the
/// `OperationCanceledException`). `None`: not cancellable (an interrupted turn winding down).
pub(crate) async fn approve_with(
    sink: &mut dyn AgentEventSink,
    request: EngineToolRequest,
    cancel: Option<&CancelToken>,
) -> bool {
    match cancel {
        None => sink.approve(request).await,
        Some(ct) => tokio::select! {
            biased;
            r = sink.approve(request) => r,
            _ = ct.cancelled() => false,
        },
    }
}

/// `sink.RunHostToolAsync(name, args, ct)`; cancelled: "Cancelled by the user." (an error).
pub(crate) async fn host_tool_with(
    sink: &mut dyn AgentEventSink,
    name: &str,
    args_json: &str,
    cancel: Option<&CancelToken>,
) -> HostToolResult {
    let cancelled = || HostToolResult::error("Cancelled by the user.");
    match cancel {
        None => sink.run_host_tool(name, args_json).await,
        Some(ct) => tokio::select! {
            biased;
            r = sink.run_host_tool(name, args_json) => r,
            _ = ct.cancelled() => cancelled(),
        },
    }
}

/// `…: {Tail(stderr, 600)}` / `… (exit code n).`: what a bridge says when its CLI exits.
pub(crate) fn exit_message(what: &str, transport: Option<&mut Box<dyn BridgeTransport>>) -> String {
    let (code, err) = match transport {
        Some(t) => (t.exit_code(), t.stderr_tail()),
        None => (None, String::new()),
    };
    let code = code
        .map(|c| format!(" (exit code {c})"))
        .unwrap_or_default();
    let err = err.trim();
    if err.is_empty() {
        return format!("{what}{code}.");
    }
    let count = err.chars().count();
    let tail = if count <= 600 {
        err.to_string()
    } else {
        format!(
            "\u{2026}{}",
            err.chars().skip(count - 600).collect::<String>()
        )
    };
    format!("{what}{code}: {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_names_normalize() {
        assert_eq!(AgentEngines::normalize(Some("claude-code")), Some("claude"));
        assert_eq!(AgentEngines::normalize(Some("Codex")), Some("codex"));
        assert_eq!(AgentEngines::normalize(None), Some("native"));
        assert_eq!(AgentEngines::normalize(Some("gpt")), None);
    }
}
