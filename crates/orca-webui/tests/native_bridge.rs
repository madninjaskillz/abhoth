//! Bridge engines in the web session (`LiveSessionAdapter.SetEngineAsync`, bridge turns on the
//! bus, the engine's Lead bar). The bridge is a fake; no real CLI runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, ToolRegistry};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::engines::{
    AgentEngine, AgentEngineEvent, AgentEngines, AgentTurn, BridgeStart, CancelToken, EngineError,
    EngineHost, EngineReadiness, EngineReadinessState, EngineTurnCompleted,
};
use orca_core::live::{LiveEventBus, types};
use orca_webui::LiveSession;
use orca_webui::native::{AgentLiveSession, NativeOptions};
use serde_json::Value;

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

struct FakeBridge {
    turns: Arc<Mutex<u32>>,
}

#[async_trait]
impl AgentEngine for FakeBridge {
    fn id(&self) -> &'static str {
        AgentEngines::CLAUDE
    }
    fn display_name(&self) -> &'static str {
        AgentEngines::display_name_of(AgentEngines::CLAUDE)
    }
    fn uses_slots(&self) -> bool {
        false
    }
    fn session_id(&self) -> Option<&str> {
        Some("cli-1")
    }
    async fn run_turn(
        &mut self,
        turn: AgentTurn<'_>,
        _cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        *self.turns.lock().unwrap() += 1;
        turn.events.on_event(AgentEngineEvent::SessionStarted {
            session_id: Some("cli-1".into()),
            model: None,
            working_directory: None,
        });
        turn.events.on_event(AgentEngineEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            original_name: "Read".into(),
            args_json: r#"{"path":"a.txt"}"#.into(),
        });
        turn.events.on_event(AgentEngineEvent::ToolResult {
            id: "t1".into(),
            name: "read_file".into(),
            result: "x".into(),
            is_error: false,
            args_json: None,
        });
        turn.events
            .on_event(AgentEngineEvent::TextDelta("Hello from the bridge".into()));
        turn.events
            .on_event(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
                outcome: "success".into(),
                final_text: Some("Hello from the bridge".into()),
                output_tokens: Some(7),
                ..Default::default()
            }));
        Ok(())
    }
    async fn dispose(&mut self) {}
}

fn readiness(id: &str, logged_in: bool) -> EngineReadiness {
    EngineReadiness {
        engine_id: id.to_string(),
        state: if logged_in {
            EngineReadinessState::Ready
        } else {
            EngineReadinessState::NotLoggedIn
        },
        message: if logged_in {
            "Claude Code 2.1.326".into()
        } else {
            "Claude Code isn't logged in.".into()
        },
        executable: None,
        version: None,
        login_command: (!logged_in).then(|| vec!["claude".into(), "auth".into(), "login".into()]),
    }
}

struct Fixture {
    live: Arc<AgentLiveSession>,
    seen: Seen,
    _sub: Box<dyn std::any::Any>,
    starts: Arc<Mutex<Vec<BridgeStart>>>,
    turns: Arc<Mutex<u32>>,
    logged_in: Arc<Mutex<bool>>,
}

fn fixture(cwd: &std::path::Path) -> Fixture {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = "http://127.0.0.1:9/v1".into();
    config.lm_studio.models = vec!["m".into()];
    config.permissions.auto_approve_all = true;
    config.bridges.claude.progress_bars = false;
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let session = AgentSession::new(config, client, ToolRegistry::new(), ToolContext::new(cwd));

    let starts = Arc::new(Mutex::new(Vec::new()));
    let turns = Arc::new(Mutex::new(0));
    let logged_in = Arc::new(Mutex::new(false));
    let mut engines = EngineHost::new();
    let li = logged_in.clone();
    engines.readiness_check = Some(Arc::new(move |id, _| {
        let r = readiness(id, *li.lock().unwrap());
        Box::pin(async move { Ok(r) })
    }));
    let (s, t) = (starts.clone(), turns.clone());
    engines.bridge_factory = Some(Box::new(move |start| {
        s.lock().unwrap().push(start.clone());
        Box::new(FakeBridge { turns: t.clone() })
    }));

    let bus = Arc::new(LiveEventBus::new());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let sub = bus.subscribe(move |e| sink.lock().unwrap().push((e.kind.clone(), e.data.clone())));
    let live = AgentLiveSession::start_with(
        session,
        bus,
        "headless",
        NativeOptions {
            engines: Some(engines),
            ..Default::default()
        },
    );
    Fixture {
        live,
        seen,
        _sub: Box::new(sub),
        starts,
        turns,
        logged_in,
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn kinds(seen: &Seen, kind: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k == kind)
        .map(|(_, d)| d.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_that_needs_a_login_stays_native_and_offers_it() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let r = f.live.set_engine("claude").await;
    assert!(!r.ok);
    assert_eq!(r.engine, "native");
    let auth = r.auth.expect("login offered");
    assert_eq!(auth.engine, "claude");
    assert_eq!(auth.login_command, "claude auth login");
    let events = kinds(&f.seen, types::ENGINE_AUTH);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["loginCommand"], "claude auth login");
    assert_eq!(
        f.live.get_snapshot().await.unwrap().engine.as_deref(),
        Some("native")
    );
    assert!(
        f.live
            .set_engine("nope")
            .await
            .message
            .unwrap()
            .contains("Unknown engine 'nope'")
    );
    f.live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turns_run_on_the_bridge_and_reach_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    *f.logged_in.lock().unwrap() = true;
    let r = f.live.set_engine("claude").await;
    assert!(r.ok, "{:?}", r.message);
    assert_eq!(r.engine, "claude");
    assert_eq!(kinds(&f.seen, types::ENGINE)[0]["engine"], "claude");
    let snap = f.live.get_snapshot().await.unwrap();
    assert_eq!(snap.engine.as_deref(), Some("claude"));
    assert!(snap.agents[0].is_lead);
    assert_eq!(snap.agents[0].slot.as_deref(), Some("claude"));
    assert_eq!(snap.agents[0].model.as_deref(), Some("claude-code"));

    for prompt in ["first", "second"] {
        f.live.submit_prompt(prompt, "web").await;
        let want = if prompt == "first" { 1 } else { 2 };
        until("turn end", || kinds(&f.seen, types::TURN_END).len() == want).await;
    }
    assert_eq!(*f.turns.lock().unwrap(), 2);
    // One CLI for the session: started once, not resumed (a fresh conversation).
    let starts = f.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].resume, None);
    let ends = kinds(&f.seen, types::TURN_END);
    assert_eq!(ends[0]["outcome"], "done");
    assert_eq!(ends[0]["tokens"], 7);
    let text: String = kinds(&f.seen, types::ASSISTANT_DELTA)
        .iter()
        .filter_map(|d| d["text"].as_str().map(str::to_string))
        .collect();
    assert!(text.contains("Hello from the bridge"), "{text}");
    assert!(!kinds(&f.seen, types::TOOL_CALL).is_empty());
    // Switching while idle works; back to native.
    assert!(f.live.set_engine("native").await.ok);
    assert_eq!(
        f.live.get_snapshot().await.unwrap().engine.as_deref(),
        Some("native")
    );
    f.live.shutdown().await;
}
