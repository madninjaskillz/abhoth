//! Ports of `RequestStatsTests`, `UsageLoggingChatClientTests` and the ledger / usage-log
//! tests of `AgentSwarmTests`.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::client::{ClientSettings, OpenAiClient, ServedModels};
use crate::logging::MemoryLog;

fn chunk(delta: &str) -> String {
    format!(
        "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":null}}]}}\n\n"
    )
}

const TAIL: &str = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\
\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":12,\"total_tokens\":1012,\"prompt_tokens_details\":{\"cached_tokens\":900}},\
\"timings\":{\"cache_n\":900,\"prompt_n\":100,\"prompt_ms\":85.5,\"predicted_n\":12,\"predicted_ms\":140.2}}\n\n\
data: [DONE]\n\n";

/// Shaped like Strata's stream: one token per chunk, usage + timings on the last chunk.
fn body() -> String {
    let mut s = chunk(r#"{"role":"assistant","content":""}"#);
    for i in 0..5 {
        s.push_str(&chunk(&format!(r#"{{"reasoning_content":"t{i}"}}"#)));
    }
    s.push_str(&chunk(r#"{"content":"ok"}"#));
    s.push_str(&chunk(
        r#"{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}"#,
    ));
    s.push_str(&chunk(
        r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\"}"}}]}"#,
    ));
    s.push_str(TAIL);
    s
}

/// Serves `body` as an SSE stream at `/v1/chat/completions`.
async fn sse_client(body: String) -> OpenAiClient {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let body = body.clone();
            async move { ([("content-type", "text/event-stream")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    OpenAiClient::new(
        ClientSettings::new(format!("http://{addr}/v1"), "k", Some("m".into())),
        Arc::new(ServedModels::new()),
    )
}

fn logged(inner: OpenAiClient, log: &Arc<MemoryLog>, route: &str) -> UsageLoggingBackend {
    UsageLoggingBackend::new(
        Arc::new(inner),
        Some(log.clone() as Arc<dyn LogSink>),
        USAGE_CATEGORY,
        route,
    )
    .with_provider(None, None, Some(Arc::new(ProviderUsageLedger::new())))
}

#[tokio::test]
async fn streaming_logs_reasoning_content_tool_split_timings_and_calls() {
    let log = Arc::new(MemoryLog::default());
    let client = logged(sse_client(body()).await, &log, "lead on A1");
    let mut options = ChatOptions::default();
    options.set("reasoning_effort", json!("low"));

    client
        .stream_with_control("lead", &[ChatMessage::user("hi")], &options, &mut |_| {
            StreamControl::Continue
        })
        .await
        .unwrap();

    let lines = log.messages();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
        line.starts_with(
            "Slot usage lead on A1: prompt 1000, cached 900, re-read 100, output 12, reasoning 5, "
        ),
        "{line}"
    );
    assert!(line.contains("| content=1 tool=6 "), "{line}");
    assert!(line.contains("calls=read"), "{line}");
    assert!(line.contains("effort=low"), "{line}");
    assert!(
        line.contains("prompt_n=100 cache_n=900 prompt_ms=86 predicted_n=12 predicted_ms=140"),
        "{line}"
    );
    assert!(
        regex::Regex::new(r"first_think_ms=\d+ first_answer_ms=\d+")
            .unwrap()
            .is_match(line),
        "{line}"
    );
    assert!(!line.contains("aborted"), "{line}");
}

#[tokio::test]
async fn streaming_stopped_early_still_logs_what_streamed() {
    let log = Arc::new(MemoryLog::default());
    let client = logged(sse_client(body()).await, &log, "agent on B1");

    let mut n = 0;
    client
        .stream_with_control(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |e| {
                if matches!(e, StreamEvent::Reasoning(_)) {
                    n += 1;
                    if n == 3 {
                        return StreamControl::Stop;
                    }
                }
                StreamControl::Continue
            },
        )
        .await
        .unwrap();

    let lines = log.messages();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.contains("reasoning 3,"), "{line}");
    assert!(line.contains("aborted=1"), "{line}");
    assert!(line.contains("first_answer_ms=-"), "{line}");
}

#[test]
fn split_prefers_server_reasoning_count_and_scales_multi_token_chunks() {
    let mut stats = RequestStats::new();
    stats.observe_event(&StreamEvent::Usage(Usage {
        completion_tokens: Some(10),
        reasoning_tokens: Some(7),
        ..Usage::default()
    }));
    assert_eq!(stats.split(), (7, 0, 3));

    let mut packed = RequestStats::new();
    for _ in 0..8 {
        packed.observe_event(&StreamEvent::Reasoning("abc".into()));
    }
    for _ in 0..8 {
        packed.observe_event(&StreamEvent::Text("xy".into()));
    }
    packed.observe_event(&StreamEvent::Usage(Usage {
        completion_tokens: Some(8),
        ..Usage::default()
    }));
    assert_eq!(packed.split(), (4, 4, 0));
}

#[test]
fn server_timings_parses_strata_shape() {
    let t = ServerTimings::parse(
        r#"{"cache_n":3,"prompt_n":52,"prompt_ms":857.1,"predicted_n":57,"predicted_ms":1035.0,"draft_n":48}"#,
    );
    assert_eq!(
        t,
        Some(ServerTimings {
            prompt_n: 52,
            cache_n: 3,
            prompt_ms: 857.1,
            predicted_n: 57,
            predicted_ms: 1035.0
        })
    );
    assert_eq!(ServerTimings::parse("[1]"), None);
    assert_eq!(ServerTimings::parse("{"), None);
}

struct NullBackend;

#[async_trait]
impl ChatBackend for NullBackend {
    async fn complete(
        &self,
        _actor: &str,
        _messages: &[ChatMessage],
        _options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(ChatResponse {
            text: "ok".into(),
            ..ChatResponse::default()
        })
    }

    async fn stream_with_control(
        &self,
        _actor: &str,
        _messages: &[ChatMessage],
        _options: &ChatOptions,
        _on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        Err(ChatError::Protocol("not supported".into()))
    }
}

#[test]
fn shared_route_is_labelled_with_the_current_actor() {
    let client = UsageLoggingBackend::new(Arc::new(NullBackend), None, USAGE_CATEGORY, "provider");
    assert_eq!(client.route("lead"), "lead on provider");
    // The actor comes with each request (C#: an AsyncLocal per flow).
    assert_eq!(client.route("ab12cd34"), "ab12cd34 on provider");
    assert_eq!(client.route("lead"), "lead on provider");
}

#[test]
fn slot_route_is_kept_as_is() {
    let client = UsageLoggingBackend::new(Arc::new(NullBackend), None, USAGE_CATEGORY, "lead on A");
    assert_eq!(client.route("ab12cd34"), "lead on A");
}

#[tokio::test]
async fn a_response_without_usage_or_output_is_not_logged() {
    let log = Arc::new(MemoryLog::default());
    let client = UsageLoggingBackend::new(
        Arc::new(NullBackend),
        Some(log.clone() as Arc<dyn LogSink>),
        USAGE_CATEGORY,
        "provider",
    );
    // A non-streaming reply has no chunks: without usage and calls there is nothing to log.
    client
        .complete("lead", &[ChatMessage::user("hi")], &ChatOptions::default())
        .await
        .unwrap();
    assert!(log.messages().is_empty());
}

#[test]
fn effort_is_off_when_the_request_turns_thinking_off() {
    let mut options = ChatOptions::default();
    options.set("reasoning_effort", json!("high"));
    options.set("chat_template_kwargs", json!({"enable_thinking": false}));
    let stats = RequestStats::start(Some(&options), None, None, None);
    assert_eq!(stats.effort.as_deref(), Some("off"));
}

#[test]
fn ledger_provider_usage_is_added_up_and_costed() {
    // AgentSwarmTests.ProviderUsage_IsAddedUpPerProvider_AndCosted, at the ledger: the
    // orchestrator part is in the orchestration port.
    let ledger = ProviderUsageLedger::new();
    ledger.set_pricing(
        "haiku",
        Some(ProviderPricing {
            input_per_m_tok: 0.10,
            output_per_m_tok: 0.50,
            cache_read_per_m_tok: 0.01,
            ..ProviderPricing::default()
        }),
    );
    ledger.record(Some("haiku"), Some("claude-haiku-5-5"), 4000, 2000, 0, 100);
    let rows = ledger.snapshot();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.provider, "haiku");
    assert_eq!(row.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(row.requests, 1);
    assert_eq!(row.prompt, 4000);
    assert_eq!(row.cached, 2000);
    let want = (2000.0 * 0.10 + 2000.0 * 0.01 + 100.0 * 0.50) / 1e6;
    assert!((row.cost_usd.unwrap() - want).abs() < 1e-9);
}

#[test]
fn ledger_long_context_rate_and_local_row_first() {
    let ledger = ProviderUsageLedger::new();
    ledger.set_pricing(
        "haiku",
        Some(ProviderPricing {
            input_per_m_tok: 1.0,
            output_per_m_tok: 1.0,
            long_context_threshold: Some(100_000),
            long_context_factor: 5.0,
            ..ProviderPricing::default()
        }),
    );
    ledger.record(Some("haiku"), Some("m"), 200_000, 0, 0, 0);
    ledger.record(None, None, 10, 0, 0, 5);
    let rows = ledger.snapshot();
    assert_eq!(rows[0].provider, "local");
    assert_eq!(rows[0].cost_usd, None);
    assert!((rows[1].cost_usd.unwrap() - 1.0).abs() < 1e-9); // 200k * $1/M * 5
}

#[test]
fn ledger_keys_ignore_case() {
    let ledger = ProviderUsageLedger::new();
    ledger.record(Some("Haiku"), None, 1, 0, 0, 1);
    ledger.record(Some("haiku"), None, 1, 0, 0, 1);
    ledger.record(Some(" "), None, 1, 0, 0, 1);
    let rows = ledger.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].provider, "local");
    assert_eq!(rows[1].provider, "Haiku");
    assert_eq!(rows[1].requests, 2);
}

#[test]
fn usage_log_names_provider_and_model() {
    let log = MemoryLog::default();
    let mut stats = RequestStats::start(
        None,
        Some("haiku"),
        Some("claude-haiku-5-5"),
        Some(Arc::new(ProviderUsageLedger::new())),
    );
    stats.observe_response(&ChatResponse {
        text: "x".into(),
        usage: Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            ..Usage::default()
        },
        usage_reported: true,
        ..ChatResponse::default()
    });
    stats.log(Some(&log), USAGE_CATEGORY, "abc on haiku");
    let lines = log.messages();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("Slot usage abc on haiku: prompt 10"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("provider=haiku model=claude-haiku-5-5"),
        "{}",
        lines[0]
    );
}

#[test]
fn provider_usage_text() {
    let u = ProviderUsage {
        provider: "haiku".into(),
        model: Some("claude-haiku-5-5".into()),
        requests: 12,
        prompt: 340_000,
        cached: 300_000,
        cache_write: 20_000,
        output: 9_000,
        cost_usd: Some(0.0412),
    };
    assert_eq!(
        u.to_string(),
        "haiku (claude-haiku-5-5): 12 requests, prompt 340,000 (cached 300,000, written 20,000), output 9,000, $0.0412"
    );
    let local = ProviderUsage {
        provider: "local".into(),
        model: None,
        requests: 1,
        prompt: 5,
        cached: 0,
        cache_write: 0,
        output: 1,
        cost_usd: None,
    };
    assert_eq!(
        local.to_string(),
        "local: 1 requests, prompt 5 (cached 0, written 0), output 1"
    );
}

#[test]
fn whole_numbers_round_like_dotnet() {
    assert_eq!(format_whole(85.5), "86");
    assert_eq!(format_whole(140.2), "140");
    assert_eq!(format_whole(0.4), "0");
    assert_eq!(round_half_even(2.5), 2);
    assert_eq!(round_half_even(3.5), 4);
    assert_eq!(round_half_even(2.6), 3);
}
