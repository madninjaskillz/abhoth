//! Glance (SUPERVISOR.md 12): the SSE feed under a fast fake source (a soak: thousands of
//! patches rebuild the exact state, falling behind re-syncs from a snapshot, nothing leaks
//! when clients leave), the redacted public listener and its limits, and the admin-only card
//! actions relayed to a fake control plane. Nothing here talks to a real host.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::{get, post};
use common::{FakeSession, Host, TOKEN};
use futures_util::StreamExt;
use orca_sensors::feed::{FeedEvent, FeedMessage, Severity, TelemetryFeed, apply_merge_patch};
use orca_webui::glance::{GlanceOptions, GlanceSource, PublicOptions, load_fixture, restamp};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/glance-feed.jsonl")
}

/// A source that publishes its first frame and then waits (the tests publish the rest).
fn quiet_options() -> GlanceOptions {
    let mut o = GlanceOptions::new(GlanceSource::Fixture(fixture()));
    o.interval = Duration::from_secs(3600);
    o.flush = Duration::from_millis(50);
    o.ping = Duration::from_millis(200);
    o
}

struct Sse {
    stream: futures_util::stream::BoxStream<'static, reqwest::Result<Vec<u8>>>,
    buf: String,
}

#[derive(Debug)]
struct Ev {
    event: String,
    id: Option<u64>,
    data: String,
}

impl Sse {
    async fn open(r: reqwest::RequestBuilder) -> (StatusCode, Option<Self>) {
        let r = r.send().await.unwrap();
        let st = r.status();
        if !st.is_success() {
            return (st, None);
        }
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        (
            st,
            Some(Self {
                stream: r.bytes_stream().map(|c| c.map(|b| b.to_vec())).boxed(),
                buf: String::new(),
            }),
        )
    }

    async fn next(&mut self) -> Ev {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut ev = Ev {
                    event: "message".into(),
                    id: None,
                    data: String::new(),
                };
                let mut comment = true;
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        ev.event = v.trim().into();
                        comment = false;
                    } else if let Some(v) = line.strip_prefix("id:") {
                        ev.id = v.trim().parse().ok();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        if !ev.data.is_empty() {
                            ev.data.push('\n');
                        }
                        ev.data.push_str(v.strip_prefix(' ').unwrap_or(v));
                        comment = false;
                    }
                }
                if comment {
                    continue; // keep-alive
                }
                return ev;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.stream.next())
                .await
                .expect("the stream went quiet")
                .expect("the stream ended")
                .unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

async fn host_with(tweak: impl FnOnce(&mut orca_webui::WebUiOptions)) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::new(dir.path());
    let h = Host::start_with(session, tweak).await;
    std::mem::forget(dir);
    h
}

#[tokio::test(flavor = "multi_thread")]
async fn off_by_default() {
    let h = host_with(|_| {}).await;
    let info: Value = h
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["glance"], false);
    assert_eq!(
        h.api(Method::GET, "/api/glance/stream")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

fn msg(ev: &Ev) -> FeedMessage {
    let m: FeedMessage = serde_json::from_str(&ev.data).unwrap();
    assert_eq!(Some(m.seq()), ev.id, "the SSE id is the feed seq");
    m
}

#[tokio::test(flavor = "multi_thread")]
async fn soak_thousands_of_patches_rebuild_the_state_and_nothing_leaks() {
    let h = host_with(|o| o.glance = Some(quiet_options())).await;
    let glance = h.server.state().glance.clone().unwrap();
    let info: Value = h
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["glance"], true);
    for _ in 0..100 {
        if glance.full.snapshot().state.get("dash").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let base_refs = Arc::strong_count(&glance.full);
    let (st, sse) = Sse::open(h.api(Method::GET, "/api/glance/stream")).await;
    assert_eq!(st, StatusCode::OK);
    let mut sse = sse.unwrap();
    let FeedMessage::Snapshot(snap) = msg(&sse.next().await) else {
        panic!("snapshot first")
    };
    assert!(
        snap.state["history"]["hist"].is_object(),
        "the snapshot carries the seed history"
    );
    assert!(
        snap.state["gpus"]["A"]["core_c"].is_number(),
        "GPUs in the hub's GpuSample shape"
    );
    let mut state = snap.state;
    let mut last = snap.seq;

    // A publisher far faster than the 1 s production flush: varying values, engines that come
    // and go, and events between the patches.
    let frames = load_fixture(&fixture()).unwrap();
    const N: usize = 3000;
    let pubg = glance.clone();
    let publisher = tokio::spawn(async move {
        for i in 0..N {
            let mut f = restamp(&frames[i % frames.len()], 2.0e9 + i as f64);
            f["host"]["cpu"] = json!(i % 100);
            if i % 500 > 250
                && let Some(llm) = f["llm"].as_object_mut()
            {
                llm.insert(
                    "extra:1".into(),
                    json!({"key": "extra:1", "role": "extra", "tok_s": i}),
                );
            }
            pubg.publish_legacy(&f, false);
            pubg.flush();
            if i % 300 == 0 {
                pubg.event(FeedEvent {
                    kind: "switch.step".into(),
                    severity: Severity::Info,
                    text: format!("step {i}"),
                    data: Value::Null,
                });
            }
            if i % 50 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    let (mut patches, mut resyncs, mut events) = (0, 0, 0);
    let mut apply = |m: FeedMessage, state: &mut Value, last: &mut u64| match m {
        FeedMessage::Patch { seq, patch, .. } => {
            assert_eq!(seq, *last + 1, "messages arrive in order without gaps");
            apply_merge_patch(state, &patch);
            *last = seq;
            patches += 1;
        }
        FeedMessage::Event { seq, .. } => {
            assert_eq!(seq, *last + 1);
            *last = seq;
            events += 1;
        }
        FeedMessage::Snapshot(s) => {
            // Fell behind: the server starts it over from a snapshot.
            *state = s.state;
            *last = s.seq;
            resyncs += 1;
        }
    };
    while !publisher.is_finished() || last < glance.full.snapshot().seq {
        let ev = sse.next().await;
        if ev.event == "ping" {
            continue;
        }
        apply(msg(&ev), &mut state, &mut last);
    }
    publisher.await.unwrap();
    // Through JSON text as the client got it (serde_json may parse a float one ULP off).
    let hub_state: Value = serde_json::from_str(&glance.full.snapshot().state.to_string()).unwrap();
    assert!(
        state == hub_state,
        "patches rebuild the exact state; difference: {}",
        orca_sensors::feed::merge_patch_diff(&state, &hub_state).unwrap_or_default()
    );
    assert!(
        patches > 500,
        "{patches} patches, {events} events, {resyncs} resyncs"
    );
    assert!(events > 0 || resyncs > 0);
    // A quiet feed still pings.
    let mut pinged = false;
    for _ in 0..5 {
        if sse.next().await.event == "ping" {
            pinged = true;
            break;
        }
    }
    assert!(pinged);
    drop(sse);
    for _ in 0..100 {
        if Arc::strong_count(&glance.full) <= base_refs {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        Arc::strong_count(&glance.full) <= base_refs,
        "a closed client leaves nothing behind"
    );

    // Last-Event-ID resumes from the hub's replay.
    let seq = glance.full.snapshot().seq;
    glance.event(FeedEvent {
        kind: "alert.fired".into(),
        severity: Severity::Warning,
        text: "x".into(),
        data: Value::Null,
    });
    let (_, s2) = Sse::open(
        h.api(Method::GET, "/api/glance/stream")
            .header("Last-Event-ID", seq.to_string()),
    )
    .await;
    let ev = s2.unwrap().next().await;
    assert_eq!((ev.event.as_str(), ev.id), ("event", Some(seq + 1)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_public_view_is_redacted_limited_and_has_no_controls() {
    let client = tempfile::tempdir().unwrap();
    std::fs::write(
        client.path().join("glance.html"),
        "<!doctype html><title>Glance</title>",
    )
    .unwrap();
    std::fs::create_dir(client.path().join("assets")).unwrap();
    std::fs::write(client.path().join("assets/glance-abc.js"), "console.log(1)").unwrap();
    std::fs::write(client.path().join("index.html"), "the app").unwrap();
    let dir = client.path().to_path_buf();
    let h = host_with(|o| {
        let mut g = quiet_options();
        g.public = Some(PublicOptions {
            streams_per_ip: 2,
            ..PublicOptions::new("127.0.0.1:0")
        });
        o.glance = Some(g);
        o.client_dir = Some(dir);
    })
    .await;
    let glance = h.server.state().glance.clone().unwrap();
    let base = format!("http://{}", glance.public_addr().unwrap());
    let http = reqwest::Client::new();
    for _ in 0..100 {
        if glance
            .public
            .as_ref()
            .unwrap()
            .snapshot()
            .state
            .get("dash")
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let page = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers().get("set-cookie").is_none());
    assert!(
        page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    assert!(page.text().await.unwrap().contains("<title>Glance"));
    assert_eq!(
        http.get(format!("{base}/assets/glance-abc.js"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for (m, p) in [
        (Method::GET, "/index.html"),
        (Method::GET, "/api/info"),
        (Method::GET, "/api/glance/snapshot"),
        (Method::POST, "/api/glance/cards/A/offline"),
        (Method::GET, "/assets/../index.html"),
        (Method::GET, "/assets/x.html"),
    ] {
        let st = http
            .request(m.clone(), format!("{base}{p}"))
            .send()
            .await
            .unwrap()
            .status();
        assert!(
            st == StatusCode::NOT_FOUND || st == StatusCode::METHOD_NOT_ALLOWED,
            "{m} {p}: {st}"
        );
    }

    let snap = http
        .get(format!("{base}/public/glance/snapshot"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let full = glance.full.snapshot().state.to_string();
    for (word, why) in [
        ("\"requests\":[", "recent requests"),
        ("\"containers\"", "container list"),
        ("oom-victim", "OOM process"),
        ("last_error", "error text"),
        ("app-3", "container name"),
    ] {
        assert!(full.contains(word), "the fixture has {why}");
        assert!(!snap.contains(word), "the public view leaks {why}");
    }
    assert!(snap.contains("\"redacted\""));

    // The stream is redacted too, and limited per address.
    let (st, s1) = Sse::open(http.get(format!("{base}/public/glance/stream"))).await;
    assert_eq!(st, StatusCode::OK);
    let mut s1 = s1.unwrap();
    let ev = s1.next().await;
    assert_eq!(ev.event, "snapshot");
    assert!(!ev.data.contains("oom-victim"));
    let (_, s2) = Sse::open(http.get(format!("{base}/public/glance/stream"))).await;
    let (st3, _) = Sse::open(http.get(format!("{base}/public/glance/stream"))).await;
    assert_eq!(st3, StatusCode::TOO_MANY_REQUESTS);
    drop(s2);
    let mut freed = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (st, s) = Sse::open(http.get(format!("{base}/public/glance/stream"))).await;
        if st == StatusCode::OK {
            freed = true;
            drop(s);
            break;
        }
    }
    assert!(freed, "a closed stream frees its slot");
    glance.publish_legacy(
        &{
            let mut f = restamp(&load_fixture(&fixture()).unwrap()[3], 2.1e9);
            f["errors"] = json!({"collector": "a private error"});
            f
        },
        false,
    );
    glance.flush();
    loop {
        let ev = s1.next().await;
        if ev.event == "patch" {
            assert!(!ev.data.contains("private error"), "{}", ev.data);
            assert!(!ev.data.contains("oom-victim"));
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn card_actions_are_admin_only_and_relayed_to_the_control_plane() {
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let s2 = seen.clone();
    let control = axum::Router::new()
        .route(
            "/cards/{card}/{action}",
            post(
                move |axum::extract::Path((c, a)): axum::extract::Path<(String, String)>,
                      axum::Json(b): axum::Json<Value>| {
                    let seen = s2.clone();
                    async move {
                        seen.lock().unwrap().push((format!("{c}/{a}"), b));
                        (StatusCode::ACCEPTED, axum::Json(json!({"id": "job-1"})))
                    }
                },
            ),
        )
        .route(
            "/jobs/{id}",
            get(|| async {
                axum::Json(json!({"state": "done", "steps": [{"msg": "card A offline"}]}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, control).await.unwrap() });

    let h = host_with(|o| {
        let mut g = quiet_options();
        g.control_url = Some(format!("http://{addr}/"));
        o.glance = Some(g);
    })
    .await;

    // The admin (the launch token stands for them).
    let r = h
        .api(Method::POST, "/api/glance/cards/A/offline")
        .json(&json!({"reason": "test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], "job-1");
    let (what, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(what, "A/offline");
    assert_eq!(body["requestedBy"], "glance:admin");
    let j: Value = h
        .api(Method::GET, "/api/glance/jobs/job-1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(j["state"], "done");
    for bad in [
        "/api/glance/cards/A/explode",
        "/api/glance/cards/A%2F..%2Fx/offline",
    ] {
        let st = h.api(Method::POST, bad).send().await.unwrap().status();
        assert!(
            st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND,
            "{bad}: {st}"
        );
    }

    // A signed-in user who is not an admin reads the feed but can't press the buttons.
    let r = h
        .api(Method::POST, "/api/admin/users")
        .json(&json!({"username": "bob", "password": "user password 123", "admin": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
    let http = reqwest::Client::new();
    let login = http
        .post(h.url("/api/auth/login"))
        .json(&json!({"username": "bob", "password": "user password 123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = login.json::<Value>().await.unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = Sse::open(
        http.get(h.url("/api/glance/stream"))
            .header("Cookie", &cookie),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let st = http
        .post(h.url("/api/glance/cards/A/online"))
        .header("Cookie", &cookie)
        .header("X-OpenOrca-CSRF", &csrf)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing reached the control plane"
    );
    // Signed out: nothing at all.
    let st = reqwest::get(h.url("/api/glance/snapshot"))
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let _ = TOKEN;
}
