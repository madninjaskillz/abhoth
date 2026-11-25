//! The agent: tool registry, permissions, prompts and the model/tool loop.

pub mod permissions;
pub mod prompts;
pub mod registry;
pub mod run_evidence;
pub mod session;
pub mod tool_hooks;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
pub use tool_hooks::ToolHooks;
