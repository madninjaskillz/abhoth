//! The stream guards of the turn loop against the fake server: the thinking-runaway cancel,
//! the per-step reasoning cap, partial output kept when a stream dies mid-response, adaptive
//! reasoning, the thinking budget note, the server error probe after an empty reply and the
//! startup probes. Ports of the matching `StreamingResponseProcessorTests`,
//! `AgentLoopScenarioTests` and `AdaptiveReasoningScenarioTests` cases.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::ToolContext;
use orca_core::agent::{AgentObserver, AgentSession, NullObserver, TurnEnd};
use orca_core::chat::reasoning_cap::{NUDGE, TAIL_INTRO};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply, ServerOptions};
use serde_json::{Value, json};

fn config(server: &FakeOpenAi, native: bool) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["model-a".into()];
    config.lm_studio.native_tool_calling = native;
    config.agent.verification_mode = "off".into();
    config.permissions.auto_approve_all = true;
    config
}

fn session_with(config: OrcaConfig, work: &std::path::Path) -> AgentSession {
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let registry = orca_cli::build_registry(&config);
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(work));
    s.retry_base_delay = Duration::from_millis(1);
    s.failover_delay = Duration::from_millis(1);
    s.set_system_prompt("system".into());
    s
}

/// Pure reasoning in 16-character chunks, no text part at all (llama.cpp's null role content).
fn spiral(chunks: usize) -> Reply {
    Reply {
        reasoning: "r".repeat(16 * chunks),
        prompt_tokens: 100,
        completion_tokens: 10,
        ..Reply::default()
    }
    .with_reasoning_chunks(16)
    .with_null_role_content()
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().unwrap()
}

#[derive(Default)]
struct Notices(Vec<String>);
impl AgentObserver for Notices {
    fn on_notice(&mut self, message: &str) {
        self.0.push(message.to_string());
    }
}

#[tokio::test]
async fn thinking_runaway_three_strikes_ends_turn_early() {
    let server = FakeOpenAi::start(vec![spiral(40)]).await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.thinking.runaway_cap_chunks = 8;
    let mut s = session_with(c, work.path());
    let mut notices = Notices::default();
    let end = s.run_turn("do something", &mut notices).await.unwrap();
    assert_eq!(end, TurnEnd::Done);

    // Strikes 1-2 redirect, strike 3 launches one low-temperature recovery attempt, a spiral
    // during recovery ends the turn.
    let reqs = server.requests();
    assert_eq!(reqs.len(), 4);
    let redirects = s
        .messages
        .iter()
        .filter(|m| {
            m.content
                .as_deref()
                .is_some_and(|t| t.contains("entirely internal reasoning"))
        })
        .count();
    assert_eq!(redirects, 3);
    assert_eq!(reqs[3].body["temperature"], json!(0.2));
    assert!(reqs[2].body["temperature"] != json!(0.2));
    assert!(
        notices
            .0
            .iter()
            .any(|n| n.starts_with("Thinking runaway cancelled after 8 reasoning chunks"))
    );
}

#[tokio::test]
async fn long_thinking_then_action_is_not_flagged() {
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: "r".repeat(16 * 20),
            ..Reply::text("The answer is 42.")
        }
        .with_reasoning_chunks(16),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.thinking.runaway_cap_chunks = 100;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        s.last_assistant_response.as_deref(),
        Some("The answer is 42.")
    );
}

#[tokio::test]
async fn runaway_needs_no_text_part_on_the_reasoning_channel() {
    // C# trips the reasoning-channel guard only while no text item arrived: a stream that
    // opens with an empty content delta never trips it.
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: "r".repeat(16 * 40),
            ..Reply::text("done")
        }
        .with_reasoning_chunks(16),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.thinking.runaway_cap_chunks = 8;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 1);
    assert_eq!(s.last_assistant_response.as_deref(), Some("done"));
}

#[tokio::test]
async fn think_tags_count_toward_the_runaway_guard() {
    let text = format!("<think>{}", "x".repeat(8 * 30));
    let server = FakeOpenAi::start(vec![
        Reply::text(text),
        Reply::text("The answer is 42, after thinking it over."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.thinking.runaway_cap_chunks = 10;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    let last = messages(&reqs[1].body).last().unwrap();
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("entirely internal reasoning")
    );
}

async fn reasoning_cap_case(reasoning_sent_back: bool) {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("notes.txt").display().to_string();
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: format!("Plan: write notes.txt. {}", "r".repeat(16 * 500)),
            ..Reply::text("never reached")
        }
        .with_reasoning_chunks(16),
        Reply {
            reasoning: "r".repeat(16 * 500),
            ..Reply::text(format!(
                "<tool_call>\n{{\"name\": \"write_file\", \"arguments\": {{\"path\": {}, \"content\": \"hello\"}}}}\n</tool_call>",
                serde_json::to_string(&path).unwrap()
            ))
        }
        .with_reasoning_chunks(16),
        Reply::text("done"),
    ])
    .await;
    let mut c = config(&server, false);
    c.agent.reasoning.max_step_tokens = 100;
    if reasoning_sent_back {
        c.slots = Some(Default::default());
    }
    let mut s = session_with(c, work.path());
    s.run_turn("write notes.txt", &mut NullObserver)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(work.path().join("notes.txt")).unwrap(),
        "hello"
    );
    let reqs = server.requests();
    let (first, second) = (messages(&reqs[0].body), messages(&reqs[1].body));
    assert_eq!(
        second.len(),
        first.len() + if reasoning_sent_back { 2 } else { 1 }
    );
    for (a, b) in first.iter().zip(second) {
        assert_eq!(a, b, "the re-ask only appends");
    }
    let nudge = second.last().unwrap();
    assert_eq!(nudge["role"], "user");
    let text = nudge["content"].as_str().unwrap();
    assert!(text.starts_with(NUDGE));
    if reasoning_sent_back {
        assert_eq!(text, NUDGE);
        let partial = &s.messages[first.len()];
        assert!(
            partial
                .reasoning
                .as_deref()
                .unwrap()
                .starts_with("Plan: write notes.txt.")
        );
    } else {
        assert!(text.contains(TAIL_INTRO));
    }
    // One fire only: the second long think ran to its tool call.
    let fires = s
        .messages
        .iter()
        .filter(|m| {
            m.role == orca_core::client::Role::User
                && m.content.as_deref().is_some_and(|t| t.starts_with(NUDGE))
        })
        .count();
    assert_eq!(fires, 1);
}

#[tokio::test]
async fn reasoning_cap_long_thinking_step_is_cut_and_asked_again_then_the_tool_call_runs() {
    reasoning_cap_case(false).await;
}

#[tokio::test]
async fn reasoning_cap_long_thinking_step_is_cut_and_asked_again_with_reasoning_sent_back() {
    reasoning_cap_case(true).await;
}

#[tokio::test]
async fn reasoning_cap_leaves_short_thinking_and_tagged_thinking_alone() {
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: "r".repeat(16 * 10),
            ..Reply::text("The answer is 42.")
        }
        .with_reasoning_chunks(16),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.agent.reasoning.max_step_tokens = 100;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 1);

    // Thinking inside <think> tags is text, not the reasoning channel: not capped.
    let server = FakeOpenAi::start(vec![Reply::text(format!(
        "<think>{}</think>Done.",
        "r".repeat(4000)
    ))])
    .await;
    let mut c = config(&server, true);
    c.agent.reasoning.max_step_tokens = 100;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_stream_that_dies_after_a_complete_text_tool_call_runs_it() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("x.txt"), "alpha\n").unwrap();
    let call = "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"x.txt\"}}\n</tool_call>";
    // The role chunk, then the call in 8-character pieces, then a little more text.
    let events = 1 + call.chars().count().div_ceil(8) + 1;
    let server = FakeOpenAi::start(vec![
        Reply::text(format!("{call} and then I will summarise it")).dying_after(events),
        Reply::text("x.txt holds a single line that says alpha, nothing more."),
    ])
    .await;
    let mut s = session_with(config(&server, false), work.path());
    let mut notices = Notices::default();
    s.run_turn("read x.txt", &mut notices).await.unwrap();
    assert_eq!(s.tool_call_history.len(), 1);
    assert_eq!(server.requests().len(), 2);
    assert!(
        notices
            .0
            .iter()
            .any(|n| n.starts_with("Stream interrupted mid-response"))
    );
    // The partial text is the assistant turn.
    let second = messages(&server.requests()[1].body).clone();
    let assistant = &second[second.len() - 2];
    assert_eq!(assistant["role"], "assistant");
    assert!(
        assistant["content"]
            .to_string()
            .contains("</tool_call> and the")
    );
}

#[tokio::test]
async fn a_stream_that_dies_without_a_tool_call_is_not_retried_when_the_error_is_not_transient() {
    let server = FakeOpenAi::start(vec![
        Reply::text("I am about to look at the files and then").dying_after(3),
        Reply::text("never asked"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session_with(config(&server, true), work.path());
    let mut notices = Notices::default();
    let end = s.run_turn("q", &mut notices).await.unwrap();
    assert_eq!(end, TurnEnd::Done);
    assert_eq!(server.requests().len(), 1);
    assert!(
        notices
            .0
            .iter()
            .any(|n| n.starts_with("Stream interrupted: ")
                && n.ends_with("partial text had no complete tool call."))
    );
    assert_eq!(s.last_assistant_response, None);
}

#[tokio::test]
async fn a_stream_that_dies_before_any_text_fails_the_turn() {
    // Nothing to salvage: the error ends the turn, as the C# exception does.
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: "thinking".into(),
            ..Reply::text("text")
        }
        .with_null_role_content()
        .dying_after(2),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session_with(config(&server, true), work.path());
    assert!(s.run_turn("q", &mut NullObserver).await.is_err());
}

#[tokio::test]
async fn an_empty_text_reply_with_a_content_item_is_not_an_empty_response() {
    // The stream opens with "content": "" (a text item in C#): not empty, even without usage.
    let server = FakeOpenAi::start(vec![Reply::default(), Reply::text("never")]).await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session_with(config(&server, true), work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 1);
    assert_eq!(s.last_assistant_response.as_deref(), Some(""));
}

#[tokio::test]
async fn an_empty_reply_probes_the_server_for_its_error() {
    let server = FakeOpenAi::start(vec![
        Reply::empty(),
        Reply::error(500, r#"{"error":{"message":"model crashed"}}"#),
        Reply::text("Recovered and answering now, after the retry."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, false);
    c.lm_studio.max_tokens = Some(1234);
    let mut s = session_with(c, work.path());
    let mut notices = Notices::default();
    s.run_turn("q", &mut notices).await.unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), 3);
    let probe = &reqs[1].body;
    let keys: Vec<&String> = probe.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["model", "messages", "max_tokens", "temperature", "stream"]
    );
    assert_eq!(probe["max_tokens"], 1234);
    assert_eq!(probe["stream"], false);
    assert_eq!(
        probe["messages"][1],
        json!({"role": "user", "content": "q"})
    );
    assert!(
        notices
            .0
            .contains(&"LLM server error: model crashed".to_string())
    );
    assert!(
        notices
            .0
            .contains(&"LLM returned an empty response.".to_string())
    );
}

#[tokio::test]
async fn an_empty_reply_whose_probe_reports_an_overflow_compacts_instead() {
    let server = FakeOpenAi::start(vec![
        Reply::empty(),
        Reply::error(
            400,
            r#"{"error":{"message":"the request exceeds the available context size (4096 tokens)"}}"#,
        ),
        Reply::text("answer"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, false);
    c.context.context_window_size = 32768;
    let mut s = session_with(c, work.path());
    s.run_turn("q", &mut NullObserver).await.unwrap();
    assert_eq!(s.config.context.context_window_size, 4096);
}

#[tokio::test]
async fn thinking_budget_note_follows_the_assistant_turn() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("x.txt"), "alpha\n").unwrap();
    let server = FakeOpenAi::start(vec![
        Reply {
            reasoning: "r".repeat(16 * 5),
            ..Reply::tool("read_file", json!({"path": "x.txt"}))
        }
        .with_reasoning_chunks(16),
        Reply::text("x.txt holds a single line that says alpha, nothing more."),
    ])
    .await;
    let mut c = config(&server, true);
    c.thinking.budget_tokens = 3;
    let mut s = session_with(c, work.path());
    s.run_turn("read x.txt", &mut NullObserver).await.unwrap();
    let second = messages(&server.requests()[1].body).clone();
    let n = second.len();
    assert_eq!(second[n - 3]["role"], "assistant");
    assert_eq!(
        second[n - 2]["content"],
        "Note: Your thinking exceeded the budget of 3 tokens (5 used). Keep reasoning concise for simple tasks."
    );
    assert_eq!(second[n - 1]["role"], "tool");
}

fn thinking_off(body: &Value) -> bool {
    body["chat_template_kwargs"]["enable_thinking"] == json!(false)
}

async fn adaptive_run(adaptive: Option<&str>, break_the_edit: bool) -> (Vec<Value>, AgentSession) {
    let work = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "write_file",
            json!({"path": "notes.txt", "content": "hello world\n"}),
        ),
        Reply::tool(
            "edit_file",
            json!({"path": "notes.txt", "old_string": if break_the_edit { "no such text" } else { "world" }, "new_string": "there"}),
        ),
        Reply::tool("read_file", json!({"path": "notes.txt"})),
        Reply::text("Wrote notes.txt, changed one word in it and read it back: it says hello there."),
    ])
    .await;
    let mut c = config(&server, true);
    c.agent.reasoning.adaptive = adaptive.map(str::to_string);
    let mut s = session_with(c, work.path());
    s.run_turn(
        "write notes.txt, then change world to there",
        &mut NullObserver,
    )
    .await
    .unwrap();
    let bodies = server.requests().into_iter().map(|r| r.body).collect();
    (bodies, s)
}

#[tokio::test]
async fn lead_skips_thinking_after_a_successful_write_and_edit_not_after_a_read() {
    let (bodies, s) = adaptive_run(Some("lead"), false).await;
    assert_eq!(
        s.tool_call_history.iter().filter(|r| !r.is_error).count(),
        3
    );
    let off: Vec<bool> = bodies.iter().take(4).map(thinking_off).collect();
    assert_eq!(off, [false, true, true, false]);
    for b in bodies.iter().take(4) {
        assert_eq!(b["reasoning_effort"], "medium");
    }
}

#[tokio::test]
async fn lead_thinks_after_a_failed_edit() {
    let (bodies, s) = adaptive_run(Some("all"), true).await;
    assert!(
        s.tool_call_history
            .iter()
            .any(|r| r.name == "edit_file" && r.is_error)
    );
    let off: Vec<bool> = bodies.iter().take(3).map(thinking_off).collect();
    assert_eq!(off, [false, true, false]);
}

#[tokio::test]
async fn lead_not_adaptive_sends_nothing_new() {
    for adaptive in [None, Some("worker")] {
        let (bodies, _) = adaptive_run(adaptive, false).await;
        for b in &bodies {
            assert!(!thinking_off(b));
            assert!(b.get("reasoning_effort").is_none());
        }
    }
}

#[tokio::test]
async fn lead_with_the_reasoning_cap_too_a_reask_keeps_thinking_off_and_is_not_a_skip() {
    let work = tempfile::tempdir().unwrap();
    let long_think = || {
        Reply {
            reasoning: "r".repeat(16 * 500),
            ..Reply::text("never reached")
        }
        .with_reasoning_chunks(16)
    };
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "write_file",
            json!({"path": "notes.txt", "content": "hello world\n"}),
        ),
        long_think(),
        Reply::tool(
            "edit_file",
            json!({"path": "notes.txt", "old_string": "world", "new_string": "there"}),
        ),
        Reply::tool(
            "write_file",
            json!({"path": "notes.txt", "content": "hello again\n"}),
        ),
        Reply::tool("read_file", json!({"path": "notes.txt"})),
        Reply::text(
            "Wrote notes.txt, changed one word in it and read it back: it says hello again.",
        ),
    ])
    .await;
    let mut c = config(&server, true);
    c.agent.reasoning.adaptive = Some("lead".into());
    c.agent.reasoning.adaptive_max_skips = 2;
    c.agent.reasoning.max_step_tokens = 100;
    let mut s = session_with(c, work.path());
    s.run_turn("write notes.txt", &mut NullObserver)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(work.path().join("notes.txt")).unwrap(),
        "hello again\n"
    );
    let off: Vec<bool> = server
        .requests()
        .iter()
        .take(5)
        .map(|r| thinking_off(&r.body))
        .collect();
    // first request, after write (skip 1), its re-ask, after edit (skip 2), after write (limit)
    assert_eq!(off, [false, true, true, true, false]);
}

#[tokio::test]
async fn startup_probes_negotiate_max_tokens_and_lower_the_window_to_the_servers() {
    let server = FakeOpenAi::start_with(
        vec![
            Reply::error(
                400,
                r#"{"error":{"message":"max_tokens must be less than or equal to `8192`"}}"#,
            ),
            Reply::text("hello"),
        ],
        ServerOptions::default()
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"model-a","owned_by":"llamacpp","max_model_len":32768}]}"#,
            )
            .route("GET /health", 200, r#"{"status":"ok"}"#),
    )
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.context.context_window_size = 262144;
    let mut s = session_with(c, work.path());
    s.startup_probes = true;
    s.server_identity = orca_core::client::ServerIdentity::new(s.probe_http.clone());
    let mut notices = Notices::default();
    s.run_turn("hi", &mut notices).await.unwrap();
    assert_eq!(s.config.lm_studio.max_tokens, Some(8192));
    assert_eq!(s.config.context.context_window_size, 32768);
    let reqs = server.requests();
    assert_eq!(reqs[0].body["max_tokens"], 32768);
    assert_eq!(reqs[1].body["max_completion_tokens"], 8192);
    assert_eq!(
        server.probes(),
        [
            "GET /api/v0/models",
            "POST /api/show",
            "GET /v1/models",
            "GET /health"
        ]
    );
    assert!(
        notices
            .0
            .contains(&"Negotiated max output tokens: 8192".to_string())
    );

    // Once per session.
    s.run_turn("again", &mut NullObserver).await.unwrap();
    assert_eq!(server.probes().len(), 4);
}

#[tokio::test]
async fn startup_probes_never_ask_a_gateways_health() {
    let server = FakeOpenAi::start_with(
        vec![Reply::text("hello")],
        ServerOptions::default()
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"model-a","object":"model","owned_by":"openai","max_model_len":4096}]}"#,
            )
            .route(
                "GET /api/v0/models",
                200,
                r#"{"data":[{"id":"model-a","max_context_length":65536}]}"#,
            ),
    )
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut c = config(&server, true);
    c.lm_studio.max_tokens = Some(2048);
    c.context.context_window_size = 262144;
    let mut s = session_with(c, work.path());
    s.startup_probes = true;
    s.server_identity = orca_core::client::ServerIdentity::new(s.probe_http.clone());
    s.run_turn("hi", &mut NullObserver).await.unwrap();
    assert_eq!(server.probes(), ["GET /api/v0/models", "GET /v1/models"]);
    // LM Studio's window is adopted; the gateway's list is the gateway's, not a window rule.
    assert_eq!(s.config.context.context_window_size, 4096);
}
