//! Probes the API with descending `max_tokens` values to find the largest one it accepts
//! (`MaxTokensNegotiator.cs`), so providers with per-model limits (Groq) don't reject every
//! request. Skipped when `lmStudio.maxTokens` is set. Also the raw "server error probe" the
//! turn loop sends after an empty reply (`ErrorRecoveryHandler.ProbeForServerErrorAsync`).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use super::f32_json;
use super::types::{ChatMessage, ChatOptions};
use crate::config::OrcaConfig;

const FALLBACK_VALUES: [i32; 4] = [32768, 16384, 8192, 4096];

/// "less than or equal to `8192`" or "less than or equal to 8192".
static LIMIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)less than or equal to `?(\d+)`?").expect("valid regex"));

/// The probe body (`ProbePayload`): model, messages, max_tokens, temperature, stream.
pub fn probe_payload(model: &str, messages: Value, max_tokens: i32, temperature: f32) -> Value {
    json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": f32_json(temperature),
        "stream": false,
    })
}

/// POST the probe; `(accepted, error body)`. A transport failure is `(false, None)`.
pub async fn probe(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    max_tokens: i32,
) -> (bool, Option<String>) {
    let body = probe_payload(
        model,
        json!([{"role": "user", "content": "hi"}]),
        max_tokens,
        0.0,
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let Ok(response) = http
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
    else {
        return (false, None);
    };
    if response.status().is_success() {
        return (true, None);
    }
    (false, response.text().await.ok())
}

/// The accepted `max_tokens`, or None when negotiation is skipped (`lmStudio.maxTokens` set)
/// or every value was refused.
pub async fn negotiate(config: &OrcaConfig, http: &reqwest::Client) -> Option<i32> {
    let lm = &config.lm_studio;
    if let Some(v) = lm.max_tokens {
        tracing::debug!("maxTokens already set to {v}; skipping negotiation");
        return None;
    }
    let base_url = lm.base_url.trim_end_matches('/');
    let api_key = &lm.api_key;
    let model = lm.primary_model().unwrap_or("default");

    let first = FALLBACK_VALUES[0];
    let (accepted, error_body) = probe(http, base_url, api_key, model, first).await;
    if accepted {
        tracing::info!("max tokens negotiation: {first} accepted");
        return Some(first);
    }
    if let Some(parsed) = error_body.as_deref().and_then(parse_limit_from_error) {
        tracing::info!("max tokens negotiation: parsed limit {parsed} from the error");
        return Some(parsed);
    }
    for value in &FALLBACK_VALUES[1..] {
        if probe(http, base_url, api_key, model, *value).await.0 {
            tracing::info!("max tokens negotiation: {value} accepted on fallback");
            return Some(*value);
        }
    }
    tracing::warn!("max tokens negotiation failed: all probe values rejected");
    None
}

/// The numeric limit in an API error ("less than or equal to `8192`"), looking in the JSON
/// error message first.
pub fn parse_limit_from_error(error_body: &str) -> Option<i32> {
    let mut search = error_body.to_string();
    if let Ok(doc) = serde_json::from_str::<Value>(error_body)
        && let Some(err) = doc.get("error")
    {
        if let Some(s) = err.as_str() {
            search = s.to_string();
        } else if let Some(m) = err.get("message").and_then(Value::as_str) {
            search = m.to_string();
        }
    }
    LIMIT
        .captures(&search)
        .and_then(|c| c[1].parse::<i32>().ok())
}

/// `ProbeForServerErrorAsync`: the same conversation as a plain non-streaming request (text
/// only), to surface the error text a failed stream hides. Returns the server's error message
/// when it rejects the request, None when it accepts it or does not answer.
pub async fn probe_for_server_error(
    http: &reqwest::Client,
    config: &OrcaConfig,
    messages: &[ChatMessage],
    options: &ChatOptions,
) -> Option<String> {
    let wire: Vec<Value> = messages
        .iter()
        .map(|m| json!({"role": m.role.as_str(), "content": probe_text(m)}))
        .collect();
    // C# sends options.ModelId, which BuildChatOptions always sets to the primary model.
    let model = config
        .lm_studio
        .primary_model()
        .unwrap_or("default")
        .to_string();
    let body = probe_payload(
        &model,
        Value::Array(wire),
        options.max_output_tokens.unwrap_or(50),
        options.temperature.unwrap_or(0.7),
    );
    let url = format!(
        "{}/chat/completions",
        config.lm_studio.base_url.trim_end_matches('/')
    );
    let response = http
        .post(url)
        .header(
            "Authorization",
            format!("Bearer {}", config.lm_studio.api_key),
        )
        .json(&body)
        .send()
        .await
        .ok()?;
    if response.status().is_success() {
        return None;
    }
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    tracing::warn!("server error probe got {status}: {text}");
    if let Ok(doc) = serde_json::from_str::<Value>(&text)
        && let Some(err) = doc.get("error")
    {
        return Some(match err {
            Value::String(s) => s.clone(),
            _ => match err.get("message") {
                Some(Value::String(m)) => m.clone(),
                // A non-string message: C#'s GetString() would throw and the probe give up.
                Some(other) if !other.is_null() => return None,
                _ => text,
            },
        });
    }
    Some(text)
}

/// The text parts of a message as C# joins them (tool results and reasoning carry none).
fn probe_text(m: &ChatMessage) -> String {
    match m.role {
        super::types::Role::Tool => String::new(),
        _ => m.content.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    //! `ParseLimitFromError` cases of `MaxTokensNegotiatorTests.cs`; the negotiation cases run
    //! against the fake server in `tests/max_tokens_negotiator.rs`.
    use super::*;

    #[test]
    fn parse_limit_from_error_valid_patterns_returns_expected_value() {
        for (body, expected) in [
            (
                r#"{"error":{"message":"max_tokens must be less than or equal to `8192`"}}"#,
                8192,
            ),
            (
                r#"{"error":{"message":"max_tokens must be less than or equal to 131072"}}"#,
                131072,
            ),
            (
                r#"{"error":"max_tokens must be less than or equal to `16384`"}"#,
                16384,
            ),
            ("max_tokens must be less than or equal to 4096", 4096),
        ] {
            assert_eq!(parse_limit_from_error(body), Some(expected), "{body}");
        }
    }

    #[test]
    fn parse_limit_from_error_no_parseable_returns_null() {
        for body in [
            "",
            "some random error",
            r#"{"error":{"message":"rate limit exceeded"}}"#,
            "max_tokens is too large",
        ] {
            assert_eq!(parse_limit_from_error(body), None, "{body}");
        }
    }
}
