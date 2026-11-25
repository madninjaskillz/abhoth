//! The agent: tool registry, permissions, prompts and the model/tool loop.

pub mod permissions;
pub mod prompts;
pub mod registry;
pub mod session;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
