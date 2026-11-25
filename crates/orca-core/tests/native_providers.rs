//! The Anthropic Messages API and Ollama clients against fake local servers that assert the
//! request bodies and headers. The keys are fake values given through config; no real
//! service is called and no environment variable is read or set.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use orca_core::client::anthropic::{AnthropicClient, AnthropicSettings};
use orca_core::client::ollama::{OllamaClient, OllamaSettings};
use orca_core::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ImagePart, RequestStats, ServedModels,
    StreamControl, StreamEvent, ToolCall, ToolDefinition, client_for_model,
    native_provider_backend,
};
use orca_core::config::{OrcaConfig, ProviderConfig};
use serde_json::{Value, json};

const FAKE_KEY: &str = "test-key-not-a-real-one";

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
}

/// Replies with the scripted (status, content type, body) in order; records each request.
/// A scripted reply: status, content type, body.
type Reply = (u16, &'static str, String);

#[derive(Clone, Default)]
struct Fake {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    async fn start(replies: Vec<Reply>) -> (Self, String) {
        let me = Fake {
            replies: Arc::new(Mutex::new(replies.into())),
            seen: Arc::default(),
        };
        let f = me.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let f = f.clone();
            async move {
                f.seen.lock().unwrap().push(Seen {
                    path: uri.path().to_string(),
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let (status, ct, body) = f.replies.lock().unwrap().pop_front().unwrap_or((
                    500,
                    "text/plain",
                    "no reply scripted".into(),
                ));
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("content-type", ct)],
                    body,
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (me, format!("http://127.0.0.1:{port}"))
    }

    fn only(&self) -> Seen {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        seen[0].clone()
    }
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn conversation() -> Vec<ChatMessage> {
    let mut assistant = ChatMessage::assistant("Let me look.");
    assistant.reasoning = Some("unsigned thoughts".into());
    assistant.tool_calls = vec![
        ToolCall {
            id: "toolu_1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        },
        ToolCall {
            id: "toolu_2".into(),
            name: "read_file".into(),
            arguments: "not json".into(),
        },
    ];
    vec![
        ChatMessage::system("You are a test."),
        ChatMessage::user("Read the files."),
        assistant,
        ChatMessage::tool_result("toolu_1", "alpha"),
        ChatMessage::tool_result("toolu_2", "beta")
            .with_images([ImagePart::new(vec![1, 2, 3], "image/png")]),
    ]
}

fn options() -> ChatOptions {
    let mut o = ChatOptions {
        temperature: Some(0.7),
        top_p: Some(0.9),
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }],
        ..ChatOptions::default()
    };
    o.set("reasoning_effort", json!("high"));
    o.set("chat_template_kwargs", json!({"enable_thinking": true}));
    o.set("num_ctx", json!(16384));
    o
}

fn anthropic(base: &str) -> AnthropicClient {
    // The configured base URL may end in /v1; the client adds /v1/messages itself.
    AnthropicClient::new(AnthropicSettings::new(
        Some(&format!("{base}/v1")),
        FAKE_KEY,
        "claude-test-model",
    ))
}

fn anthropic_stream() -> String {
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-test-model", "role": "assistant", "content": [],
            "usage": {"input_tokens": 12, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 900, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Hmm."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hello"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": " there."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"b"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": ".txt\", \"scale\": 1.0}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
        json!({"type": "message_stop"}),
    ])
}

#[tokio::test]
async fn anthropic_streams_text_thinking_and_tool_calls_with_cache_usage() {
    let (fake, base) = Fake::start(vec![(200, "text/event-stream", anthropic_stream())]).await;
    let client = anthropic(&base);
    let mut events = Vec::new();
    let r = client
        .stream_with_control("lead", &conversation(), &options(), &mut |e| {
            events.push(e);
            StreamControl::Continue
        })
        .await
        .unwrap();

    // The response.
    assert_eq!(r.text, "Hello there.");
    assert_eq!(r.reasoning, "Hmm.");
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "toolu_9");
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(
        r.tool_calls[0].arguments,
        r#"{"path": "b.txt", "scale": 1.0}"#
    );
    assert_eq!(r.model.as_deref(), Some("claude-test-model"));
    assert!(r.usage_reported);
    assert_eq!(r.usage.prompt_tokens, Some(1012));
    assert_eq!(r.usage.cached_tokens, Some(900));
    assert_eq!(r.usage.cache_creation_tokens, Some(100));
    assert_eq!(r.usage.completion_tokens, Some(42));
    assert!(events.contains(&StreamEvent::Text("Hello".into())));
    assert!(events.contains(&StreamEvent::Reasoning("Hmm.".into())));
    assert!(events.contains(&StreamEvent::ToolCallStarted {
        index: 0,
        name: "read_file".into()
    }));

    // The usage line carries the cache writes.
    let mut stats = RequestStats::new();
    for e in &events {
        stats.observe_event(e);
    }
    let line = stats.message("lead").unwrap();
    assert!(
        line.starts_with("Slot usage lead: prompt 1012, cached 900, re-read 112, output 42"),
        "{line}"
    );
    assert!(line.contains(" cache_write=100"), "{line}");

    // The request.
    let req = fake.only();
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.headers["x-api-key"], FAKE_KEY);
    assert_eq!(req.headers["anthropic-version"], "2023-06-01");
    assert!(req.headers.get("authorization").is_none());
    let b = &req.body;
    assert_eq!(b["model"], "claude-test-model");
    assert_eq!(b["max_tokens"], 32000);
    assert_eq!(b["stream"], true);
    assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(b["output_config"], json!({"effort": "high"}));
    assert_eq!(
        b["system"],
        json!([{"type": "text", "text": "You are a test."}])
    );
    for dropped in [
        "temperature",
        "top_p",
        "reasoning_effort",
        "chat_template_kwargs",
        "num_ctx",
    ] {
        assert!(b.get(dropped).is_none(), "{dropped}");
    }
    assert_eq!(
        b["tools"],
        json!([{"name": "read_file", "description": "Read a file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}])
    );
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Read the files."}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.txt"}},
                {"type": "tool_use", "id": "toolu_2", "name": "read_file", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "alpha"},
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                    {"type": "text", "text": "beta"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQID"}},
                ]},
            ]},
        ])
    );
}

#[tokio::test]
async fn anthropic_non_streaming_request_and_errors() {
    let body = json!({"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-test-model",
        "content": [{"type": "text", "text": "Done."}, {"type": "tool_use", "id": "toolu_3", "name": "glob", "input": {"pattern": "*.rs"}}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 7}});
    let (fake, base) = Fake::start(vec![
        (200, "application/json", body.to_string()),
        (
            401,
            "application/json",
            json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}).to_string(),
        ),
    ])
    .await;
    let client = anthropic(&base);
    let mut o = ChatOptions {
        max_output_tokens: Some(1234),
        // A model id from the OpenAI-side config is not sent to Anthropic.
        model: Some("qwen3-local".into()),
        ..ChatOptions::default()
    };
    o.set("reasoning_effort", json!("off"));
    let r = client
        .complete("lead", &[ChatMessage::user("hi")], &o)
        .await
        .unwrap();
    assert_eq!(r.text, "Done.");
    assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    assert_eq!(r.tool_calls[0].arguments, r#"{"pattern":"*.rs"}"#);
    assert_eq!(r.usage.prompt_tokens, Some(5));
    assert_eq!(r.usage.completion_tokens, Some(7));
    let req = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(req.body["model"], "claude-test-model");
    assert_eq!(req.body["max_tokens"], 1234);
    assert_eq!(req.body["output_config"], json!({"effort": "low"}));
    assert!(req.body.get("stream").is_none());
    assert!(req.body.get("tools").is_none());

    match client
        .complete("lead", &[ChatMessage::user("hi")], &ChatOptions::default())
        .await
    {
        Err(ChatError::Http { status, body, .. }) => {
            assert_eq!(status, 401);
            assert!(body.contains("authentication_error"));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn anthropic_error_event_mid_stream_is_an_interruption() {
    let stream = sse(&[
        json!({"type": "message_start", "message": {"model": "claude-test-model", "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Partial"}}),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ]);
    let (_fake, base) = Fake::start(vec![(200, "text/event-stream", stream)]).await;
    match anthropic(&base)
        .stream_with_control(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| StreamControl::Continue,
        )
        .await
    {
        Err(ChatError::Interrupted { message, partial }) => {
            assert!(message.contains("overloaded_error"), "{message}");
            assert_eq!(partial.text, "Partial");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn main_model_with_the_anthropic_provider_speaks_the_messages_api() {
    let (fake, base) = Fake::start(vec![(200, "text/event-stream", anthropic_stream())]).await;
    let mut config = OrcaConfig::default();
    config.lm_studio.provider = Some("anthropic".into());
    config.lm_studio.base_url = base.clone();
    config.lm_studio.api_key = FAKE_KEY.into();
    config.lm_studio.models = vec!["claude-test-model".into()];
    config.lm_studio.max_tokens = Some(4096);
    let client = client_for_model(&config, Arc::new(ServedModels::new()), "claude-test-model");
    let r = client
        .stream(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.text, "Hello there.");
    let req = fake.only();
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.headers["x-api-key"], FAKE_KEY);
    assert_eq!(req.body["max_tokens"], 4096);
}

#[tokio::test]
async fn named_providers_get_their_own_kind_of_client() {
    let (fake, base) = Fake::start(vec![(200, "text/event-stream", anthropic_stream())]).await;
    let config = OrcaConfig::default();
    let provider = ProviderConfig {
        kind: "Anthropic".into(),
        base_url: base.clone(),
        api_key: Some(FAKE_KEY.into()),
        // Not set in this process: the key in the config is used.
        api_key_env: Some("ABHOTH_TEST_UNSET_PROVIDER_KEY_VARIABLE".into()),
        max_tokens: Some(2048),
        ..ProviderConfig::default()
    };
    let backend = native_provider_backend(&config, "claude", &provider, "claude-test-model")
        .unwrap()
        .unwrap();
    backend
        .stream_with_control(
            "agent",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| StreamControl::Continue,
        )
        .await
        .unwrap();
    let req = fake.only();
    assert_eq!(req.headers["x-api-key"], FAKE_KEY);
    assert_eq!(req.body["max_tokens"], 2048);

    let openai = ProviderConfig::default();
    assert!(
        native_provider_backend(&config, "local", &openai, "m")
            .unwrap()
            .is_none()
    );
    let odd = ProviderConfig {
        kind: "gemini".into(),
        ..ProviderConfig::default()
    };
    assert_eq!(
        native_provider_backend(&config, "g", &odd, "m")
            .err()
            .unwrap(),
        "Unknown provider type 'gemini' for provider 'g'. Expected 'openai-compat', 'anthropic', or 'ollama'."
    );
}

fn ndjson(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

#[tokio::test]
async fn ollama_streams_ndjson_with_tools_and_options() {
    let stream = ndjson(&[
        json!({"model": "qwen-test", "message": {"role": "assistant", "content": "", "thinking": "Thinking."}, "done": false}),
        json!({"model": "qwen-test", "message": {"role": "assistant", "content": "Sure"}, "done": false}),
        json!({"model": "qwen-test", "message": {"role": "assistant", "content": ", reading.",
            "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "c.txt", "limit": 2.50}}}]}, "done": false}),
        json!({"model": "qwen-test", "message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop",
            "prompt_eval_count": 321, "eval_count": 54}),
    ]);
    let (fake, base) = Fake::start(vec![(200, "application/x-ndjson", stream)]).await;
    let mut settings = OllamaSettings::new(Some(&format!("{base}/")), "qwen-test");
    settings.num_ctx = Some(65536);
    let client = OllamaClient::new(settings);
    let mut events = Vec::new();
    let r = client
        .stream_with_control("agent", &conversation(), &options(), &mut |e| {
            events.push(e);
            StreamControl::Continue
        })
        .await
        .unwrap();
    assert_eq!(r.text, "Sure, reading.");
    assert_eq!(r.reasoning, "Thinking.");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[0].id, "call_0");
    let args: Value = serde_json::from_str(&r.tool_calls[0].arguments).unwrap();
    assert_eq!(args["path"], "c.txt");
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(r.usage.prompt_tokens, Some(321));
    assert_eq!(r.usage.completion_tokens, Some(54));
    assert!(events.contains(&StreamEvent::ToolCallStarted {
        index: 0,
        name: "read_file".into()
    }));

    let req = fake.only();
    assert_eq!(req.path, "/api/chat");
    let b = &req.body;
    assert_eq!(b["model"], "qwen-test");
    assert_eq!(b["stream"], true);
    // The request's own num_ctx wins over the provider's context window.
    assert_eq!(
        b["options"],
        json!({"temperature": 0.7, "top_p": 0.9, "num_ctx": 16384})
    );
    assert!(b.get("reasoning_effort").is_none());
    assert!(b.get("chat_template_kwargs").is_none());
    assert_eq!(b["tools"][0]["function"]["name"], "read_file");
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(
        b["messages"],
        json!([
            {"role": "system", "content": "You are a test."},
            {"role": "user", "content": "Read the files."},
            {"role": "assistant", "content": "Let me look.", "tool_calls": [
                {"function": {"name": "read_file", "arguments": {"path": "a.txt"}}},
                {"function": {"name": "read_file", "arguments": {}}},
            ]},
            {"role": "tool", "content": "alpha"},
            {"role": "tool", "content": "beta", "images": ["AQID"]},
        ])
    );
}

#[tokio::test]
async fn ollama_complete_context_window_and_errors() {
    let (fake, base) = Fake::start(vec![
        (
            200,
            "application/json",
            json!({"model": "qwen-test", "message": {"role": "assistant", "content": "Hi!"}, "done": true,
                "done_reason": "length", "prompt_eval_count": 4, "eval_count": 2}).to_string(),
        ),
        (
            404,
            "application/json",
            json!({"error": "model \"nope\" not found, try pulling it first"}).to_string(),
        ),
        (
            200,
            "application/x-ndjson",
            ndjson(&[json!({"error": "unexpected server failure"})]),
        ),
    ])
    .await;
    let provider = ProviderConfig {
        kind: "ollama".into(),
        base_url: base.clone(),
        context_window: Some(32768),
        ..ProviderConfig::default()
    };
    let client = native_provider_backend(
        &OrcaConfig::default(),
        "local-ollama",
        &provider,
        "qwen-test",
    )
    .unwrap()
    .unwrap();
    let o = ChatOptions {
        max_output_tokens: Some(100),
        ..ChatOptions::default()
    };
    let r = client
        .complete("agent", &[ChatMessage::user("hi")], &o)
        .await
        .unwrap();
    assert_eq!(r.text, "Hi!");
    assert_eq!(r.finish_reason.as_deref(), Some("length"));
    let req = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(req.body["stream"], false);
    assert_eq!(
        req.body["options"],
        json!({"num_predict": 100, "num_ctx": 32768})
    );

    match client
        .complete("agent", &[ChatMessage::user("hi")], &ChatOptions::default())
        .await
    {
        Err(ChatError::Http {
            status: 404, body, ..
        }) => assert!(body.contains("not found")),
        other => panic!("{other:?}"),
    }
    match client
        .stream_with_control(
            "agent",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| StreamControl::Continue,
        )
        .await
    {
        Err(ChatError::Server(m)) => assert_eq!(m, "unexpected server failure"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn anthropic_sends_back_only_signed_reasoning() {
    let (fake, base) = Fake::start(vec![
        (200, "text/event-stream", anthropic_stream()),
        (200, "text/event-stream", anthropic_stream()),
    ])
    .await;
    let client = anthropic(&base);
    let r = client
        .stream_with_control(
            "lead",
            &[ChatMessage::user("q")],
            &ChatOptions::default(),
            &mut |_| StreamControl::Continue,
        )
        .await
        .unwrap();
    assert_eq!(r.reasoning_signature.as_deref(), Some("sig"));
    let signed = r.to_message();
    assert_eq!(signed.reasoning_signature.as_deref(), Some("sig"));

    let mut unsigned = ChatMessage::assistant("");
    unsigned.reasoning = Some("only unsigned".into());
    let conversation = vec![
        ChatMessage::user("q"),
        signed,
        ChatMessage::tool_result("toolu_9", "ok"),
        unsigned,
        ChatMessage::user("q3"),
    ];
    client
        .stream_with_control("lead", &conversation, &ChatOptions::default(), &mut |_| {
            StreamControl::Continue
        })
        .await
        .unwrap();
    let seen = fake.seen.lock().unwrap()[1].clone();
    let messages = seen.body["messages"].as_array().unwrap().clone();
    let sent = &messages[1]["content"];
    assert_eq!(
        sent[0],
        json!({"type": "thinking", "thinking": "Hmm.", "signature": "sig"})
    );
    assert_eq!(sent[1]["type"], "text");
    assert_eq!(sent[2]["type"], "tool_use");
    // The unsigned-only turn is dropped; the user turns around it merge.
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(
        messages[2]["content"][1],
        json!({"type": "text", "text": "q3"})
    );
}

#[test]
fn prompt_size_counts_cache_reads_and_writes() {
    use orca_core::chat::conversation::Conversation;
    use orca_core::client::Usage;
    // A server that reports only the uncached input (C#'s Anthropic adapter does).
    let uncached = Usage {
        prompt_tokens: Some(5),
        cached_tokens: Some(900),
        cache_creation_tokens: Some(100),
        ..Usage::default()
    };
    assert_eq!(Conversation::prompt_tokens_of(&uncached), Some(1005));
    // The Rust Anthropic client reports the whole prompt.
    let whole = Usage {
        prompt_tokens: Some(1005),
        ..uncached
    };
    assert_eq!(Conversation::prompt_tokens_of(&whole), Some(1005));
}
