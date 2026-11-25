//! Ports of `ClaudeBridgeTests.cs` (`ClaudeStreamMapperTests`, `ClaudeBridgeEngineTests`).
//! `ClaudeBridgeLiveTests` is not ported: it needs the real `claude` CLI and its login.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::*;
use crate::chat::Conversation;
use crate::client::Role;
use crate::engines::test_support::*;

fn map_fixture(fixture: &str) -> Vec<AgentEngineEvent> {
    let mut mapper = ClaudeStreamMapper::new(Some(CWD));
    let mut events = Vec::new();
    for line in lines(fixture) {
        if line.contains("\"type\":\"user\"") && !line.contains("tool_result") {
            mapper.begin_turn();
        }
        events.extend(mapper.map_line(&line));
    }
    events
}

fn calls(events: &[AgentEngineEvent]) -> Vec<(String, String, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEngineEvent::ToolCall {
                id,
                name,
                original_name,
                args_json,
            } => Some((
                id.clone(),
                name.clone(),
                original_name.clone(),
                args_json.clone(),
            )),
            _ => None,
        })
        .collect()
}

fn results(events: &[AgentEngineEvent]) -> Vec<(String, String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEngineEvent::ToolResult {
                id,
                name,
                result,
                is_error,
                ..
            } => Some((id.clone(), name.clone(), result.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn completed(events: &[AgentEngineEvent]) -> Vec<EngineTurnCompleted> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEngineEvent::TurnCompleted(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

fn texts(events: &[AgentEngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEngineEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn multi_turn_edit_maps_session_tools_edits_and_results() {
    let events = map_fixture(CLAUDE_MULTI_TURN_EDIT);

    let AgentEngineEvent::SessionStarted {
        session_id, model, ..
    } = &events[0]
    else {
        panic!("first event is the session: {:?}", events[0]);
    };
    assert_eq!(
        session_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000001")
    );
    assert_eq!(model.as_deref(), Some("claude-haiku-5-5"));

    let calls = calls(&events);
    assert_eq!(
        calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(),
        ["write_file", "read_file", "edit_file", "bash"]
    );
    assert_eq!(
        calls.iter().map(|c| c.2.as_str()).collect::<Vec<_>>(),
        ["Write", "Read", "Edit", "Bash"]
    );

    // OpenOrca keys for the tool card / permission rules / edit markers.
    let write: Value = serde_json::from_str(&calls[0].3).unwrap();
    assert!(write["path"].as_str().unwrap().ends_with("hello.txt"));
    assert!(write.get("file_path").is_some());
    let bash: Value = serde_json::from_str(&calls[3].3).unwrap();
    assert_eq!(bash["_reason"], "Print contents of hello.txt");

    let results = results(&events);
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|r| !r.3));
    assert_eq!(
        calls.iter().map(|c| c.0.clone()).collect::<Vec<_>>(),
        results.iter().map(|r| r.0.clone()).collect::<Vec<_>>()
    );
    assert!(
        results[0].2.starts_with("Wrote 1 line to "),
        "{}",
        results[0].2
    );
    assert!(results[2].2.contains("Edited "));
    assert!(results[2].2.contains("+2 \u{2212}1"));
    assert!(results[2].2.contains("+there"));
    assert!(results[3].2.contains("hi\nthere"));

    let edits: Vec<&AgentEngineEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEngineEvent::FileEdited { .. }))
        .collect();
    assert_eq!(edits.len(), 2);
    for e in edits {
        let AgentEngineEvent::FileEdited { path, .. } = e else {
            unreachable!()
        };
        assert!(path.ends_with("hello.txt"));
    }

    let done = completed(&events);
    assert_eq!(done.len(), 2);
    assert!(done.iter().all(|d| d.outcome == "success"));
    assert!(done.iter().all(|d| d.cost_usd.unwrap() > 0.0));
    assert!(done.iter().all(|d| d.output_tokens.unwrap() > 0));
    assert!(done[1].final_text.as_deref().unwrap().contains("there"));

    // No partial messages in this recording: the full text blocks are the answer.
    assert!(texts(&events).iter().any(|t| t.contains("hello.txt")));
}

#[test]
fn denied_write_streamed_text_is_not_repeated_and_denial_is_an_error() {
    let mut mapper = ClaudeStreamMapper::new(Some(CWD));
    let events: Vec<AgentEngineEvent> = lines(CLAUDE_DENIED_WRITE)
        .iter()
        .flat_map(|l| mapper.map_line(l))
        .collect();

    let results = results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].3);
    assert_eq!(results[0].1, "write_file");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEngineEvent::FileEdited { .. }))
    );

    let streamed: String = texts(&events).concat();
    let done = completed(&events);
    assert_eq!(done.len(), 1);
    // Each delta once; the full assistant message is not shown again.
    assert_eq!(done[0].final_text.as_deref(), Some(streamed.as_str()));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEngineEvent::ThinkingProgress(_)))
    );
    assert_eq!(mapper.turn_text(), streamed);
}

#[test]
fn interrupt_result_is_cancelled() {
    let done = completed(&map_fixture(CLAUDE_INTERRUPT));
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].outcome, "cancelled");
    assert_eq!(done[1].outcome, "success");
    assert_eq!(done[1].final_text.as_deref(), Some("ok"));
}

#[test]
fn sub_agent_traffic_and_junk_are_skipped() {
    let mut mapper = ClaudeStreamMapper::new(Some(CWD));
    assert!(mapper.map_line("not json").is_empty());
    assert!(
        mapper
            .map_line(r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"m","content":[{"type":"text","text":"inner"}]}}"#)
            .is_empty()
    );
    assert!(
        mapper
            .map_line(r#"{"type":"system","subtype":"hook_started"}"#)
            .is_empty()
    );
}

#[test]
fn rate_limit_other_than_allowed_is_a_notice() {
    let mut mapper = ClaudeStreamMapper::new(Some(CWD));
    let e = mapper.map_line(
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#,
    );
    assert_eq!(
        e,
        vec![AgentEngineEvent::Notice {
            level: "warning".into(),
            text: "Claude Code rate limit: rejected (five_hour)".into()
        }]
    );
    assert!(
        mapper
            .map_line(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#)
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// ClaudeBridgeEngineTests

pub(super) struct Harness {
    pub engine: ClaudeBridgeEngine,
    pub fakes: Arc<Mutex<Vec<FakeClaude>>>,
    pub starts: Arc<Mutex<Vec<ProcessSpec>>>,
}

impl Harness {
    pub fn fake(&self, i: usize) -> FakeClaude {
        self.fakes.lock().unwrap()[i].clone()
    }

    pub fn fake_count(&self) -> usize {
        self.fakes.lock().unwrap().len()
    }

    pub fn start_args(&self, i: usize) -> Vec<String> {
        self.starts.lock().unwrap()[i].arguments.clone()
    }

    pub fn start_count(&self) -> usize {
        self.starts.lock().unwrap().len()
    }
}

pub(super) fn create(
    turns: Vec<Vec<String>>,
    options: Option<ClaudeBridgeOptions>,
    configure: impl Fn(&mut FakeClaudeState) + Send + 'static,
) -> Harness {
    let fakes = Arc::new(Mutex::new(Vec::new()));
    let starts = Arc::new(Mutex::new(Vec::new()));
    let mut options = options.unwrap_or_default();
    options.working_directory = CWD.into();
    options.skip_version_check = true;
    options.executable_path = Some("claude-fake".into());
    let factory = claude_factory(turns, fakes.clone(), starts.clone(), configure);
    Harness {
        engine: ClaudeBridgeEngine::new(options, Some(factory)),
        fakes,
        starts,
    }
}

fn index_of(args: &[String], s: &str) -> usize {
    args.iter()
        .position(|a| a == s)
        .unwrap_or_else(|| panic!("{s} in {args:?}"))
}

#[tokio::test]
async fn two_turns_one_process_approvals_answered_and_conversation_updated() {
    let mut h = create(load_claude_turns(CLAUDE_MULTI_TURN_EDIT), None, |_| {});
    let mut conversation = Conversation::new();
    let mut sink = RecordingSink::new(true);
    let ct = CancelToken::new();

    conversation.add_user_message("Create hello.txt");
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "Create hello.txt", &mut sink),
            &ct,
        )
        .await
        .unwrap();
    conversation.add_user_message("Append there");
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "Append there", &mut sink),
            &ct,
        )
        .await
        .unwrap();

    assert_eq!(h.fake_count(), 1);
    assert_eq!(h.start_count(), 1);
    assert_eq!(
        h.engine.session_id(),
        Some("00000000-0000-4000-8000-000000000001")
    );
    assert_eq!(conversation.engine_id.as_deref(), Some("claude"));
    assert_eq!(
        conversation.engine_session_id.as_deref(),
        h.engine.session_id()
    );
    assert_eq!(conversation.messages().len(), 4); // user, assistant, user, assistant

    // The Write and Edit asked for permission; both approved with the input unchanged.
    let approvals = sink.approvals();
    assert_eq!(
        approvals
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["write_file", "edit_file"]
    );
    assert!(approvals.iter().all(|a| a.risk_level == "Moderate"));
    let answers = h.fake(0).written_of_type("control_response", None);
    assert_eq!(answers.len(), 2);
    assert!(
        answers
            .iter()
            .all(|a| a["response"]["response"]["behavior"] == "allow")
    );

    let user = &h.fake(0).written_of_type("user", None)[0];
    assert_eq!(user["message"]["content"], "Create hello.txt");
}

#[tokio::test]
async fn denied_sends_deny_to_the_cli() {
    let mut h = create(load_claude_turns(CLAUDE_DENIED_WRITE), None, |_| {});
    let mut sink = RecordingSink::new(false);
    let mut conversation = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "make a.txt", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();

    let answers = h.fake(0).written_of_type("control_response", None);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["response"]["response"]["behavior"], "deny");
}

#[tokio::test]
async fn cancel_sends_interrupt_then_throws_cancelled_process_kept_for_next_turn() {
    let mut h = create(load_claude_turns(CLAUDE_INTERRUPT), None, |s| {
        s.hold_after = Some(52)
    });
    let mut sink = RecordingSink::new(true);
    let watcher = sink.clone();
    let cts = CancelToken::new();
    let mut conversation = Conversation::new();
    let (result, _) = tokio::join!(
        h.engine
            .run_turn(AgentTurn::new(&mut conversation, "sleep", &mut sink), &cts),
        async {
            wait_until(|| watcher.tool_calls().len() >= 2).await;
            cts.cancel();
        }
    );

    assert_eq!(result, Err(EngineError::Cancelled));
    let fake = h.fake(0);
    assert_eq!(
        fake.written_of_type("control_request", Some("interrupt"))
            .len(),
        1
    );
    assert!(!fake.killed());
    let done = sink.completed();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].outcome, "cancelled");

    // The same process carries on with the next turn.
    fake.state().hold_after = None;
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "Say ok", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(h.start_count(), 1);
    assert_eq!(
        sink.completed().last().unwrap().final_text.as_deref(),
        Some("ok")
    );
}

#[tokio::test]
async fn cancel_cli_ignores_interrupt_kills_tree_and_resumes_next_turn() {
    let options = ClaudeBridgeOptions {
        interrupt_grace: Duration::from_millis(200),
        ..Default::default()
    };
    let mut h = create(load_claude_turns(CLAUDE_INTERRUPT), Some(options), |s| {
        s.hold_after = Some(52);
        s.ignore_interrupt = true;
    });
    let mut sink = RecordingSink::new(true);
    let watcher = sink.clone();
    let cts = CancelToken::new();
    let mut conversation = Conversation::new();
    let (result, _) = tokio::join!(
        h.engine
            .run_turn(AgentTurn::new(&mut conversation, "sleep", &mut sink), &cts),
        async {
            wait_until(|| watcher.tool_calls().len() >= 2).await;
            cts.cancel();
        }
    );
    assert_eq!(result, Err(EngineError::Cancelled));
    assert!(h.fake(0).killed());

    // Next turn: a new process that resumes the CLI's session.
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "Say ok", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(h.start_count(), 2);
    let args = h.start_args(1);
    let i = index_of(&args, "--resume");
    assert_eq!(Some(args[i + 1].as_str()), h.engine.session_id());
}

#[test]
fn arguments_stream_json_stdio_permissions_model_resume() {
    let engine = ClaudeBridgeEngine::new(
        ClaudeBridgeOptions {
            model: Some("sonnet".into()),
            resume_session_id: Some("abc".into()),
            extra_args: vec!["--effort".into(), "low".into()],
            isolate_settings: false,
            ..Default::default()
        },
        None,
    );
    assert_eq!(
        engine.build_arguments(AgentTurnMode::Normal),
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--model",
            "sonnet",
            "--resume",
            "abc",
            "--effort",
            "low"
        ]
    );
    assert!(!engine.uses_slots());
    assert_eq!(engine.display_name(), "claude-code");
}

#[test]
fn arguments_isolated_by_default_no_settings_files_and_ask_rules_for_every_tool() {
    let temp = std::env::temp_dir().to_string_lossy().into_owned();
    let options = ClaudeBridgeOptions::from_config(&BridgeCliConfig::default(), &temp, None, None);
    assert!(options.isolate_settings);
    let args = ClaudeBridgeEngine::new(options, None).build_arguments(AgentTurnMode::Normal);
    assert!(args.contains(&"--setting-sources=".to_string()));
    let file = &args[index_of(&args, "--settings") + 1];
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let ask: Vec<&str> = doc["permissions"]["ask"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for tool in ["Bash", "Read", "Glob", "Grep", "Edit", "Write", "WebFetch"] {
        assert!(ask.contains(&tool), "{tool}");
    }

    let opted_out = ClaudeBridgeOptions::from_config(
        &BridgeCliConfig {
            isolate_settings: false,
            ..Default::default()
        },
        &temp,
        None,
        None,
    );
    let plain = ClaudeBridgeEngine::new(opted_out, None).build_arguments(AgentTurnMode::Normal);
    assert!(!plain.contains(&"--setting-sources=".to_string()));
    assert!(!plain.contains(&"--settings".to_string()));
}

#[test]
fn environment_strips_keys_and_nested_session_markers_sets_progress_session() {
    let mut env: HashMap<String, String> = [
        ("ANTHROPIC_API_KEY", "k1"),
        ("OPENORCA_ANTHROPIC_KEY", "k2"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", "s"),
        ("PATH", "p"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    apply_environment(&mut env, false, Some("openorca-x"));
    assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    assert!(!env.contains_key("OPENORCA_ANTHROPIC_KEY"));
    assert!(!env.contains_key("CLAUDECODE"));
    assert!(!env.contains_key("CLAUDE_CODE_SESSION_ID"));
    assert_eq!(env["PATH"], "p");
    assert_eq!(env["PROGRESS_SESSION"], "openorca-x");

    let mut passed: HashMap<String, String> =
        [("OPENORCA_ANTHROPIC_KEY".to_string(), "k2".to_string())].into();
    apply_environment(&mut passed, true, None);
    assert_eq!(passed["ANTHROPIC_API_KEY"], "k2");
    assert!(!passed.contains_key("OPENORCA_ANTHROPIC_KEY"));
    assert!(!passed.contains_key("PROGRESS_SESSION"));
}

#[tokio::test]
async fn missing_binary_clear_error() {
    let missing = std::env::temp_dir()
        .join(format!("no-such-dir-{}", std::process::id()))
        .join("claude.exe")
        .to_string_lossy()
        .into_owned();
    let err = locate(Some(&missing), &CancelToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Unavailable(m) if m.contains("not found")),
        "{err}"
    );

    let mut engine = ClaudeBridgeEngine::new(
        ClaudeBridgeOptions {
            executable_path: Some(missing),
            ..Default::default()
        },
        None,
    );
    let mut sink = RecordingSink::new(true);
    let mut conversation = Conversation::new();
    let r = engine
        .run_turn(
            AgentTurn::new(&mut conversation, "hi", &mut sink),
            &CancelToken::new(),
        )
        .await;
    assert!(matches!(r, Err(EngineError::Unavailable(_))));
}

#[tokio::test]
async fn exited_mid_turn_is_a_failure_with_the_exit_code() {
    // The CLI closes stdout after the session line: the turn fails and the process is gone.
    let turns = vec![vec![
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"m","cwd":"C:\\work\\proj"}"#.to_string(),
    ]];
    let mut h = create(turns, None, |_| {});
    let mut sink = RecordingSink::new(true);
    let mut conversation = Conversation::new();
    let watcher = h.fakes.clone();
    let ct = CancelToken::new();
    let (r, _) = tokio::join!(
        h.engine
            .run_turn(AgentTurn::new(&mut conversation, "hi", &mut sink), &ct),
        async move {
            wait_until(|| {
                watcher
                    .lock()
                    .unwrap()
                    .first()
                    .is_some_and(|f| !f.written_of_type("user", None).is_empty())
            })
            .await;
            let f = watcher.lock().unwrap()[0].clone();
            f.exit();
        }
    );
    assert_eq!(
        r,
        Err(EngineError::Failed(
            "Claude Code exited mid-turn (exit code 0).".into()
        ))
    );
    assert!(
        conversation
            .messages()
            .iter()
            .all(|m| m.role != Role::Assistant)
    );
}
