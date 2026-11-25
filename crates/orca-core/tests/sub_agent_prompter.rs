//! Sub-agent calls that need approval: foreground agents ask the Lead's prompter (C# shares one
//! `PermissionManager`), background agents (`InteractionScope.EnterNonInteractive`) are refused.

use std::sync::{Arc, Mutex};

use orca_abstractions::{Tool, ToolContext, ToolResult, ToolRiskLevel};
use orca_core::agent::permissions::{
    PermissionDecision, PermissionPrompter, SharedPrompter, non_interactive,
};
use orca_core::agent::sub_agent_tools::{ClientFactory, SessionToolExecutor, SharedPipeline};
use orca_core::agent::{AgentSession, ToolHooks, ToolRegistry};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{AgentOrchestrator, DefaultClientProvider, SubAgentToolExecutor};
use serde_json::{Value, json};

struct Danger;

#[async_trait::async_trait]
impl Tool for Danger {
    fn name(&self) -> &str {
        "danger"
    }
    fn description(&self) -> &str {
        "danger"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Dangerous
    }
    fn parameter_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            content: "ran".into(),
            ..ToolResult::default()
        }
    }
}

struct Answer(PermissionDecision, Arc<Mutex<Vec<String>>>);

impl PermissionPrompter for Answer {
    fn prompt(&mut self, tool_name: &str, risk: &str) -> PermissionDecision {
        self.1.lock().unwrap().push(format!("{tool_name} {risk}"));
        self.0
    }
}

fn config() -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.base_url = "http://127.0.0.1:9/v1".into();
    c.lm_studio.models = vec!["m".into()];
    c
}

fn setup(decision: PermissionDecision) -> (SessionToolExecutor, Arc<Mutex<Vec<String>>>) {
    let config = config();
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Danger));
    let cwd = std::env::temp_dir();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut lead = AgentSession::new(
        config.clone(),
        client.clone(),
        registry.clone(),
        ToolContext::new(&cwd),
    );
    lead.set_prompter(Box::new(Answer(decision, asked.clone())));
    let shared = SharedPipeline::new();
    let prompter: SharedPrompter = lead.shared_prompter.clone().unwrap();
    shared.lock().prompter = Some(prompter);
    let orchestrator = AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(Arc::new(client.clone()))),
        config.clone(),
        AgentTypeRegistry::new(),
        cwd.clone(),
    );
    let factory: ClientFactory = Arc::new(move || client.clone());
    let exec = SessionToolExecutor::new(
        config,
        registry,
        ToolContext::new(&cwd),
        ToolHooks::default(),
        factory,
        shared,
        &orchestrator,
    );
    (exec, asked)
}

#[tokio::test]
async fn foreground_sub_agents_ask_the_leads_prompter() {
    let (exec, asked) = setup(PermissionDecision::Approved);
    let r = exec.execute("danger", "{}").await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.content, "ran");
    assert_eq!(*asked.lock().unwrap(), ["danger Dangerous"]);

    let (exec, _) = setup(PermissionDecision::Denied);
    assert!(exec.execute("danger", "{}").await.is_error);
}

#[tokio::test]
async fn yes_to_all_from_a_sub_agent_holds_for_the_next_call() {
    let (exec, asked) = setup(PermissionDecision::ApproveAll);
    assert!(!exec.execute("danger", "{}").await.is_error);
    assert!(!exec.execute("danger", "{}").await.is_error);
    assert_eq!(asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn background_sub_agents_never_prompt() {
    let (exec, asked) = setup(PermissionDecision::Approved);
    let r = non_interactive(exec.execute("danger", "{}")).await;
    assert!(r.is_error);
    assert!(asked.lock().unwrap().is_empty());
}
