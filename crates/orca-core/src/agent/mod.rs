//! The agent: tool registry, permissions, prompts and the model/tool loop.

mod context;
mod early_runs;
pub mod llm_tags;
pub mod long_task;
pub mod loop_messages;
pub mod pair_vote;
pub mod permissions;
pub mod policies;
pub mod prompt_constants;
pub mod prompts;
pub mod registry;
pub mod response_quality;
pub mod run_evidence;
pub mod session;
mod streaming;
pub mod sub_agent_tools;
pub mod system_prompt;
pub mod think_tag_filter;
pub mod tool_call_parser;
pub mod tool_hooks;
mod turn;

pub use registry::ToolRegistry;
pub use session::{AgentError, AgentObserver, AgentSession, NullObserver, ToolCallRecord, TurnEnd};
pub use tool_hooks::ToolHooks;
