//! A health check or a model listing can never switch a GPU (ROUTER-CONFIG.md 7.4; the
//! incident of ROUTER.md 1.1, where LiteLLM's `GET /health` started a real switch).
//!
//! On-demand is on (not a dry run), anonymous callers are allowed (the worst case), and the
//! fake profiles API records every call. Every probe-like path, with every method and a chat
//! body naming the cold model, on every listener (gateway, qfn-lb, pig-slots), must leave the
//! profiles API untouched: not even a read, because only a chat admission reaches `ensure`.
//! A positive control shows the same router does switch for a real chat, and that
//! `x-abhoth-no-switch: 1` and a missing key stop it.

mod common;

use common::*;
use orca_router::config::{OnDemandModel, RouterConfig};
use orca_router::server::Mode;
use serde_json::json;

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

fn cfg(a: &str, b: &str, relay: &str, api: &str, dir: &std::path::Path) -> RouterConfig {
    let mut cfg = two_cards(a, b, Some(relay), dir);
    for u in cfg.upstreams.values_mut() {
        u.catalog_model = Some("inkling2-mini".into());
    }
    let od = &mut cfg.on_demand;
    od.enabled = true;
    od.dry_run = false;
    od.profiles_api = api.to_string();
    od.guards_file = Some(fresh_guards(dir));
    od.poll_slots = false;
    od.min_stay_seconds = 0;
    od.idle_grace_seconds = 0;
    od.queue_seconds = 2;
    od.switch_timeout_seconds = 2;
    od.models.insert(
        "flash-next".into(),
        OnDemandModel {
            model: "flash-cat".into(),
            engine: "ninfer".into(),
            engine_profile: "ninfer-x".into(),
            served_id: String::new(),
        },
    );
    cfg.slots.enabled = true;
    cfg
}

fn profiles_api() -> impl Fn(&Seen) -> Reply + Send + Sync + 'static {
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

const PATHS: &[&str] = &[
    "/health",
    "/health/liveliness",
    "/health/liveness",
    "/health/readiness",
    "/v1/models",
    "/models",
    "/v1/models/flash-next",
    "/v1/model/info",
    "/model/info",
    "/status",
    "/metrics",
    "/observe",
    "/ondemand",
    "/",
    "/list",
    "/claim",
    "/slots/list",
    "/slots/claim",
    "/a/health",
    "/b/v1/models",
    "/a/v1/chat/completions",
    "/ninfer/health",
    "/ninfer/v1/chat/completions",
    "/admin/status",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/chat/completions/x",
];

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"];

#[tokio::test]
async fn no_probe_on_any_listener_can_switch() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let relay = Fake::ok("relay").await;
    let api = Fake::start(profiles_api()).await;
    let dir = tempfile::tempdir().unwrap();
    let c = http();
    let body = json!({"model": "flash-next", "messages": [{"role": "user", "content": "hi"}]});
    let mut sent = 0;
    for mode in [Mode::Gateway, Mode::Lb, Mode::Slots] {
        let r = start_router(
            cfg(&a.url(), &b.url(), &relay.url(), &api.url(), dir.path()),
            None,
            mode,
        )
        .await;
        for p in PATHS {
            for m in METHODS {
                let method = reqwest::Method::from_bytes(m.as_bytes()).unwrap();
                let _ = c
                    .request(method, format!("{}{p}", r.url))
                    .json(&body)
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await;
                sent += 1;
            }
        }
        // a chat body on the non-gateway listeners is passed to a card or refused, never decided
        if mode != Mode::Gateway {
            let _ = c
                .post(format!("{}/v1/chat/completions", r.url))
                .json(&body)
                .send()
                .await;
        }
        assert_eq!(
            r.st.ext.ondemand.limiter.count(),
            0,
            "{mode:?}: a switch was started"
        );
    }
    assert!(sent > 500);
    assert!(
        api.seen().is_empty(),
        "the profiles API was called: {:?}",
        api.seen()
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect::<Vec<_>>()
    );
    assert!(relay.seen().is_empty());
}

async fn real_chat(r: &Router_, headers: &[(&str, &str)]) -> u16 {
    let mut rb = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .json(&json!({"model": "flash-next", "messages": [{"role": "user", "content": "hi"}]}));
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    rb.send().await.unwrap().status().as_u16()
}

fn switches(api: &Fake) -> usize {
    api.seen()
        .iter()
        .filter(|s| s.method == "POST" && s.path == "/switch")
        .count()
}

#[tokio::test]
async fn a_real_chat_switches_unless_forbidden() {
    if orca_router::ondemand::quiet(orca_router::proxy::now_epoch()).is_some() {
        return; // 06:45-07:30 UK: nothing may switch, the positive control cannot run
    }
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let relay = Fake::ok("relay").await;
    let dir = tempfile::tempdir().unwrap();

    // a real chat: one switch asked for
    let api = Fake::start(profiles_api()).await;
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), &api.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    real_chat(&r, &[]).await;
    assert_eq!(switches(&api), 1);

    // x-abhoth-no-switch: 1 (another router, the canary, an internal caller): none
    let api = Fake::start(profiles_api()).await;
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), &api.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    assert_eq!(real_chat(&r, &[("x-abhoth-no-switch", "1")]).await, 503);
    assert_eq!(switches(&api), 0);

    // keys required (the control plane's setting): an anonymous chat never switches
    let api = Fake::start(profiles_api()).await;
    let mut c = cfg(&a.url(), &b.url(), &relay.url(), &api.url(), dir.path());
    c.on_demand.require_key = true;
    let r = start_router(c, None, Mode::Gateway).await;
    assert_eq!(real_chat(&r, &[]).await, 503);
    assert_eq!(switches(&api), 0);

    // an excluded card is never picked: with both cards excluded, nothing switches
    let api = Fake::start(profiles_api()).await;
    let mut c = cfg(&a.url(), &b.url(), &relay.url(), &api.url(), dir.path());
    c.on_demand.exclude_cards = vec!["A".into(), "B".into()];
    let r = start_router(c, None, Mode::Gateway).await;
    assert_eq!(real_chat(&r, &[]).await, 503);
    assert_eq!(switches(&api), 0);
}
