//! Sub-agents and orchestration (port of `OpenOrca.Core.Orchestration`).

pub mod activity;
pub mod brief_files;
pub mod cancel;
pub mod client;
pub mod context;
pub mod custom_agents;
pub mod events;
pub mod labels;
pub mod orchestrator;
pub mod palette;
pub mod prompt_loader;
pub mod read_only_command;
pub mod repetition;
pub mod step_budget;
pub mod transcript;
pub mod types;
pub mod workflow;

pub use cancel::CancelToken;
pub use client::{
    AgentChatClient, AgentClientProvider, AgentLease, DefaultClientProvider, ProviderRoute,
    SubAgentToolExecutor,
};
pub use context::{AgentContext, AgentSpec, AgentState};
pub use events::AgentEvent;
pub use orchestrator::*;
pub mod manager;
pub use manager::{AgentManager, SubagentOutcome, SubagentTask};
pub mod workflow_runner;
pub mod planner_phase;
