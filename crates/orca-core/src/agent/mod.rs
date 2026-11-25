//! The agent: tool registry, permissions, prompts and the model/tool loop.

pub mod permissions;
pub mod prompt_constants;
pub mod prompts;
pub mod registry;
pub mod run_evidence;
pub mod session;
pub mod system_prompt;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
