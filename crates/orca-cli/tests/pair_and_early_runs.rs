//! Pair model programming (`PairModelVoter` in `AgentLoopRunner`) and the early read-only tool
//! runs (`EarlyToolStarter` in `ToolCallExecutor`) in the turn loop, against the fake server.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::ToolContext;
use orca_core::agent::pair_vote::PairStats;
use orca_core::agent::{AgentObserver, AgentSession, NullObserver, TurnEnd};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Recorded, Reply};
use serde_json::{Value, json};

fn session(server: &FakeOpenAi, work: &std::path::Path, pair: bool) -> AgentSession {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["model-a".into(), "org/model-b".into()];
    config.lm_studio.native_tool_calling = true;
    config.lm_studio.pair_model_programming = pair;
    config.agent.verification_mode = "off".into();
    config.agent.repeat_call_notes = false;
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let registry = orca_cli::build_registry(&config);
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(work));
    s.retry_base_delay = Duration::from_millis(1);
    s.failover_delay = Duration::from_millis(1);
    s.set_system_prompt("system".into());
    s
}

const WRAP_UP_A: &str = "Primary answer: the file holds one line that says alpha, nothing more.";
const WRAP_UP_B: &str = "Backup answer: the file holds a single line, alpha, and nothing else.";

fn vote(model: &str, choice: &str) -> Reply {
    Reply::text(format!("{{\"choice\":\"{choice}\"}}")).for_model(model)
}

/// Requests that are vote requests (one user message, temperature 0).
fn vote_requests(reqs: &[Recorded]) -> Vec<&Recorded> {
    reqs.iter()
        .filter(|r| r.body["max_completion_tokens"] == json!(1024))
        .collect()
}

#[derive(Default)]
struct Notices(Vec<String>);
impl AgentObserver for Notices {
    fn on_notice(&mut self, message: &str) {
        self.0.push(message.to_string());
    }
}

#[tokio::test]
async fn alternate_answer_wins_when_both_models_vote_for_it() {
    let server = FakeOpenAi::start(vec![
        Reply::text(WRAP_UP_A).for_model("model-a"),
        Reply::text(WRAP_UP_B).for_model("org/model-b"),
        vote("model-a", "B"),
        vote("org/model-b", "B"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    let mut notices = Notices::default();
    let end = s.run_turn("what is in f.txt?", &mut notices).await.unwrap();
    assert_eq!(end, TurnEnd::Done);
    assert_eq!(s.last_assistant_response.as_deref(), Some(WRAP_UP_B));
    let last = s.messages.last().unwrap();
    assert_eq!(last.content.as_deref(), Some(WRAP_UP_B));
    assert_eq!(
        s.pair_stats,
        PairStats {
            backup_wins: 1,
            ..PairStats::default()
        }
    );
    assert!(
        notices
            .0
            .contains(&"Winner: model-b (majority)".to_string())
    );
    assert!(
        notices
            .0
            .contains(&"  model-a \u{2192} model-b".to_string())
    );
    assert!(
        notices
            .0
            .contains(&"Pair mode: 1 vote \u{2014} backup \u{2713} 1".to_string())
    );

    let reqs = server.requests();
    assert_eq!(reqs.len(), 4);
    // The alternate: not streamed, its own model last, the same tools, temperature kept.
    let alt = reqs
        .iter()
        .find(|r| {
            r.body["model"] == "org/model-b"
                && r.body.get("stream").is_none()
                && r.body.get("tools").is_some()
        })
        .expect("alternate request");
    let keys: Vec<&str> = alt
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.first(), Some(&"temperature"));
    assert_eq!(keys.last(), Some(&"model"));
    let primary = reqs
        .iter()
        .find(|r| r.body["stream"] == json!(true))
        .unwrap();
    assert_eq!(alt.body["messages"], primary.body["messages"]);
    assert_eq!(alt.body["tools"], primary.body["tools"]);
    assert_eq!(alt.body["temperature"], primary.body["temperature"]);

    // The votes: every model, temperature 0, 1024 tokens, the model right after the messages.
    let votes = vote_requests(&reqs);
    assert_eq!(votes.len(), 2);
    for v in &votes {
        let keys: Vec<&str> = v
            .body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["temperature", "messages", "model", "max_completion_tokens"]
        );
        assert_eq!(v.body["temperature"], json!(0));
        let msgs = v.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0]["content"],
            Value::String(format!(
                "The user said: what is in f.txt?\n\nResponse A:\n{WRAP_UP_A}\n\nResponse B:\n{WRAP_UP_B}\n\nWhich response better addresses the user's request? Reply with ONLY valid JSON and nothing else: {{\"choice\":\"A\"}}, {{\"choice\":\"B\"}}"
            ))
        );
    }
    let mut voters: Vec<&str> = votes
        .iter()
        .map(|v| v.body["model"].as_str().unwrap())
        .collect();
    voters.sort();
    assert_eq!(voters, vec!["model-a", "org/model-b"]);
}

#[tokio::test]
async fn a_split_vote_keeps_the_primary() {
    let server = FakeOpenAi::start(vec![
        Reply::text(WRAP_UP_A).for_model("model-a"),
        Reply::text(WRAP_UP_B).for_model("org/model-b"),
        vote("model-a", "A"),
        vote("org/model-b", "B"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    let mut notices = Notices::default();
    s.run_turn("what is in f.txt?", &mut notices).await.unwrap();
    assert_eq!(s.last_assistant_response.as_deref(), Some(WRAP_UP_A));
    // A tie at the top with the primary in it: primary confirmed, no majority, not counted.
    assert_eq!(s.pair_stats, PairStats::default());
    assert!(
        notices
            .0
            .contains(&"Winner: model-a (primary confirmed)".to_string())
    );
}

#[tokio::test]
async fn a_failed_alternate_counts_as_a_voting_failure() {
    let server = FakeOpenAi::start(vec![
        Reply::text(WRAP_UP_A).for_model("model-a"),
        Reply::error(500, "{\"error\":\"down\"}").for_model("org/model-b"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    s.run_turn("what is in f.txt?", &mut NullObserver)
        .await
        .unwrap();
    assert_eq!(s.last_assistant_response.as_deref(), Some(WRAP_UP_A));
    assert_eq!(
        s.pair_stats,
        PairStats {
            backup_failures: 1,
            ..PairStats::default()
        }
    );
    assert!(vote_requests(&server.requests()).is_empty());
}

#[tokio::test]
async fn a_winning_alternate_tool_call_keeps_the_loop_going() {
    let server = FakeOpenAi::start(vec![
        Reply::text(WRAP_UP_A).for_model("model-a"),
        Reply {
            tool_calls: vec![("read_file".into(), json!({"path": "f.txt"}).to_string())],
            ..Reply::text("Let me read f.txt first.")
        }
        .for_model("org/model-b"),
        vote("model-a", "B"),
        vote("org/model-b", "B"),
        // Next iteration: the alternate has nothing to say, so there is no vote.
        Reply::text(WRAP_UP_A).for_model("model-a"),
        Reply::text("").for_model("org/model-b"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        work.path().join("f.txt"),
        "alpha
",
    )
    .unwrap();
    let mut s = session(&server, work.path(), true);
    let end = s.run_turn("what is in f.txt?", &mut NullObserver).await;
    assert_eq!(end.unwrap(), TurnEnd::Done);
    assert_eq!(s.tool_call_history.len(), 1);
    assert_eq!(s.tool_call_history[0].name, "read_file");
    let call_turn = s
        .messages
        .iter()
        .find(|m| !m.tool_calls.is_empty())
        .expect("the alternate's call is in history");
    assert_eq!(
        call_turn.content.as_deref(),
        Some("Let me read f.txt first.")
    );
    assert_eq!(s.last_assistant_response.as_deref(), Some(WRAP_UP_A));
    assert_eq!(
        s.pair_stats,
        PairStats {
            backup_wins: 1,
            ..PairStats::default()
        }
    );
    let reqs = server.requests();
    assert_eq!(reqs.len(), 6);
    // C# sends the winner's text as parts with an empty first part, tool calls or not.
    let follow_up = reqs
        .iter()
        .filter(|r| r.body["stream"] == json!(true))
        .nth(1)
        .unwrap();
    let echoed = follow_up.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(
        echoed["content"],
        json!([{"type": "text", "text": ""}, {"type": "text", "text": "Let me read f.txt first."}])
    );
}

#[tokio::test]
async fn pair_mode_off_sends_one_request() {
    let server = FakeOpenAi::start(vec![Reply::text(WRAP_UP_A)]).await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), false);
    s.run_turn("what is in f.txt?", &mut NullObserver)
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn read_only_calls_start_while_the_response_streams() {
    let server = FakeOpenAi::start(vec![
        Reply {
            tool_calls: vec![
                ("read_file".into(), json!({"path": "a.txt"}).to_string()),
                ("read_file".into(), json!({"path": "b.txt"}).to_string()),
            ],
            ..Reply::text("")
        },
        Reply::text("a.txt says alpha and b.txt says beta; both files hold one line each."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(work.path().join("b.txt"), "beta\n").unwrap();
    let mut s = session(&server, work.path(), false);
    s.run_turn("read both", &mut NullObserver).await.unwrap();
    // The first call completed when the second began and ran early; the last one only
    // completes with the stream and runs normally.
    assert_eq!(s.early_tool_runs_used, 1);
    // Records and results as without early runs: call order, both read.
    let names: Vec<_> = s
        .tool_call_history
        .iter()
        .map(|r| r.arguments.clone().unwrap())
        .collect();
    assert!(names[0].contains("a.txt") && names[1].contains("b.txt"));
    let reqs = server.requests();
    let msgs = reqs[1].body["messages"].as_array().unwrap();
    let tool_results: Vec<&str> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("alpha"));
    assert!(tool_results[1].contains("beta"));
}

#[tokio::test]
async fn nothing_runs_early_after_a_write() {
    let server = FakeOpenAi::start(vec![
        Reply {
            tool_calls: vec![
                (
                    "write_file".into(),
                    json!({"path": "a.txt", "content": "new"}).to_string(),
                ),
                ("read_file".into(), json!({"path": "a.txt"}).to_string()),
                ("read_file".into(), json!({"path": "a.txt"}).to_string()),
            ],
            ..Reply::text("")
        },
        Reply::text("a.txt now says new; I wrote it and read it back twice to check."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), false);
    s.config.permissions.auto_approve_all = true;
    s.run_turn("write and read", &mut NullObserver)
        .await
        .unwrap();
    assert_eq!(s.early_tool_runs_used, 0);
    assert_eq!(s.tool_call_history.len(), 3);
    assert!(
        s.tool_call_history[1]
            .result
            .as_deref()
            .unwrap()
            .contains("new")
    );
}

#[tokio::test]
async fn early_runs_off_runs_everything_normally() {
    let server = FakeOpenAi::start(vec![
        Reply {
            tool_calls: vec![
                ("read_file".into(), json!({"path": "a.txt"}).to_string()),
                ("read_file".into(), json!({"path": "a.txt"}).to_string()),
            ],
            ..Reply::text("")
        },
        Reply::text("a.txt says alpha; I read it twice and it holds exactly one line."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.txt"), "alpha\n").unwrap();
    let mut s = session(&server, work.path(), false);
    s.config.agent.early_read_only_tools = false;
    s.run_turn("read", &mut NullObserver).await.unwrap();
    assert_eq!(s.early_tool_runs_used, 0);
    assert_eq!(s.tool_call_history.len(), 2);
}
