//! Ported from `WorkflowTests.Runner_TwoStages_ChainsOutputsIntoLaterTemplates`
//! (`tests/OpenOrca.Core.Tests/Orchestration/WorkflowTests.cs`).

use std::sync::{Arc, Mutex};

use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, Role, StreamControl, StreamEvent,
};
use orca_core::config::OrcaConfig;
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::workflow::WorkflowDefinition;
use orca_core::orchestration::workflow_runner::run_workflow;
use orca_core::orchestration::{
    AgentChatClient, AgentOrchestrator, CancelToken, DefaultClientProvider,
};

/// Every agent completes at once with a text answer echoing its last user message.
struct EchoClient;

fn echo(messages: &[ChatMessage]) -> ChatResponse {
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    ChatResponse {
        text: format!("ECHO: {last_user}"),
        content_items: 1,
        text_parts: 1,
        ..ChatResponse::default()
    }
}

#[async_trait::async_trait]
impl AgentChatClient for EchoClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        _options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(echo(messages))
    }

    async fn stream(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        _options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = echo(messages);
        on_event(StreamEvent::Text(r.text.clone()));
        Ok(r)
    }
}

#[tokio::test]
async fn runner_two_stages_chains_outputs_into_later_templates() {
    let mut config = OrcaConfig::default();
    config.lm_studio.models = vec!["m".into()];
    config.lm_studio.native_tool_calling = true; // the client answers text, no tools
    let dir = tempfile::tempdir().unwrap();
    let orchestrator = AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(Arc::new(EchoClient))),
        config,
        AgentTypeRegistry::new(),
        dir.path(),
    );
    let workflow = WorkflowDefinition::parse(
        r#"{
          "name": "chain",
          "stages": [
            { "name": "a", "agents": [ { "task": "MARKER-ONE {{input}}" } ] },
            { "name": "b", "agents": [ { "task": "combine: {{stage.1.output}}" } ] }
          ]
        }"#,
    )
    .unwrap();

    let statuses = Mutex::new(Vec::<String>::new());
    let on_status = |s: &str| statuses.lock().unwrap().push(s.to_string());
    let result = run_workflow(
        &orchestrator,
        &workflow,
        "seed-input",
        Some(&on_status),
        &CancelToken::new(),
    )
    .await
    .unwrap();

    // Stage 2's agent saw stage 1's output (which echoed MARKER-ONE and the input).
    assert!(result.contains("MARKER-ONE"), "{result}");
    assert!(result.contains("seed-input"), "{result}");
    assert_eq!(statuses.lock().unwrap().len(), 2);
}
