//! The OpenAI-compatible chat client and request shaping.

pub mod backend;
pub mod context_overflow;
pub mod context_window_detector;
pub mod demo;
pub mod max_tokens_negotiator;
pub mod openai;
pub mod request_parameters;
pub mod served_models;
pub mod server_identity;
pub mod tool_images;
pub mod types;
pub mod uncensored_gate;
pub mod usage;

pub use backend::{ChatBackend, is_transport_failure};
pub use openai::{ChatError, ClientSettings, OpenAiClient, SseParser};
pub use served_models::{ServedModel, ServedModels};
pub use server_identity::{ServerIdentity, ServerInfo, ServerKind};
pub use types::*;
pub use usage::{
    ProviderUsage, ProviderUsageLedger, RequestStats, ServerTimings, UsageLoggingBackend,
};

use serde_json::Value;

/// An `f32` as JSON with its shortest decimal form (`0.05`, not `0.05000000074505806`), as the
/// C# SDK writes a `float`.
pub fn f32_json(v: f32) -> Value {
    // System.Text.Json writes a whole float without a fraction ("0", "1").
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        return Value::from(v as i64);
    }
    format!("{v}")
        .parse::<f64>()
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// A client for the main model server described by `lmStudio` (`LmStudioClientFactory`).
pub fn client_from_config(
    config: &crate::config::OrcaConfig,
    served: std::sync::Arc<ServedModels>,
) -> Result<OpenAiClient, ChatError> {
    let model = config
        .lm_studio
        .primary_model()
        .map(str::to_string)
        .ok_or(ChatError::NoModel)?;
    Ok(client_for_model(config, served, &model))
}

/// A client for one named model on the main model server (`LmStudioClientFactory.Create(model)`):
/// pair programming's alternate models and voters.
pub fn client_for_model(
    config: &crate::config::OrcaConfig,
    served: std::sync::Arc<ServedModels>,
    model: &str,
) -> OpenAiClient {
    let lm = &config.lm_studio;
    let model = model.to_string();
    let mut settings = ClientSettings::new(lm.base_url.clone(), lm.resolve_api_key(), Some(model));
    settings.request_timeout = std::time::Duration::from_secs(lm.timeout_seconds.max(1) as u64);
    settings.idle_timeout =
        std::time::Duration::from_secs(lm.streaming_timeout_seconds.max(1) as u64);
    settings.session_field = config.agent.uncensored.field();
    settings.uncensored_gate = Some(uncensored_gate::UncensoredGate::for_config(
        config,
        &lm.base_url,
        Some(lm.resolve_api_key()),
    ));
    settings.vision = lm.vision;
    OpenAiClient::new(settings, served)
}

/// A client for a named OpenAI-compatible provider (`ChatClientRegistry.CreateOpenAiCompatClient`):
/// its base URL (else lmStudio.baseUrl) and key; the `/uncensored` field only when
/// `agent.uncensored.providers` lists it or it is on a local server's host.
pub fn client_for_provider(
    config: &crate::config::OrcaConfig,
    served: std::sync::Arc<ServedModels>,
    provider_name: &str,
    provider: &crate::config::ProviderConfig,
    model: &str,
) -> OpenAiClient {
    let lm = &config.lm_studio;
    let base_url = if provider.base_url.trim().is_empty() {
        lm.base_url.clone()
    } else {
        provider.base_url.clone()
    };
    let key = provider_api_key(provider);
    let mut settings = ClientSettings::new(base_url.clone(), key.clone(), Some(model.to_string()));
    settings.request_timeout = std::time::Duration::from_secs(lm.timeout_seconds.max(1) as u64);
    settings.idle_timeout =
        std::time::Duration::from_secs(lm.streaming_timeout_seconds.max(1) as u64);
    settings.session_field = config
        .agent
        .uncensored
        .field_for(config, provider_name, &base_url);
    settings.uncensored_gate = Some(uncensored_gate::UncensoredGate::for_config(
        config,
        &base_url,
        Some(key),
    ));
    settings.vision = lm.vision;
    OpenAiClient::new(settings, served)
}

/// `ChatClientRegistry.ResolveApiKey`: the provider's env variable, else its key, else
/// "not-needed".
fn provider_api_key(provider: &crate::config::ProviderConfig) -> String {
    provider
        .api_key_env
        .as_deref()
        .filter(|e| !e.trim().is_empty())
        .and_then(|e| std::env::var(e).ok())
        .filter(|k| !k.trim().is_empty())
        .or_else(|| provider.api_key.clone().filter(|k| !k.trim().is_empty()))
        .unwrap_or_else(|| "not-needed".into())
}
