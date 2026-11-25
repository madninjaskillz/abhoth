//! The session hub over the websocket: invoke + events, a real native turn against the fake
//! model server, browser approvals, cancel, and reconnect with resume.

mod common;

use common::{FakeSession, Host, Ws, native_session};
use orca_core::live::types;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_streams_the_turn_and_lands_in_the_snapshot() {
    let model = FakeOpenAi::start(vec![Reply::text("Hello from the model.")]).await;
    let work = tempfile::tempdir().unwrap();
    let session = native_session(&model.base_url(), work.path(), false);
    let host = Host::start(session).await;
    let pid = host.primary();
    let mut ws = host.connect().await;
    assert!(!ws.resumed);

    let snap = ws.invoke("GetSnapshot", json!([pid])).await.unwrap();
    assert_eq!(snap["mode"], "headless");
    assert_eq!(snap["busy"], false);
    assert!(
        snap["agents"]["agents"].is_array(),
        "agents wrapped as in C#"
    );

    let r = ws
        .invoke("SendPrompt", json!([pid, "say hello"]))
        .await
        .unwrap();
    assert!(r.is_null(), "queued: {r}");
    let events = ws.events_until(|e| e["type"] == types::TURN_END).await;
    let k = kinds(&events);
    let start = k.iter().position(|t| t == types::TURN_START).unwrap();
    assert_eq!(k[start + 1], types::USER);
    assert_eq!(events[start + 1]["data"]["text"], "say hello");
    assert_eq!(events[start + 1]["data"]["source"], "web");
    let text: String = events
        .iter()
        .filter(|e| e["type"] == types::ASSISTANT_DELTA)
        .map(|e| e["data"]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(text, "Hello from the model.");
    assert_eq!(events.last().unwrap()["data"]["outcome"], "done");
    let seqs: Vec<i64> = events.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq rises: {seqs:?}");

    // The busy event after turnEnd, then the snapshot has the finished turn.
    ws.events_until(|e| e["type"] == types::BUSY && e["data"]["busy"] == false)
        .await;
    let snap = ws.invoke("GetSnapshot", json!([pid])).await.unwrap();
    let history = snap["history"].as_array().unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[0]["kind"], "user");
    assert_eq!(history[1]["text"], "Hello from the model.");
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_errors_use_the_csharp_texts() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let pid = host.primary();
    let mut ws = host.connect().await;
    assert_eq!(
        ws.invoke("SendPrompt", json!(["nope", "x"])).await,
        Err("No such project.".into())
    );
    assert_eq!(
        ws.invoke("SetEngine", json!([pid, "gpt-9000"])).await,
        Err("Unknown engine: gpt-9000".into())
    );
    assert!(ws.invoke("NoSuchMethod", json!([])).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_and_huge_prompts_are_refused() {
    let model = FakeOpenAi::start(vec![]).await;
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(native_session(&model.base_url(), work.path(), false)).await;
    let pid = host.primary();
    let mut ws = host.connect().await;
    assert_eq!(
        ws.invoke("SendPrompt", json!([pid, "   "])).await.unwrap(),
        "Empty prompt."
    );
    let huge = "x".repeat(200_001);
    assert_eq!(
        ws.invoke("SendPrompt", json!([pid, huge])).await.unwrap(),
        "Prompt too long (200,001 characters; the limit is 200,000)."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_approvals_go_to_the_browser() {
    let model = FakeOpenAi::start(vec![
        Reply::tool("write_file", json!({"path": "out.txt", "content": "hi\n"})),
        Reply::text("Wrote out.txt for you, it now contains a single greeting line."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(native_session(&model.base_url(), work.path(), true)).await;
    let pid = host.primary();
    let mut ws = host.connect().await;
    ws.invoke("SendPrompt", json!([pid, "write it"]))
        .await
        .unwrap();
    let events = ws.events_until(|e| e["type"] == types::PERMISSION).await;
    let request = &events.last().unwrap()["data"];
    assert_eq!(request["id"], "p0001");
    assert_eq!(request["tool"], "write_file");

    let snap = ws.invoke("GetSnapshot", json!([pid])).await.unwrap();
    assert_eq!(snap["pendingPermissions"][0]["id"], "p0001");

    assert_eq!(
        ws.invoke("ResolvePermission", json!([pid, "p0001", "yes"]))
            .await
            .unwrap(),
        true
    );
    let events = ws.events_until(|e| e["type"] == types::TURN_END).await;
    let k = kinds(&events);
    assert!(k.contains(&types::PERMISSION_RESOLVED.to_string()), "{k:?}");
    let result = events
        .iter()
        .find(|e| e["type"] == types::TOOL_RESULT)
        .unwrap();
    assert_eq!(result["data"]["name"], "write_file");
    assert_eq!(result["data"]["isError"], false);
    assert_eq!(events.last().unwrap()["data"]["outcome"], "done");
    assert_eq!(
        std::fs::read_to_string(work.path().join("out.txt")).unwrap(),
        "hi\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_while_waiting_for_approval_ends_the_turn() {
    let model = FakeOpenAi::start(vec![
        Reply::tool("write_file", json!({"path": "out.txt", "content": "hi\n"})),
        Reply::text("unused"),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(native_session(&model.base_url(), work.path(), true)).await;
    let pid = host.primary();
    let mut ws = host.connect().await;
    ws.invoke("SendPrompt", json!([pid, "write it"]))
        .await
        .unwrap();
    ws.events_until(|e| e["type"] == types::PERMISSION).await;
    assert_eq!(ws.invoke("Cancel", json!([pid])).await.unwrap(), true);
    let events = ws.events_until(|e| e["type"] == types::TURN_END).await;
    let resolved = events
        .iter()
        .find(|e| e["type"] == types::PERMISSION_RESOLVED)
        .unwrap();
    assert_eq!(resolved["data"]["decision"], "no");
    assert_eq!(events.last().unwrap()["data"]["outcome"], "cancelled");
    assert!(!work.path().join("out.txt").exists());
    // Nothing runs now: a second cancel has nothing to stop.
    assert_eq!(ws.invoke("Cancel", json!([pid])).await.unwrap(), false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_resumes_and_replays_what_was_missed() {
    let work = tempfile::tempdir().unwrap();
    let session = FakeSession::new(work.path());
    let host = Host::start(session.clone()).await;
    let pid = host.primary();
    let port = host.server.port();

    let mut ws = host.connect().await;
    let id = ws.connection_id.clone();
    ws.invoke("GetSnapshot", json!([pid])).await.unwrap();
    let last = ws.last;
    // A call whose answer is never read, then the socket drops.
    ws.send_invoke("SendPrompt", json!([pid, "while away"]))
        .await;
    ws.drop_socket().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // Events published while nobody is connected.
    session.bus.publish_turn_start(0, "web");
    session
        .bus
        .publish(types::USER, json!({"text": "while away", "source": "web"}));
    session.bus.publish_turn_end(5, 1, "done");

    let mut back = Ws::connect(port, Some((&id, last))).await;
    assert!(back.resumed);
    assert_eq!(back.connection_id, id);
    let mut saw_result = false;
    let events = loop {
        let v = back.next().await;
        assert!(v["n"].as_u64().unwrap() > last, "only what was missed");
        if v["type"] == "result" {
            assert!(v["result"].is_null());
            saw_result = true;
        }
        if v["type"] == "event" && v["method"] == "events" {
            let evs = v["args"][1].as_array().unwrap().clone();
            if evs.iter().any(|e| e["type"] == types::TURN_END) {
                break evs;
            }
        }
    };
    assert!(saw_result, "the answer to the call made before the drop");
    assert_eq!(events.last().unwrap()["data"]["outcome"], "done");
    assert_eq!(*session.prompts.lock().unwrap(), vec!["while away"]);

    // An unknown or expired id starts a fresh connection.
    let fresh = Ws::connect(port, Some(("nope", 3))).await;
    assert!(!fresh.resumed);
    assert_ne!(fresh.connection_id, id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_connection_is_forgotten_at_once() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let port = host.server.port();
    let mut ws = host.connect().await;
    let id = ws.connection_id.clone();
    ws.send(json!({"type": "close"})).await;
    ws.drop_socket().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let again = Ws::connect(port, Some((&id, 0))).await;
    assert!(!again.resumed);
}
