//! The Claude engine against a fake `claude` (src/bin/fake_claude.rs) replaying stream-json:
//! answers, streaming, usage, client tools over the MCP bridge, rate limits, errors and
//! failover, the session pool, cancellation and access.
#![cfg(feature = "claude-engine")]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt;
use orca_router::config::{RouterConfig, Snapshot};
use orca_router::server::Mode;
use orca_router::state::AppState;
use serde_json::{Value, json};

const HELLO: &str = include_str!("fixtures/claude/hello.jsonl");

fn fake_bin() -> &'static str {
    env!("CARGO_BIN_EXE_abhoth-fake-claude")
}

struct Rig {
    r: Router_,
    dir: tempfile::TempDir,
}

impl Rig {
    fn scenario(&self) -> PathBuf {
        self.dir.path().join("scenario.jsonl")
    }
    fn set_scenario(&self, text: &str) {
        std::fs::write(self.scenario(), text).unwrap();
    }
    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("record.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn starts(&self) -> usize {
        self.records()
            .iter()
            .filter(|r| r.get("argv").is_some())
            .count()
    }
    fn load(&self) -> (u32, u32) {
        self.r.st.ext.claude.load()
    }
    async fn wait_idle(&self, within: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < within {
            if self.load() == (0, 0) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

/// A router whose `claude` runs the fake with `scenario`; `claude` and `rest` are merged into
/// the config.
async fn rig_with(scenario: &str, claude: Value, rest: Value) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let sc = dir.path().join("scenario.jsonl");
    std::fs::write(&sc, scenario).unwrap();
    let rec = dir.path().join("record.jsonl");
    let mut c = json!({
        "enabled": true,
        "command": fake_bin(),
        "extraArgs": ["--fake-scenario", sc, "--fake-record", rec],
        "workDir": dir.path().join("eng"),
        "routes": {"claude-haiku": "haiku"},
    });
    for (k, v) in claude.as_object().unwrap() {
        c[k] = v.clone();
    }
    let mut cfg = json!({"claude": c});
    for (k, v) in rest.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    let cfg: RouterConfig = serde_json::from_value(cfg).unwrap();
    let r = start_router(cfg, None, Mode::Gateway).await;
    Rig { r, dir }
}

async fn make(scenario: &str, claude: Value) -> Rig {
    rig_with(scenario, claude, json!({})).await
}

fn hello_body(stream: bool) -> Value {
    json!({"model": "claude-haiku", "stream": stream,
           "stream_options": {"include_usage": true}, "max_tokens": 16,
           "messages": [{"role": "system", "content": "You are terse."},
                        {"role": "user", "content": "Say hello."}]})
}

/// SSE `data:` payloads (without [DONE]) and whether [DONE] ended it.
async fn sse(resp: reqwest::Response) -> (Vec<Value>, bool) {
    let text = resp.text().await.unwrap();
    let mut out = Vec::new();
    let mut done = false;
    for l in text.lines() {
        let Some(d) = l.strip_prefix("data: ") else {
            continue;
        };
        if d == "[DONE]" {
            done = true;
        } else {
            out.push(serde_json::from_str(d).unwrap());
        }
    }
    (out, done)
}

#[tokio::test]
async fn answers_like_an_openai_model() {
    let tdir = tempfile::tempdir().unwrap();
    let token = tdir.path().join("token");
    std::fs::write(&token, "test-oauth-token-value\n").unwrap();
    // the token file, as a container would mount it, and a HOME of its own
    let home = tdir.path().join("claude-home");
    let rig = rig_with(
        HELLO,
        json!({"oauthTokenFile": token, "home": home}),
        json!({}),
    )
    .await;
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-abhoth-card"], "claude");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "claude-haiku-5-5");
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Hello! How can I help?"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 734);
    assert_eq!(v["usage"]["completion_tokens"], 11);
    assert_eq!(v["usage"]["total_tokens"], 745);

    // what the CLI was given
    let recs = rig.records();
    let argv: Vec<String> = serde_json::from_value(recs[0]["argv"].clone()).unwrap();
    let after = |f: &str| argv[argv.iter().position(|a| a == f).unwrap() + 1].clone();
    assert_eq!(after("--tools"), "");
    assert_eq!(after("--model"), "haiku");
    assert_eq!(after("--permission-mode"), "dontAsk");
    assert!(argv.contains(&"--safe-mode".into()));
    assert!(argv.contains(&"--no-session-persistence".into()));
    assert!(Path::new(&recs[0]["cwd"].as_str().unwrap()).ends_with("work"));
    assert_eq!(
        recs[0]["env"]["CLAUDE_CODE_OAUTH_TOKEN"],
        "test-oauth-token-value"
    );
    assert!(recs[0]["env"]["ANTHROPIC_API_KEY"].is_null());
    assert_eq!(recs[0]["env"]["HOME"], home.display().to_string());
    assert!(home.is_dir(), "the engine creates its HOME");
    assert_eq!(recs[0]["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "16");
    assert_eq!(recs[1]["system"], "You are terse.");
    let user = &recs[2]["stdin"];
    assert_eq!(user["type"], "user");
    assert_eq!(user["message"]["content"][0]["text"], "Say hello.");
    // the system prompt file is gone, the session released
    assert!(rig.wait_idle(Duration::from_secs(5)).await);
    let prompts = std::fs::read_dir(rig.dir.path().join("eng/prompts")).unwrap();
    assert_eq!(prompts.count(), 0);

    // /v1/models lists the route; the qfn-lb passthrough does not reach the virtual card
    let models: Value = http()
        .get(format!("{}/v1/models", rig.r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "claude-haiku");
    assert_eq!(models["data"][0]["max_output_tokens"], 32000);
    let lb = http()
        .get(format!("{}/claude/v1/models", rig.r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(lb.status(), 404);
    let m = http()
        .get(format!("{}/metrics", rig.r.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(m.contains("abhoth_claude_sessions_started_total 1"), "{m}");
}

#[tokio::test]
async fn streams_with_usage() {
    let rig = make(HELLO, json!({})).await;
    let resp = chat(&rig.r.url, hello_body(true)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let (chunks, done) = sse(resp).await;
    assert!(done);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello! How can I help?");
    let fin = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"] == "stop")
        .unwrap();
    assert_eq!(fin["usage"]["completion_tokens"], 11);
    // include_usage: the extra chunk with empty choices, last
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"].as_array().unwrap().len(), 0);
    assert_eq!(last["usage"]["prompt_tokens"], 734);
    // the request log found the usage too
    assert!(rig.wait_idle(Duration::from_secs(5)).await);
}

fn tool_scenario() -> String {
    let ev = |e: Value| json!({"type": "stream_event", "event": e, "parent_tool_use_id": null});
    [
        json!({"fake": "read_user"}),
        json!({"fake": "mcp", "message": {"jsonrpc": "2.0", "id": 0, "method": "initialize",
                                           "params": {"protocolVersion": "2025-06-18"}}}),
        json!({"fake": "mcp", "message": {"jsonrpc": "2.0", "method": "notifications/initialized"}}),
        json!({"fake": "mcp", "message": {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}}),
        json!({"type": "system", "subtype": "init", "model": "claude-haiku-5-5",
               "tools": ["mcp__c__get_weather"]}),
        ev(json!({"type": "message_start", "message": {"model": "claude-haiku-5-5",
            "usage": {"input_tokens": 3, "cache_creation_input_tokens": 1120, "output_tokens": 1}}})),
        ev(json!({"type": "content_block_start", "index": 0,
                  "content_block": {"type": "text", "text": ""}})),
        ev(json!({"type": "content_block_delta", "index": 0,
                  "delta": {"type": "text_delta", "text": "Checking."}})),
        ev(json!({"type": "content_block_start", "index": 1, "content_block":
            {"type": "tool_use", "id": "toolu_1", "name": "mcp__c__get_weather", "input": {}}})),
        ev(json!({"type": "content_block_delta", "index": 1,
                  "delta": {"type": "input_json_delta", "partial_json": "{\"city\": "}})),
        ev(json!({"type": "content_block_delta", "index": 1,
                  "delta": {"type": "input_json_delta", "partial_json": "\"Paris\"}"}})),
        ev(json!({"type": "content_block_stop", "index": 1})),
        ev(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                  "usage": {"output_tokens": 56}})),
        ev(json!({"type": "message_stop"})),
        // the real CLI would now call the tool: the engine has stopped it before that
        json!({"fake": "hang"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
}

fn weather_tools() -> Value {
    json!([{"type": "function", "function": {"name": "get_weather",
        "description": "Current weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                       "required": ["city"]}}}])
}

#[tokio::test]
async fn client_tools_become_tool_calls() {
    let rig = make(&tool_scenario(), json!({})).await;
    let t = Instant::now();
    let resp = chat(
        &rig.r.url,
        json!({"model": "claude-haiku", "stream": true, "tools": weather_tools(),
               "messages": [{"role": "user", "content": "Weather in Paris?"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let (chunks, done) = sse(resp).await;
    assert!(done);
    let calls: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].get(0))
        .collect();
    assert_eq!(calls[0]["id"], "toolu_1");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    let args: String = calls
        .iter()
        .filter_map(|c| c["function"]["arguments"].as_str())
        .collect();
    assert_eq!(args, r#"{"city": "Paris"}"#);
    let fin = chunks
        .iter()
        .find(|c| !c["choices"][0]["finish_reason"].is_null())
        .unwrap();
    assert_eq!(fin["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(fin["usage"]["prompt_tokens"], 1123);
    // the hanging CLI was stopped as soon as the message ended
    assert!(rig.wait_idle(Duration::from_secs(5)).await);
    assert!(t.elapsed() < Duration::from_secs(10));

    // what crossed the control protocol
    let recs = rig.records();
    let argv: Vec<String> = serde_json::from_value(recs[0]["argv"].clone()).unwrap();
    let after = |f: &str| argv[argv.iter().position(|a| a == f).unwrap() + 1].clone();
    assert_eq!(after("--allowedTools"), "mcp__c");
    let mcp: Value = serde_json::from_str(&after("--mcp-config")).unwrap();
    assert_eq!(mcp["mcpServers"]["c"]["type"], "sdk");
    let stdin: Vec<&Value> = recs.iter().filter_map(|r| r.get("stdin")).collect();
    assert_eq!(stdin[0]["request"]["subtype"], "initialize");
    assert_eq!(stdin[1]["type"], "user");
    let replies: Vec<&Value> = stdin
        .iter()
        .filter_map(|s| s.pointer("/response/response/mcp_response"))
        .collect();
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let tool = &replies[2]["result"]["tools"][0];
    assert_eq!(tool["name"], "get_weather");
    assert_eq!(tool["inputSchema"]["required"][0], "city");

    // non-streaming: the same as message.tool_calls
    let rig = rig_with(&tool_scenario(), json!({}), json!({})).await;
    let v: Value = chat(
        &rig.r.url,
        json!({"model": "claude-haiku", "tools": weather_tools(),
               "messages": [{"role": "user", "content": "Weather in Paris?"}]}),
    )
    .await
    .json()
    .await
    .unwrap();
    let m = &v["choices"][0]["message"];
    assert_eq!(m["content"], "Checking.");
    assert_eq!(
        m["tool_calls"][0]["function"]["arguments"],
        r#"{"city": "Paris"}"#
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

fn limited(resets_in: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    [
        json!({"fake": "read_user"}),
        json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected",
               "resetsAt": now + resets_in, "rateLimitType": "five_hour"}}),
        json!({"type": "assistant", "error": "rate_limit", "message": {"content":
               [{"type": "text", "text": "You've hit your limit · resets 5pm"}]}}),
        json!({"type": "result", "subtype": "success", "is_error": true,
               "result": "You've hit your limit · resets 5pm"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
}

#[tokio::test]
async fn rate_limits_are_429_with_retry_after() {
    let rig = make(&limited(120), json!({"rateLimitWaitSeconds": 5})).await;
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 429);
    let ra: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((100..=121).contains(&ra), "{ra}");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");
    // while limited, no CLI is started: straight to 429
    let resp = chat(&rig.r.url, hello_body(true)).await;
    assert_eq!(resp.status(), 429);
    assert_eq!(rig.starts(), 1);
}

#[tokio::test]
async fn a_short_rate_limit_is_waited_out() {
    let rig = make(&limited(2), json!({"rateLimitWaitSeconds": 30})).await;
    assert_eq!(chat(&rig.r.url, hello_body(false)).await.status(), 429);
    rig.set_scenario(HELLO);
    let t = Instant::now();
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 200);
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn cli_failures_fail_over_to_another_card() {
    let a = Fake::start(|_| {
        Reply::Json(
            200,
            json!({"id": "x", "object": "chat.completion", "model": "local",
                   "choices": [{"index": 0, "message": {"role": "assistant", "content": "local"},
                                "finish_reason": "stop"}]}),
        )
    })
    .await;
    let scenario = [
        json!({"fake": "read_user"}),
        json!({"fake": "stderr", "text": "Invalid API key · Please run /login"}),
        json!({"fake": "exit", "code": 1}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let rig = rig_with(
        &scenario,
        json!({}),
        json!({
            "upstreams": {"A": {"url": a.url(), "engine": "ninfer",
                                "servedId": "local", "lanes": 1}},
            // the Claude card first, the local card as the fallback
            "deployments": [
                {"name": "claude-haiku", "cards": ["claude"], "servedId": "haiku"},
                {"name": "claude-haiku", "cards": ["A"], "servedId": "local"}
            ]
        }),
    )
    .await;
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-abhoth-card"], "A");
    assert_eq!(rig.starts(), 1);

    // alone, the same failure is a 503 with the CLI's message
    let rig = make(&scenario, json!({})).await;
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["type"], "upstream_auth_error");
    assert!(v["error"]["message"].as_str().unwrap().contains("/login"));

    // a CLI that is not installed
    let rig = make(HELLO, json!({"command": "abhoth-no-such-claude-cli"})).await;
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 503);
}

fn slow_hello(ms: u64) -> String {
    let mut s = format!(
        "{}\n{}\n",
        json!({"fake": "read_user"}),
        json!({"fake": "sleep", "ms": ms})
    );
    s.push_str(HELLO.split_once('\n').unwrap().1);
    s
}

#[tokio::test]
async fn sessions_queue_then_429() {
    // one session, a short queue: the second request is refused with Retry-After
    let rig = make(
        &slow_hello(2500),
        json!({"maxSessions": 1, "queueSeconds": 1}),
    )
    .await;
    let url = rig.r.url.clone();
    let first = tokio::spawn(async move { chat(&url, hello_body(false)).await.status() });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rig.load().0, 1);
    let t = Instant::now();
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().contains_key("retry-after"));
    assert!(t.elapsed() >= Duration::from_millis(900));
    assert_eq!(first.await.unwrap(), 200);

    // a longer queue: it waits its turn
    let rig = make(
        &slow_hello(800),
        json!({"maxSessions": 1, "queueSeconds": 30}),
    )
    .await;
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let url = rig.r.url.clone();
        tasks.push(tokio::spawn(async move {
            chat(&url, hello_body(false)).await.status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.load(), (1, 2));
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert!(rig.wait_idle(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn many_sessions_at_once() {
    // the default pool is wide: 20 at once start together, none queues
    let rig = make(&slow_hello(600), json!({})).await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let url = rig.r.url.clone();
        tasks.push(tokio::spawn(async move {
            chat(&url, hello_body(false)).await.status()
        }));
    }
    let t = Instant::now();
    let mut peak = 0;
    while t.elapsed() < Duration::from_secs(5) && peak < 20 {
        peak = peak.max(rig.load().0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert_eq!(peak, 20);
    assert_eq!(rig.r.st.snapshot().config.upstreams["claude"].lanes, 32);
}

#[tokio::test]
async fn a_client_that_leaves_stops_the_cli() {
    let mut s: Vec<String> = HELLO.lines().take(6).map(str::to_string).collect();
    s.push(json!({"fake": "hang"}).to_string());
    let rig = make(&s.join("\n"), json!({})).await;
    let resp = chat(&rig.r.url, hello_body(true)).await;
    assert_eq!(resp.status(), 200);
    let mut body = resp.bytes_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("assistant"));
    assert_eq!(rig.load().0, 1);
    drop(body);
    assert!(
        rig.wait_idle(Duration::from_secs(5)).await,
        "{:?}",
        rig.load()
    );
}

#[tokio::test]
async fn a_cli_that_hangs_times_out() {
    let s = format!(
        "{}\n{}",
        json!({"fake": "read_user"}),
        json!({"fake": "hang"})
    );
    let rig = make(&s, json!({"timeoutSeconds": 1})).await;
    let t = Instant::now();
    let resp = chat(&rig.r.url, hello_body(false)).await;
    assert_eq!(resp.status(), 504);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(rig.wait_idle(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn bad_requests_are_400_and_start_nothing() {
    let rig = make(HELLO, json!({})).await;
    let mut b = hello_body(false);
    b["n"] = json!(2);
    assert_eq!(chat(&rig.r.url, b).await.status(), 400);
    let mut b = hello_body(false);
    b["tools"] = json!([{"type": "function", "function": {"name": "has space"}}]);
    let resp = chat(&rig.r.url, b).await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tool name")
    );
    assert_eq!(rig.starts(), 0);
}

#[tokio::test]
async fn access_is_admins_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("admin-key");
    std::fs::write(&key, "admin-secret\n").unwrap();
    let cfg: RouterConfig = serde_json::from_value(json!({
        "settings": {"adminKeyFile": key},
        "claude": {"enabled": true}
    }))
    .unwrap();
    let st = AppState::new(Snapshot::build(cfg, None).unwrap(), None);
    let snap = st.snapshot();
    let lan: std::net::IpAddr = "203.0.113.20".parse().unwrap();
    let local: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let none = axum::http::HeaderMap::new();
    let mut admin = axum::http::HeaderMap::new();
    admin.insert("authorization", "Bearer admin-secret".parse().unwrap());
    let mut wrong = axum::http::HeaderMap::new();
    wrong.insert("authorization", "Bearer nope".parse().unwrap());
    use orca_router::claude::allowed;
    assert!(!allowed(&st, &snap, &none, Some(lan)).await);
    assert!(!allowed(&st, &snap, &wrong, Some(lan)).await);
    assert!(allowed(&st, &snap, &admin, Some(lan)).await);
    assert!(allowed(&st, &snap, &none, Some(local)).await);

    // opened up on purpose
    let cfg: RouterConfig =
        serde_json::from_value(json!({"claude": {"enabled": true, "access": "users"}})).unwrap();
    let st = AppState::new(Snapshot::build(cfg, None).unwrap(), None);
    assert!(allowed(&st, &st.snapshot(), &none, Some(lan)).await);
    // keys: lists need the control plane's key checks: admins only without it
    let cfg: RouterConfig =
        serde_json::from_value(json!({"claude": {"enabled": true, "access": "keys:me"}})).unwrap();
    let st = AppState::new(Snapshot::build(cfg, None).unwrap(), None);
    assert!(!allowed(&st, &st.snapshot(), &none, Some(lan)).await);
    assert!(allowed(&st, &st.snapshot(), &none, Some(local)).await);
}

#[test]
fn the_example_config_loads() {
    let v: Value =
        serde_json::from_str(include_str!("../../../deploy/router/claude.example.json")).unwrap();
    let cfg: RouterConfig = serde_json::from_value(v).unwrap();
    assert_eq!(cfg.claude.access, "admins");
    let snap = Snapshot::build(cfg, None).unwrap();
    assert_eq!(snap.config.upstreams["claude"].lanes, 32);
    assert_eq!(snap.config.deployments.len(), 3);
}
