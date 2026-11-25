//! The native web session beyond running turns: ask_user questions answered in the browser,
//! the project settings page, and switching a project session's profile / mode.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Host;
use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, ToolRegistry};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::live::{LiveEventBus, types};
use orca_testkit::{FakeOpenAi, Reply};
use orca_webui::LiveSession;
use orca_webui::native::{AgentLiveSession, NativeOptions, QuestionBroker, Rebuilt};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

type Seen = Arc<Mutex<Vec<(String, Value)>>>;
type Asked = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

struct Asker(Arc<QuestionBroker>);

#[async_trait::async_trait]
impl orca_tools_utility::UserPrompter for Asker {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        self.0.ask(question, options).await
    }
}

fn config(base_url: &str, model: &str) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.base_url = base_url.to_string();
    c.lm_studio.api_key = "k".into();
    c.lm_studio.models = vec![model.into()];
    c.lm_studio.native_tool_calling = true;
    c.agent.verification_mode = "off".into();
    c.permissions.auto_approve_all = true;
    c
}

fn session(
    config: OrcaConfig,
    cwd: &std::path::Path,
    questions: &Arc<QuestionBroker>,
) -> AgentSession {
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_all(orca_tools_utility::interactive_tools(Some(Arc::new(
        Asker(questions.clone()),
    ))));
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    s.retry_base_delay = Duration::from_millis(1);
    s.failover_delay = Duration::from_millis(1);
    s
}

fn recorder(bus: &Arc<LiveEventBus>) -> (Seen, impl Drop) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let sub = bus.subscribe(move |e| sink.lock().unwrap().push((e.kind.clone(), e.data.clone())));
    (seen, sub)
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

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_goes_to_the_browser_and_the_answer_goes_back_to_the_model() {
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "ask_user",
            json!({"question": "Which colour?", "options": ["red", "blue"]}),
        ),
        Reply::text("Blue it is, then; I will use blue everywhere from now on."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let (seen, _sub) = recorder(&bus);
    let live = AgentLiveSession::start_with(
        session(config(&server.base_url(), "m"), work.path(), &questions),
        bus.clone(),
        "headless",
        NativeOptions {
            questions: Some(questions.clone()),
            ..Default::default()
        },
    );
    assert!(live.submit_prompt("pick a colour", "web").await.is_none());
    until("the question", || !questions.pending().is_empty()).await;
    let snap = live.get_snapshot().await.unwrap();
    let pending = snap.pending_questions.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "q0001");
    assert_eq!(pending[0].question, "Which colour?");
    assert_eq!(pending[0].options, vec!["red", "blue"]);

    assert!(!live.answer_question("q0001", "   ").await, "empty answer");
    assert!(!live.answer_question("q9999", "blue").await, "unknown id");
    assert!(live.answer_question("q0001", " blue ").await);
    until("the turn end", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|(k, _)| k == types::TURN_END)
    })
    .await;
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    let tool_msg = reqs[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert!(
        tool_msg["content"].as_str().unwrap().contains("blue"),
        "{tool_msg}"
    );
    let events = seen.lock().unwrap().clone();
    let asked = events.iter().find(|(k, _)| k == types::QUESTION).unwrap();
    assert_eq!(
        asked.1,
        json!({"id": "q0001", "question": "Which colour?", "options": ["red", "blue"]})
    );
    let resolved = events
        .iter()
        .find(|(k, _)| k == types::QUESTION_RESOLVED)
        .unwrap();
    assert_eq!(resolved.1, json!({"id": "q0001", "answer": "blue"}));
    live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_turn_withdraws_its_question() {
    let server = FakeOpenAi::start(vec![Reply::tool(
        "ask_user",
        json!({"question": "Go on?", "options": ["yes", "no"]}),
    )])
    .await;
    let work = tempfile::tempdir().unwrap();
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let (seen, _sub) = recorder(&bus);
    let live = AgentLiveSession::start_with(
        session(config(&server.base_url(), "m"), work.path(), &questions),
        bus.clone(),
        "headless",
        NativeOptions {
            questions: Some(questions.clone()),
            ..Default::default()
        },
    );
    live.submit_prompt("ask me", "web").await;
    until("the question", || !questions.pending().is_empty()).await;
    assert!(live.cancel().await);
    until("the withdrawal", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|(k, d)| k == types::QUESTION_RESOLVED && d["answer"].is_null())
    })
    .await;
    assert!(questions.pending().is_empty());
    live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn project_settings_describe_the_session_change_it_and_save_config_json() {
    let server = FakeOpenAi::start(vec![Reply::text(
        "Hello there, this is a reply long enough to count as a wrap-up.",
    )])
    .await;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let live = AgentLiveSession::start_with(
        session(config(&server.base_url(), "m"), work.path(), &questions),
        bus,
        "headless",
        NativeOptions {
            config_home: Some(home.path().to_path_buf()),
            overlay: Some(orca_core::config::schema::ProfileOverlay::new(
                "fast",
                vec!["lmStudio.maxTokens".into()],
            )),
            ..Default::default()
        },
    );
    let host = Host::start(live.clone()).await;
    let pid = host.primary();
    let page: Value = host
        .api(Method::GET, &format!("/api/projects/{pid}/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["profile"], "fast");
    assert!(
        page["configFile"]
            .as_str()
            .unwrap()
            .ends_with("config.json")
    );
    let ok: Value = host
        .api(Method::PUT, &format!("/api/projects/{pid}/settings"))
        .json(&json!({"key": "lmStudio.temperature", "value": 0.15}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    let refused = host
        .api(Method::PUT, &format!("/api/projects/{pid}/settings"))
        .json(&json!({"key": "lmStudio.maxTokens", "value": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let refused: Value = refused.json().await.unwrap();
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("Set by profile fast")
    );
    let saved = std::fs::read_to_string(home.path().join("config.json")).unwrap();
    assert!(saved.contains("0.15"), "{saved}");

    // The next turn uses the changed setting.
    live.submit_prompt("hi", "web").await;
    until("the request", || !server.requests().is_empty()).await;
    assert_eq!(server.requests()[0].body["temperature"], json!(0.15));
    live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_session_switches_profile_and_mode_before_its_next_turn_on_the_same_conversation()
{
    let wrap = "Done here, and this reply is long enough to count as a wrap-up.";
    let server = FakeOpenAi::start(vec![Reply::text(wrap), Reply::text(wrap)]).await;
    let work = tempfile::tempdir().unwrap();
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let asked: Asked = Arc::default();
    let (base, cwd, q, log) = (
        server.base_url(),
        work.path().to_path_buf(),
        questions.clone(),
        asked.clone(),
    );
    let rebuild: orca_webui::native::RebuildFn = Arc::new(move |profile, mode| {
        log.lock().unwrap().push((profile.clone(), mode.clone()));
        let s = session(config(&base, "model-b"), &cwd, &q);
        Box::pin(async move {
            Ok(Rebuilt {
                session: s,
                overlay: None,
            })
        })
    });
    let live = AgentLiveSession::start_with(
        session(
            config(&server.base_url(), "model-a"),
            work.path(),
            &questions,
        ),
        bus,
        "worker",
        NativeOptions {
            questions: Some(questions.clone()),
            rebuild: Some(rebuild),
            ..Default::default()
        },
    );
    live.submit_prompt("first", "web").await;
    until("the first turn", || server.requests().len() == 1).await;
    until("idle", || !live.bus().in_turn()).await;

    let r = live.set_mode(Some("peer")).await;
    assert!(r.ok && r.message.is_none(), "{r:?}");
    let r = live.set_profile(Some("dense2")).await;
    assert!(r.ok);
    live.submit_prompt("second", "web").await;
    until("the second turn", || server.requests().len() == 2).await;
    assert_eq!(
        asked.lock().unwrap().clone(),
        vec![(Some("dense2".to_string()), Some("peer".to_string()))]
    );
    let second = &server.requests()[1].body;
    assert_eq!(second["model"], "model-b");
    let users: Vec<&str> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(users, vec!["first", "second"], "the same conversation");
    let snap = live.get_snapshot().await.unwrap();
    assert_eq!(snap.model.as_deref(), Some("model-b"));
    live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_primary_session_keeps_its_profile_and_mode_as_in_csharp() {
    let work = tempfile::tempdir().unwrap();
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let live = AgentLiveSession::start_with(
        session(
            config("http://127.0.0.1:9/v1", "m"),
            work.path(),
            &questions,
        ),
        bus,
        "headless",
        NativeOptions::default(),
    );
    let r = live.set_profile(Some("x")).await;
    assert!(!r.ok);
    assert!(r.message.unwrap().contains("set where it was started"));
    let r = live.set_mode(Some("peer")).await;
    assert!(!r.ok);
    live.shutdown().await;
}
