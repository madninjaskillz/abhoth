//! `ModelDiscovery`: the models the server lists at `/v1/models` (non-LLM models such as
//! Whisper or embeddings left out), cached for five minutes per process. Errors give an empty
//! list, as in C#.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use orca_core::config::OrcaConfig;
use serde_json::Value;

static CACHE: Mutex<Option<(Vec<String>, Instant)>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_secs(300);

/// `GetAvailableModelsAsync`.
pub async fn available_models(config: &OrcaConfig) -> Vec<String> {
    if let Some((models, at)) = CACHE.lock().unwrap().as_ref()
        && at.elapsed() < CACHE_TTL
    {
        return models.clone();
    }
    let base = config.lm_studio.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let Ok(http) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return Vec::new();
    };
    let response = http
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", config.lm_studio.api_key),
        )
        .send()
        .await;
    let Ok(response) = response else {
        tracing::warn!("Failed to discover models from {url}");
        return Vec::new();
    };
    if !response.status().is_success() {
        tracing::warn!("Failed to discover models from {url}: {}", response.status());
        return Vec::new();
    }
    let Ok(body) = response.json::<Value>().await else {
        return Vec::new();
    };
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let models: Vec<String> = data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            (!is_non_llm_model(&id, item)).then_some(id)
        })
        .collect();
    tracing::info!("Found {} models at {url}", models.len());
    *CACHE.lock().unwrap() = Some((models.clone(), Instant::now()));
    models
}

/// `InvalidateCache`.
pub fn invalidate_cache() {
    *CACHE.lock().unwrap() = None;
}

/// `IsNonLlmModel`: a tiny `context_window` (under 1024), or a known speech / embedding /
/// image model family by name.
pub fn is_non_llm_model(model_id: &str, item: &Value) -> bool {
    if let Some(cw) = item.get("context_window").and_then(Value::as_i64)
        && cw < 1024
    {
        return true;
    }
    let id = model_id.to_lowercase();
    ["whisper", "orpheus", "text-embedding", "tts-", "dall-e"]
        .iter()
        .any(|p| id.contains(p))
}

#[cfg(test)]
mod tests {
    //! Ported from ModelDiscoveryTests.cs.
    use super::*;
    use serde_json::json;

    #[test]
    fn is_non_llm_model_name_patterns_without_metadata() {
        for (id, expected) in [
            ("whisper-large-v3", true),
            ("whisper-large-v3-turbo", true),
            ("canopylabs/orpheus-v1-english", true),
            ("canopylabs/orpheus-arabic-saudi", true),
            ("text-embedding-ada-002", true),
            ("tts-1-hd", true),
            ("dall-e-3", true),
            ("llama-3.3-70b-versatile", false),
            ("qwen/qwen3-32b", false),
            ("meta-llama/llama-4-scout-17b-16e-instruct", false),
        ] {
            let item = json!({"id": id, "object": "model"});
            assert_eq!(is_non_llm_model(id, &item), expected, "{id}");
        }
    }

    #[test]
    fn is_non_llm_model_context_window_filters_small_values() {
        for (cw, expected) in [
            (448, true),
            (512, true),
            (1023, true),
            (1024, false),
            (4096, false),
            (131072, false),
        ] {
            let item = json!({"id": "some-model", "object": "model", "context_window": cw});
            assert_eq!(is_non_llm_model("some-model", &item), expected, "{cw}");
        }
    }

    #[test]
    fn is_non_llm_model_no_metadata_llm_name_passes_through() {
        let item = json!({"id": "my-local-model", "object": "model"});
        assert!(!is_non_llm_model("my-local-model", &item));
    }

    #[test]
    fn is_non_llm_model_context_window_overrides_name() {
        let id = "meta-llama/llama-prompt-guard-2-86m";
        let item = json!({"id": id, "object": "model", "context_window": 512});
        assert!(is_non_llm_model(id, &item));
    }
}
