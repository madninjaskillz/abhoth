//! Ported from `EngineHostTests.cs` (the host-neutral parts).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;
use crate::engines::{AgentEngineEvent, EngineToolRequest, EngineTurnCompleted, HostToolResult};

#[derive(Default)]
struct FakeState {
    approved: Option<bool>,
    disposed: bool,
    block: bool,
    modes: Vec<AgentTurnMode>,
    starts: Vec<BridgeStart>,
    host_answer: Option<HostToolResult>,
}

/// A bridge that reports a tool call (asking approval), its result, a file edit and an answer.
struct FakeBridge(Arc<Mutex<FakeState>>, &'static str);

#[async_trait]
impl AgentEngine for FakeBridge {
    fn id(&self) -> &'static str {
        self.1
    }
    fn display_name(&self) -> &'static str {
        AgentEngines::display_name_of(self.1)
    }
    fn uses_slots(&self) -> bool {
        false
    }
    fn session_id(&self) -> Option<&str> {
        Some("cli-session-1")
    }
    async fn run_turn(
        &mut self,
        turn: AgentTurn<'_>,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        self.0.lock().unwrap().modes.push(turn.mode);
        turn.events.on_event(AgentEngineEvent::SessionStarted {
            session_id: Some("cli-session-1".into()),
            model: Some("m".into()),
            working_directory: None,
        });
        turn.events.on_event(AgentEngineEvent::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            original_name: "Write".into(),
            args_json: r#"{"file_path":"/w/a.txt","path":"a.txt","content":"x"}"#.into(),
        });
        let approved = turn
            .events
            .approve(EngineToolRequest {
                name: "write_file".into(),
                original_name: "Write".into(),
                args_json: r#"{"path":"a.txt"}"#.into(),
                risk_level: "Moderate".into(),
                tool_call_id: Some("t1".into()),
            })
            .await;
        self.0.lock().unwrap().approved = Some(approved);
        let answer = turn
            .events
            .run_host_tool(
                "ask_user",
                r#"{"question":"Colour?","options":["Red","Blue"]}"#,
            )
            .await;
        self.0.lock().unwrap().host_answer = Some(answer);
        turn.events.on_event(AgentEngineEvent::ToolResult {
            id: "t1".into(),
            name: "write_file".into(),
            result: "Wrote 1 line to a.txt".into(),
            is_error: false,
            args_json: None,
        });
        turn.events.on_event(AgentEngineEvent::FileEdited {
            path: "/w/a.txt".into(),
            tool_call_id: "t1".into(),
        });
        if self.0.lock().unwrap().block {
            cancel.cancelled().await;
            return Err(EngineError::Cancelled);
        }
        turn.events
            .on_event(AgentEngineEvent::TextDelta("done".into()));
        turn.events
            .on_event(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
                outcome: "success".into(),
                session_id: Some("cli-session-1".into()),
                final_text: Some("done".into()),
                output_tokens: Some(5),
                ..Default::default()
            }));
        turn.conversation.add_assistant_message("done");
        turn.conversation.engine_id = Some(self.1.into());
        turn.conversation.engine_session_id = Some("cli-session-1".into());
        Ok(())
    }
    async fn dispose(&mut self) {
        self.0.lock().unwrap().disposed = true;
    }
}

#[derive(Default)]
struct Sink {
    events: Vec<AgentEngineEvent>,
    allow: bool,
}

#[async_trait]
impl AgentEventSink for Sink {
    fn on_event(&mut self, event: AgentEngineEvent) {
        self.events.push(event);
    }
    async fn approve(&mut self, _request: EngineToolRequest) -> bool {
        self.allow
    }
    async fn run_host_tool(&mut self, _name: &str, _args_json: &str) -> HostToolResult {
        HostToolResult::ok("Blue")
    }
}

fn ready(id: &str, state: EngineReadinessState, message: &str) -> EngineReadiness {
    EngineReadiness {
        engine_id: id.to_string(),
        state,
        message: message.to_string(),
        executable: None,
        version: None,
        login_command: (state == EngineReadinessState::NotLoggedIn)
            .then(|| vec!["claude".into(), "auth".into(), "login".into()]),
    }
}

fn host_with(state: EngineReadinessState, message: &'static str) -> EngineHost {
    let mut host = EngineHost::new();
    host.readiness_check = Some(Box::new(move |id, _| {
        let r = ready(id, state, message);
        Box::pin(async move { Ok(r) })
    }));
    host
}

fn with_fake(host: &mut EngineHost, id: &'static str) -> Arc<Mutex<FakeState>> {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let s = state.clone();
    host.bridge_factory = Some(Box::new(move |start| {
        s.lock().unwrap().starts.push(start.clone());
        Box::new(FakeBridge(s.clone(), id))
    }));
    state
}

fn config() -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.bridges.claude.progress_bars = false;
    c.bridges.codex.progress_bars = false;
    c
}

async fn turn(
    host: &mut EngineHost,
    config: &OrcaConfig,
    conversation: &mut Conversation,
    sink: &mut Sink,
    mode: AgentTurnMode,
) -> Result<(), EngineError> {
    host.run_bridge_turn(
        BridgeTurnRequest {
            config,
            cwd: Path::new("/w"),
            conversation,
            session_id: Some("s1"),
            host_tools: Vec::new(),
            sink,
            mode,
        },
        &CancelToken::new(),
    )
    .await
}

#[tokio::test]
async fn bridge_turn_runs_and_keeps_the_cli_session() {
    let mut host = host_with(EngineReadinessState::Ready, "ok");
    let fake = with_fake(&mut host, "claude");
    let mut config = config();
    let mut conversation = Conversation::new();
    assert!(host.switch("claude", &mut config).await.switched);
    assert_eq!(EngineHost::footer_label(&config), Some("claude-code"));

    conversation.add_user_message("make a.txt");
    let mut sink = Sink {
        allow: true,
        ..Default::default()
    };
    turn(
        &mut host,
        &config,
        &mut conversation,
        &mut sink,
        AgentTurnMode::Normal,
    )
    .await
    .unwrap();
    assert_eq!(fake.lock().unwrap().approved, Some(true));
    assert_eq!(
        conversation.engine_session_id.as_deref(),
        Some("cli-session-1")
    );
    assert!(
        sink.events
            .iter()
            .any(|e| matches!(e, AgentEngineEvent::TextDelta(t) if t == "done"))
    );
    // The bridge is kept for the next turn (one CLI process).
    conversation.add_user_message("again");
    turn(
        &mut host,
        &config,
        &mut conversation,
        &mut sink,
        AgentTurnMode::Ask,
    )
    .await
    .unwrap();
    let state = fake.lock().unwrap();
    assert_eq!(state.starts.len(), 1);
    assert_eq!(state.starts[0].progress_session, "openorca-s1");
    assert_eq!(state.starts[0].resume, None);
    assert_eq!(state.modes, [AgentTurnMode::Normal, AgentTurnMode::Ask]);
    assert_eq!(state.host_answer, Some(HostToolResult::ok("Blue")));
}

#[tokio::test]
async fn a_conversation_from_the_same_engine_resumes_its_cli_session() {
    let mut host = host_with(EngineReadinessState::Ready, "ok");
    let fake = with_fake(&mut host, "codex");
    let mut config = config();
    config.agent.engine = "codex".into();
    let mut conversation = Conversation::new();
    conversation.engine_id = Some("codex".into());
    conversation.engine_session_id = Some("thread-9".into());
    conversation.add_user_message("hi");
    turn(
        &mut host,
        &config,
        &mut conversation,
        &mut Sink::default(),
        AgentTurnMode::Normal,
    )
    .await
    .unwrap();
    assert_eq!(
        fake.lock().unwrap().starts[0].resume.as_deref(),
        Some("thread-9")
    );
}

#[tokio::test]
async fn switch_not_logged_in_stays_native_and_raises_auth_required() {
    let mut host = host_with(
        EngineReadinessState::NotLoggedIn,
        "Claude Code is not logged in.",
    );
    let seen = Arc::new(Mutex::new(None::<EngineAuthRequired>));
    let s = seen.clone();
    host.on_auth_required(Arc::new(move |a| *s.lock().unwrap() = Some(a.clone())));
    let mut config = config();
    let result = host.switch("claude", &mut config).await;
    assert!(!result.switched);
    assert_eq!(EngineHost::engine_id(&config), "native");
    assert_eq!(config.agent.engine, "native");
    let auth = seen.lock().unwrap().clone().unwrap();
    assert_eq!(auth.login_command_line(), "claude auth login");
}

#[tokio::test]
async fn switch_missing_stays_native() {
    let mut host = host_with(EngineReadinessState::NotInstalled, "not found on PATH");
    let mut config = config();
    let result = host.switch("codex", &mut config).await;
    assert!(!result.switched);
    assert!(result.error.unwrap().contains("not found"));
    assert_eq!(EngineHost::engine_id(&config), "native");
}

#[tokio::test]
async fn switch_ends_the_old_bridge_and_unknown_names_are_rejected() {
    let mut host = host_with(EngineReadinessState::Ready, "ok");
    let fake = with_fake(&mut host, "claude");
    let mut config = config();
    let mut conversation = Conversation::new();
    host.switch("claude", &mut config).await;
    conversation.add_user_message("hi");
    turn(
        &mut host,
        &config,
        &mut conversation,
        &mut Sink::default(),
        AgentTurnMode::Normal,
    )
    .await
    .unwrap();
    let unknown = host.switch("gpt-9", &mut config).await;
    assert!(!unknown.switched);
    assert_eq!(
        unknown.error.as_deref(),
        Some("Unknown engine 'gpt-9'. Engines: native, claude, codex.")
    );
    assert!(host.switch("native", &mut config).await.switched);
    assert!(fake.lock().unwrap().disposed);
    assert_eq!(EngineHost::footer_label(&config), None);
}

#[tokio::test]
async fn verify_selected_falls_back_to_native_when_not_ready() {
    let mut host = host_with(EngineReadinessState::NotLoggedIn, "x");
    let mut config = config();
    config.agent.engine = "claude".into();
    let r = host.verify_selected(&mut config).await.unwrap();
    assert!(!r.is_ready());
    assert_eq!(config.agent.engine, "native");
    // Native selected: nothing to verify.
    assert!(host.verify_selected(&mut config).await.is_none());
}

#[tokio::test]
async fn cancel_stops_the_bridge_turn() {
    let mut host = host_with(EngineReadinessState::Ready, "ok");
    let fake = with_fake(&mut host, "claude");
    let mut config = config();
    host.switch("claude", &mut config).await;
    fake.lock().unwrap().block = true;
    let mut conversation = Conversation::new();
    conversation.add_user_message("hi");
    let cancel = CancelToken::new();
    cancel.cancel_after(std::time::Duration::from_millis(50));
    let mut sink = Sink::default();
    let result = host
        .run_bridge_turn(
            BridgeTurnRequest {
                config: &config,
                cwd: Path::new("/w"),
                conversation: &mut conversation,
                session_id: None,
                host_tools: Vec::new(),
                sink: &mut sink,
                mode: AgentTurnMode::Normal,
            },
            &cancel,
        )
        .await;
    assert_eq!(result, Err(EngineError::Cancelled));
}

#[tokio::test]
async fn progress_bars_follow_the_bridge_bucket_and_clear_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with(EngineReadinessState::Ready, "ok");
    host.progress_roots = Some(vec![dir.path().to_path_buf()]);
    let _fake = with_fake(&mut host, "claude");
    let changes = Arc::new(Mutex::new(0));
    let c = changes.clone();
    host.on_progress_changed(Arc::new(move |_| *c.lock().unwrap() += 1));
    let mut config = OrcaConfig::default();
    assert!(config.bridges.claude.progress_bars);
    host.switch("claude", &mut config).await;
    let mut conversation = Conversation::new();
    conversation.add_user_message("hi");
    turn(
        &mut host,
        &config,
        &mut conversation,
        &mut Sink::default(),
        AgentTurnMode::Normal,
    )
    .await
    .unwrap();
    assert_eq!(host.progress_session().as_deref(), Some("openorca-s1"));
    host.reset().await;
    assert!(host.progress_session().is_none());
    assert!(host.progress_bars().is_empty());
    assert!(*changes.lock().unwrap() >= 1);
}
