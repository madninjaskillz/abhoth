//! Sticky inference slots for sub-agents (`AgentOrchestrator.Slots`, `AcquireSlotAsync`,
//! `ReleaseSlotAsync`): every agent on the default model leases its own slot for its whole run
//! (waiting while all are busy) and talks to that copy only; a nested spawn that finds no free
//! slot borrows its spawner's. Agents on a named provider or a type model take no slot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::client::{AgentChatClient, AgentClientProvider, AgentLease, ProviderRoute};
use super::context::AgentContext;
use super::types::AgentTypeDefinition;
use crate::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent,
};
use crate::slots::{SlotLease, SlotRequest, SlotRuntime};

/// A [`ChatBackend`] (a slot-pinned client) used as an agent's client.
pub struct BackendAgentClient(pub Arc<dyn ChatBackend>);

#[async_trait::async_trait]
impl AgentChatClient for BackendAgentClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.0.complete(actor, messages, options).await
    }

    async fn stream(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.0
            .stream_with_control(actor, messages, options, on_event)
            .await
    }
}

/// An agent's client used as the app-wide (ambient) backend inside its flow.
pub struct AgentClientBackend(pub Arc<dyn AgentChatClient>);

#[async_trait::async_trait]
impl ChatBackend for AgentClientBackend {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.0.complete(actor, messages, options).await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.0.stream(actor, messages, options, on_event).await
    }
}

/// What an agent holds: its own lease, or a borrowed client (no lease).
struct Held {
    lease: Option<Arc<SlotLease>>,
    client: Arc<dyn AgentChatClient>,
}

/// An [`AgentClientProvider`] over a [`SlotRuntime`]; everything but slot leasing goes to
/// `inner` (the main client, type models, named providers).
pub struct SlotClientProvider {
    inner: Arc<dyn AgentClientProvider>,
    runtime: Arc<SlotRuntime>,
    held: Mutex<HashMap<String, Held>>,
}

impl SlotClientProvider {
    pub fn new(inner: Arc<dyn AgentClientProvider>, runtime: Arc<SlotRuntime>) -> Self {
        Self {
            inner,
            runtime,
            held: Mutex::new(HashMap::new()),
        }
    }

    pub fn runtime(&self) -> &Arc<SlotRuntime> {
        &self.runtime
    }

    fn held_client(&self, id: &str) -> Option<Arc<dyn AgentChatClient>> {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|h| h.client.clone())
    }
}

#[async_trait::async_trait]
impl AgentClientProvider for SlotClientProvider {
    fn default_client(&self) -> Arc<dyn AgentChatClient> {
        self.inner.default_client()
    }

    fn client_for_model(&self, model_ref: &str) -> Option<Arc<dyn AgentChatClient>> {
        self.inner.client_for_model(model_ref)
    }

    fn provider_route(&self, name: &str) -> Option<ProviderRoute> {
        self.inner.provider_route(name)
    }

    fn uses_slots(&self) -> bool {
        true
    }

    fn ambient_backend(
        &self,
        client: Option<&Arc<dyn AgentChatClient>>,
    ) -> Option<Arc<dyn ChatBackend>> {
        // C# `Slots?.SetAmbient(context.SlotClient ?? RouteOf(context)?.Client)`: null = the
        // Lead's client (an agent on a type model compacts through the Lead's).
        Some(match client {
            Some(c) => Arc::new(AgentClientBackend(c.clone())),
            None => self.runtime.lead_backend(),
        })
    }

    async fn acquire(
        &self,
        agent: &Arc<AgentContext>,
        spawner: Option<&Arc<AgentContext>>,
        waiting: &(dyn Fn() + Send + Sync),
    ) -> Option<AgentLease> {
        let last_slot = agent.state().last_slot.clone();
        let request = SlotRequest::agent(
            &agent.id,
            &format!("OpenOrca {}: {}", agent.tag(), agent.label()),
        )
        .with_prefer_slot(last_slot.as_deref())
        .with_heavy(agent.heavy);
        let allocator = self.runtime.allocator.clone();
        let mut lease = allocator.try_acquire(&request).await;
        if lease.is_none()
            && let Some(spawner) = spawner
            && let Some(borrowed) = self.held_client(&spawner.id)
        {
            // The spawner is blocked on this call, so its copy is idle; waiting could deadlock.
            let spawner_slot = spawner.state().slot.clone();
            let slot = spawner_slot
                .as_deref()
                .map(|s| format!("{}*", s.trim_end_matches('*')));
            self.held.lock().unwrap_or_else(|e| e.into_inner()).insert(
                agent.id.clone(),
                Held {
                    lease: None,
                    client: borrowed.clone(),
                },
            );
            return Some(AgentLease {
                client: borrowed,
                slot,
                note: Some(format!(
                    "no free slot; sharing {}'s slot {}",
                    spawner.tag(),
                    spawner_slot.unwrap_or_default()
                )),
            });
        }
        if lease.is_none() {
            waiting();
            lease = Some(allocator.acquire(&request, None).await);
        }
        let lease = lease?;
        let weak: Weak<AgentContext> = Arc::downgrade(agent);
        let key = Arc::as_ptr(&lease) as usize;
        lease.on_changed(move |l| {
            let Some(ctx) = weak.upgrade() else { return };
            let mut s = ctx.state();
            // Only while it is still this agent's lease (a follow-up takes a new one).
            if s.lease_key != Some(key) {
                return;
            }
            s.slot = l.slot();
            if let Some(slot) = l.slot() {
                s.last_slot = Some(slot);
            }
        });
        agent.state().lease_key = Some(key);
        let slot = lease.slot();
        let client: Arc<dyn AgentChatClient> =
            Arc::new(BackendAgentClient(self.runtime.client_for(lease.clone())));
        self.held.lock().unwrap_or_else(|e| e.into_inner()).insert(
            agent.id.clone(),
            Held {
                lease: Some(lease),
                client: client.clone(),
            },
        );
        let note = match &slot {
            None => "no healthy model copy; using the fallback".to_string(),
            Some(s) => format!("pinned to slot {s}"),
        };
        Some(AgentLease {
            client,
            slot,
            note: Some(note),
        })
    }

    async fn release(&self, agent: &AgentContext) {
        agent.state().lease_key = None;
        let held = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&agent.id);
        if let Some(lease) = held.and_then(|h| h.lease) {
            self.runtime.allocator.release(&lease).await;
        }
    }

    fn note_type(&self, agent: &AgentContext, type_def: &AgentTypeDefinition) {
        self.inner.note_type(agent, type_def);
    }
}
