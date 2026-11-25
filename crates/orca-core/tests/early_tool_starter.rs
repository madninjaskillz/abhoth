//! Port of `EarlyToolStarterTests`: read-only calls start while the response is still
//! streaming (agent.earlyReadOnlyTools).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use orca_abstractions::ToolResult;
use orca_core::chat::EarlyToolStarter;
use orca_core::client::{
    ChatMessage, ChatOptions, ClientSettings, OpenAiClient, ServedModels, StreamControl,
    ToolCallDelta,
};
use orca_testkit::{FakeOpenAi, Reply};

type Ran = Arc<Mutex<Vec<String>>>;

fn starter(ran: &Ran) -> EarlyToolStarter {
    starter_with(ran, |name, _| name == "read" || name == "grep")
}

fn starter_with(
    ran: &Ran,
    eligible: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
) -> EarlyToolStarter {
    let ran = ran.clone();
    EarlyToolStarter::new(eligible, move |name, args| {
        ran.lock().unwrap().push(format!("{name} {args}"));
        async move { ToolResult::success(format!("ran {name}")) }.boxed()
    })
}

/// One raw streaming chunk carrying a tool-call delta.
fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: &str) -> ToolCallDelta {
    ToolCallDelta {
        index,
        id: id.map(str::to_string),
        name: name.map(str::to_string),
        arguments: Some(args.to_string()),
    }
}

#[tokio::test]
async fn call_starts_when_the_next_call_begins_and_the_result_is_taken_once() {
    let ran = Ran::default();
    let mut s = starter(&ran);
    s.observe(&delta(0, Some("c0"), Some("read"), "{\"path\":"));
    s.observe(&delta(0, None, None, "\"a.txt\"}"));
    assert_eq!(s.started_count(), 0); // call 0 may still be growing

    s.observe(&delta(1, Some("c1"), Some("read"), "{\"path\":\"b.txt\"}"));
    assert_eq!(s.started_count(), 1);

    // Whitespace differs: same JSON.
    let early = s.try_take(Some("c0"), "read", "{\"path\": \"a.txt\"}");
    assert!(early.is_some());
    assert_eq!(early.unwrap().await.unwrap().content, "ran read");
    assert!(
        s.try_take(Some("c0"), "read", "{\"path\":\"a.txt\"}")
            .is_none()
    );
    // The last call only completes when the stream ends: the executor runs it normally.
    assert!(
        s.try_take(Some("c1"), "read", "{\"path\":\"b.txt\"}")
            .is_none()
    );
    assert_eq!(s.used_count(), 1);
}

#[tokio::test]
async fn nothing_starts_after_an_ineligible_call() {
    let ran = Ran::default();
    let mut s = starter(&ran);
    s.observe(&delta(
        0,
        Some("c0"),
        Some("write"),
        "{\"path\":\"a.txt\",\"content\":\"x\"}",
    ));
    s.observe(&delta(1, Some("c1"), Some("read"), "{\"path\":\"a.txt\"}"));
    s.observe(&delta(2, Some("c2"), Some("read"), "{\"path\":\"b.txt\"}"));
    // The read of a.txt must see the write, so it (and everything after) waits for the executor.
    assert_eq!(s.started_count(), 0);
    assert!(ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn different_arguments_are_not_taken() {
    let ran = Ran::default();
    let mut s = starter(&ran);
    s.observe(&delta(0, Some("c0"), Some("grep"), "{\"pattern\":\"x\"}"));
    s.observe(&delta(1, Some("c1"), Some("read"), "{}"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        s.try_take(Some("c0"), "grep", "{\"pattern\":\"y\"}")
            .is_none()
    );
    assert!(
        s.try_take(Some("c0"), "read", "{\"pattern\":\"x\"}")
            .is_none()
    );
}

#[tokio::test]
async fn broken_json_is_not_started() {
    let ran = Ran::default();
    let mut s = starter(&ran);
    s.observe(&delta(0, Some("c0"), Some("read"), "{\"path\":"));
    s.observe(&delta(1, Some("c1"), Some("read"), "{}"));
    assert_eq!(s.started_count(), 0);
}

#[tokio::test]
async fn normalize_applies_to_the_run_and_the_match() {
    let ran = Ran::default();
    let mut s = starter(&ran).with_normalize(|a| a.replace(",\"reason\":\"r\"", ""));
    s.observe(&delta(
        0,
        Some("c0"),
        Some("read"),
        "{\"path\":\"a\",\"reason\":\"r\"}",
    ));
    s.observe(&delta(1, Some("c1"), Some("read"), "{}"));
    let early = s.try_take(Some("c0"), "read", "{\"path\":\"a\"}");
    assert!(early.is_some());
    early.unwrap().await.unwrap();
    assert_eq!(
        *ran.lock().unwrap(),
        vec!["read {\"path\":\"a\"}".to_string()]
    );
}

/// `UpdatesWithoutRawToolCalls_AreIgnored`: the starter only ever sees tool-call fragments; the
/// client hands it none for a text-only stream.
#[tokio::test]
async fn updates_without_raw_tool_calls_are_ignored() {
    let server = FakeOpenAi::start(vec![Reply::text("hello")]).await;
    let client = OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", Some("m".into())),
        Arc::new(ServedModels::new()),
    );
    let ran = Ran::default();
    let mut s = starter(&ran);
    let mut on_event = |_| StreamControl::Continue;
    let mut on_delta = |d: &ToolCallDelta| s.observe(d);
    let response = client
        .stream_observed(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut on_event,
            Some(&mut on_delta),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "hello");
    assert_eq!(s.started_count(), 0);
}

#[tokio::test]
async fn dispose_cancels_runs_nobody_took() {
    struct SetOnDrop(Arc<Mutex<bool>>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = true;
        }
    }
    let cancelled = Arc::new(Mutex::new(false));
    let running = Arc::new(tokio::sync::Notify::new());
    let (c, r) = (cancelled.clone(), running.clone());
    let mut s: EarlyToolStarter = EarlyToolStarter::new(
        |_, _| true,
        move |_, _| {
            let (c, r) = (c.clone(), r.clone());
            async move {
                let _guard = SetOnDrop(c);
                r.notify_one();
                std::future::pending::<()>().await;
                ToolResult::success("never")
            }
            .boxed()
        },
    );
    s.observe(&delta(0, Some("c0"), Some("read"), "{}"));
    s.observe(&delta(1, Some("c1"), Some("read"), "{}"));
    assert_eq!(s.started_count(), 1);
    tokio::time::timeout(Duration::from_secs(5), running.notified())
        .await
        .unwrap();

    drop(s);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !*cancelled.lock().unwrap() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_client_reports_every_tool_call_fragment() {
    let server = FakeOpenAi::start(vec![Reply {
        tool_calls: vec![
            ("read".into(), "{\"path\":\"a\"}".into()),
            ("grep".into(), "{\"pattern\":\"x\"}".into()),
        ],
        ..Reply::text("")
    }])
    .await;
    let client = OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", Some("m".into())),
        Arc::new(ServedModels::new()),
    );
    let ran = Ran::default();
    let mut s = starter(&ran);
    let mut on_event = |_| StreamControl::Continue;
    let mut on_delta = |d: &ToolCallDelta| s.observe(d);
    let response = client
        .stream_observed(
            "lead",
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            &mut on_event,
            Some(&mut on_delta),
        )
        .await
        .unwrap();
    assert_eq!(response.tool_calls.len(), 2);
    // The first call completed when the second began; the second only at the end.
    assert_eq!(s.started_count(), 1);
    let early = s
        .try_take(Some("call_0"), "read", "{\"path\":\"a\"}")
        .unwrap();
    assert_eq!(early.await.unwrap().content, "ran read");
}
