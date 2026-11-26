use serde_json::{Value, json};

use super::convert::{Collector, Out, Translator, Usage, prepare, sse_chunk};
use super::*;

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn prepare_single_turn_system_images_effort() {
    let p = prepare(&obj(json!({
        "model": "haiku",
        "messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "developer", "content": [{"type": "text", "text": "No lists."}]},
            {"role": "user", "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}
        ],
        "reasoning_effort": "minimal",
        "max_tokens": 50,
        "stream": true,
        "stream_options": {"include_usage": true}
    })))
    .unwrap();
    assert_eq!(p.model, "haiku");
    assert_eq!(p.system.as_deref(), Some("Be terse.\n\nNo lists."));
    assert_eq!(p.content.len(), 3);
    assert_eq!(p.content[1]["source"]["media_type"], "image/png");
    assert_eq!(p.content[1]["source"]["data"], "AAAA");
    assert_eq!(p.content[2]["source"]["type"], "url");
    assert_eq!(p.effort.as_deref(), Some("low"));
    assert_eq!(p.max_tokens, Some(50));
    assert!(p.stream && p.include_usage);
    assert!(p.tools.is_empty());
}

#[test]
fn prepare_history_becomes_one_transcript_turn() {
    let p = prepare(&obj(json!({
        "model": "sonnet",
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "18C, sunny"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather",
            "description": "Weather by city",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
    })))
    .unwrap();
    assert_eq!(p.content.len(), 1, "adjacent text merged");
    let t = p.content[0]["text"].as_str().unwrap();
    assert!(t.contains("<user>\nWeather in Paris?\n</user>"));
    assert!(
        t.contains(r#"<tool_call id="call_1" name="get_weather">{"city":"Paris"}</tool_call>"#)
    );
    assert!(t.contains("<tool_result id=\"call_1\">\n18C, sunny\n</tool_result>"));
    assert_eq!(p.tools[0]["name"], "get_weather");
    assert_eq!(
        p.tools[0]["inputSchema"]["properties"]["city"]["type"],
        "string"
    );
}

#[test]
fn prepare_refuses_what_it_cannot_do() {
    let base = |extra: Value| {
        let mut b = obj(json!({"model": "haiku", "messages": [{"role": "user", "content": "hi"}]}));
        for (k, v) in extra.as_object().unwrap() {
            b.insert(k.clone(), v.clone());
        }
        prepare(&b)
    };
    assert!(base(json!({"n": 2})).unwrap_err().contains("n > 1"));
    assert!(base(json!({"reasoning_effort": "huge"})).is_err());
    let bad_name = json!({"tools": [{"type": "function", "function": {"name": "a b"}}]});
    assert!(base(bad_name).unwrap_err().contains("tool name"));
    let long = "x".repeat(57);
    assert!(base(json!({"tools": [{"type": "function", "function": {"name": long}}]})).is_err());
    assert!(base(json!({"tools": [{"type": "web_search"}]})).is_err());
    // tool_choice none: tools are not offered
    let none = json!({"tool_choice": "none",
                      "tools": [{"type": "function", "function": {"name": "f"}}]});
    assert!(base(none).unwrap().tools.is_empty());
    let audio = obj(json!({"model": "haiku", "messages": [{"role": "user",
        "content": [{"type": "input_audio", "input_audio": {}}]}]}));
    assert!(prepare(&audio).unwrap_err().contains("input_audio"));
    assert!(prepare(&obj(json!({"model": "haiku", "messages": []}))).is_err());
    assert!(
        prepare(&obj(
            json!({"model": "h", "messages": [{"role": "system", "content": "x"}]})
        ))
        .is_err()
    );
}

fn feed(tr: &mut Translator, lines: &[Value]) -> Vec<Out> {
    lines.iter().flat_map(|l| tr.push(l, 1_000)).collect()
}

fn ev(e: Value) -> Value {
    json!({"type": "stream_event", "event": e, "parent_tool_use_id": null})
}

#[test]
fn translator_text_thinking_usage() {
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "system", "subtype": "init", "model": "claude-haiku-x"}),
            ev(
                json!({"type": "message_start", "message": {"model": "claude-haiku-x",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1}}}),
            ),
            ev(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
            ),
            ev(json!({"type": "content_block_delta", "index": 0,
                      "delta": {"type": "thinking_delta", "thinking": "hmm"}})),
            ev(json!({"type": "content_block_start", "index": 1,
                      "content_block": {"type": "text", "text": ""}})),
            ev(json!({"type": "content_block_delta", "index": 1,
                      "delta": {"type": "text_delta", "text": "Hi"}})),
            // the whole message again: already passed on from the stream events
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}}),
            ev(
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                      "usage": {"output_tokens": 4}}),
            ),
            ev(json!({"type": "message_stop"})),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "Hi",
                   "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 4}}),
        ],
    );
    assert_eq!(tr.model.as_deref(), Some("claude-haiku-x"));
    let usage = Usage {
        input: 10,
        cache_creation: 0,
        cache_read: 90,
        output: 4,
    };
    assert_eq!(
        outs,
        vec![
            Out::Reasoning("hmm".into()),
            Out::Text("Hi".into()),
            Out::Finish {
                reason: "stop".into(),
                usage
            }
        ]
    );
    assert!(tr.done());
    let u = usage.to_openai();
    assert_eq!(u["prompt_tokens"], 100);
    assert_eq!(u["completion_tokens"], 4);
    assert_eq!(u["total_tokens"], 104);
    assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 90);
}

#[test]
fn translator_tool_use_ends_at_message_stop() {
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            ev(json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}})),
            ev(
                json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "tool_use", "id": "toolu_1", "name": "mcp__c__get_weather", "input": {}}}),
            ),
            ev(json!({"type": "content_block_delta", "index": 0,
                      "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}})),
            ev(json!({"type": "content_block_delta", "index": 0,
                      "delta": {"type": "input_json_delta", "partial_json": "\"Paris\"}"}})),
            ev(
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                      "usage": {"output_tokens": 7}}),
            ),
            ev(json!({"type": "message_stop"})),
            json!({"type": "result", "subtype": "success", "is_error": false}),
        ],
    );
    assert_eq!(
        outs[0],
        Out::ToolStart {
            index: 0,
            id: "toolu_1".into(),
            name: "get_weather".into()
        }
    );
    let Out::Finish { reason, usage } = outs.last().unwrap() else {
        panic!("{outs:?}")
    };
    assert_eq!(reason, "tool_calls");
    assert_eq!((usage.input, usage.output), (5, 7));
    assert_eq!(outs.len(), 4, "nothing after the finish: {outs:?}");
    let mut c = Collector::default();
    outs.iter().for_each(|o| c.push(o));
    let v = c.completion("id1", 1, "m");
    let m = &v["choices"][0]["message"];
    assert!(m["content"].is_null());
    assert_eq!(m["tool_calls"][0]["function"]["name"], "get_weather");
    assert_eq!(
        m["tool_calls"][0]["function"]["arguments"],
        r#"{"city":"Paris"}"#
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn translator_whole_messages_without_partials() {
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "assistant", "message": {"model": "m1", "content": [
                {"type": "text", "text": "Calling."},
                {"type": "tool_use", "id": "t1", "name": "mcp__c__f", "input": {"a": 1}}],
                "usage": {"input_tokens": 3, "output_tokens": 2}}}),
        ],
    );
    assert_eq!(outs[0], Out::Text("Calling.".into()));
    assert_eq!(
        outs[2],
        Out::ToolArgs {
            index: 0,
            json: r#"{"a":1}"#.into()
        }
    );
    assert!(matches!(&outs[3], Out::Finish { reason, .. } if reason == "tool_calls"));
}

#[test]
fn translator_rate_limits_and_errors() {
    // the rate-limit event carries the reset; the result says it failed
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "rate_limit_event", "rate_limit_info":
                {"status": "rejected", "resetsAt": 1_120, "rateLimitType": "five_hour"}}),
            json!({"type": "assistant", "error": "rate_limit", "message":
                {"content": [{"type": "text", "text": "You've hit your limit"}]}}),
        ],
    );
    assert_eq!(
        outs,
        vec![Out::RateLimited {
            retry_after: Some(120),
            message: "You've hit your limit".into()
        }]
    );
    // older CLIs: "...|<reset>" in the result text
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "result", "subtype": "success", "is_error": true,
                  "result": "Claude AI usage limit reached|1300"}),
        ],
    );
    assert!(matches!(
        &outs[0],
        Out::RateLimited {
            retry_after: Some(300),
            ..
        }
    ));
    // not logged in: 503, so a fallback card can take it
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "result", "subtype": "success", "is_error": true,
                  "result": "Invalid API key · Please run /login"}),
        ],
    );
    assert!(
        matches!(&outs[0], Out::Error { status: 503, kind, .. } if kind == "upstream_auth_error")
    );
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
                  "errors": ["something broke"]}),
        ],
    );
    assert!(
        matches!(&outs[0], Out::Error { status: 502, message, .. } if message == "something broke")
    );
    let mut tr = Translator::default();
    let outs = feed(
        &mut tr,
        &[json!({"type": "result", "is_error": true, "result": "API Error: 529 Overloaded"})],
    );
    assert!(matches!(&outs[0], Out::Error { status: 503, .. }));
}

#[test]
fn sse_chunks() {
    let s = sse_chunk(
        &Out::ToolStart {
            index: 0,
            id: "t".into(),
            name: "f".into(),
        },
        "id",
        1,
        "m",
    )
    .unwrap();
    let v: Value = serde_json::from_str(s.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(
        v["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "f"
    );
    let s = sse_chunk(
        &Out::Finish {
            reason: "stop".into(),
            usage: Usage {
                input: 1,
                output: 2,
                ..Usage::default()
            },
        },
        "id",
        1,
        "m",
    )
    .unwrap();
    assert!(s.contains("\"finish_reason\":\"stop\"") && s.contains("\"total_tokens\":3"));
}

fn enabled() -> RouterConfig {
    serde_json::from_value(json!({"claude": {"enabled": true}})).unwrap()
}

#[cfg(feature = "claude-engine")]
#[test]
fn expand_adds_the_virtual_card_once() {
    let mut c = enabled();
    expand(&mut c).unwrap();
    expand(&mut c).unwrap();
    let u = &c.upstreams["claude"];
    assert!(u.is_virtual());
    assert_eq!(u.lanes, 32);
    assert_eq!(u.engine, ENGINE);
    let names: Vec<&str> = c.deployments.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["claude-opus", "claude-sonnet", "claude-haiku"]);
    assert_eq!(c.deployments[2].served_id, "haiku");
    assert_eq!(c.deployments[0].model_info["access"], "admins");
    assert_eq!(c.on_demand.exclude_cards, ["claude"]);
    crate::config::validate(&c).unwrap();
    let snap = Snapshot::build(enabled(), None).unwrap();
    // no qfn-lb passthrough and never the main pool
    assert!(snap.lb_cards("claude").is_none());
    assert!(snap.lb_cards(ENGINE).is_none());
    assert!(snap.main_pool().is_empty());
    let models = crate::server::models(&snap);
    assert_eq!(models["data"].as_array().unwrap().len(), 3);
}

#[cfg(feature = "claude-engine")]
#[test]
fn expand_refuses_bad_configs() {
    let mut c = enabled();
    c.upstreams.insert(
        "claude".into(),
        Upstream {
            url: "http://h:1".into(),
            engine: "ninfer".into(),
            served_id: "x".into(),
            lanes: 1,
            key_file: None,
            online: true,
            catalog_model: None,
        },
    );
    assert!(
        expand(&mut c)
            .unwrap_err()
            .contains("already a real upstream")
    );
    let mut c = enabled();
    c.claude.access = "everyone".into();
    assert!(expand(&mut c).is_err());
    let mut c = enabled();
    c.claude.max_sessions = 0;
    assert!(expand(&mut c).is_err());
    // a hand-written virtual upstream without the engine
    let mut c = RouterConfig::default();
    c.upstreams.insert(
        "x".into(),
        Upstream {
            url: VIRTUAL_URL.into(),
            engine: ENGINE.into(),
            served_id: String::new(),
            lanes: 1,
            key_file: None,
            online: true,
            catalog_model: None,
        },
    );
    assert!(crate::config::validate(&c).is_err());
}

#[cfg(not(feature = "claude-engine"))]
#[test]
fn without_the_feature_the_section_is_refused() {
    assert!(
        Snapshot::build(enabled(), None)
            .unwrap_err()
            .contains("claude-engine")
    );
    // off: passes through untouched
    Snapshot::build(RouterConfig::default(), None).unwrap();
}

#[cfg(feature = "claude-engine")]
#[test]
fn cli_args_plain_model_and_tools() {
    use super::engine::{cli_args, mcp_reply};
    let cfg = ClaudeConfig::default();
    let mut p = prepare(&obj(json!({"model": "opus", "reasoning_effort": "high",
        "messages": [{"role": "user", "content": "hi"}]})))
    .unwrap();
    let a = cli_args(&cfg, &p, std::path::Path::new("/tmp/p.txt"));
    let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
    assert_eq!(a[pos("--tools") + 1], "");
    assert_eq!(a[pos("--model") + 1], "opus");
    assert_eq!(a[pos("--effort") + 1], "high");
    assert_eq!(a[pos("--permission-mode") + 1], "dontAsk");
    assert_eq!(a[pos("--system-prompt-file") + 1], "/tmp/p.txt");
    assert_eq!(a[pos("--mcp-config") + 1], r#"{"mcpServers":{}}"#);
    assert!(a.contains(&"--safe-mode".into()) && a.contains(&"--no-session-persistence".into()));
    assert!(!a.iter().any(|x| x == "--allowedTools"));
    p.tools = vec![json!({"name": "f", "description": "", "inputSchema": {"type": "object"}})];
    let a = cli_args(&cfg, &p, std::path::Path::new("/tmp/p.txt"));
    let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
    let mcp: Value = serde_json::from_str(&a[pos("--mcp-config") + 1]).unwrap();
    assert_eq!(
        mcp,
        json!({"mcpServers": {"c": {"type": "sdk", "name": "c"}}})
    );
    assert_eq!(a[pos("--allowedTools") + 1], "mcp__c");

    let r = mcp_reply(
        &p.tools,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    assert_eq!(r["result"]["tools"][0]["name"], "f");
    let r = mcp_reply(
        &p.tools,
        &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26"}}),
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    let r = mcp_reply(
        &p.tools,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
    );
    assert_eq!(r["error"]["code"], -32601);
}
