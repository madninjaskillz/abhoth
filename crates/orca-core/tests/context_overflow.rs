//! Port of `tests/OpenOrca.Core.Tests/Client/ContextOverflowTests.cs`, plus tests for the
//! context window probe (`ContextWindowDetector`, which has no C# tests).

use std::sync::Arc;

use orca_core::client::context_overflow::{
    is_overflow, is_overflow_error, try_parse_context_size, try_parse_context_size_error,
};
use orca_core::client::{
    ChatMessage, ChatOptions, ClientSettings, OpenAiClient, ServedModels, context_window_detector,
};
use orca_testkit::{FakeOpenAi, Reply};

// Captured live from a LiteLLM proxy in front of llama-server.
const LITE_LLM_BODY: &str = "{\"error\":{\"message\":\"litellm.ContextWindowExceededError: litellm.BadRequestError: ContextWindowExceededError: OpenAIException - request (200010 tokens) exceeds the available context size (131072 tokens), try increasing it\\nmodel=qwen3.8-27b-coder. context_window_fallbacks=None. fallbacks=None.\",\"type\":null,\"param\":null,\"code\":\"400\"}}";

#[test]
fn recognises_provider_overflow_messages() {
    for message in [
        "request (200010 tokens) exceeds the available context size (131072 tokens), try increasing it",
        "This model's maximum context length is 32768 tokens. However, you requested 40000 tokens",
        "Your input exceeds the context window of this model.",
        "{\"error\":{\"code\":\"context_length_exceeded\"}}",
        "prompt is too long: 210000 tokens > 200000 maximum",
        "Trying to keep the first 9000 tokens when context the overflows. However, the model is loaded with context length of 8192 tokens, which is not enough. Try to load the model with a larger context length, or provide a shorter input. The number of tokens to keep is greater than the context length",
        "litellm.ContextWindowExceededError: something",
        "prompt too long; exceeded max context length by 100 tokens",
    ] {
        assert!(is_overflow(message), "{message}");
    }
}

#[test]
fn ignores_other_errors() {
    for message in [
        "max_tokens must be less than or equal to `8192`",
        "Rate limit reached: too many tokens per minute, token limit exceeded",
        "HTTP 503 Service Unavailable",
        "connection refused",
        "",
    ] {
        assert!(!is_overflow(message), "{message}");
    }
}

#[test]
fn parses_reported_context_size() {
    for (message, expected) in [
        ("exceeds the available context size (131072 tokens)", 131072),
        ("maximum context length is 32,768 tokens", 32768),
        (
            "Input (9000 tokens) is longer than the model's context length (8192 tokens)",
            8192,
        ),
        (
            "prompt has 5,000 tokens, but the configured context size is 4,096 tokens",
            4096,
        ),
    ] {
        assert_eq!(try_parse_context_size(message), Some(expected), "{message}");
    }
}

#[test]
fn parses_nothing_when_no_size_stated() {
    assert_eq!(try_parse_context_size("context_length_exceeded"), None);
}

#[derive(Debug)]
struct Wrapper(std::io::Error);
impl std::fmt::Display for Wrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wrapper")
    }
}
impl std::error::Error for Wrapper {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

#[test]
fn walks_inner_exceptions() {
    let err = Wrapper(std::io::Error::other(
        "exceeds the available context size (4096 tokens)",
    ));
    assert!(is_overflow_error(&err));
    assert_eq!(try_parse_context_size_error(&err), Some(4096));
}

#[tokio::test]
async fn real_sdk_exception_from_400_is_recognised() {
    // The error the client returns for the live 400 must carry the text.
    let server = FakeOpenAi::start(vec![Reply::error(400, LITE_LLM_BODY)]).await;
    let client = OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", Some("m".into())),
        Arc::new(ServedModels::default()),
    );
    let err = client
        .stream(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(is_overflow_error(&err), "{err}");
    assert_eq!(try_parse_context_size_error(&err), Some(131072));
}

// ── ContextWindowDetector ──

/// A one-route-per-path HTTP server answering every request with a fixed JSON body by path.
async fn json_server(routes: Vec<(&'static str, &'static str)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| ("200 OK", *b))
                    .unwrap_or(("404 Not Found", "{}"));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn detector_reads_lm_studio_max_context_length_for_the_model() {
    let base = json_server(vec![(
        "/api/v0/models",
        r#"{"data":[{"id":"other","max_context_length":4096},{"id":"Qwen-Coder","max_context_length":32768}]}"#,
    )])
    .await;
    let http = reqwest::Client::new();
    assert_eq!(
        context_window_detector::detect(&base, Some("qwen-coder"), &http).await,
        Some(32768)
    );
    // No model named: the first entry with a window.
    assert_eq!(
        context_window_detector::detect(&base, None, &http).await,
        Some(4096)
    );
}

#[tokio::test]
async fn detector_falls_back_to_ollama_show() {
    let base = json_server(vec![(
        "/api/show",
        r#"{"model_info":{"general.architecture":"llama","llama.context_length":131072}}"#,
    )])
    .await;
    let http = reqwest::Client::new();
    assert_eq!(
        context_window_detector::detect(&base, Some("llama3"), &http).await,
        Some(131072)
    );
    // Ollama needs a model name.
    assert_eq!(
        context_window_detector::detect(&base, None, &http).await,
        None
    );
}

#[tokio::test]
async fn detector_returns_none_when_nothing_answers() {
    let base = json_server(vec![]).await;
    let http = reqwest::Client::new();
    assert_eq!(
        context_window_detector::detect(&base, Some("m"), &http).await,
        None
    );
}
