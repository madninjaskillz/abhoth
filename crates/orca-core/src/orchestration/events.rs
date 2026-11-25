//! Progress events raised by the orchestrator while a sub-agent runs (`AgentEvent` records).
//! Consumers subscribe with [`super::AgentOrchestrator::subscribe`]; a handler that panics is
//! isolated and never takes an agent down.

use std::sync::Arc;

use super::context::AgentContext;
use super::types::AgentStatus;

#[derive(Clone)]
pub enum AgentEvent {
    /// A sub-agent was created and is about to run.
    Started {
        agent_id: String,
        context: Arc<AgentContext>,
    },
    /// A sub-agent started a new loop iteration.
    IterationStarted {
        agent_id: String,
        iteration: i32,
        max_iterations: i32,
    },
    /// A sub-agent is about to execute a tool call. `activity`: the call in a few words.
    ToolCallStarted {
        agent_id: String,
        tool_name: String,
        args_preview: String,
        activity: Option<String>,
    },
    /// A sub-agent finished executing a tool call.
    ToolCallCompleted {
        agent_id: String,
        tool_name: String,
        is_error: bool,
        elapsed_ms: i64,
    },
    /// An agent (or the Lead, id `lead`) reported its progress.
    ProgressReported {
        agent_id: String,
        fraction: f64,
        note: Option<String>,
    },
    /// A queued message was handed to the sub-agent at the start of its turn.
    MessageDelivered { agent_id: String, message: String },
    /// A sub-agent's conversation was pruned and/or compacted.
    Compacted { agent_id: String },
    /// A sub-agent finished (successfully or not).
    Completed {
        agent_id: String,
        status: AgentStatus,
        iterations: i32,
    },
}

impl AgentEvent {
    pub fn agent_id(&self) -> &str {
        match self {
            AgentEvent::Started { agent_id, .. }
            | AgentEvent::IterationStarted { agent_id, .. }
            | AgentEvent::ToolCallStarted { agent_id, .. }
            | AgentEvent::ToolCallCompleted { agent_id, .. }
            | AgentEvent::ProgressReported { agent_id, .. }
            | AgentEvent::MessageDelivered { agent_id, .. }
            | AgentEvent::Compacted { agent_id }
            | AgentEvent::Completed { agent_id, .. } => agent_id,
        }
    }

    /// The C# record type name (for logs and tests).
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::Started { .. } => "AgentStarted",
            AgentEvent::IterationStarted { .. } => "AgentIterationStarted",
            AgentEvent::ToolCallStarted { .. } => "AgentToolCallStarted",
            AgentEvent::ToolCallCompleted { .. } => "AgentToolCallCompleted",
            AgentEvent::ProgressReported { .. } => "AgentProgressReported",
            AgentEvent::MessageDelivered { .. } => "AgentMessageDelivered",
            AgentEvent::Compacted { .. } => "AgentCompacted",
            AgentEvent::Completed { .. } => "AgentCompleted",
        }
    }
}

impl std::fmt::Debug for AgentEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.kind(), self.agent_id())
    }
}
