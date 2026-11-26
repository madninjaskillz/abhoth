//! Calendar reservations on the data plane (SUPERVISOR.md 9.4, SV12): placement keeps others off
//! a reserved card, the event's key goes there first, leases are refused or revoked, and a
//! `preempt` reservation ends others' in-flight streams. One test: the calendar is
//! process-wide.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use orca_router::server::Mode;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread")]
async fn a_reserved_card_takes_only_its_events_requests_and_preempts_the_rest() {
    let a = Fake::ok("A").await;
    let b = Fake::start(|s| {
        if s.body.get("stream") == Some(&Value::Bool(true)) {
            Reply::Endless
        } else {
            Reply::Json(
                200,
                json!({"id": "c1", "object": "chat.completion", "model": s.body["model"],
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "B"}, "finish_reason": "stop"}]}),
            )
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    cfg.slots.enabled = true;
    let r = start_router(cfg, None, Mode::Gateway).await;

    let file = dir.path().join("calendar.toml");
    std::fs::write(&file, "[calendar]\nenabled = true\n").unwrap();
    let store = Arc::new(orca_router::control::store::Store::memory().unwrap());
    let cal = orca_router::calendar::CalendarService::open(
        "gpu",
        "Europe/London",
        &file,
        dir.path(),
        store.clone(),
        Default::default(),
    )
    .unwrap();
    orca_router::calendar::install(cal.clone()).unwrap();

    // Before the event: someone streams on B and holds a lease there.
    let url = r.url.clone();
    let stream = tokio::spawn(async move {
        let resp = chat(
            &url,
            json!({"model": "qwen3.8-flash-next-b", "stream": true, "messages": []}),
        )
        .await;
        assert_eq!(resp.status(), 200);
        resp.text().await.unwrap_or_default()
    });
    let lease: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "early", "kind": "agent", "copy": "B"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(lease["ok"], true, "{lease}");
    for _ in 0..50 {
        if *b.open_streams.lock().unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The event: it reserves B, preempting others 30 s in (it started a minute ago).
    let now = chrono::Utc::now().timestamp();
    let start = orca_calendar::recur::to_local(cal.tz(), now - 60)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let e: orca_calendar::entry::EntryConfig = serde_json::from_value(json!({
        "title": "pipeline", "at": start, "duration": "30m", "priority": "high",
        "effects": [{"reserve_card": "B", "on_reserve": "preempt", "preempt_after": "30s"}],
        "key": {"scopes": ["chat", "use_reserved"]}
    }))
    .unwrap();
    cal.save_entry("pipe", Some(&e), None, "admin", "test")
        .unwrap();
    let (_, key) = cal.issue_key("pipe", "admin").unwrap();

    // Preempt: the stream from before ends with the error event; once per occurrence.
    assert_eq!(
        cal.preempt_tick(now),
        vec![("B".to_string(), "pipe".to_string())]
    );
    assert!(cal.preempt_tick(now + 5).is_empty());
    let text = tokio::time::timeout(Duration::from_secs(10), stream)
        .await
        .expect("the stream ends")
        .unwrap();
    assert!(text.contains("preempted_by_reservation"), "{text}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let audit = store.audit_rows(20).unwrap();
    assert!(audit.iter().any(|a| a.action == "calendar.preempt"));
    assert!(audit.iter().any(|a| a.action == "calendar.preempt.request"));

    // Others: A only; a B-only route gets 503 with when it frees up.
    for _ in 0..4 {
        let v: Value = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []}))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "A");
    }
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-b", "messages": []}),
    )
    .await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("retry-after").is_some());
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.starts_with("card B reserved by pipe until ~"), "{msg}");

    // The event's key: B first.
    let v: Value = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .bearer_auth(&key)
        .json(&json!({"model": "qwen3.8-27b", "messages": []}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "B", "{v}");
    // A key that is not one: refused, never treated as anonymous.
    let bad = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .bearer_auth("abe_notarealkeynotarealkeynotarealk")
        .json(&json!({"model": "qwen3.8-27b", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    // Leases: the early one is revoked on renew (preempt drains), new claims avoid B.
    let renew: Value = http()
        .post(format!("{}/slots/renew", r.url))
        .json(&json!({"keys": ["early"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(renew["lost"], json!(["early"]), "{renew}");
    assert!(
        renew["revoked"]["early"]
            .as_str()
            .unwrap()
            .starts_with("lease revoked: reserved by pipe")
    );
    let hard: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "k2", "kind": "agent", "copy": "B"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hard["ok"], false);
    let soft: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "k3", "kind": "agent"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(soft["copy"], "A", "{soft}");
}
