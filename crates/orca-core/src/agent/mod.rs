//! The agent: tool registry, permissions, prompts and the model/tool loop.

pub mod loop_messages;
pub mod permissions;
pub mod prompts;
pub mod registry;
pub mod run_evidence;
pub mod session;
pub mod tool_call_parser;
mod turn;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
