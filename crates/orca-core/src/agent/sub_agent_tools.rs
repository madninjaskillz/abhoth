//! Sub-agent tool calls through the host's tool pipeline (`ToolExecutionPipeline` shared with
//! sub-agents in C#).
//!
//! In C# a sub-agent's tool call runs through the same pipeline as the Lead's and shares its
//! state: the files read (read-before-edit), the "yes to all" scopes, the tool-call history and
//! the modified files. Here each sub-agent call runs on a short-lived copy of the session's
//! pipeline seeded from [`SharedPipeline`]; what the call changed flows back into it, and the
//! Lead's session takes the journal (history records, modified files, mutations) right after
//! the spawn tool returns, before it records the spawn call itself. The `--print` history so
//! lists a sub-agent's calls before the spawn that ran them, as in C#.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use orca_abstractions::{ToolContext, ToolResult};

use super::session::{AgentSession, NullObserver, ToolCallRecord};
use super::{ToolHooks, ToolRegistry};
use crate::client::{OpenAiClient, ToolCall};
use crate::config::OrcaConfig;
use crate::orchestration::{AgentOrchestrator, SubAgentToolExecutor};

/// The pipeline state the Lead shares with its sub-agents, plus what sub-agents did since the
/// Lead last took it.
#[derive(Default)]
pub struct SharedPipelineState {
    pub files_read: HashMap<PathBuf, Option<SystemTime>>,
    pub approved_scopes: HashSet<String>,
    pub plan_mode: bool,
    /// The Lead's prompter: foreground sub-agents' calls that need approval ask through it.
    pub prompter: Option<super::permissions::SharedPrompter>,
    /// Sub-agent calls not yet in the Lead's history, oldest first.
    pub journal: Vec<ToolCallRecord>,
    /// Files sub-agents changed (as their calls named them).
    pub files_modified: Vec<String>,
    /// Successful file modifications by sub-agents.
    pub mutations: u64,
}

/// Shared between the Lead's session and every sub-agent call.
#[derive(Default, Clone)]
pub struct SharedPipeline(Arc<Mutex<SharedPipelineState>>);

impl SharedPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, SharedPipelineState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Before a spawn tool runs: hand the Lead's state to its sub-agents.
    pub(super) fn seed_from(&self, session: &AgentSession) {
        let mut s = self.lock();
        s.files_read = session.files_read.clone();
        s.approved_scopes
            .extend(session.approved_scopes.iter().cloned());
        s.plan_mode = session.plan_mode;
        s.prompter = session.shared_prompter.clone();
    }

    /// After a spawn tool returned: take what its sub-agents did into the Lead's session.
    pub(super) fn drain_into(&self, session: &mut AgentSession) {
        let mut s = self.lock();
        session.tool_call_history.append(&mut s.journal);
        for path in s.files_modified.drain(..) {
            if !session
                .files_modified
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&path))
            {
                session.files_modified.push(path);
            }
        }
        session.mutation_epoch += std::mem::take(&mut s.mutations);
        for (k, v) in &s.files_read {
            session.files_read.insert(k.clone(), *v);
        }
        session
            .approved_scopes
            .extend(s.approved_scopes.iter().cloned());
    }
}

/// Builds the client a pipeline copy carries (it never sends a request with it).
pub type ClientFactory = Arc<dyn Fn() -> OpenAiClient + Send + Sync>;

/// [`SubAgentToolExecutor`] over a copy of the session's tool pipeline.
pub struct SessionToolExecutor {
    config: OrcaConfig,
    registry: ToolRegistry,
    ctx: ToolContext,
    hooks: ToolHooks,
    client: ClientFactory,
    shared: SharedPipeline,
    orchestrator: Weak<AgentOrchestrator>,
}

impl SessionToolExecutor {
    pub fn new(
        config: OrcaConfig,
        registry: ToolRegistry,
        ctx: ToolContext,
        hooks: ToolHooks,
        client: ClientFactory,
        shared: SharedPipeline,
        orchestrator: &Arc<AgentOrchestrator>,
    ) -> Self {
        Self {
            config,
            registry,
            ctx,
            hooks,
            client,
            shared,
            orchestrator: Arc::downgrade(orchestrator),
        }
    }
}

#[async_trait::async_trait]
impl SubAgentToolExecutor for SessionToolExecutor {
    async fn execute(&self, tool_name: &str, args_json: &str) -> ToolResult {
        let (plan_mode, files_read, scopes, prompter) = {
            let s = self.shared.lock();
            (
                s.plan_mode,
                s.files_read.clone(),
                s.approved_scopes.clone(),
                s.prompter.clone(),
            )
        };
        // The pipeline's policies (input size, plan mode, sandbox); the Lead-only policy is not
        // among them.
        let block = {
            let orchestrator = self.orchestrator.upgrade();
            let types = orchestrator.as_ref().map(|o| o.type_registry().clone());
            super::policies::check_blocked_for_sub_agent(
                &self.config,
                &self.registry,
                plan_mode,
                self.config.lm_studio.native_tool_calling,
                tool_name,
                args_json,
                types.as_ref(),
            )
        };
        if let Some(block) = block {
            tracing::info!("blocked sub-agent tool {tool_name}: {}", block.message);
            return ToolResult {
                content: block.message,
                is_error: block.is_error,
                ..ToolResult::default()
            };
        }

        let mut copy = AgentSession::new(
            self.config.clone(),
            (self.client)(),
            self.registry.clone(),
            self.ctx.clone(),
        );
        copy.tool_hooks = self.hooks.clone();
        copy.plan_mode = plan_mode;
        copy.files_read = files_read;
        copy.approved_scopes = scopes;
        // Foreground agents ask the user as the Lead does; background ones run non-interactive
        // and are refused before the prompter is reached.
        copy.prompter = prompter.map(|p| {
            Box::new(super::permissions::ForwardingPrompter(p))
                as Box<dyn super::permissions::PermissionPrompter>
        });
        let call = ToolCall {
            id: String::new(),
            name: tool_name.to_string(),
            arguments: args_json.to_string(),
        };
        let result = copy.execute_tool_call(&call, &mut NullObserver).await;

        let mut s = self.shared.lock();
        for (k, v) in copy.files_read {
            s.files_read.insert(k, v);
        }
        s.approved_scopes.extend(copy.approved_scopes);
        s.journal.append(&mut copy.tool_call_history);
        for path in copy.files_modified {
            if !s
                .files_modified
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&path))
            {
                s.files_modified.push(path);
            }
        }
        s.mutations += copy.mutation_epoch;
        result
    }
}
