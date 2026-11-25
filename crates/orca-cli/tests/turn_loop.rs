//! The turn loop against the fake server: text tool calls, loop guards, retries, failover and
//! the iteration-limit handoff (`AgentLoopRunner` / `ToolCallDispatcher` behaviour).

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, NullObserver, TurnEnd};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

fn session(server: &FakeOpenAi, work: &std::path::Path, native: bool) -> AgentSession {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["model-a".into(), "model-b".into()];
    config.lm_studio.native_tool_calling = native;
    config.agent.verification_mode = "off".into();
    session_with(config, work)
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

fn empty() -> Reply {
    Reply::default()
}

fn last_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn text_mode_runs_tagged_calls_and_returns_results_as_a_user_message() {
    let server = FakeOpenAi::start(vec![
        Reply::text(
            "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"f.txt\"}}\n</tool_call>",
        ),
        Reply::text("The file f.txt holds one line that says alpha, nothing more."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("f.txt"), "alpha\n").unwrap();
    let mut s = session(&server, work.path(), false);
    let end = s.run_turn("read f.txt", &mut NullObserver).await.unwrap();
    assert_eq!(end, TurnEnd::Done);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[0].body.get("tools").is_none(),
        "text mode sends no tools"
    );
    let follow_up = last_user_text(&reqs[1].body);
    assert!(follow_up.starts_with(
        "Here are the results of the tool calls you made:\n\n[Tool result for read_file]\n"
    ));
    assert!(follow_up.contains("alpha"));
    assert!(follow_up.ends_with("If you need to use more tools, use <tool_call> tags."));
    assert_eq!(s.tool_call_history.len(), 1);
}

#[tokio::test]
async fn doom_loop_skips_the_third_identical_read_and_redirects() {
    let read = || Reply::tool("read_file", json!({"path": "f.txt"}));
    let server = FakeOpenAi::start(vec![
        read(),
        read(),
        read(),
        Reply::text("I read f.txt several times; it holds a single x and nothing else."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("f.txt"), "x\n").unwrap();
    let mut s = session(&server, work.path(), true);
    s.config.agent.repeat_call_notes = false;
    let end = s.run_turn("check", &mut NullObserver).await.unwrap();
    assert_eq!(end, TurnEnd::Done);
    assert_eq!(s.tool_call_history.len(), 2, "the third call was not run");

    let reqs = server.requests();
    let msgs = reqs[3].body["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "tool");
    let content = last["content"].as_str().unwrap();
    assert!(content.starts_with("Not run: this exact call (same tool, same arguments)"));
    assert!(content.ends_with("explain what you're stuck on."));
}

#[tokio::test]
async fn a_second_doom_loop_ends_the_turn() {
    let read = || Reply::tool("read_file", json!({"path": "f.txt"}));
    let server =
        FakeOpenAi::start(vec![read(), read(), read(), read(), Reply::text("never")]).await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("f.txt"), "x\n").unwrap();
    let mut s = session(&server, work.path(), true);
    s.run_turn("check", &mut NullObserver).await.unwrap();
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn second_identical_failure_gets_a_warning() {
    // `_reason` differs, so the doom-loop guard sees different calls while retry detection
    // (which strips `_reason`) sees the same one, as in C#.
    let server = FakeOpenAi::start(vec![
        Reply::tool("read_file", json!({"path": "missing.txt", "_reason": "a"})),
        Reply::tool("read_file", json!({"path": "missing.txt", "_reason": "b"})),
        Reply::tool("read_file", json!({"path": "missing.txt", "_reason": "c"})),
        Reply::text("The file missing.txt does not exist, so there is nothing to read here."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    s.run_turn("read it", &mut NullObserver).await.unwrap();
    let reqs = server.requests();
    let tool_text = |i: usize| {
        reqs[i].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(!tool_text(1).contains("WARNING"));
    assert!(tool_text(2).ends_with(
        "WARNING: This tool call has failed 2 times with the same error. Do NOT retry with the same arguments. Try a different approach or tool."
    ));
    assert!(tool_text(3).starts_with("STOP: This tool call has failed 3 times"));
    // At three identical failures the loop also adds the retry-loop redirect.
    assert!(
        last_user_text(&reqs[3].body).starts_with("SYSTEM NOTICE: A tool has now failed 3 times")
    );
}

#[tokio::test]
async fn empty_replies_retry_the_same_model_then_fail_over() {
    let server = FakeOpenAi::start(vec![
        empty(),
        empty(),
        empty(),
        empty(),
        Reply::text("Hello from the backup model, answering your greeting now."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    let end = s.run_turn("hi", &mut NullObserver).await.unwrap();
    assert_eq!(end, TurnEnd::Done);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 5);
    assert!(reqs[0].body.get("tools").is_some());
    assert!(
        reqs[1].body.get("tools").is_none(),
        "an empty native reply is retried without tools"
    );
    assert_eq!(reqs[3].body["model"], "model-a");
    assert_eq!(reqs[4].body["model"], "model-b");
    assert_eq!(
        s.last_assistant_response.as_deref(),
        Some("Hello from the backup model, answering your greeting now.")
    );
}

#[tokio::test]
async fn iteration_limit_asks_for_a_handoff() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("list_directory", json!({"path": "."})),
        Reply::tool("glob", json!({"pattern": "*.txt"})),
        Reply::text(
            "## Done\nnothing\n## In progress\nlooking\n## Failing tests\nnone\n## Next step\nread",
        ),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.models = vec!["m".into()];
    config.lm_studio.native_tool_calling = true;
    config.agent.main_loop_max_iterations = 2;
    config.agent.max_iterations_hard_cap = 2;
    let mut s = session_with(config, work.path());
    let end = s.run_turn("explore", &mut NullObserver).await.unwrap();
    assert_eq!(end, TurnEnd::IterationLimit);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[2].body["stream"], Value::Null);
    let ask = last_user_text(&reqs[2].body);
    assert!(ask.starts_with(
        "You have reached the iteration limit for this turn (2 iterations). Do NOT call any more tools."
    ));
    assert!(
        s.last_assistant_response
            .as_deref()
            .unwrap()
            .starts_with("## Done")
    );
}

#[tokio::test]
async fn truncated_text_call_is_nudged_to_continue() {
    let server = FakeOpenAi::start(vec![
        Reply::text("<tool_call>\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.txt\", \"content\": \"ab"),
        Reply::text("I could not finish that call, and I will stop here for now, sorry."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), false);
    s.run_turn("write a.txt", &mut NullObserver).await.unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert!(last_user_text(&reqs[1].body).starts_with(
        "Your previous response was cut off mid-tool-call (the <tool_call> tag was never closed)."
    ));
    assert!(!work.path().join("a.txt").exists());
}

#[tokio::test]
async fn a_failing_test_run_blocks_the_wrap_up_twice() {
    let done =
        || Reply::text("All done: the implementation is complete and every test passes now.");
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "bash",
            json!({"command": "cargo test --manifest-path no-such-dir-xyz/Cargo.toml || echo \"test result: FAILED. 0 passed; 1 failed\""}),
        ),
        done(),
        done(),
        done(),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    s.config.permissions.auto_approve_all = true;
    s.run_turn("run the tests", &mut NullObserver)
        .await
        .unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), 4, "two refusals, then the turn ends");
    for r in &reqs[2..4] {
        assert!(last_user_text(&r.body).starts_with(
            "STOP \u{2014} the task is NOT complete: the most recent test/build run FAILED."
        ));
    }
}
