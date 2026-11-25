//! Detects the loaded model's real context window from the server (`ContextWindowDetector.cs`)
//! instead of trusting the 8192 default. LM Studio reports it on `/api/v0/models`; Ollama on
//! `/api/show`. Failure returns None and the configured value stands.

use serde_json::{Value, json};

/// Probe `base_url` (an OpenAI-style `.../v1` URL) for `model`'s context window. The caller's
/// `http` client sets the timeout.
pub async fn detect(base_url: &str, model: Option<&str>, http: &reqwest::Client) -> Option<i32> {
    let mut root = base_url.trim_end_matches('/');
    if root.len() >= 3 && root[root.len() - 3..].eq_ignore_ascii_case("/v1") {
        root = &root[..root.len() - 3];
    }
    match try_lm_studio(root, model, http).await {
        Some(n) => Some(n),
        None => try_ollama(root, model, http).await,
    }
}

fn positive_i32(v: &Value) -> Option<i32> {
    v.as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n > 0)
}

async fn try_lm_studio(root: &str, model: Option<&str>, http: &reqwest::Client) -> Option<i32> {
    let result: Result<Option<i32>, reqwest::Error> = async {
        let text = http
            .get(format!("{root}/api/v0/models"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            return Ok(None);
        };
        let Some(data) = doc.get("data").and_then(Value::as_array) else {
            return Ok(None);
        };
        for entry in data {
            let id = entry.get("id").and_then(Value::as_str);
            if let Some(m) = model
                && !id.is_some_and(|id| id.eq_ignore_ascii_case(m))
            {
                continue;
            }
            if let Some(n) = entry.get("max_context_length").and_then(positive_i32) {
                return Ok(Some(n));
            }
        }
        Ok(None)
    }
    .await;
    result.unwrap_or_else(|e| {
        tracing::debug!("LM Studio context window probe failed: {e}");
        None
    })
}

async fn try_ollama(root: &str, model: Option<&str>, http: &reqwest::Client) -> Option<i32> {
    let model = model?;
    let result: Result<Option<i32>, reqwest::Error> = async {
        let response = http
            .post(format!("{root}/api/show"))
            .json(&json!({ "name": model }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let Ok(doc) = serde_json::from_str::<Value>(&response.text().await?) else {
            return Ok(None);
        };
        let Some(info) = doc.get("model_info").and_then(Value::as_object) else {
            return Ok(None);
        };
        // The key is architecture-prefixed, e.g. "llama.context_length".
        Ok(info.iter().find_map(|(k, v)| {
            k.to_ascii_lowercase()
                .ends_with(".context_length")
                .then(|| positive_i32(v))
                .flatten()
        }))
    }
    .await;
    result.unwrap_or_else(|e| {
        tracing::debug!("Ollama context window probe failed: {e}");
        None
    })
}
