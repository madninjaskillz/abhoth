//! The chat client against the fake server: wire format, streaming, served-model tracking.
//! Ports `RequestParametersTests`, `ThinkingLevelsTests` and `ServedModelsTests`.

use std::sync::Arc;

use orca_core::client::served_models::{self, ServedModels};
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ClientSettings, OpenAiClient, ToolDefinition,
    request_parameters,
};
use orca_core::config::{OrcaConfig, SamplingConfig, ThinkingConfig, thinking_levels};
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

fn client(server: &FakeOpenAi, served: Arc<ServedModels>) -> OpenAiClient {
    OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", Some("qwen3.8-27b".into())),
        served,
    )
}

fn apply(config: &OrcaConfig) -> ChatOptions {
    let mut o = ChatOptions::default();
    request_parameters::apply(&mut o, config, Some("m"), false);
    o
}

async fn sent_body(options: &ChatOptions) -> Value {
    let server = FakeOpenAi::start(vec![Reply::text("hi")]).await;
    client(&server, Arc::new(ServedModels::new()))
        .complete("t", &[ChatMessage::user("hi")], options)
        .await
        .unwrap();
    server.requests()[0].body.clone()
}

// ── Wire format ──

#[tokio::test]
async fn streaming_request_matches_the_csharp_wire_format() {
    let server = FakeOpenAi::start(vec![Reply::text("hello there")]).await;
    let c = client(&server, Arc::new(ServedModels::new()));
    let mut options = apply(&OrcaConfig::default());
    options.max_output_tokens = Some(4096);
    options.tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file.".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }];
    let mut text = String::new();
    let r = c
        .stream(
            "t",
            &[ChatMessage::system("sys"), ChatMessage::user("hi")],
            &options,
            &mut |e| {
                if let orca_core::client::StreamEvent::Text(t) = e {
                    text.push_str(&t)
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (r.text.as_str(), text.as_str()),
        ("hello there", "hello there")
    );
    assert_eq!(r.usage.completion_tokens, Some(10));

    let req = &server.requests()[0];
    assert_eq!(req.header("authorization"), Some("Bearer k"));
    let b = &req.body;
    assert_eq!(b["model"], "qwen3.8-27b");
    assert_eq!(b["temperature"], json!(0.7));
    assert_eq!(b["max_completion_tokens"], 4096);
    assert_eq!(b["stream"], true);
    assert_eq!(b["stream_options"], json!({"include_usage": true}));
    assert_eq!(b["tool_choice"], "auto");
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["function"]["name"], "read_file");
    assert_eq!(
        b["messages"],
        json!([{"role":"system","content":"sys"},{"role":"user","content":"hi"}])
    );
}

#[tokio::test]
async fn streamed_tool_calls_and_reasoning_are_assembled() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("read_file", json!({"path": "hello.txt"})).with_reasoning("Let me look."),
    ])
    .await;
    let c = client(&server, Arc::new(ServedModels::new()));
    let r = c
        .stream(
            "t",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.reasoning, "Let me look.");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(
        serde_json::from_str::<Value>(&r.tool_calls[0].arguments).unwrap(),
        json!({"path":"hello.txt"})
    );
    assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));

    // The assistant turn goes back with content "" and its tool calls; the reasoning only when
    // echoing (preserve-thinking).
    let wire = r.to_message().to_wire(false);
    assert_eq!(wire["content"], "");
    assert_eq!(wire["tool_calls"][0]["function"]["name"], "read_file");
    assert!(wire.get("reasoning_content").is_none());
    assert_eq!(
        r.to_message().to_wire(true)["reasoning_content"],
        "Let me look."
    );
    let tool = ChatMessage::tool_result("call_0", "hi").to_wire(false);
    assert_eq!(
        tool,
        json!({"role":"tool","tool_call_id":"call_0","content":"hi"})
    );
}

#[tokio::test]
async fn http_errors_surface_with_status_and_body() {
    let server = FakeOpenAi::start(vec![Reply::error(400, r#"{"error":"bad"}"#)]).await;
    let err = client(&server, Arc::new(ServedModels::new()))
        .stream(
            "t",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    match err {
        ChatError::Http { status, body, .. } => {
            assert_eq!((status, body.as_str()), (400, r#"{"error":"bad"}"#))
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn session_field_goes_last_unless_the_request_sets_it() {
    let server = FakeOpenAi::start(vec![Reply::text("a")]).await;
    let mut settings = ClientSettings::new(server.base_url(), "k", Some("m".into()));
    settings.session_field = Some(("uncensored".into(), json!(true)));
    let c = OpenAiClient::new(settings, Arc::new(ServedModels::new()));
    let mut options = ChatOptions {
        top_k: Some(20),
        ..ChatOptions::default()
    };
    options.set("min_p", json!(0.05));
    let body = c
        .build_body(&[ChatMessage::user("x")], &options, true)
        .unwrap();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(
        keys.iter()
            .rev()
            .take(4)
            .rev()
            .map(|k| k.as_str())
            .collect::<Vec<_>>(),
        ["top_k", "min_p", "uncensored", "model"],
        "extra-body fields, the session field, then model (as the SDK writes it)"
    );
    options.set("uncensored", json!(false));
    let body = c
        .build_body(&[ChatMessage::user("x")], &options, true)
        .unwrap();
    assert_eq!(body["uncensored"], false);
}

// ── RequestParametersTests ──

fn qwen_config(preset: &str) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.temperature = 0.7;
    c.lm_studio.sampling = Some(SamplingConfig {
        preset: Some(preset.into()),
        ..SamplingConfig::default()
    });
    c
}

#[tokio::test]
async fn sampling_fields_reach_the_wire() {
    let mut c = OrcaConfig::default();
    c.lm_studio.temperature = 0.6;
    c.lm_studio.sampling = Some(SamplingConfig {
        top_p: Some(0.95),
        top_k: Some(20),
        min_p: Some(0.05),
        presence_penalty: Some(1.5),
        repetition_penalty: Some(1.05),
        ..SamplingConfig::default()
    });
    let b = sent_body(&apply(&c)).await;
    assert_eq!(b["temperature"], json!(0.6));
    assert_eq!(b["top_p"], json!(0.95));
    assert_eq!(b["top_k"], json!(20));
    assert_eq!(b["min_p"], json!(0.05));
    assert_eq!(b["presence_penalty"], json!(1.5));
    assert_eq!(b["repeat_penalty"], json!(1.05));
    assert_eq!(b["repetition_penalty"], json!(1.05));
}

#[tokio::test]
async fn unset_sampling_sends_nothing_extra() {
    let b = sent_body(&apply(&OrcaConfig::default())).await;
    for f in [
        "top_p",
        "top_k",
        "min_p",
        "presence_penalty",
        "repeat_penalty",
        "repetition_penalty",
        "reasoning_effort",
        "chat_template_kwargs",
    ] {
        assert!(b.get(f).is_none(), "{f} should not be sent");
    }
    assert_eq!(b["temperature"], json!(0.7));
}

#[tokio::test]
async fn reasoning_effort_reaches_the_wire() {
    let mut c = OrcaConfig::default();
    c.thinking.reasoning_effort = Some("low".into());
    assert_eq!(sent_body(&apply(&c)).await["reasoning_effort"], "low");
}

#[test]
fn preset_fills_unset_fields_and_explicit_fields_win() {
    let o = apply(&qwen_config("qwen-thinking-coding"));
    assert_eq!(
        (o.temperature, o.top_p, o.top_k, o.presence_penalty),
        (Some(0.6), Some(0.95), Some(20), Some(0.0))
    );
    assert_eq!(
        o.additional["min_p"],
        json!(0),
        "a whole float is written without a fraction"
    );

    let mut c = qwen_config("qwen-thinking-coding");
    let s = c.lm_studio.sampling.as_mut().unwrap();
    s.temperature = Some(0.4);
    s.presence_penalty = Some(1.0);
    let o = apply(&c);
    assert_eq!(
        (o.temperature, o.presence_penalty, o.top_k),
        (Some(0.4), Some(1.0), Some(20))
    );
}

#[test]
fn model_sampling_overrides_global_for_that_model_only() {
    let mut c = OrcaConfig::default();
    c.lm_studio.sampling = Some(SamplingConfig {
        top_k: Some(40),
        ..SamplingConfig::default()
    });
    let mut per = indexmap::IndexMap::new();
    per.insert(
        "Qwen-Coder".to_string(),
        SamplingConfig {
            preset: Some("qwen-thinking-coding".into()),
            top_k: Some(10),
            ..SamplingConfig::default()
        },
    );
    c.lm_studio.model_sampling = Some(per);
    let mut qwen = ChatOptions::default();
    request_parameters::apply(&mut qwen, &c, Some("qwen-coder"), false);
    let mut other = ChatOptions::default();
    request_parameters::apply(&mut other, &c, Some("llama"), false);
    assert_eq!((qwen.top_k, qwen.top_p), (Some(10), Some(0.95)));
    assert_eq!((other.top_k, other.top_p), (Some(40), None));
}

#[test]
fn keep_temperature_and_unknown_preset() {
    let mut o = ChatOptions {
        temperature: Some(0.2),
        ..ChatOptions::default()
    };
    request_parameters::apply(
        &mut o,
        &qwen_config("qwen-thinking-coding"),
        Some("m"),
        true,
    );
    assert_eq!((o.temperature, o.top_k), (Some(0.2), Some(20)));
    let o = apply(&qwen_config("nope"));
    assert_eq!((o.temperature, o.top_k), (Some(0.7), None));
}

#[test]
fn role_effort_replaces_global_and_off_disables_thinking() {
    let mut c = OrcaConfig::default();
    c.thinking.reasoning_effort = Some("low".into());
    let mut o = apply(&c);
    request_parameters::apply_effort(&mut o, None);
    assert_eq!(o.additional["reasoning_effort"], "low");
    request_parameters::apply_effort(&mut o, Some(" medium "));
    assert_eq!(o.additional["reasoning_effort"], "medium");

    let mut o = ChatOptions::default();
    o.set("reasoning_effort", json!("low"));
    o.set("chat_template_kwargs", json!({"x": 1}));
    request_parameters::apply_effort(&mut o, Some("off"));
    assert!(!o.additional.contains_key("reasoning_effort"));
    assert_eq!(
        o.additional["chat_template_kwargs"],
        json!({"enable_thinking": false, "x": 1})
    );
    // enable_thinking is the first key (Strata applies them in order).
    assert_eq!(
        o.additional["chat_template_kwargs"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap(),
        "enable_thinking"
    );
}

#[test]
fn agent_reasoning_and_sampling_round_trip_through_config_json() {
    let c: OrcaConfig =
        serde_json::from_str(r#"{"agent":{"reasoning":{"lead":"medium","worker":"low"}}}"#)
            .unwrap();
    assert_eq!(
        (
            c.agent.reasoning.lead.as_deref(),
            c.agent.reasoning.worker.as_deref()
        ),
        (Some("medium"), Some("low"))
    );
    let c: OrcaConfig = serde_json::from_str(
        r#"{"lmStudio":{"sampling":{"preset":"qwen-thinking-coding","minP":0.05},"modelSampling":{"m":{"topK":5}}}}"#,
    )
    .unwrap();
    let s = c.lm_studio.sampling.unwrap();
    assert_eq!(
        (s.preset.as_deref(), s.min_p),
        (Some("qwen-thinking-coding"), Some(0.05))
    );
    assert_eq!(c.lm_studio.model_sampling.unwrap()["m"].top_k, Some(5));
}

// ── ThinkingLevelsTests ──

#[test]
fn levels_map_to_budgets() {
    for (name, budget) in [
        ("minimal", 1024),
        ("low", 2048),
        ("medium", 8192),
        ("HIGH", 16384),
    ] {
        let l = thinking_levels::resolve(&ThinkingConfig {
            level: Some(name.into()),
            ..ThinkingConfig::default()
        })
        .unwrap();
        assert!(l.enabled);
        assert_eq!(l.budget_tokens, Some(budget));
    }
    let off = thinking_levels::resolve(&ThinkingConfig {
        level: Some("off".into()),
        budget_tokens: 500,
        ..ThinkingConfig::default()
    });
    assert_eq!(
        off,
        Some(thinking_levels::Level {
            enabled: false,
            budget_tokens: None
        })
    );
    let unlimited = thinking_levels::resolve(&ThinkingConfig {
        level: Some("unlimited".into()),
        ..ThinkingConfig::default()
    });
    assert_eq!(
        unlimited,
        Some(thinking_levels::Level {
            enabled: true,
            budget_tokens: None
        })
    );
    let custom = thinking_levels::resolve(&ThinkingConfig {
        level: Some("medium".into()),
        budget_tokens: 3000,
        ..ThinkingConfig::default()
    });
    assert_eq!(custom.unwrap().budget_tokens, Some(3000));
}

#[test]
fn unset_or_unknown_level_sends_nothing() {
    for level in [None, Some(""), Some("extreme")] {
        let mut c = OrcaConfig::default();
        c.thinking.level = level.map(str::to_string);
        assert!(!apply(&c).additional.contains_key("chat_template_kwargs"));
    }
    assert!(!thinking_levels::is_valid(Some("extreme")));
}

#[tokio::test]
async fn level_reaches_the_wire() {
    let mut c = OrcaConfig::default();
    c.thinking.level = Some("medium".into());
    let b = sent_body(&apply(&c)).await;
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(b["thinking_budget_tokens"], 8192);
    c.thinking.level = Some("off".into());
    let b = sent_body(&apply(&c)).await;
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
    assert!(b.get("thinking_budget_tokens").is_none());
}

// ── ServedModelsTests ──

#[tokio::test]
async fn gateway_header_names_the_served_model() {
    let served = Arc::new(ServedModels::new());
    let server = FakeOpenAi::start(vec![
        Reply::text("hi").with_served_header("openai/inkling-2-mini"),
    ])
    .await;
    let c = client(&server, served.clone());
    c.complete("a", &[ChatMessage::user("hi")], &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(
        server.requests()[0].body["model"],
        "qwen3.8-27b",
        "what is sent does not change"
    );
    let m = served.for_actor("a").unwrap();
    assert_eq!(
        (m.requested.as_deref(), m.served.as_deref()),
        (Some("qwen3.8-27b"), Some("inkling-2-mini"))
    );

    let r = c
        .stream(
            "b",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(r.served_model.as_deref(), Some("inkling-2-mini"));
    assert_eq!(
        served.for_actor("b").unwrap().served.as_deref(),
        Some("inkling-2-mini")
    );
}

#[tokio::test]
async fn direct_server_reply_model_counts_and_an_echo_clears() {
    let served = Arc::new(ServedModels::new());
    let server = FakeOpenAi::start(vec![
        Reply {
            model: Some("inkling-2-mini".into()),
            ..Reply::text("hi")
        },
        Reply::text("hi"), // echoes the alias
    ])
    .await;
    let c = client(&server, served.clone());
    c.stream(
        "a",
        &[ChatMessage::user("hi")],
        &ChatOptions::default(),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        served.for_actor("a").unwrap().served.as_deref(),
        Some("inkling-2-mini")
    );
    c.stream(
        "a",
        &[ChatMessage::user("hi")],
        &ChatOptions::default(),
        &mut |_| {},
    )
    .await
    .unwrap();
    let m = served.for_actor("a").unwrap();
    assert_eq!(
        (m.requested.as_deref(), m.served),
        (Some("qwen3.8-27b"), None)
    );
}

#[tokio::test]
async fn actors_are_kept_apart() {
    let served = Arc::new(ServedModels::new());
    let s1 = FakeOpenAi::start(vec![
        Reply::text("hi").with_served_header("openai/inkling-2-mini"),
    ])
    .await;
    let s2 = FakeOpenAi::start(vec![
        Reply::text("hi").with_served_header("hosted_vllm/qwen3.8-27b-awq"),
    ])
    .await;
    let (c1, c2) = (client(&s1, served.clone()), client(&s2, served.clone()));
    let msgs = [ChatMessage::user("hi")];
    let opts = ChatOptions::default();
    let (a, b) = tokio::join!(
        c1.complete("lead", &msgs, &opts),
        c2.complete("helper", &msgs, &opts)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(
        served.for_actor("lead").unwrap().served.as_deref(),
        Some("inkling-2-mini")
    );
    assert_eq!(
        served.for_actor("helper").unwrap().served.as_deref(),
        Some("qwen3.8-27b-awq")
    );
}

#[test]
fn clean_drops_the_provider_prefix() {
    for (raw, want) in [
        (Some("openai/inkling-2-mini"), Some("inkling-2-mini")),
        (Some("inkling-2-mini"), Some("inkling-2-mini")),
        (Some("  hosted_vllm/qwen3.8-27b "), Some("qwen3.8-27b")),
        (Some(""), None),
        (None, None),
        (Some("trailing/"), Some("trailing/")),
    ] {
        assert_eq!(served_models::clean(raw).as_deref(), want, "{raw:?}");
    }
}

#[test]
fn complete_header_beats_body_and_is_used_once() {
    let t = ServedModels::new();
    t.begin("x");
    t.note_header("x", "inkling-2-mini");
    assert_eq!(
        t.complete("x", Some("qwen3.8-27b"), Some("qwen3.8-27b"), false)
            .served
            .as_deref(),
        Some("inkling-2-mini")
    );
    assert_eq!(
        t.complete("x", Some("qwen3.8-27b"), Some("qwen3.8-27b"), false)
            .served,
        None
    );
    assert_eq!(
        t.complete("x", Some("Qwen3.8-27B"), Some("qwen3.8-27b"), false)
            .served,
        None
    );
    assert!(t.for_actor("nobody").is_none());
    // A slot copy (direct): the id sent is what served.
    assert_eq!(
        t.complete("y", Some("qwen3.8-27b"), None, true)
            .served
            .as_deref(),
        Some("qwen3.8-27b")
    );
}

/// Tool schemas reach the wire in the M.E.AI `ToolJson` shape (seen in the C# requests).
#[test]
fn tool_parameters_use_the_meai_wire_shape() {
    let def = orca_core::client::ToolDefinition {
        name: "t".into(),
        description: "d".into(),
        parameters: json!({"type": "object", "properties": {"a": {"type": "object", "properties": {}, "required": ["x"]}}, "required": ["a"], "$schema": "x"}),
    };
    let wire = def.to_wire();
    assert_eq!(
        serde_json::to_string(&wire["function"]["parameters"]).unwrap(),
        r#"{"type":"object","required":["a"],"properties":{"a":{"type":"object","properties":{},"required":["x"]}},"additionalProperties":false}"#
    );
    let empty = orca_core::client::wire_parameters(&json!({"type": "object", "properties": {}}));
    assert_eq!(
        empty,
        json!({"type": "object", "required": [], "properties": {}, "additionalProperties": false})
    );
}
