//! Ported from `EngineHostTests.cs` (the REPL renderer's parts): a bridge turn fills the
//! session state the REPL reads, approvals go through the permission rules and the prompter,
//! and host tools run Abhoth's own tool.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orca_abstractions::{Tool, ToolContext, ToolResult, ToolRiskLevel};
use orca_core::agent::AgentSession;
use orca_core::agent::permissions::{PermissionDecision, PermissionPrompter};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::engines::{AgentEventSink, EngineToolRequest, EngineTurnCompleted, HostToolResult};
use serde_json::{Value, json};

use super::*;

fn session(mut config: OrcaConfig) -> AgentSession {
    config.lm_studio.base_url = "http://127.0.0.1:9/v1".into();
    config.lm_studio.models = vec!["m".into()];
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    AgentSession::new(
        config,
        client,
        Default::default(),
        ToolContext::new(std::env::temp_dir()),
    )
}

fn request(risk: &str) -> EngineToolRequest {
    EngineToolRequest {
        name: "write_file".into(),
        original_name: "Write".into(),
        args_json: r#"{"path":"a.txt"}"#.into(),
        risk_level: risk.into(),
        tool_call_id: Some("t1".into()),
    }
}

#[tokio::test]
async fn bridge_turn_renders_into_the_session_state() {
    let mut config = OrcaConfig::default();
    config.permissions.auto_approve_all = true;
    let mut s = session(config);
    let (console, out) = Console::buffer();
    let mut view = ReplBridgeView::new(console, ReplState::default(), &[]);
    {
        let mut sink = SessionBridgeSink::new(&mut s, &mut view);
        sink.on_event(AgentEngineEvent::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            original_name: "Write".into(),
            args_json: r#"{"file_path":"/w/a.txt","path":"a.txt","content":"x"}"#.into(),
        });
        assert!(sink.approve(request("Moderate")).await);
        sink.on_event(AgentEngineEvent::ToolResult {
            id: "t1".into(),
            name: "write_file".into(),
            result: "Wrote 1 line to a.txt".into(),
            is_error: false,
            args_json: None,
        });
        sink.on_event(AgentEngineEvent::FileEdited {
            path: "/w/a.txt".into(),
            tool_call_id: "t1".into(),
        });
        sink.on_event(AgentEngineEvent::FileEdited {
            path: "/W/A.txt".into(),
            tool_call_id: "t1".into(),
        });
        sink.on_event(AgentEngineEvent::TextDelta("done".into()));
        sink.on_event(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
            outcome: "success".into(),
            final_text: Some("done".into()),
            output_tokens: Some(5),
            ..Default::default()
        }));
    }
    view.end_turn();
    assert_eq!(s.last_assistant_response.as_deref(), Some("done"));
    assert_eq!(s.files_modified, ["/w/a.txt"]);
    assert_eq!(s.tool_call_history.len(), 1);
    assert_eq!(s.tool_call_history[0].name, "write_file");
    assert!(
        s.tool_call_history[0]
            .arguments
            .as_deref()
            .unwrap()
            .contains("file_path")
    );
    assert_eq!(s.total_output_tokens, 5);
    let text = crate::ui::console::strip_ansi(&Console::buffer_text(&out));
    assert!(text.contains("write_file"), "{text}");
    assert!(text.contains("done"), "{text}");
}

struct Prompter(Arc<Mutex<Vec<String>>>, PermissionDecision);

impl PermissionPrompter for Prompter {
    fn prompt(&mut self, tool_name: &str, risk: &str) -> PermissionDecision {
        self.0.lock().unwrap().push(format!("{tool_name}:{risk}"));
        self.1
    }
}

#[tokio::test]
async fn approvals_ask_the_prompter_and_remember_yes_to_all() {
    let mut s = session(OrcaConfig::default());
    let asked = Arc::new(Mutex::new(Vec::new()));
    s.prompter = Some(Box::new(Prompter(
        asked.clone(),
        PermissionDecision::ApproveAll,
    )));
    let (console, _) = Console::buffer();
    let mut view = ReplBridgeView::new(console, ReplState::default(), &[]);
    let mut sink = SessionBridgeSink::new(&mut s, &mut view);
    assert!(sink.approve(request("Dangerous")).await);
    assert!(sink.approve(request("Dangerous")).await);
    assert_eq!(*asked.lock().unwrap(), ["write_file:Dangerous"]);
}

#[tokio::test]
async fn denied_without_a_prompter_says_so() {
    let mut s = session(OrcaConfig::default());
    let (console, out) = Console::buffer();
    let mut view = ReplBridgeView::new(console, ReplState::default(), &[]);
    let mut sink = SessionBridgeSink::new(&mut s, &mut view);
    assert!(!sink.approve(request("Dangerous")).await);
    let text = crate::ui::console::strip_ansi(&Console::buffer_text(&out));
    assert!(text.contains("\u{2298} Write denied"), "{text}");
}

struct FakeAskUser(Arc<Mutex<Option<String>>>);

#[async_trait]
impl Tool for FakeAskUser {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the user"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        *self.0.lock().unwrap() = args["question"].as_str().map(str::to_string);
        ToolResult::success("Blue")
    }
}

#[tokio::test]
async fn host_tools_run_ask_user_from_the_registry() {
    let mut s = session(OrcaConfig::default());
    assert!(orca_core::engines::print::host_tools(Some(&s.registry)).is_empty());
    let asked = Arc::new(Mutex::new(None));
    s.registry.register(Arc::new(FakeAskUser(asked.clone())));
    let specs = orca_core::engines::print::host_tools(Some(&s.registry));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "ask_user");
    let (console, _) = Console::buffer();
    let mut view = ReplBridgeView::new(console, ReplState::default(), &[]);
    let mut sink = SessionBridgeSink::new(&mut s, &mut view);
    let answer = sink
        .run_host_tool(
            "ask_user",
            r#"{"question":"Colour?","options":["Red","Blue"]}"#,
        )
        .await;
    assert_eq!(answer, HostToolResult::ok("Blue"));
    assert_eq!(asked.lock().unwrap().as_deref(), Some("Colour?"));
    let missing = sink.run_host_tool("nope", "{}").await;
    assert!(missing.is_error);
}

#[test]
fn external_bars_show_letter_label_and_percent() {
    let bars = vec![ProgressFileBar {
        key: "build".into(),
        label: "Build the app".into(),
        pct: 37.0,
        colour: "purple".into(),
        started: 1,
        updated: 2,
        source: Some("env".into()),
    }];
    let plain = crate::ui::console::strip_ansi(&format_external_bars(&bars));
    assert_eq!(plain, " D  Build the app 37%");
}
