//! Phase 3: built-in on-demand switching against a fake profiles API.

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt;
use orca_router::config::{OnDemandModel, RouterConfig, Snapshot};
use orca_router::server::Mode;
use serde_json::{Value, json};

fn fresh_guards(dir: &std::path::Path) -> String {
    let p = dir.join("guards.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    std::fs::write(
        &p,
        json!({"at": now, "video": null, "bench": null, "music": false}).to_string(),
    )
    .unwrap();
    p.to_string_lossy().to_string()
}

fn ondemand_cfg(
    a: &str,
    b: &str,
    relay: &str,
    api: &str,
    dir: &std::path::Path,
    dry: bool,
) -> RouterConfig {
    let mut cfg = two_cards(a, b, Some(relay), dir);
    for u in cfg.upstreams.values_mut() {
        u.catalog_model = Some("inkling2-mini".into());
    }
    let od = &mut cfg.on_demand;
    od.enabled = true;
    od.dry_run = dry;
    od.profiles_api = api.to_string();
    od.guards_file = Some(fresh_guards(dir));
    od.poll_slots = false;
    od.min_stay_seconds = 0;
    od.idle_grace_seconds = 0;
    od.models.insert(
        "flash-next".into(),
        OnDemandModel {
            model: "flash-cat".into(),
            engine: "ninfer".into(),
            engine_profile: "ninfer-x".into(),
            served_id: String::new(),
        },
    );
    cfg.slots.enabled = true; // leases from the router's own registry (none held)
    cfg
}

fn profiles_api_fake() -> impl Fn(&Seen) -> Reply + Send + Sync + 'static {
    |s: &Seen| {
        let p = s.path.split('?').next().unwrap_or("");
        match (s.method.as_str(), p) {
            ("GET", "/live") => Reply::Json(200, json!({"layout": "L", "job": null})),
            ("GET", "/profiles") => Reply::Json(
                200,
                json!({"layouts": {"L": {"picks": {
                "A": {"engine": "ninfer", "engineProfile": "p", "model": "inkling2-mini"},
                "B": {"engine": "ninfer", "engineProfile": "p", "model": "inkling2-mini"}}}}}),
            ),
            ("POST", "/compose") => Reply::Json(200, json!({"layout": "combo-test"})),
            ("GET", "/plan") => Reply::Json(200, json!({"changes": {"B": {}}})),
            ("POST", "/switch") => Reply::Json(202, json!({"id": "job1", "estimateSeconds": 5})),
            ("GET", "/jobs/job1") => Reply::Json(200, json!({"state": "running"})),
            _ => Reply::Json(404, json!({"error": "?"})),
        }
    }
}

#[tokio::test]
async fn dry_run_decides_but_never_switches() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let relay = Fake::ok("relay").await;
    let api = Fake::start(profiles_api_fake()).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ondemand_cfg(
        &a.url(),
        &b.url(),
        &relay.url(),
        &api.url(),
        dir.path(),
        true,
    );
    let r = start_router(cfg, None, Mode::Gateway).await;
    let resp = chat(&r.url, json!({"model": "flash-next", "messages": []})).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    let e: Value = resp.json().await.unwrap();
    let msg = e["error"]["message"].as_str().unwrap();
    assert!(msg.contains("dry run: would switch card B"), "{msg}");
    // nothing was asked of the profiles API beyond reads, nothing went to the relay or the cards
    assert!(
        api.seen().iter().all(|s| s.method == "GET"),
        "{:?}",
        api.seen()
            .iter()
            .map(|s| s.path.clone())
            .collect::<Vec<_>>()
    );
    assert!(relay.seen().is_empty() && a.chats().is_empty() && b.chats().is_empty());
    let v: Value = http()
        .get(format!("{}/ondemand", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["dryRun"], true);
    assert_eq!(v["recent"][0]["action"], "block");
    // a live catalog name still goes straight to its card
    let resp = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []})).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn quiet_or_video_blocks_with_retry_after() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let relay = Fake::ok("relay").await;
    let api = Fake::start(profiles_api_fake()).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ondemand_cfg(
        &a.url(),
        &b.url(),
        &relay.url(),
        &api.url(),
        dir.path(),
        false,
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    std::fs::write(
        cfg.on_demand.guards_file.as_ref().unwrap(),
        json!({"at": now, "video": "ai-news"}).to_string(),
    )
    .unwrap();
    let r = start_router(cfg, None, Mode::Gateway).await;
    let resp = chat(&r.url, json!({"model": "flash-next", "messages": []})).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "300");
    assert!(resp.text().await.unwrap().contains("video pipeline"));
    assert!(api.seen().iter().all(|s| s.method == "GET"));
}

#[tokio::test]
async fn switch_then_stream_from_the_new_card() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let relay = Fake::ok("relay").await;
    let api = Fake::start(profiles_api_fake()).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ondemand_cfg(
        &a.url(),
        &b.url(),
        &relay.url(),
        &api.url(),
        dir.path(),
        false,
    );
    let r = start_router(cfg.clone(), None, Mode::Gateway).await;
    let resp = chat(
        &r.url,
        json!({"model": "flash-next", "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut s = resp.bytes_stream();
    let first = String::from_utf8_lossy(&s.next().await.unwrap().unwrap()).to_string();
    assert!(
        first.contains("reasoning_content") && first.contains("Switching card B to flash-next"),
        "{first}"
    );
    // the profiles API got the composed layout and the switch request
    let posts: Vec<Seen> = api
        .seen()
        .into_iter()
        .filter(|s| s.method == "POST")
        .collect();
    assert_eq!(posts[0].path, "/compose");
    assert_eq!(
        posts[0].body["cards"]["B"],
        json!({"engine": "ninfer", "engineProfile": "ninfer-x", "model": "flash-cat"})
    );
    assert_eq!(posts[0].body["cards"]["A"]["model"], "inkling2-mini");
    assert_eq!(posts[1].path, "/switch");
    assert_eq!(posts[1].body["layout"], "combo-test");
    // the runner switched card B: the next router.json says so
    let mut c2 = cfg.clone();
    c2.upstreams["B"].catalog_model = Some("flash-cat".into());
    c2.upstreams["B"].served_id = "flash-served".into();
    r.st.swap(Snapshot::build(c2, None).unwrap());
    let t0 = Instant::now();
    let mut rest = String::new();
    while let Some(c) = tokio::time::timeout(Duration::from_secs(10), s.next())
        .await
        .unwrap()
    {
        rest.push_str(&String::from_utf8_lossy(&c.unwrap()));
    }
    assert!(t0.elapsed() < Duration::from_secs(9));
    assert!(rest.contains("flash-next is live on card B"), "{rest}");
    assert!(
        rest.contains("\"content\":\"B\"") && rest.ends_with("data: [DONE]\n\n"),
        "{rest}"
    );
    // forwarded to card B with its served id; only one switch for the target
    assert_eq!(b.chats().last().unwrap().body["model"], "flash-served");
    assert_eq!(api.seen().iter().filter(|s| s.path == "/switch").count(), 1);
    assert!(relay.seen().is_empty());
}
