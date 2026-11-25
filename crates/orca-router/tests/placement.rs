//! Phase 2: the pig-slots API served by the router, lease-aware placement, admission with Lead
//! priority, session affinity, /status and /metrics.

mod common;

use std::time::{Duration, Instant};

use common::*;
use orca_core::slots::http::HttpSlotRegistry;
use orca_core::slots::registry::{SlotClaim, SlotRegistry};
use orca_router::server::Mode;
use serde_json::{Value, json};

fn claim(key: &str, kind: &str, copy: Option<&str>) -> SlotClaim {
    SlotClaim {
        key: key.into(),
        owner: "test".into(),
        host: "h".into(),
        pid: 42,
        label: key.into(),
        kind: kind.into(),
        copy: copy.map(str::to_string),
        ..SlotClaim::default()
    }
}

#[tokio::test]
async fn orca_core_slot_client_works_against_the_router() {
    let a = Fake::ok("A").await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &a.url(), None, dir.path());
    cfg.slots.enabled = true;
    cfg.slots.state_file = Some(dir.path().join("leases.json").to_string_lossy().to_string());
    let r = start_router(cfg.clone(), None, Mode::Slots).await;
    let reg = HttpSlotRegistry::new(&r.url, Duration::from_secs(5));

    let lead = reg.claim(&claim("lead", "main", Some("A"))).await.unwrap();
    assert!(lead.ok);
    assert_eq!(lead.slot.as_deref(), Some("A1"));
    // balanced: the next agent goes to the less loaded copy B
    let ag = reg.claim(&claim("agent-1", "agent", None)).await.unwrap();
    assert_eq!(ag.copy.as_deref(), Some("B"));
    // idempotent per key
    let again = reg.claim(&claim("agent-1", "agent", None)).await.unwrap();
    assert_eq!(again.slot, ag.slot);
    assert_eq!(
        reg.renew(&["lead".into(), "gone".into()]).await.unwrap(),
        vec!["gone".to_string()]
    );
    let busy = reg.busy_slots().await.unwrap().unwrap();
    assert_eq!(busy.len(), 2);
    // fill copy A (4 lanes): A2..A4, then a hard copy-A claim is full
    for i in 2..=4 {
        assert!(
            reg.claim(&claim(&format!("a{i}"), "agent", Some("A")))
                .await
                .unwrap()
                .ok
        );
    }
    let full = reg.claim(&claim("a5", "agent", Some("A"))).await.unwrap();
    assert!(!full.ok);
    assert_eq!(full.reason.as_deref(), Some("full"));
    reg.release("a4").await.unwrap();
    reg.release_all("h", 42).await.unwrap();
    assert!(reg.busy_slots().await.unwrap().unwrap().is_empty());
    // the raw /list shape
    let list: Value = http()
        .get(format!("{}/list", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["ttl"], 90.0);
    assert_eq!(list["free"].as_array().unwrap().len(), 8);
    assert_eq!(list["load"], json!({"A": 0, "B": 0}));
    // leases.json is written in pig-slots' format
    assert!(dir.path().join("leases.json").exists());
}

#[tokio::test]
async fn frozen_layout_refuses_claims() {
    let a = Fake::ok("A").await;
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("layout.json");
    std::fs::write(
        &layout,
        r#"{"layout": "x", "slots": ["A1", "A2", "B1"], "frozen": true, "reason": "switch"}"#,
    )
    .unwrap();
    let mut cfg = two_cards(&a.url(), &a.url(), None, dir.path());
    cfg.slots.enabled = true;
    cfg.slots.layout_file = Some(layout.to_string_lossy().to_string());
    let r = start_router(cfg, None, Mode::Gateway).await;
    let resp: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "k", "kind": "agent"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp, json!({"ok": false, "reason": "switching"}));
    let list: Value = http()
        .get(format!("{}/slots/list", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["layout"], "x");
    assert_eq!(list["frozen"], true);
    assert_eq!(list["free"], json!(["A1", "A2", "B1"]));
}

#[tokio::test]
async fn lease_aware_placement_follows_the_lease() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    cfg.slots.enabled = true;
    cfg.placement.lease_aware = true;
    let r = start_router(cfg, None, Mode::Gateway).await;
    let c: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "agent-on-b", "kind": "agent", "copy": "B"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["copy"], "B");
    for _ in 0..3 {
        let resp = http()
            .post(format!("{}/v1/chat/completions", r.url))
            .header("x-abhoth-slot-key", "agent-on-b")
            .json(&json!({"model": "qwen3.8-27b", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["x-abhoth-card"], "B");
    }
    // prompt_cache_key equal to the lease key works too
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-27b", "messages": [], "prompt_cache_key": "agent-on-b"}),
    )
    .await;
    assert_eq!(resp.headers()["x-abhoth-card"], "B");
    // prompt_cache_key still reaches the engine (ninfer accepts it)
    assert_eq!(
        b.chats().last().unwrap().body["prompt_cache_key"],
        "agent-on-b"
    );
}

#[tokio::test]
async fn affinity_sticks_a_session_to_one_card() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    cfg.placement.affinity = true;
    let r = start_router(cfg, None, Mode::Gateway).await;
    let mut cards = std::collections::HashMap::new();
    for s in 0..6 {
        let key = format!("session-{s}");
        let first = chat(
            &r.url,
            json!({"model": "jarvis-brain", "messages": [], "prompt_cache_key": key}),
        )
        .await;
        let card = first.headers()["x-abhoth-card"]
            .to_str()
            .unwrap()
            .to_string();
        for _ in 0..3 {
            let again = chat(
                &r.url,
                json!({"model": "jarvis-brain", "messages": [], "prompt_cache_key": key}),
            )
            .await;
            assert_eq!(again.headers()["x-abhoth-card"].to_str().unwrap(), card);
        }
        cards.insert(key, card);
    }
    // sessions spread over both cards
    let on_a = cards.values().filter(|c| *c == "A").count();
    assert!(on_a > 0 && on_a < 6, "{cards:?}");
}

#[tokio::test]
async fn admission_queues_agents_and_lets_the_lead_through() {
    // card A only (pinned name), 2 lanes, one lane reserved for the Lead
    let a = Fake::start(|_| {
        Reply::Sse(vec![
            (0, "data: {}\n\n".into()),
            (700, "data: [DONE]\n\n".into()),
        ])
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &dead_url().await, None, dir.path());
    cfg.upstreams["A"].lanes = 2;
    cfg.slots.enabled = true;
    cfg.placement.admission = true;
    cfg.placement.lead_reserve = 1;
    cfg.placement.queue_seconds = 10;
    let r = start_router(cfg, None, Mode::Gateway).await;
    let c: Value = http()
        .post(format!("{}/slots/claim", r.url))
        .json(&json!({"key": "lead", "kind": "main", "copy": "A"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["ok"], true);
    let body = json!({"model": "qwen3.8-flash-next-a", "messages": [], "stream": true});
    // agent 1 takes the only non-reserved lane
    let url = r.url.clone();
    let b1 = body.clone();
    let t0 = Instant::now();
    let first = tokio::spawn(async move { chat(&url, b1).await.text().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(150)).await;
    // agent 2 queues behind it
    let url = r.url.clone();
    let b2 = body.clone();
    let second = tokio::spawn(async move {
        let t = Instant::now();
        chat(&url, b2).await.text().await.unwrap();
        t.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st: Value = http()
        .get(format!("{}/status", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["cards"]["A"]["queued"], 1, "{st}");
    // the Lead does not wait
    let tl = Instant::now();
    let resp = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .header("x-abhoth-slot-key", "lead")
        .json(&body)
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap();
    assert!(
        tl.elapsed() < Duration::from_millis(1000),
        "lead waited {:?}",
        tl.elapsed()
    );
    first.await.unwrap();
    let waited = second.await.unwrap();
    // agent 2 started only after agent 1 finished (~700 ms)
    assert!(
        waited >= Duration::from_millis(900),
        "agent 2 waited only {waited:?}"
    );
    assert!(t0.elapsed() >= Duration::from_millis(1300));
    let m = http()
        .get(format!("{}/metrics", r.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(m.contains("abhoth_router_queued_total 1"), "{m}");
    assert!(m.contains("abhoth_router_inflight{card=\"A\"} 0"));
}

#[tokio::test]
async fn placement_is_off_by_default() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    assert!(!cfg.placement.admission && !cfg.placement.affinity && !cfg.placement.lease_aware);
    assert!(!cfg.slots.enabled);
    let r = start_router(cfg, None, Mode::Gateway).await;
    // slots API off: 404
    let resp = http()
        .get(format!("{}/slots/list", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // plain balancing alternates over idle cards
    let mut seen = Vec::new();
    for _ in 0..4 {
        let resp = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []})).await;
        seen.push(
            resp.headers()["x-abhoth-card"]
                .to_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(
        seen.contains(&"A".to_string()) && seen.contains(&"B".to_string()),
        "{seen:?}"
    );
}

#[tokio::test]
async fn drain_stops_new_requests_and_waits() {
    let a = Fake::start(|_| {
        Reply::Sse(vec![
            (0, "data: {}\n\n".into()),
            (500, "data: [DONE]\n\n".into()),
        ])
    })
    .await;
    let b = Fake::ok("B").await;
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &b.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let url = r.url.clone();
    let inflight = tokio::spawn(async move {
        chat(
            &url,
            json!({"model": "qwen3.8-flash-next-a", "messages": [], "stream": true}),
        )
        .await
        .text()
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    let d: Value = http()
        .post(format!("{}/admin/cards/A/drain", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["drained"], true);
    assert!(t0.elapsed() >= Duration::from_millis(300));
    inflight.await.unwrap();
    // while draining, pool names go to B only
    for _ in 0..3 {
        let resp = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []})).await;
        assert_eq!(resp.headers()["x-abhoth-card"], "B");
    }
    http()
        .post(format!("{}/admin/cards/A/undrain", r.url))
        .send()
        .await
        .unwrap();
    let st: Value = http()
        .get(format!("{}/status", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["cards"]["A"]["draining"], false);
}
