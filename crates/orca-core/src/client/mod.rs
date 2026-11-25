//! The OpenAI-compatible chat client and request shaping.

pub mod backend;
pub mod context_overflow;
pub mod context_window_detector;
pub mod max_tokens_negotiator;
pub mod openai;
pub mod request_parameters;
pub mod served_models;
pub mod demo;
pub mod server_identity;
pub mod tool_images;
pub mod types;

pub use backend::{ChatBackend, is_transport_failure};
pub use openai::{ChatError, ClientSettings, OpenAiClient, SseParser};
pub use served_models::{ServedModel, ServedModels};
pub use server_identity::{ServerIdentity, ServerInfo, ServerKind};
pub use types::*;

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
    let lm = &config.lm_studio;
    let model = lm
        .primary_model()
        .map(str::to_string)
        .ok_or(ChatError::NoModel)?;
    let mut settings = ClientSettings::new(lm.base_url.clone(), lm.resolve_api_key(), Some(model));
    settings.request_timeout = std::time::Duration::from_secs(lm.timeout_seconds.max(1) as u64);
    settings.idle_timeout =
        std::time::Duration::from_secs(lm.streaming_timeout_seconds.max(1) as u64);
    settings.session_field = config.agent.uncensored.field();
    settings.vision = lm.vision;
    Ok(OpenAiClient::new(settings, served))
}
