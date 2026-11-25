//! Ports of `SpawnAgentToolTests` and `ReportProgressToolTests`.
//!
//! The C# tools take delegates (`AgentSpawner`, `ParallelSpawner`, `Configure(reporter)`); the
//! Rust tools are wired to the session's `AgentManager` through an `AgentToolsHandle`, so these
//! tests run them against a real orchestrator with a recording chat client and check what the
//! orchestrator received.

use std::sync::{Arc, Mutex};

use orca_abstractions::{Tool, ToolContext, ToolErrorCode, ToolResult, ToolRiskLevel};
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, Role, StreamControl, StreamEvent,
};
use orca_core::config::OrcaConfig;
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{
    AgentChatClient, AgentContext, AgentManager, AgentOrchestrator, DefaultClientProvider,
};
use orca_tools_agent::{AgentToolsHandle, all_tools};
use serde_json::Value;

/// Answers "result" and records the first user message of every request.
#[derive(Default)]
struct RecordingClient {
    tasks: Mutex<Vec<String>>,
}

impl RecordingClient {
    fn respond(&self, messages: &[ChatMessage]) -> ChatResponse {
        let task = messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        self.tasks.lock().unwrap().push(task);
        ChatResponse {
            text: "result".into(),
            content_items: 1,
            text_parts: 1,
            ..ChatResponse::default()
        }
    }
}

#[async_trait::async_trait]
impl AgentChatClient for RecordingClient {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(self.respond(messages))
    }

    async fn stream(
        &self,
        _: &str,
        messages: &[ChatMessage],
        _: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.respond(messages);
        on_event(StreamEvent::Text(r.text.clone()));
        on_event(StreamEvent::UpdateEnd {
            tool_call_delta: false,
        });
        Ok(r)
    }
}

struct Wired {
    handle: AgentToolsHandle,
    orchestrator: Arc<AgentOrchestrator>,
    client: Arc<RecordingClient>,
}

fn wired() -> Wired {
    let client = Arc::new(RecordingClient::default());
    let orchestrator = AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(client.clone())),
        OrcaConfig::default(),
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    let handle = AgentToolsHandle::new();
    handle.set(Some(AgentManager::new(orchestrator.clone())));
    Wired {
        handle,
        orchestrator,
        client,
    }
}

/// The tool named `name` from `all_tools`.
fn tool_of(handle: &AgentToolsHandle, name: &str) -> Arc<dyn Tool> {
    all_tools(handle)
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap()
}

fn unwired(name: &str) -> Arc<dyn Tool> {
    tool_of(&AgentToolsHandle::new(), name)
}

async fn run(tool: &Arc<dyn Tool>, args: &str) -> ToolResult {
    let args: Value = serde_json::from_str(args).unwrap();
    tool.execute(&args, &ToolContext::new(std::env::temp_dir()))
        .await
}

fn agents(o: &AgentOrchestrator) -> Vec<Arc<AgentContext>> {
    o.get_all_agents()
}

fn required(schema: &Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

// ── SpawnAgentToolTests ──────────────────────────────────────────────────────

#[test]
fn name_is_spawn_agent() {
    assert_eq!(unwired("spawn_agent").name(), "spawn_agent");
}

#[test]
fn description_lists_agent_types() {
    let tool = unwired("spawn_agent");
    for t in ["explore", "plan", "bash", "review", "general"] {
        assert!(tool.description().contains(t), "{t}");
    }
}

#[tokio::test]
async fn execute_passes_task_and_agent_type() {
    let w = wired();
    let tool = tool_of(&w.handle, "spawn_agent");

    let result = run(&tool, r#"{"task": "find files", "agent_type": "explore"}"#).await;

    assert!(!result.is_error, "{}", result.content);
    let agents = agents(&w.orchestrator);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].task, "find files");
    assert_eq!(agents[0].agent_type, "explore");
    assert_eq!(*w.client.tasks.lock().unwrap(), ["find files"]);
}

#[tokio::test]
async fn execute_defaults_to_general_when_no_agent_type() {
    let w = wired();
    run(
        &tool_of(&w.handle, "spawn_agent"),
        r#"{"task": "do something"}"#,
    )
    .await;
    assert_eq!(agents(&w.orchestrator)[0].agent_type, "general");
}

#[tokio::test]
async fn execute_returns_error_when_spawner_not_configured() {
    let result = run(&unwired("spawn_agent"), r#"{"task": "test"}"#).await;
    assert!(result.is_error);
    assert!(result.content.contains("not configured"));
}

// ExecuteAsync_ReturnsError_OnSpawnerException: not ported. It throws from the C# spawner
// delegate; the Rust tool calls the AgentManager, which reports agent failures as the agent's
// result and has no failure path that reaches the tool.

#[test]
fn parameter_schema_agent_type_has_description() {
    let schema = unwired("spawn_agent").parameter_schema();
    let description = schema["properties"]["agent_type"]["description"]
        .as_str()
        .unwrap();
    for t in [
        "explore",
        "plan",
        "bash",
        "review",
        "general",
        "Custom types",
    ] {
        assert!(description.contains(t), "{t}");
    }
}

#[tokio::test]
async fn execute_passes_the_label_and_derives_one_when_omitted() {
    let w = wired();
    let tool = tool_of(&w.handle, "spawn_agent");

    run(&tool, r#"{"task": "scan auth", "label": "Scan auth code"}"#).await;
    run(&tool, r#"{"task": "scan auth"}"#).await;

    let labels: Vec<String> = agents(&w.orchestrator).iter().map(|a| a.label()).collect();
    assert_eq!(labels[0], "Scan auth code");
    // C# passes null on and the orchestrator derives the label from the task.
    assert_ne!(labels[1], "Scan auth code");
    assert!(!labels[1].is_empty());
    assert!(tool.parameter_schema()["properties"].get("label").is_some());
}

#[tokio::test]
async fn spawn_agents_passes_each_label() {
    let w = wired();
    let tool = tool_of(&w.handle, "spawn_agents");

    let result = run(
        &tool,
        r#"{"agents":[{"task":"a","agent_type":"explore","label":"Read alpha"},{"task":"b"}]}"#,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let agents = agents(&w.orchestrator);
    let specs: Vec<(String, String)> = agents
        .iter()
        .map(|a| (a.task.clone(), a.agent_type.clone()))
        .collect();
    assert_eq!(
        specs,
        [
            ("a".to_string(), "explore".to_string()),
            ("b".to_string(), "general".to_string())
        ]
    );
    assert_eq!(agents[0].label(), "Read alpha");
    assert_ne!(agents[1].label(), "Read alpha"); // derived
}

#[test]
fn parameter_schema_task_is_required() {
    let required = required(&unwired("spawn_agent").parameter_schema());
    assert!(required.contains(&"task".to_string()));
    assert!(!required.contains(&"agent_type".to_string()));
}

// ── ReportProgressToolTests ──────────────────────────────────────────────────
// The C# reporter delegate is the orchestrator's ReportProgress here: called outside a
// sub-agent, it moves the Lead's bar.

#[test]
fn metadata_is_read_only_and_named_report_progress() {
    let tool = unwired("report_progress");
    assert_eq!(tool.name(), "report_progress");
    assert_eq!(tool.risk_level(), ToolRiskLevel::ReadOnly);
    assert_eq!(required(&tool.parameter_schema()), ["percent"]);
}

#[tokio::test]
async fn execute_passes_percent_and_label() {
    let w = wired();
    let result = run(
        &tool_of(&w.handle, "report_progress"),
        r#"{"percent": 40, "label": "reading tests"}"#,
    )
    .await;

    assert!(!result.is_error);
    assert_eq!(result.content, "Progress recorded: 40%.");
    assert_eq!(w.orchestrator.lead_progress(), Some(0.4));
    assert_eq!(w.orchestrator.lead_note().as_deref(), Some("reading tests"));
}

#[tokio::test]
async fn execute_normalises_the_percent() {
    for (args, expected) in [
        (r#"{"percent": 150}"#, 100),
        (r#"{"percent": "35%"}"#, 35),
        (r#"{"percent": 0.5}"#, 50),
        (r#"{"percent": 72.4}"#, 72),
        (r#"{"percent": 0}"#, 0),
    ] {
        let w = wired();
        let result = run(&tool_of(&w.handle, "report_progress"), args).await;
        assert!(!result.is_error, "{args}");
        assert_eq!(
            result.content,
            format!("Progress recorded: {expected}%."),
            "{args}"
        );
        assert_eq!(
            w.orchestrator.lead_progress(),
            Some(f64::from(expected) / 100.0),
            "{args}"
        );
        assert!(w.orchestrator.lead_note().is_none());
    }
}

#[tokio::test]
async fn execute_rejects_bad_percent() {
    for args in [r#"{}"#, r#"{"percent": "lots"}"#, r#"{"percent": -5}"#] {
        let w = wired();
        let result = run(&tool_of(&w.handle, "report_progress"), args).await;
        assert!(result.is_error, "{args}");
        assert_eq!(result.error_code, ToolErrorCode::InvalidArguments, "{args}");
        assert!(w.orchestrator.lead_progress().is_none(), "{args}");
    }
}

#[tokio::test]
async fn execute_without_reporter_is_an_error() {
    let result = run(&unwired("report_progress"), r#"{"percent": 10}"#).await;
    assert!(result.is_error);
    assert!(result.content.contains("not configured"));
}
