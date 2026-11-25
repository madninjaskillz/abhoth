//! The agent: tool registry, permissions, prompts and the model/tool loop.

pub mod llm_tags;
pub mod permissions;
pub mod prompts;
pub mod registry;
pub mod response_quality;
pub mod run_evidence;
pub mod session;
pub mod tool_call_parser;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
