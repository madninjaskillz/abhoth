//! `MaxTokensNegotiatorTests.cs` against the fake server (each probe is a chat completion
//! request that takes the next scripted reply).

use std::time::Duration;

use orca_core::client::max_tokens_negotiator::negotiate;
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::json;

fn config(server: &FakeOpenAi, max_tokens: Option<i32>) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.base_url = server.base_url();
    c.lm_studio.api_key = "test-key".into();
    c.lm_studio.models = vec!["test-model".into()];
    c.lm_studio.max_tokens = max_tokens;
    c
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn accepted() -> Reply {
    Reply::text("ok")
}

#[tokio::test]
async fn negotiate_async_user_has_explicit_max_tokens_returns_null() {
    let server = FakeOpenAi::start(vec![accepted()]).await;
    assert_eq!(negotiate(&config(&server, Some(2048)), &http()).await, None);
    assert_eq!(server.requests().len(), 0);
}

#[tokio::test]
async fn negotiate_async_api_accepts_first_value_returns_32768() {
    let server = FakeOpenAi::start(vec![accepted()]).await;
    assert_eq!(
        negotiate(&config(&server, None), &http()).await,
        Some(32768)
    );
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body,
        json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 32768, "temperature": 0, "stream": false})
    );
    assert_eq!(reqs[0].header("authorization"), Some("Bearer test-key"));
}

#[tokio::test]
async fn negotiate_async_api_rejects_with_parseable_limit_returns_parsed_value() {
    let server = FakeOpenAi::start(vec![Reply::error(
        400,
        r#"{"error":{"message":"max_tokens must be less than or equal to `8192`","type":"invalid_request_error"}}"#,
    )])
    .await;
    assert_eq!(negotiate(&config(&server, None), &http()).await, Some(8192));
    // Only one probe needed.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn negotiate_async_api_rejects_with_parseable_limit_no_backticks_returns_parsed_value() {
    let server = FakeOpenAi::start(vec![Reply::error(
        400,
        r#"{"error":"max_tokens must be less than or equal to 4096"}"#,
    )])
    .await;
    assert_eq!(negotiate(&config(&server, None), &http()).await, Some(4096));
}

#[tokio::test]
async fn negotiate_async_api_rejects_without_limit_falls_back_to_next_accepted() {
    // The first three probes fail with an unparseable error, the fourth (4096) succeeds.
    let server = FakeOpenAi::start(vec![
        Reply::error(400, "unknown error"),
        Reply::error(400, "unknown error"),
        Reply::error(400, "unknown error"),
        accepted(),
    ])
    .await;
    assert_eq!(negotiate(&config(&server, None), &http()).await, Some(4096));
    let sizes: Vec<_> = server
        .requests()
        .iter()
        .map(|r| r.body["max_tokens"].clone())
        .collect();
    assert_eq!(sizes, [32768, 16384, 8192, 4096]);
}

#[tokio::test]
async fn negotiate_async_all_probes_fail_returns_null() {
    let server = FakeOpenAi::start(vec![Reply::error(400, "unknown error"); 4]).await;
    assert_eq!(negotiate(&config(&server, None), &http()).await, None);
    // Tried all 4 fallback values.
    assert_eq!(server.requests().len(), 4);
}
