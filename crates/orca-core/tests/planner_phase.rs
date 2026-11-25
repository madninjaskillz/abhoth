//! Ported from `tests/OpenOrca.Cli.Tests/PlannerPhaseTests.cs`: a plan written before the
//! Lead's first turn, saved and handed over.

use std::sync::{Arc, Mutex};

use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent, Usage,
};
use orca_core::config::{OrcaConfig, PlannerConfig};
use orca_core::orchestration::context::{AgentContext, AgentSpec};
use orca_core::orchestration::planner_phase::{
    PlannerPhase, plan_text, planner_enabled, planner_task,
};
use orca_core::orchestration::read_only_command;
use orca_core::orchestration::types::{AgentStatus, AgentTypeRegistry};
use orca_core::orchestration::{
    AgentChatClient, AgentClientProvider, AgentOrchestrator, CancelToken, MAX_RESULT_CHARS,
    ProviderRoute,
};

/// Answers every request with `answer` (None: fails like a 400).
struct FixedClient {
    answer: Option<String>,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    options: Mutex<Vec<ChatOptions>>,
}

impl FixedClient {
    fn new(answer: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.map(str::to_string),
            requests: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
        })
    }

    fn respond(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.options.lock().unwrap().push(options.clone());
        let Some(answer) = &self.answer else {
            return Err(ChatError::Http {
                status: 400,
                body: "bad request".into(),
                url: String::new(),
            });
        };
        Ok(ChatResponse {
            text: answer.clone(),
            usage: Usage {
                prompt_tokens: Some(10_000),
                completion_tokens: Some(2_000),
                cached_tokens: None,
            },
            content_items: 1,
            text_parts: 1,
            ..ChatResponse::default()
        })
    }
}

#[async_trait::async_trait]
impl AgentChatClient for FixedClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.respond(messages, options)
    }

    async fn stream(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.respond(messages, options)?;
        on_event(StreamEvent::Text(r.text.clone()));
        on_event(StreamEvent::UpdateEnd {
            tool_call_delta: false,
        });
        Ok(r)
    }
}

/// The local model for default agents, "fable" as a named provider.
struct Provider {
    local: Arc<FixedClient>,
    fable: Arc<FixedClient>,
}

#[async_trait::async_trait]
impl AgentClientProvider for Provider {
    fn default_client(&self) -> Arc<dyn AgentChatClient> {
        self.local.clone()
    }

    fn provider_route(&self, name: &str) -> Option<ProviderRoute> {
        (name == "fable").then(|| ProviderRoute {
            name: "fable".into(),
            model: "claude-fable-5-2".into(),
            client: self.fable.clone(),
            context_window: None,
            max_tokens: None,
        })
    }
}

struct Fixture {
    phase: PlannerPhase,
    fable: Arc<FixedClient>,
    local: Arc<FixedClient>,
    _dir: tempfile::TempDir,
}

fn create(plan_answer: Option<&str>, enabled: bool, effort: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut config = OrcaConfig::default();
    if enabled {
        config.agent.planner = Some(PlannerConfig {
            provider: Some("fable".into()),
            max_steps: 5,
            plan_file: "PLAN.md".into(),
            effort: Some(effort.into()),
            ..PlannerConfig::default()
        });
    }
    let fable = FixedClient::new(plan_answer);
    let local = FixedClient::new(Some("local"));
    let orchestrator = AgentOrchestrator::new(
        Arc::new(Provider {
            local: local.clone(),
            fable: fable.clone(),
        }),
        config.clone(),
        AgentTypeRegistry::new(),
        dir.path(),
    );
    Fixture {
        phase: PlannerPhase::new(config, orchestrator, dir.path().to_path_buf()),
        fable,
        local,
        _dir: dir,
    }
}

fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

#[tokio::test]
async fn plan_is_saved_and_handed_to_the_lead_once_only() {
    let mut f = create(Some("## 1. Architecture\nsrc/App.cs ..."), true, "high");
    let ct = CancelToken::new();

    let first = f
        .phase
        .prepare_first_message("Build the launcher in SPEC.md.", &ct)
        .await;

    assert!(first.starts_with("Build the launcher in SPEC.md."));
    assert!(lf(&first).contains("<plan>\n## 1. Architecture\nsrc/App.cs ...\n</plan>"));
    assert!(first.contains("claude-fable-5-2"));
    assert!(first.contains("also saved as PLAN.md"));
    let saved = std::fs::read_to_string(f.phase.working_directory.join("PLAN.md")).unwrap();
    assert_eq!(lf(&saved), "## 1. Architecture\nsrc/App.cs ...\n");
    // The planner got the task and the required sections; the local model was not used.
    let requests = f.fable.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let brief = requests[0].last().unwrap().content.clone().unwrap();
    assert!(brief.contains("Build the launcher in SPEC.md."));
    assert!(brief.contains("Interfaces and contracts"));
    assert!(brief.contains("real data formats to sample from this machine"));
    assert!(brief.contains("verification checklist"));
    assert!(f.local.requests.lock().unwrap().is_empty());
    // (The C# test also checks the cost booked in the provider usage ledger; the ledger is not
    // ported yet.)

    assert_eq!(f.phase.prepare_first_message("next", &ct).await, "next");
    assert_eq!(f.fable.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn long_plan_is_saved_and_handed_over_in_full() {
    // A real plan ran to ~68k chars; the 16k cut on agent results must not apply to it.
    let mut plan = String::from("## Plan\n");
    for i in 0..3000 {
        plan.push_str(&format!("- step {i:04} of the plan\n"));
    }
    plan.push_str("## 5. Verification checklist\n- build");
    assert!(plan.len() > MAX_RESULT_CHARS * 3);
    let mut f = create(Some(&plan), true, "high");

    let first = f
        .phase
        .prepare_first_message("Build it.", &CancelToken::new())
        .await;

    let saved = lf(&std::fs::read_to_string(f.phase.working_directory.join("PLAN.md")).unwrap());
    assert_eq!(saved, format!("{plan}\n"));
    assert!(!saved.contains("truncated"));
    assert!(lf(&first).contains("## 5. Verification checklist\n- build\n</plan>"));
}

#[tokio::test]
async fn configured_effort_is_sent_with_every_planner_request() {
    let mut f = create(Some("## Plan"), true, "medium");

    f.phase
        .prepare_first_message("Build it.", &CancelToken::new())
        .await;

    let options = f.fable.options.lock().unwrap().clone();
    assert!(!options.is_empty());
    for o in options {
        assert_eq!(
            o.additional.get("reasoning_effort"),
            Some(&serde_json::json!("medium"))
        );
    }
}

fn run(status: AgentStatus, result: Option<&str>) -> AgentContext {
    let c = AgentContext::new(AgentSpec {
        task: "plan".into(),
        agent_type: "planner".into(),
        ..AgentSpec::default()
    });
    {
        let mut s = c.state();
        s.status = status;
        s.result = result.map(str::to_string);
    }
    c
}

#[test]
fn plan_text_takes_an_answer_written_at_the_step_limit() {
    assert_eq!(
        plan_text(&run(AgentStatus::Completed, Some(" ## Plan "))).as_deref(),
        Some("## Plan")
    );
    assert_eq!(
        plan_text(&run(
            AgentStatus::CompletedPartial,
            Some("[Partial result \u{2014} iteration limit reached]\n## Plan\nAll of it.")
        ))
        .as_deref(),
        Some("## Plan\nAll of it.")
    );
    assert_eq!(
        plan_text(&run(
            AgentStatus::CompletedPartial,
            Some("Agent reached maximum iterations without completing.")
        )),
        None
    );
    assert_eq!(plan_text(&run(AgentStatus::Failed, Some("boom"))), None);
    assert_eq!(plan_text(&run(AgentStatus::Completed, Some("  "))), None);
}

#[test]
fn planner_task_states_the_step_budget() {
    let task = planner_task("Build it.", "PLAN.md", 15);
    assert!(task.contains("You have 15 steps"));
    // The planner is told which commands it may run.
    assert!(lf(&task).contains(lf(&read_only_command::allowed_summary()).trim()));
}

#[tokio::test]
async fn failed_planner_leaves_the_prompt_unchanged() {
    let mut f = create(None, true, "high");
    assert_eq!(
        f.phase
            .prepare_first_message("Build it.", &CancelToken::new())
            .await,
        "Build it."
    );
    assert!(!f.phase.working_directory.join("PLAN.md").exists());
}

#[tokio::test]
async fn no_planner_configured_passes_through() {
    let mut f = create(Some("plan"), false, "high");
    assert_eq!(
        f.phase
            .prepare_first_message("Build it.", &CancelToken::new())
            .await,
        "Build it."
    );
    assert!(f.fable.requests.lock().unwrap().is_empty());
}

#[test]
fn planner_config_is_off_without_a_provider() {
    let mut config = OrcaConfig::default();
    config.agent.planner = Some(PlannerConfig::default());
    assert!(!planner_enabled(&config));
    config.agent.planner = Some(PlannerConfig {
        provider: Some("fable".into()),
        ..PlannerConfig::default()
    });
    assert!(planner_enabled(&config));
    assert_eq!(PlannerConfig::default().effort.as_deref(), Some("high"));
}
