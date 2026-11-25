//! Phase 1: the parity gateway against fake engines.

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt;
use orca_router::import::{ImportOptions, from_litellm};
use orca_router::server::Mode;
use serde_json::{Value, json};

#[tokio::test]
async fn models_match_litellm_golden() {
    let cfg = from_litellm(
        include_str!("fixtures/litellm-config.yaml"),
        include_str!("fixtures/nginx.conf"),
        &ImportOptions::default(),
    )
    .unwrap();
    let r = start_router(cfg, None, Mode::Gateway).await;
    let got = http()
        .get(format!("{}/v1/models", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let text = got.text().await.unwrap();
    let golden: Value = serde_json::from_str(include_str!("fixtures/litellm-models.json")).unwrap();
    // byte-identical, key order included
    assert_eq!(text, golden.to_string());
    // OpenOrca's ServerIdentity sees a gateway
    assert!(orca_core::client::server_identity::is_gateway_list(&text));

    let info: Value = http()
        .get(format!("{}/v1/model/info", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/litellm-model-info.trimmed.json")).unwrap();
    let (g, o) = (
        golden["data"].as_array().unwrap(),
        info["data"].as_array().unwrap(),
    );
    assert_eq!(g.len(), o.len());
    for (a, b) in g.iter().zip(o) {
        assert_eq!(a["model_name"], b["model_name"]);
        assert_eq!(a["litellm_params"]["model"], b["litellm_params"]["model"]);
        let (ma, mb) = (
            a["model_info"].as_object().unwrap(),
            b["model_info"].as_object().unwrap(),
        );
        for (k, v) in ma {
            if k == "id" {
                continue;
            }
            assert_eq!(Some(v), mb.get(k), "{} model_info.{k}", a["model_name"]);
        }
        assert_eq!(mb.len(), ma.len(), "{}: same keys", a["model_name"]);
    }
    assert!(!info.to_string().contains("api_key"));
}

#[tokio::test]
async fn health_endpoints_send_nothing_upstream() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &b.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let live = http()
        .get(format!("{}/health/liveliness", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), 200);
    assert_eq!(live.text().await.unwrap(), "\"I'm alive!\"");
    let h: Value = http()
        .get(format!("{}/health", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h, json!({"status": "ok", "service": "abhoth-router"}));
    for p in ["/api/v0/models", "/api/show", "/v1/embeddings"] {
        let r = http().get(format!("{}{p}", r.url)).send().await.unwrap();
        assert_eq!(r.status(), 404, "{p}");
    }
    assert!(a.seen().is_empty() && b.seen().is_empty());
}

#[tokio::test]
async fn alias_is_rewritten_and_keyed() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &b.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .header("authorization", "Bearer whatever-the-client-has")
        .json(
            &json!({"model": "jarvis-brain", "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.7, "max_tokens": 400, "response_format": {"type": "json_object"},
            "chat_template_kwargs": {"enable_thinking": false}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-litellm-model-name"],
        "openai/inkling-2-mini"
    );
    assert_eq!(resp.headers()["x-served-model"], "inkling-2-mini");
    let card = resp.headers()["x-abhoth-card"]
        .to_str()
        .unwrap()
        .to_string();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], card.as_str());
    let seen = if card == "A" { a.chats() } else { b.chats() };
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s.path, "/v1/chat/completions");
    assert_eq!(s.headers["authorization"], "Bearer test-engine-key");
    assert_eq!(
        s.body,
        json!({"model": "inkling-2-mini", "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.7, "max_tokens": 400, "chat_template_kwargs": {"enable_thinking": false},
            "reasoning_effort": "none"})
    );
}

#[tokio::test]
async fn effort_is_clamped_per_deployment() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &b.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-b", "messages": [], "reasoning_effort": "high",
        "chat_template_kwargs": {"preserve_thinking": true}}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s = &b.chats()[0];
    assert_eq!(s.body["reasoning_effort"], "medium");
    assert_eq!(
        s.body["chat_template_kwargs"],
        json!({"preserve_thinking": true})
    );
    assert!(a.chats().is_empty());
}

#[tokio::test]
async fn streaming_passes_through_without_buffering() {
    let a = Fake::start(|_| {
        Reply::Sse(vec![
            (0, "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n".into()),
            (600, "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n".into()),
            (0, "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n".into()),
            (0, "data: [DONE]\n\n".into()),
        ])
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = two_cards(&a.url(), &dead_url().await, None, dir.path());
    let log = dir.path().join("requests.jsonl");
    cfg.settings.request_log = Some(log.to_string_lossy().to_string());
    let r = start_router(cfg, None, Mode::Gateway).await;
    let t0 = Instant::now();
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-a", "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut s = resp.bytes_stream();
    let first = s.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("one"));
    assert!(
        t0.elapsed() < Duration::from_millis(450),
        "first chunk took {:?}",
        t0.elapsed()
    );
    let mut rest = Vec::new();
    while let Some(c) = s.next().await {
        rest.extend_from_slice(&c.unwrap());
    }
    let rest = String::from_utf8_lossy(&rest);
    assert!(rest.contains("two") && rest.ends_with("data: [DONE]\n\n"));
    assert!(t0.elapsed() >= Duration::from_millis(550));
    // the log line: card, ttft, usage from the stream
    tokio::time::sleep(Duration::from_millis(100)).await;
    let line: Value = serde_json::from_str(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(line["card"], "A");
    assert_eq!(line["status"], 200);
    assert_eq!(line["usage"]["prompt_tokens"], 7);
    assert!(line["ttftSeconds"].as_f64().unwrap() < 0.45);
    assert!(line.get("end").is_none());
    assert_eq!(r.st.inflight("A"), 0);
}

#[tokio::test]
async fn connect_failure_fails_over_once_and_cools() {
    let b = Fake::ok("B").await;
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&dead_url().await, &b.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    for _ in 0..4 {
        let resp = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []})).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["x-abhoth-card"], "B");
    }
    assert_eq!(b.chats().len(), 4);
    let cards = r.st.cards();
    // A failed once, then was skipped while cooling
    assert_eq!(cards["A"].failures, 1);
}

#[tokio::test]
async fn http_503_fails_over_but_429_and_4xx_pass_through() {
    let a = Fake::start(|s| match s.body["messages"][0]["content"].as_str() {
        Some("busy") => Reply::Json(429, json!({"error": {"message": "inference request queue is full", "code": 429}})),
        Some("bad") => Reply::Json(400, json!({"error": {"message": "unknown field", "code": "speed_projection_not_supported"}})),
        _ => Reply::Json(503, json!({"error": "loading"})),
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
    // pinned to A: 429 comes back as is, with its Retry-After, no second try
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-a", "messages": [{"role": "user", "content": "busy"}]}),
    )
    .await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "3");
    assert!(resp.text().await.unwrap().contains("queue is full"));
    // a 400 passes through unchanged (the uncensored safety net reads its code)
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-a", "messages": [{"role": "user", "content": "bad"}]}),
    )
    .await;
    assert_eq!(resp.status(), 400);
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("speed_projection_not_supported")
    );
    assert_eq!(a.chats().len(), 2);
    // a pool name: A's 503 fails over to B
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-27b", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-abhoth-card"], "B");
    // a pinned name whose only card answers 503: the 503 comes back
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-a", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn unknown_model_and_bad_json_are_litellm_shaped() {
    let a = Fake::ok("A").await;
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &a.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = chat(&r.url, json!({"model": "no-such-model", "messages": []})).await;
    assert_eq!(resp.status(), 400);
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["error"]["type"], "invalid_request_error");
    assert_eq!(e["error"]["code"], "400");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid model name passed in model=no-such-model")
    );
    let resp = http()
        .post(format!("{}/v1/chat/completions", r.url))
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["error"]["param"], "request_body");
    assert!(a.seen().is_empty());
}

#[tokio::test]
async fn both_cards_down_is_503_with_retry_after() {
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&dead_url().await, &dead_url().await, None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = chat(&r.url, json!({"model": "qwen3.8-27b", "messages": []})).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "10");
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["error"]["type"], "no_upstream");
}

#[tokio::test]
async fn client_disconnect_cancels_upstream() {
    let a = Fake::start(|_| Reply::Endless).await;
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &a.url(), None, dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-flash-next-a", "messages": [], "stream": true}),
    )
    .await;
    let mut s = resp.bytes_stream();
    s.next().await.unwrap().unwrap();
    assert_eq!(*a.open_streams.lock().unwrap(), 1);
    assert_eq!(r.st.inflight("A"), 1);
    drop(s);
    let t0 = Instant::now();
    while (*a.open_streams.lock().unwrap() != 0 || r.st.inflight("A") != 0)
        && t0.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        *a.open_streams.lock().unwrap(),
        0,
        "upstream stream still open"
    );
    assert_eq!(r.st.inflight("A"), 0);
}

#[tokio::test]
async fn relay_names_go_to_the_relay() {
    let relay = Fake::ok("relay").await;
    let a = Fake::ok("A").await;
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        two_cards(&a.url(), &a.url(), Some(&relay.url()), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let resp = chat(&r.url, json!({"model": "flash-next", "messages": []})).await;
    assert_eq!(resp.status(), 200);
    let s = &relay.chats()[0];
    assert_eq!(s.path, "/ondemand/v1/chat/completions");
    assert_eq!(s.headers["authorization"], "Bearer ondemand-relay");
    assert_eq!(s.body["model"], "flash-next");
    assert_eq!(s.body["reasoning_effort"], "low");
    // /v1/models: relay names have no max_output_tokens (as LiteLLM)
    let m: Value = http()
        .get(format!("{}/v1/models", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fl = m["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "flash-next")
        .unwrap();
    assert!(fl.get("max_output_tokens").is_none());
}

#[tokio::test]
async fn qfn_lb_paths() {
    let (a, b) = (Fake::ok("A").await, Fake::ok("B").await);
    let dir = tempfile::tempdir().unwrap();
    let cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    let r = start_router(cfg.clone(), None, Mode::Gateway).await;
    let resp = http()
        .get(format!("{}/b/slots", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(b.seen()[0].path, "/slots");
    assert_eq!(
        b.seen()[0].headers["authorization"],
        "Bearer test-engine-key"
    );
    let resp = http()
        .post(format!("{}/a/v1/chat/completions?x=1", r.url))
        .json(&json!({"model": "anything", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let s = a.seen().last().cloned().unwrap();
    assert_eq!(s.path, "/v1/chat/completions?x=1");
    // no rewriting on the qfn-lb paths
    assert_eq!(s.body["model"], "anything");
    let resp = http()
        .get(format!("{}/ninfer/health", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // the compat listener: every path, "/" = the main pool
    let lb = start_router(cfg, None, Mode::Lb).await;
    let resp = http()
        .get(format!("{}/health", lb.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = http()
        .get(format!("{}/a/v1/models", lb.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(a.seen().iter().any(|s| s.path == "/v1/models"));
}

#[tokio::test]
async fn hot_reload_is_atomic_and_keeps_a_good_config() {
    let a = Fake::ok("A").await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = two_cards(&a.url(), &a.url(), None, dir.path());
    let path = dir.path().join("router.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let r = start_router(cfg.clone(), Some(path.clone()), Mode::Gateway).await;
    tokio::spawn(orca_router::reload::watch(
        r.st.clone(),
        Duration::from_millis(100),
    ));
    let rev = |url: String| async move {
        let v: Value = http()
            .get(format!("{url}/v1/model/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["data"][0]["model_info"]["gateway_rev"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(rev(r.url.clone()).await, "rev1");
    let mut c2 = cfg.clone();
    c2.rev = "rev2".into();
    c2.deployments.retain(|d| d.name != "jarvis-brain");
    std::fs::write(&path, serde_json::to_string_pretty(&c2).unwrap()).unwrap();
    let t0 = Instant::now();
    while rev(r.url.clone()).await != "rev2" && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rev(r.url.clone()).await, "rev2");
    let resp = chat(&r.url, json!({"model": "jarvis-brain", "messages": []})).await;
    assert_eq!(resp.status(), 400);
    // a broken file keeps rev2 and shows the error
    std::fs::write(&path, "{ half written").unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rev(r.url.clone()).await, "rev2");
    let st: Value = http()
        .get(format!("{}/status", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(st["configError"].as_str().is_some());
    // POST /admin/reload from loopback
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let resp = http()
        .post(format!("{}/admin/reload", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(rev(r.url.clone()).await, "rev1");
}

#[tokio::test]
async fn in_flight_request_keeps_its_snapshot_across_reload() {
    let a = Fake::start(|_| {
        Reply::Sse(vec![
            (0, "data: {\"x\":1}\n\n".into()),
            (400, "data: [DONE]\n\n".into()),
        ])
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = two_cards(&a.url(), &a.url(), None, dir.path());
    let path = dir.path().join("router.json");
    std::fs::write(&path, serde_json::to_string(&cfg).unwrap()).unwrap();
    let r = start_router(cfg.clone(), Some(path.clone()), Mode::Gateway).await;
    let resp = chat(
        &r.url,
        json!({"model": "qwen3.8-27b", "messages": [], "stream": true}),
    )
    .await;
    let mut c2 = cfg;
    c2.rev = "rev2".into();
    c2.deployments.clear();
    std::fs::write(&path, serde_json::to_string(&c2).unwrap()).unwrap();
    orca_router::reload::reload_now(&r.st).unwrap();
    let text = resp.text().await.unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"));
}
