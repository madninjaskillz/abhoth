//! Port of `LiveAgentTranscriptTests` (the message and transcript parts) and `BuildAgents`: the
//! web UI's agent tabs over the real orchestrator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use orca_cli::agent_host::{ReplAgentHost, WebAgentHost};
use orca_cli::commands::AgentHost as ReplHost;
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent,
};
use orca_core::config::OrcaConfig;
use orca_core::live::LiveEventBus;
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{
    AgentChatClient, AgentManager, AgentOrchestrator, DefaultClientProvider,
};
use orca_webui::live::AgentHost;

type Respond = Box<dyn Fn(usize) -> String + Send + Sync>;

/// Replies come from `respond` (by request number, from 1); request `hold` waits for the gate.
struct ScriptedClient {
    calls: AtomicUsize,
    respond: Respond,
    hold: Option<(usize, tokio::sync::watch::Receiver<bool>)>,
}

impl ScriptedClient {
    async fn reply(&self) -> ChatResponse {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((at, gate)) = &self.hold
            && *at == n
        {
            let mut g = gate.clone();
            let _ = g.wait_for(|v| *v).await;
        }
        ChatResponse {
            text: (self.respond)(n),
            content_items: 1,
            text_parts: 1,
            ..ChatResponse::default()
        }
    }
}

#[async_trait::async_trait]
impl AgentChatClient for ScriptedClient {
    async fn complete(
        &self,
        _: &str,
        _: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(self.reply().await)
    }

    async fn stream(
        &self,
        _: &str,
        _: &[ChatMessage],
        _: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.reply().await;
        on_event(StreamEvent::Text(r.text.clone()));
        Ok(r)
    }
}

fn create(
    client: ScriptedClient,
    lead_mode: &str,
) -> (Arc<AgentOrchestrator>, Arc<WebAgentHost>, Arc<LiveEventBus>) {
    let mut config = OrcaConfig::default();
    config.lm_studio.models = vec!["m".into()];
    config.lm_studio.native_tool_calling = true;
    config.agent.lead_mode = lead_mode.into();
    let o = AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(Arc::new(client))),
        config.clone(),
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    o.set_use_streaming(true);
    let bus = Arc::new(LiveEventBus::new());
    let host = Arc::new(WebAgentHost::new(o.clone(), config, Arc::new(|| false)));
    host.attach(bus.clone(), None);
    (o, host, bus)
}

fn scripted(respond: impl Fn(usize) -> String + Send + Sync + 'static) -> ScriptedClient {
    ScriptedClient {
        calls: AtomicUsize::new(0),
        respond: Box::new(respond),
        hold: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continued_agent_runs_again_takes_queued_messages_and_refuses_bad_requests() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let mut client = scripted(|_| "working on it".into());
    client.hold = Some((2, gate_rx));
    let (o, host, _bus) = create(client, "auto");
    // The first reply ends the run: continue it from its tab, and fetch while it runs.
    let agent = o.spawn_agent("t", "general").await;
    let sent = host.send_message(&agent.id, "and then?").await;
    assert!(sent.ok);
    assert_eq!(sent.outcome, "resumed");
    let mut running = None;
    for _ in 0..200 {
        let t = host.transcript(&agent.id).await.unwrap();
        if t.entries.iter().any(|e| e.kind == "message") {
            running = Some(t);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let running = running.expect("the message shows in the transcript");
    assert!(running.can_cancel);
    assert!(running.can_message);
    assert!(running.entries.iter().any(|e| e.kind == "message"
        && e.from.as_deref() == Some("user")
        && e.text == "and then?"));

    // A message while it runs is queued for its next turn.
    let queued = host.send_message(&agent.id, "also this").await;
    assert_eq!(queued.outcome, "queued");
    gate_tx.send_replace(true);
    agent.wait().await;
    assert!(host.transcript("nope").await.is_none());
    assert_eq!(host.send_message("nope", "x").await.outcome, "notFound");
    assert_eq!(host.send_message(&agent.id, "  ").await.outcome, "empty");
    assert!(!host.cancel(&agent.id).await); // finished
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continued_agents_reply_shows_in_the_main_chat_and_goes_to_the_lead_once() {
    let (o, host, bus) = create(
        scripted(|n| {
            if n == 1 {
                "first report".into()
            } else {
                "the follow-up answer".into()
            }
        }),
        "auto",
    );
    let notices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let n2 = notices.clone();
    let _sub = bus.subscribe(move |ev| {
        if ev.kind == orca_core::live::types::NOTICE {
            n2.lock()
                .unwrap()
                .push(ev.data["text"].as_str().unwrap_or("").to_string());
        }
    });
    let agent = o.spawn_agent("t", "general").await;

    assert_eq!(
        host.send_message(&agent.id, "one more thing").await.outcome,
        "resumed"
    );
    for _ in 0..200 {
        if !agent.is_active() && !notices.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let tag = agent.tag();
    assert!(
        notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains(&format!("You messaged {tag}"))
                && n.contains("the follow-up answer"))
    );
    let note = o.drain_user_agent_notes().unwrap();
    assert!(note.contains("one more thing"));
    assert!(note.contains("the follow-up answer"));
}

#[tokio::test]
async fn hands_on_agent_cannot_be_continued_from_its_tab() {
    let (o, host, _bus) = create(scripted(|_| "done".into()), "hands-on");
    let agent = o.spawn_agent("t", "general").await;

    let t = host.transcript(&agent.id).await.unwrap();
    assert!(!t.can_message);
    let r = host.send_message(&agent.id, "more").await;
    assert!(!r.ok);
    assert_eq!(r.outcome, "notAccepting");
}

#[tokio::test]
async fn agent_bars_list_the_lead_then_the_agents() {
    let (o, host, _bus) = create(scripted(|_| "done".into()), "hands-on");
    let agent = o.spawn_agent("t", "general").await;
    let bars = host.agents();
    assert_eq!(bars.len(), 2);
    assert!(bars[0].is_lead);
    assert_eq!(bars[0].letter, "L");
    assert_eq!(bars[0].kind, "main");
    assert_eq!(bars[0].status, "idle");
    assert_eq!(bars[1].id, agent.id);
    assert_eq!(bars[1].letter, "A");
    assert_eq!(bars[1].status, "completed");
    assert_eq!(bars[1].progress, Some(1.0));
    assert_eq!(bars[1].model.as_deref(), Some("m"));
}

#[tokio::test]
async fn repl_host_runs_review_agents_through_the_orchestrator() {
    let (o, _host, _bus) = create(scripted(|_| "Looks fine.".into()), "hands-on");
    let host = ReplAgentHost::new(AgentManager::new(o));
    let outcome = host.spawn_agent("review it", "review").await.unwrap();
    assert!(outcome.completed);
    assert_eq!(outcome.status, "Completed");
    assert_eq!(outcome.result.as_deref(), Some("Looks fine."));
    assert!(host.manager().is_some());
}

#[tokio::test]
async fn wired_sub_agents_get_the_advertised_tools_and_the_session_compactor() {
    let work = tempfile::tempdir().unwrap();
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = "http://127.0.0.1:9".into();
    config.lm_studio.models = vec!["m".into()];
    let (registry, services) = orca_cli::build_registry_in(&config, work.path());
    let client = orca_core::client::client_from_config(
        &config,
        Arc::new(orca_core::client::ServedModels::new()),
    )
    .unwrap();
    let mut session = orca_core::agent::AgentSession::new(
        config,
        client,
        registry,
        orca_abstractions::ToolContext::new(work.path()),
    );
    orca_cli::wire_agents(&mut session, &services.agent_tools, None, true, None).unwrap();

    let o = session.orchestrator.clone().unwrap();
    let names: Vec<String> = o.tools().into_iter().map(|t| t.name).collect();
    let advertised: Vec<String> = session
        .registry
        .definitions()
        .into_iter()
        .map(|d| d.name)
        .filter(|n| n != "lead_mode")
        .collect();
    // ToolExposure.FilterAdvertised: the Lead's advertised list, in its order.
    assert_eq!(names, advertised);
    assert!(session.registry.names().len() > names.len());
    // The built-in types' `think` resolves to the registered tool.
    assert!(names.iter().any(|n| n == "think"));
    assert!(o.has_compactor());
}
