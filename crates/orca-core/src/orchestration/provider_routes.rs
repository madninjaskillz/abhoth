//! Named providers for sub-agents (`AgentOrchestrator.ResolveProviderRoute`): agents that
//! agent.providerSchedule or agent.planner put on a provider of config.providers get that
//! provider's client, logged with provider= and model= and added up (and costed, when the
//! provider has pricing) in the session's [`ProviderUsageLedger`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::client::{AgentChatClient, AgentClientProvider, AgentLease, ProviderRoute};
use super::context::AgentContext;
use super::slot_provider::{AgentClientBackend, BackendAgentClient};
use super::types::AgentTypeDefinition;
use crate::client::usage::{ProviderUsageLedger, USAGE_CATEGORY, UsageLoggingBackend};
use crate::client::{ChatBackend, ClientSettings, OpenAiClient, ServedModels};
use crate::config::{OrcaConfig, ProviderConfig};
use crate::logging::LogSink;

/// `ProviderClientFactory`: the raw client of a named provider (tests); None = build it from
/// the config.
pub type ProviderClientFactory =
    Arc<dyn Fn(&str) -> Option<Arc<dyn AgentChatClient>> + Send + Sync>;

/// A built provider: its raw backend, its config (if any) and its model.
type BuiltProvider = (Arc<dyn ChatBackend>, Option<ProviderConfig>, String);

/// Adds the named-provider routes to `inner` (which keeps everything else).
pub struct ProviderRoutes {
    inner: Arc<dyn AgentClientProvider>,
    config: OrcaConfig,
    ledger: Arc<ProviderUsageLedger>,
    sink: Option<Arc<dyn LogSink>>,
    factory: Option<ProviderClientFactory>,
    cache: Mutex<HashMap<String, ProviderRoute>>,
}

impl ProviderRoutes {
    pub fn new(
        inner: Arc<dyn AgentClientProvider>,
        config: OrcaConfig,
        ledger: Arc<ProviderUsageLedger>,
        sink: Option<Arc<dyn LogSink>>,
    ) -> Self {
        Self {
            inner,
            config,
            ledger,
            sink,
            factory: None,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_factory(mut self, factory: ProviderClientFactory) -> Self {
        self.factory = Some(factory);
        self
    }

    fn provider_config(&self, name: &str) -> Option<(&String, &ProviderConfig)> {
        self.config
            .providers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// `ChatClientRegistry.ResolveProvider` + `CreateClient` (OpenAI-compatible, Anthropic or
    /// Ollama).
    fn build(&self, name: &str) -> Result<BuiltProvider, String> {
        let found = self.provider_config(name).map(|(_, p)| p.clone());
        if let Some(factory) = &self.factory
            && let Some(raw) = factory(name)
        {
            let model = found
                .as_ref()
                .and_then(|p| p.primary_model().map(str::to_string))
                .unwrap_or_else(|| name.to_string());
            return Ok((Arc::new(AgentClientBackend(raw)), found, model));
        }
        let Some(provider) = found else {
            let names: Vec<&str> = self.config.providers.keys().map(String::as_str).collect();
            return Err(format!(
                "Unknown provider '{name}'. Configured providers: {}.",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ));
        };
        let model = provider
            .primary_model()
            .map(str::to_string)
            .ok_or_else(|| {
                format!("Provider '{name}' has no model (set \"model\" or \"models\").")
            })?;
        tracing::info!(
            "Creating chat client for provider '{name}' ({}), model '{model}'",
            provider.kind
        );
        if let Some(native) =
            crate::client::native_provider_backend(&self.config, name, &provider, &model)?
        {
            return Ok((native, Some(provider), model));
        }
        let base_url = if provider.base_url.trim().is_empty() {
            self.config.lm_studio.base_url.clone()
        } else {
            provider.base_url.clone()
        };
        let key = crate::config::model::resolve_key(
            provider.api_key_env.as_deref(),
            provider.api_key.as_deref(),
        )
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| "not-needed".into());
        let mut settings = ClientSettings::new(base_url, key, Some(model.clone()));
        let timeout = Duration::from_secs(self.config.lm_studio.timeout_seconds.max(1) as u64);
        settings.request_timeout = timeout;
        settings.idle_timeout = timeout;
        settings.vision = self.config.lm_studio.vision;
        let client = OpenAiClient::new(settings, Arc::new(ServedModels::new()));
        Ok((Arc::new(client), Some(provider), model))
    }
}

#[async_trait::async_trait]
impl AgentClientProvider for ProviderRoutes {
    fn default_client(&self) -> Arc<dyn AgentChatClient> {
        self.inner.default_client()
    }

    fn client_for_model(&self, model_ref: &str) -> Option<Arc<dyn AgentChatClient>> {
        self.inner.client_for_model(model_ref)
    }

    fn provider_route(&self, name: &str) -> Option<ProviderRoute> {
        let key = name.to_lowercase();
        if let Some(r) = self.cache.lock().unwrap().get(&key) {
            return Some(r.clone());
        }
        let (raw, provider, model) = match self.build(name) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    "Provider '{name}' could not be resolved; the agent runs on the local model: {e}"
                );
                return None;
            }
        };
        self.ledger
            .set_pricing(name, provider.as_ref().and_then(|p| p.pricing.clone()));
        let logged = UsageLoggingBackend::new(raw, self.sink.clone(), USAGE_CATEGORY, name)
            .with_provider(Some(name), Some(&model), Some(self.ledger.clone()));
        let route = ProviderRoute {
            name: name.to_string(),
            model,
            client: Arc::new(BackendAgentClient(Arc::new(logged))),
            context_window: provider.as_ref().and_then(|p| p.context_window),
            max_tokens: provider.as_ref().and_then(|p| p.max_tokens),
        };
        Some(
            self.cache
                .lock()
                .unwrap()
                .entry(key)
                .or_insert(route)
                .clone(),
        )
    }

    async fn acquire(
        &self,
        agent: &Arc<AgentContext>,
        spawner: Option<&Arc<AgentContext>>,
        waiting: &(dyn Fn() + Send + Sync),
    ) -> Option<AgentLease> {
        self.inner.acquire(agent, spawner, waiting).await
    }

    fn uses_slots(&self) -> bool {
        self.inner.uses_slots()
    }

    fn ambient_backend(
        &self,
        client: Option<&Arc<dyn AgentChatClient>>,
    ) -> Option<Arc<dyn ChatBackend>> {
        self.inner.ambient_backend(client)
    }

    async fn release(&self, agent: &AgentContext) {
        self.inner.release(agent).await
    }

    fn note_type(&self, agent: &AgentContext, type_def: &AgentTypeDefinition) {
        self.inner.note_type(agent, type_def)
    }

    fn usage_ledger(&self) -> Option<Arc<ProviderUsageLedger>> {
        Some(self.ledger.clone())
    }
}
