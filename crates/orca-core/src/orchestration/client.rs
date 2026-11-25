//! How sub-agents reach a model, and how they run tools.
//!
//! [`AgentChatClient`] is the part of `IChatClient` the orchestrator uses (one request, streamed
//! or not). [`AgentClientProvider`] picks the client an agent talks to: the default provider
//! hands every agent the main client; the slot allocator (sticky inference slots) plugs in here
//! by leasing a slot in [`AgentClientProvider::acquire`] and giving it back in
//! [`AgentClientProvider::release`]. [`SubAgentToolExecutor`] runs a sub-agent's tool call
//! through the host's tool pipeline (permissions, hooks, checkpoints, read-before-edit).

use std::sync::Arc;

use orca_abstractions::ToolResult;

use super::context::AgentContext;
use super::types::AgentTypeDefinition;
use crate::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, OpenAiClient, StreamControl, StreamEvent,
};

/// One model request, streamed or not.
#[async_trait::async_trait]
pub trait AgentChatClient: Send + Sync {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError>;

    /// Streamed request; `on_event` may stop it ([`StreamControl::Stop`]).
    async fn stream(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError>;
}

#[async_trait::async_trait]
impl AgentChatClient for OpenAiClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        OpenAiClient::complete(self, actor, messages, options).await
    }

    async fn stream(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.stream_with_control(actor, messages, options, on_event)
            .await
    }
}

/// A named provider an agent runs on (agent.providerSchedule): its client and limits
/// (`AgentProviderRoute`).
#[derive(Clone)]
pub struct ProviderRoute {
    pub name: String,
    pub model: String,
    pub client: Arc<dyn AgentChatClient>,
    pub context_window: Option<i32>,
    pub max_tokens: Option<i32>,
}

/// A named provider's client with its model id pinned on every request (C# wraps the route's
/// client in `ConfigureOptionsChatClient(o => o.ModelId = model)`), so callers in the agent's
/// flow that set no model still reach the provider's model.
pub struct PinnedModelClient {
    inner: Arc<dyn AgentChatClient>,
    model: String,
}

impl PinnedModelClient {
    pub fn new(inner: Arc<dyn AgentChatClient>, model: impl Into<String>) -> Self {
        Self {
            inner,
            model: model.into(),
        }
    }

    fn pin(&self, options: &ChatOptions) -> ChatOptions {
        let mut options = options.clone();
        options.model = Some(self.model.clone());
        options.model_from_client = false;
        options
    }
}

#[async_trait::async_trait]
impl AgentChatClient for PinnedModelClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .complete(actor, messages, &self.pin(options))
            .await
    }

    async fn stream(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .stream(actor, messages, &self.pin(options), on_event)
            .await
    }
}

/// What an agent got from [`AgentClientProvider::acquire`].
#[derive(Clone)]
pub struct AgentLease {
    pub client: Arc<dyn AgentChatClient>,
    /// The slot it is pinned to ("A2", or "A1*" when borrowed), for the panel; None = no slot.
    pub slot: Option<String>,
    /// A status line for its transcript ("pinned to slot A2"), if any.
    pub note: Option<String>,
}

/// Picks the client of each agent. The default ([`DefaultClientProvider`]) gives every agent the
/// main client; a slot-aware provider leases a model copy per agent.
#[async_trait::async_trait]
pub trait AgentClientProvider: Send + Sync {
    /// The main client (agents whose type names no model, before any lease).
    fn default_client(&self) -> Arc<dyn AgentChatClient>;

    /// The client of an agent type's `model` reference ("provider/model"); None = the default
    /// client (C#: no `ChatClientRegistry`, or the reference does not resolve).
    fn client_for_model(&self, _model_ref: &str) -> Option<Arc<dyn AgentChatClient>> {
        None
    }

    /// A named provider of agent.providerSchedule; None = it can't be built (the agent then runs
    /// on the local model).
    fn provider_route(&self, _name: &str) -> Option<ProviderRoute> {
        None
    }

    /// Called once an agent on the default model can run (may wait for a free slot; it is told
    /// so through `waiting`). `spawner`: the agent whose tool call spawned this one, whose slot
    /// a nested spawn may borrow. None = no slots: use [`Self::default_client`].
    async fn acquire(
        &self,
        _agent: &AgentContext,
        _spawner: Option<&AgentContext>,
        _waiting: &(dyn Fn() + Send + Sync),
    ) -> Option<AgentLease> {
        None
    }

    /// The agent finished (or failed, timed out, was cancelled): give its slot back.
    async fn release(&self, _agent: &AgentContext) {}

    /// The type this agent resolved to (for logs and providers that care).
    fn note_type(&self, _agent: &AgentContext, _type_def: &AgentTypeDefinition) {}
}

/// Every agent uses the main client.
pub struct DefaultClientProvider {
    client: Arc<dyn AgentChatClient>,
}

impl DefaultClientProvider {
    pub fn new(client: Arc<dyn AgentChatClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl AgentClientProvider for DefaultClientProvider {
    fn default_client(&self) -> Arc<dyn AgentChatClient> {
        self.client.clone()
    }
}

/// Runs a sub-agent's tool call through the host pipeline. The returned content is what the
/// agent's model sees (with the pipeline's `ERROR: ` prefix and truncation).
#[async_trait::async_trait]
pub trait SubAgentToolExecutor: Send + Sync {
    async fn execute(&self, tool_name: &str, args_json: &str) -> ToolResult;
}
