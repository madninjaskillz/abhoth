//! Context management in the agent session: usage tracking, auto-compaction before a request
//! (`AgentLoopRunner.CompactIfNeededAsync`) and overflow recovery
//! (`TryRecoverFromOverflowAsync`), against the fake OpenAI server.

use std::sync::Arc;

use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, NullObserver, ToolRegistry};
use orca_core::chat::compaction;
use orca_core::client::{
    ChatError, ChatMessage, ClientSettings, OpenAiClient, ServedModels, Usage,
};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::json;

fn config(window: i32) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.models = vec!["fake-model".into()];
    c.context.context_window_size = window;
    c.context.keep_recent_tokens = 500;
    c.context.keep_first_tokens = 0;
    c.context.thinking_aware_compaction = false;
    c.agent.verification_mode = "off".into();
    c
}

fn session(server: &FakeOpenAi, config: OrcaConfig) -> AgentSession {
    let client = OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", Some("fake-model".into())),
        Arc::new(ServedModels::default()),
    );
    let mut s = AgentSession::new(
        config,
        client,
        ToolRegistry::new(),
        ToolContext::new(std::env::temp_dir()),
    );
    s.set_system_prompt("You are a test agent.".into());
    s
}

fn with_prompt_tokens(mut r: Reply, tokens: i64) -> Reply {
    r.prompt_tokens = tokens;
    r
}

#[tokio::test]
async fn run_turn_compacts_once_the_reported_prompt_passes_the_hard_limit() {
    // Window 32768: hard limit 26215. The second response reports 32000 prompt tokens, so the
    // check before the third request summarizes the history (a non-streamed request).
    let server = FakeOpenAi::start(vec![
        with_prompt_tokens(Reply::tool("nope", json!({"blob": "a".repeat(3000)})), 1000),
        with_prompt_tokens(
            Reply::tool("nope", json!({"blob": "b".repeat(3000)})),
            32000,
        ),
        Reply::text("## Goal\nSUMMARY"),
        Reply::text("All done: nothing else to change in this test run."),
    ])
    .await;
    let mut s = session(&server, config(32768));

    s.run_turn("do the thing", &mut NullObserver).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[2].body.get("stream"), None, "the summary request");
    let last = requests[3].body["messages"].as_array().unwrap();
    let checkpoint = last
        .iter()
        .find_map(|m| m["content"].as_str().filter(|t| t.contains("<summary>")))
        .expect("the next request carries the checkpoint");
    assert!(checkpoint.contains("SUMMARY"));
    assert!(
        s.compaction_notices
            .iter()
            .any(|n| n.starts_with("Compacted"))
    );
    assert!(
        s.messages
            .iter()
            .any(|m| compaction::try_get_summary(m).is_some())
    );
}

#[tokio::test]
async fn run_turn_leaves_a_small_history_alone() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("nope", json!({"x": 1})),
        Reply::text("Finished the work; the answer is in the reply above."),
    ])
    .await;
    let mut s = session(&server, config(32768));

    s.run_turn("hi", &mut NullObserver).await.unwrap();

    assert_eq!(server.requests().len(), 2);
    assert!(s.compaction_notices.is_empty());
    // The second request's usage (100) is the base; the final reply (52 chars) is estimated.
    assert_eq!(s.estimate_context_tokens(), 100 + 15);
}

#[tokio::test]
async fn usage_from_a_rewritten_history_is_ignored() {
    let server = FakeOpenAi::start(vec![Reply::text("x")]).await;
    let mut s = session(&server, config(32768));
    s.messages.push(ChatMessage::user("hello"));
    let mark = s.context_mark();
    s.messages[1].content = Some("changed before the response came back".into());
    s.record_prompt_usage(
        mark,
        &Usage {
            prompt_tokens: Some(20_000),
            ..Usage::default()
        },
    );
    assert!(s.estimate_context_tokens() < 1000);
}

fn overflow_error() -> ChatError {
    ChatError::Http {
        status: 400,
        url: "http://x/v1/chat/completions".into(),
        body: "request (9000 tokens) exceeds the available context size (4096 tokens)".into(),
    }
}

#[tokio::test]
async fn recover_from_overflow_adopts_the_servers_window_compacts_and_retries_once() {
    let server = FakeOpenAi::start(vec![Reply::text("## Goal\nSUMMARY")]).await;
    let mut s = session(&server, config(32768));
    for i in 0..6 {
        s.messages.push(ChatMessage::user(format!(
            "question {i} {}",
            "q".repeat(800)
        )));
        s.messages.push(ChatMessage::assistant(format!(
            "answer {i} {}",
            "a".repeat(800)
        )));
    }
    let before = s.messages.len();

    assert!(s.recover_from_overflow(&overflow_error()).await);

    assert_eq!(s.config.context.context_window_size, 4096);
    assert!(s.messages.len() < before);
    assert_eq!(s.messages[0].role, orca_core::client::Role::System);
    assert!(compaction::try_get_summary(&s.messages[1]).is_some());
    assert!(
        s.compaction_notices
            .iter()
            .any(|n| n.contains("Server context window is 4,096 tokens (configured 32,768)"))
    );
    // Once per turn.
    assert!(!s.recover_from_overflow(&overflow_error()).await);
}

#[tokio::test]
async fn recover_from_overflow_ignores_other_errors_and_respects_auto_compact_off() {
    let server = FakeOpenAi::start(vec![Reply::text("## Goal\nSUMMARY")]).await;
    let mut s = session(&server, config(32768));
    let other = ChatError::Http {
        status: 500,
        url: "u".into(),
        body: "server exploded".into(),
    };
    assert!(!s.recover_from_overflow(&other).await);
    assert!(!s.overflow_recovery_used);

    let mut cfg = config(32768);
    cfg.context.auto_compact_enabled = false;
    let mut s = session(&server, cfg);
    assert!(!s.recover_from_overflow(&overflow_error()).await);
    assert!(server.requests().is_empty());
    assert!(
        s.compaction_notices
            .iter()
            .any(|n| n.contains("auto-compaction is disabled"))
    );
}
