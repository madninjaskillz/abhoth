//! Ports of `CodexBridgeTests.cs`: the Codex bridge against recorded `codex app-server`
//! streams. `CodexBridgeLiveTests` is not ported: it needs the real `codex` CLI and its login.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::chat::Conversation;
use crate::client::Role;
use crate::engines::test_support::*;

struct Harness {
    engine: CodexBridgeEngine,
    fakes: Arc<Mutex<Vec<FakeCodex>>>,
    starts: Arc<Mutex<Vec<ProcessSpec>>>,
}

impl Harness {
    fn fake(&self, i: usize) -> FakeCodex {
        self.fakes.lock().unwrap()[i].clone()
    }
}

fn create(
    fixture: &str,
    options: Option<CodexBridgeOptions>,
    configure: impl Fn(&mut FakeCodexState) + Send + 'static,
) -> Harness {
    let fakes = Arc::new(Mutex::new(Vec::new()));
    let starts = Arc::new(Mutex::new(Vec::new()));
    let mut options = options.unwrap_or_else(|| CodexBridgeOptions {
        host_tools: vec![ask_user_spec()],
        ..Default::default()
    });
    options.working_directory = CWD.into();
    options.skip_version_check = true;
    options.executable_path = Some("codex-fake".into());
    options.interrupt_grace = Duration::from_secs(2);
    let factory = codex_factory(
        load_codex_turns(fixture),
        fakes.clone(),
        starts.clone(),
        configure,
    );
    Harness {
        engine: CodexBridgeEngine::new(options, Some(factory)),
        fakes,
        starts,
    }
}

fn names(list: &[EngineToolRequest]) -> Vec<String> {
    list.iter().map(|a| a.name.clone()).collect()
}

#[tokio::test]
async fn edit_and_ask_two_turns_one_process_approvals_host_tool_and_mapping() {
    let mut h = create(CODEX_EDIT_AND_ASK, None, |_| {});
    let mut conversation = Conversation::new();
    let mut sink = RecordingSink::new(true);
    let ct = CancelToken::new();

    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "patch the files", &mut sink),
            &ct,
        )
        .await
        .unwrap();
    h.engine
        .run_turn(AgentTurn::new(&mut conversation, "ask me", &mut sink), &ct)
        .await
        .unwrap();

    assert_eq!(h.starts.lock().unwrap().len(), 1);
    assert_eq!(h.starts.lock().unwrap()[0].arguments, ["app-server"]);
    let t = h.fake(0);
    assert_eq!(h.engine.session_id(), Some("thread-1"));
    assert_eq!(conversation.engine_id.as_deref(), Some("codex"));
    assert_eq!(conversation.engine_session_id.as_deref(), Some("thread-1"));

    // The thread: OpenOrca decides approvals; ask_user is a dynamic tool.
    let start = t.requests("thread/start")[0]["params"].clone();
    assert_eq!(start["approvalPolicy"], "untrusted");
    assert_eq!(start["sandbox"], "workspace-write");
    assert_eq!(start["dynamicTools"][0]["name"], "ask_user");
    assert_eq!(start["developerInstructions"], ASK_USER_INSTRUCTIONS);
    assert_eq!(
        t.requests("initialize")[0]["params"]["capabilities"]["experimentalApi"],
        true
    );

    // Approvals: the read command, then the patch (one new file + one edit -> edit_file).
    let approvals = sink.approvals();
    assert_eq!(names(&approvals), ["bash", "edit_file"]);
    assert_eq!(approvals[0].risk_level, "Dangerous");
    let cmd: Value = serde_json::from_str(&approvals[0].args_json).unwrap();
    assert_eq!(cmd["command"], "Get-Content -LiteralPath notes.txt");
    assert_eq!(approvals[1].risk_level, "Moderate");
    assert_eq!(approvals[1].original_name, "apply_patch");
    let patch: Value = serde_json::from_str(&approvals[1].args_json).unwrap();
    assert_eq!(patch["path"], "new.txt");
    assert_eq!(patch["paths"], json!(["new.txt", "notes.txt"]));
    assert_eq!(t.response_to(0)["result"]["decision"], "accept");
    assert_eq!(t.response_to(1)["result"]["decision"], "accept");

    // Tool cards and file-edit markers.
    let calls = sink.tool_calls();
    assert_eq!(
        calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(),
        ["bash", "edit_file", "ask_user"]
    );
    let results = sink.tool_results();
    assert_eq!(results[0].2, "line one");
    assert!(!results[0].3);
    assert!(
        results[1]
            .2
            .starts_with("Wrote 1 line to new.txt\nEdited notes.txt  +1 \u{2212}1\n@@ -1 +1 @@"),
        "{}",
        results[1].2
    );
    assert_eq!(
        sink.file_edits(),
        [r"C:\work\proj\new.txt", r"C:\work\proj\notes.txt"]
    );

    // ask_user: OpenOrca's tool answers; the CLI receives the answer.
    let host = sink.host_calls();
    assert_eq!(host.len(), 1);
    assert_eq!(host[0].0, "ask_user");
    let a: Value = serde_json::from_str(&host[0].1).unwrap();
    assert_eq!(a["options"], json!(["Red", "Green", "Purple"]));
    let tool_answer = t.response_to(2)["result"].clone();
    assert_eq!(tool_answer["success"], true);
    assert_eq!(tool_answer["contentItems"][0]["text"], "Purple");
    assert_eq!(results[2].2, "Purple");

    // Turn ends: success, answer text, tokens per turn.
    let done = sink.completed();
    assert_eq!(
        done.iter().map(|d| d.outcome.as_str()).collect::<Vec<_>>(),
        ["success", "success"]
    );
    assert!(done[0].final_text.as_deref().unwrap().ends_with("DONE"));
    assert!(
        done[1]
            .final_text
            .as_deref()
            .unwrap()
            .ends_with("COLOUR=Purple")
    );
    assert!(done[0].input_tokens.unwrap() > 0);
    assert!(done[1].input_tokens.unwrap() > 0);
    assert_eq!(
        conversation
            .messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        2
    );
}

#[tokio::test]
async fn denied_patch_declined_card_shows_it() {
    let mut h = create(CODEX_DECLINED_PATCH, None, |_| {});
    let mut sink = RecordingSink::new(false);
    let mut conversation = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "make b.txt", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(names(&sink.approvals()), ["write_file"]);
    assert_eq!(h.fake(0).response_to(0)["result"]["decision"], "decline");
    let results = sink.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].3);
    assert_eq!(results[0].2, "Declined in Abhoth.");
    assert!(sink.file_edits().is_empty());
    assert!(
        sink.completed()[0]
            .final_text
            .as_deref()
            .unwrap()
            .contains("was rejected")
    );
}

#[tokio::test]
async fn ask_turn_declines_changes_without_prompting_and_says_so_in_the_message() {
    let mut h = create(CODEX_DECLINED_PATCH, None, |_| {});
    let mut sink = RecordingSink::new(true);
    let mut conversation = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "what is b?", &mut sink)
                .with_mode(AgentTurnMode::Ask),
            &CancelToken::new(),
        )
        .await
        .unwrap();

    assert!(sink.approvals().is_empty());
    let t = h.fake(0);
    assert_eq!(t.response_to(0)["result"]["decision"], "decline");
    let starts = t.requests("turn/start");
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0]["params"]["input"][0]["text"],
        format!("{ASK_MODE_PREFIX}what is b?")
    );
}

#[tokio::test]
async fn plan_turn_uses_plan_collaboration_mode_next_turn_default() {
    let mut h = create(CODEX_EDIT_AND_ASK, None, |_| {});
    let mut sink = RecordingSink::new(true);
    let ct = CancelToken::new();
    let mut c1 = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut c1, "plan", &mut sink).with_mode(AgentTurnMode::Plan),
            &ct,
        )
        .await
        .unwrap();
    let mut c2 = Conversation::new();
    h.engine
        .run_turn(AgentTurn::new(&mut c2, "go", &mut sink), &ct)
        .await
        .unwrap();

    let t = h.fake(0);
    let modes: Vec<Value> = t
        .requests("turn/start")
        .iter()
        .map(|r| r["params"]["collaborationMode"].clone())
        .collect();
    assert_eq!(
        modes.iter().map(|m| m["mode"].clone()).collect::<Vec<_>>(),
        [json!("plan"), json!("default")]
    );
    assert_eq!(modes[0]["settings"]["model"], "gpt-test");
    // Plan turns may read but not patch: the patch was declined without a prompt.
    let mut seen = names(&sink.approvals());
    seen.extend(sink.host_calls().into_iter().map(|h| h.0));
    assert_eq!(seen, ["bash", "ask_user"]);
    assert_eq!(t.response_to(1)["result"]["decision"], "decline");
}

#[tokio::test]
async fn cancel_sends_turn_interrupt_process_kept_open_card_closed() {
    let mut h = create(CODEX_INTERRUPT, None, |s| {
        s.hold_from = Some("\"method\":\"turn/completed\"".into());
    });
    let mut sink = RecordingSink::new(true);
    let cts = CancelToken::new();
    let fakes = h.fakes.clone();
    let mut conversation = Conversation::new();
    let (r, _) = tokio::join!(
        h.engine
            .run_turn(AgentTurn::new(&mut conversation, "sleep", &mut sink), &cts),
        async {
            wait_until(|| {
                let list = fakes.lock().unwrap();
                list.len() == 1
                    && list[0].written().iter().any(|w| {
                        w.get("method").is_none()
                            && w.get("result").is_some_and(|r| r.get("decision").is_some())
                    })
            })
            .await;
            cts.cancel();
        }
    );
    assert_eq!(r, Err(EngineError::Cancelled));

    let t = h.fake(0);
    let interrupt = t.requests("turn/interrupt");
    assert_eq!(interrupt.len(), 1);
    assert_eq!(interrupt[0]["params"]["threadId"], "thread-1");
    // The turn id from turn/started (the recorded one; real servers report the same id in both).
    assert_eq!(
        interrupt[0]["params"]["turnId"],
        "00000000-0000-4000-8000-000000000002"
    );
    assert!(!t.killed());
    let done = sink.completed();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].outcome, "cancelled");
    let results = sink.tool_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].2, "Interrupted.");
}

#[tokio::test]
async fn cancel_cli_ignores_interrupt_killed_then_resumed() {
    let mut h = create(CODEX_INTERRUPT, None, |s| {
        s.hold_from = Some("\"method\":\"turn/completed\"".into());
        s.ignore_interrupt = true;
    });
    let mut sink = RecordingSink::new(true);
    let cts = CancelToken::new();
    let fakes = h.fakes.clone();
    let mut conversation = Conversation::new();
    let (r, _) = tokio::join!(
        h.engine
            .run_turn(AgentTurn::new(&mut conversation, "sleep", &mut sink), &cts),
        async {
            wait_until(|| {
                let list = fakes.lock().unwrap();
                list.len() == 1 && list[0].turns_started() > 0
            })
            .await;
            cts.cancel();
        }
    );
    assert_eq!(r, Err(EngineError::Cancelled));
    assert!(h.fake(0).killed());

    // The next turn starts a new app server that resumes the same thread.
    let quick = CancelToken::new();
    quick.cancel_after(Duration::from_millis(300));
    let again = h
        .engine
        .run_turn(
            AgentTurn::new(&mut conversation, "again", &mut sink),
            &quick,
        )
        .await;
    assert!(again.is_err());
    assert_eq!(h.starts.lock().unwrap().len(), 2);
    let t1 = h.fake(1);
    let resume = t1.requests("thread/resume");
    assert_eq!(resume.len(), 1);
    assert_eq!(resume[0]["params"]["threadId"], "thread-1");
    assert!(t1.requests("thread/start").is_empty());
}

#[tokio::test]
async fn request_user_input_goes_through_ask_user() {
    let line = r#"{"method":"item/tool/requestUserInput","id":7,"params":{"threadId":"thread-1","turnId":"turn-1","itemId":"i1","isBlocking":true,"autoResolutionMs":null,"questions":[{"id":"q1","header":"Colour","question":"Which colour?","isOther":false,"isSecret":false,"options":[{"label":"Red","description":""},{"label":"Blue","description":""}]}]}}"#;
    let done = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","items":[],"status":"completed","error":null,"durationMs":5}}}"#;
    let (t, handle) = FakeCodexTransport::new(vec![vec![line.into(), done.into()]], 0);
    let mut slot = Some(t);
    let factory: BridgeTransportFactory =
        Box::new(
            move |_| Ok(Box::new(slot.take().expect("one start")) as Box<dyn BridgeTransport>),
        );
    let mut engine = CodexBridgeEngine::new(
        CodexBridgeOptions {
            host_tools: vec![ask_user_spec()],
            skip_version_check: true,
            working_directory: CWD.into(),
            ..Default::default()
        },
        Some(factory),
    );
    let mut sink = RecordingSink::new(true);
    sink.host_answer = "Blue".into();
    let mut conversation = Conversation::new();
    engine
        .run_turn(
            AgentTurn::new(&mut conversation, "x", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();

    let calls = sink.host_calls();
    assert_eq!(calls.len(), 1);
    let a: Value = serde_json::from_str(&calls[0].1).unwrap();
    assert_eq!(a["question"], "Which colour?");
    assert_eq!(a["options"], json!(["Red", "Blue"]));
    let answers = handle.response_to(7)["result"]["answers"].clone();
    assert_eq!(answers["q1"]["answers"][0], "Blue");
}

#[tokio::test]
async fn not_isolated_user_config_decides() {
    let options = CodexBridgeOptions {
        isolate_settings: false,
        model: Some("gpt-x".into()),
        ..Default::default()
    };
    let mut h = create(CODEX_DECLINED_PATCH, Some(options), |_| {});
    let mut sink = RecordingSink::new(true);
    let mut conversation = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "x", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let start = h.fake(0).requests("thread/start")[0]["params"].clone();
    assert!(start.get("approvalPolicy").is_none());
    assert!(start.get("sandbox").is_none());
    assert!(start.get("dynamicTools").is_none());
    assert_eq!(start["model"], "gpt-x");
}

#[test]
fn environment_strips_keys_and_nested_markers_sets_progress_session() {
    let mut env: HashMap<String, String> = [
        ("OPENAI_API_KEY", "k1"),
        ("CODEX_API_KEY", "k2"),
        ("OPENORCA_ANTHROPIC_KEY", "k3"),
        ("CODEX_THREAD_ID", "t"),
        ("PATH", "p"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    apply_environment(&mut env, false, Some("openorca-abc"));
    let mut keys: Vec<&String> = env.keys().collect();
    keys.sort();
    assert_eq!(keys, ["PATH", "PROGRESS_SESSION"]);
    assert_eq!(env["PROGRESS_SESSION"], "openorca-abc");

    let mut kept: HashMap<String, String> =
        [("OPENAI_API_KEY".to_string(), "k1".to_string())].into();
    apply_environment(&mut kept, true, None);
    assert_eq!(kept["OPENAI_API_KEY"], "k1");
}

#[test]
fn options_from_config() {
    let o = CodexBridgeOptions::from_config(
        &BridgeCliConfig {
            model: Some("m".into()),
            extra_args: vec!["-c".into(), "x=1".into()],
            progress_bars: false,
            ..Default::default()
        },
        "/w",
        Some("thr"),
        Some("openorca-1"),
    );
    assert_eq!(o.model.as_deref(), Some("m"));
    assert_eq!(o.resume_thread_id.as_deref(), Some("thr"));
    assert!(o.progress_session.is_none());
    assert!(o.isolate_settings);
    assert_eq!(
        CodexBridgeEngine::new(o, None).build_arguments(),
        ["app-server", "-c", "x=1"]
    );
}

#[tokio::test]
async fn missing_binary_clear_message() {
    let missing = std::env::temp_dir()
        .join(format!("no-such-codex-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let err = locate(Some(&missing), &CancelToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Unavailable(m) if m.contains("bridges.codex.path")),
        "{err}"
    );
}

#[test]
fn file_change_call_and_display_command() {
    let changes =
        json!([{"path": "C:\\work\\proj\\a.txt", "kind": {"type": "add"}, "diff": "x\n"}]);
    let (name, original, args) = file_change_call(&changes, Some(CWD));
    assert_eq!(
        (name.as_str(), original.as_str()),
        ("write_file", "apply_patch")
    );
    assert_eq!(args, r#"{"path":"a.txt","content":"x\n"}"#);
    let item = json!({"command": "pwsh -Command \"ls\"", "commandActions": [{"command": "ls"}]});
    assert_eq!(display_command(&item), "ls");
}
