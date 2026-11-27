//! The overlay, routing, and a real bridge against the router's own `/bridge/ws`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use orca_bridge::client::{self, Timing};
use orca_bridge::config::BridgeConfig;
use orca_bridge::proto::Descriptor;
use serde_json::json;

use super::*;
use crate::config::{RouterConfig, Snapshot};
use crate::state::AppState;

fn base() -> ControlConfig {
    toml::from_str(
        r#"
        version = 1
        [servers.deepthought]
        host = "127.0.0.1"
        [cards.X1]
        server = "deepthought"
        gpu_uuid = "GPU-x1"
        vram_gib = 24.0
        [engines.kokoro]
        driver = "external"
        port = { X1 = 8880 }
        "#,
    )
    .unwrap()
}

fn descriptor(id: &str) -> Descriptor {
    serde_json::from_value(json!({ "v": 1,
      "server": { "id": id, "title": "nas.local", "os": "linux", "arch": "amd64", "model_dir": "/models" },
      "cards": [
        { "id": "GPU0", "device": "gpu", "gpu_uuid": "GPU-m4000", "label": "Quadro M4000", "vram_gib": 8.0,
          "engines": { "tts": ["chatterbox", "kokoro"], "stt": ["whisper"] }, "shared": true },
        { "id": "CPU", "device": "cpu", "cores": 4, "ram_gib": 6.0, "engines": { "tts": ["kokoro"] } } ],
      "engines": [
        { "id": "kokoro", "kinds": ["tts"], "ports": { "GPU0": 18880, "CPU": 18890 }, "lifecycle": "on_demand" },
        { "id": "chatterbox", "kinds": ["tts"], "caps": { "inherits": "chatterbox", "bogus": 1 },
          "ports": { "GPU0": 10204 }, "lifecycle": "on_demand" } ] }))
    .unwrap()
}

fn hub_in(dir: &std::path::Path) -> Arc<Hub> {
    Hub::new(dir, Arc::new(base()))
}

#[test]
fn the_overlay_adds_the_bridge_server_cards_and_engines() {
    let d = tempfile::tempdir().unwrap();
    let hub = hub_in(d.path());
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("nas", descriptor("nas"), "0.16.18", "10.0.0.2", Some(mux));
    let c = effective_with(&hub, &base());
    let s = &c.servers["nas"];
    assert_eq!(s.exec, ExecKind::Bridge);
    assert_eq!(s.origin.as_deref(), Some("bridge:nas"));
    assert_eq!(s.model_dir.as_deref(), Some("/models"));
    let g = &c.cards["NAS-GPU0"];
    assert!(g.managed && g.on_demand);
    assert_eq!(g.server, "nas");
    assert_eq!(g.reserve_gib, 2.0, "a shared card keeps more back");
    assert_eq!(g.engines.as_ref().unwrap()["stt"], ["whisper"]);
    assert!(c.cards["NAS-CPU"].is_cpu());
    // kokoro is in router.toml: the bridge's cards join its port map
    let k = &c.engines["kokoro"].port;
    assert_eq!(k.get("X1"), Some(&8880));
    assert_eq!(k.get("NAS-GPU0"), Some(&18880));
    assert_eq!(k.get("NAS-CPU"), Some(&18890));
    // chatterbox is new: an external engine; unknown caps keys fall back to its kinds
    let cb = &c.engines["chatterbox"];
    assert_eq!(cb.driver, schema::DriverKind::External);
    assert_eq!(cb.port.get("NAS-GPU0"), Some(&10204));
    assert!(cb.caps.is_some());
    // router.toml is untouched and the merged config is still valid
    assert!(!base().servers.contains_key("nas"));
    let issues = crate::control::validate::validate(&c);
    assert!(
        !issues.iter().any(|i| i.to_string().contains("servers.nas")),
        "{issues:?}"
    );
    // the bridge's server never serialises with its origin
    let text = toml::to_string(&c).unwrap();
    assert!(!text.contains("origin"));
    // the admin's inventory view lists it, marked as the bridge's
    let v = crate::control::inventory::view(&c);
    let nas = v["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "nas")
        .expect("nas in the inventory view");
    assert_eq!(nas["exec"], "bridge");
    assert_eq!(nas["origin"], "bridge:nas");
}

#[test]
fn offline_or_disabled_bridges_are_listed_but_not_placed_on() {
    let d = tempfile::tempdir().unwrap();
    let hub = hub_in(d.path());
    let (mux, _out) = orca_bridge::mux::Mux::new();
    let conn_no = hub.upsert("nas", descriptor("nas"), "v", "", Some(mux));
    hub.offline("nas", conn_no + 99); // a stale connection's end changes nothing
    assert!(hub.online("nas"));
    hub.offline("nas", conn_no);
    let c = effective_with(&hub, &base());
    assert!(!c.cards["NAS-GPU0"].managed);
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("nas", descriptor("nas"), "v", "", Some(mux));
    hub.set_settings(
        "nas",
        orca_bridge::proto::Settings {
            enabled: false,
            priority: 5,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!effective_with(&hub, &base()).cards["NAS-GPU0"].managed);
    // settings survive a restart
    let hub2 = hub_in(d.path());
    assert_eq!(hub2.settings("nas").priority, 5);
}

#[test]
fn clashing_ids_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let hub = hub_in(d.path());
    let r = hub.refuse("deepthought", &descriptor("deepthought"));
    assert!(r.iter().any(|x| x.contains("already a server")), "{r:?}");
    let r = hub.refuse("nas", &descriptor("other"));
    assert!(r.iter().any(|x| x.contains("not the bridge's id")), "{r:?}");
    // a refused bridge never reaches the overlay
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("deepthought", descriptor("deepthought"), "v", "", Some(mux));
    let c = effective_with(&hub, &base());
    assert_eq!(c.servers["deepthought"].exec, ExecKind::Local);
}

#[test]
fn exec_bridge_in_router_toml_is_an_error() {
    let mut c = base();
    c.servers.get_mut("deepthought").unwrap().exec = ExecKind::Bridge;
    let issues = crate::control::validate::validate(&c);
    assert!(
        issues
            .iter()
            .any(|i| i.to_string().contains("exec = \"bridge\"")),
        "{issues:?}"
    );
}

#[test]
fn upstreams_are_direct_or_through_the_bridge() {
    let d = tempfile::tempdir().unwrap();
    let hub = hub_in(d.path());
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("nas", descriptor("nas"), "v", "", Some(mux));
    let c = effective_with(&hub, &base());
    assert_eq!(
        upstream_for(&c, "deepthought", "kokoro", "X1"),
        Some(Upstream::Http("http://127.0.0.1:8880".into()))
    );
    assert_eq!(
        upstream_for(&c, "nas", "kokoro", "NAS-GPU0"),
        Some(Upstream::Bridge {
            bridge: "nas".into(),
            engine: "kokoro".into(),
            card: "GPU0".into()
        })
    );
    assert_eq!(local_card("nas", "NAS-CPU"), "CPU");
}

// ── a real bridge against the router ─────────────────────────────────────────────────────────

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap()
    });
    a
}

async fn router_with(hub: Arc<Hub>) -> SocketAddr {
    let st = AppState::new(
        Snapshot::build(RouterConfig::default(), None).unwrap(),
        None,
    );
    st.bridges.set(hub).ok();
    serve(crate::server::app(st, crate::server::Mode::Gateway)).await
}

async fn engine() -> SocketAddr {
    serve(
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route(
                "/v1/audio/speech",
                post(|h: HeaderMap, body: axum::body::Bytes| async move {
                    // the client's credentials never reach the engine
                    assert!(h.get("authorization").is_none());
                    let mut out = b"audio:".to_vec();
                    out.extend_from_slice(&body);
                    ([("content-type", "audio/wav")], out)
                }),
            ),
    )
    .await
}

fn bridge_config(
    dir: &std::path::Path,
    router: SocketAddr,
    port: u16,
    token: &str,
) -> BridgeConfig {
    let tf = dir.join("t");
    std::fs::write(&tf, token).unwrap();
    toml::from_str(&format!(
        r#"
        [bridge]
        id = "nas"
        router = "http://{router}"
        token_file = "{}"
        state_dir = "{}"
        [detect]
        nvidia = false
        [cards.CPU]
        cores = 2
        ram_gib = 4
        [engines.kokoro]
        kinds = ["tts"]
        cards = ["CPU"]
        port = {port}
        lifecycle = "external"
        "#,
        tf.display().to_string().replace('\\', "/"),
        dir.display().to_string().replace('\\', "/")
    ))
    .unwrap()
}

fn fast() -> Timing {
    Timing {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        refused_wait: Duration::from_secs(30),
        ..Timing::default()
    }
}

async fn until(what: &str, f: impl Fn() -> bool) {
    // detection runs first (on Windows a PowerShell query): be generous
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_bridge_registers_answers_commands_and_carries_requests() {
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    let hub = hub_in(rd.path());
    let token = tokens::mint(rd.path(), "nas", false).unwrap();
    let addr = router_with(hub.clone()).await;
    let eng = engine().await;
    let h = client::spawn(
        bridge_config(bd.path(), addr, eng.port(), &token),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    // the hub knows it a moment before the bridge has read the welcome
    until("the bridge to register", || {
        hub.online("nas") && h.status().connected
    })
    .await;
    assert_eq!(h.status().server.as_deref(), Some("nas"));

    // the overlay sees it
    let c = effective_with(&hub, &base());
    assert!(c.cards["NAS-CPU"].managed);
    assert_eq!(c.engines["kokoro"].port.get("NAS-CPU"), Some(&eng.port()));

    // a command
    let v = hub
        .call("nas", "status", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(v["engines"][0]["engine"], "kokoro");

    // a request through the tunnel, with the client's key stripped
    let mut hdr = HeaderMap::new();
    hdr.insert("authorization", "Bearer client-key".parse().unwrap());
    hdr.insert("content-type", "application/json".parse().unwrap());
    let r = forward(
        &hub,
        "nas",
        "kokoro",
        "NAS-CPU",
        "POST",
        "/v1/audio/speech",
        &hdr,
        axum::body::Bytes::from_static(b"{\"input\":\"hi\"}"),
    )
    .await;
    let (status, headers, body) = body_text(r, 1 << 20).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "audio:{\"input\":\"hi\"}");
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "audio/wav")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-abhoth-bridge" && v == "nas")
    );

    // the admin API's view and test request
    let audit = |_: &str, _: &str| {};
    let (code, v) = api::handle(&hub, &axum::http::Method::GET, &[], &json!(null), &audit).await;
    assert_eq!(code, 200);
    assert_eq!(v["bridges"][0]["online"], true);
    assert_eq!(v["bridges"][0]["cards"][0]["global"], "NAS-CPU");
    let (_, v) = api::handle(
        &hub,
        &axum::http::Method::POST,
        &["nas", "request"],
        &json!({"engine": "kokoro", "card": "CPU", "method": "post", "path": "/v1/audio/speech", "body": "x"}),
        &audit,
    )
    .await;
    assert_eq!(v["status"], 200);
    assert_eq!(v["body"], "audio:x");
    // the view never holds a token
    assert!(!hub.view().to_string().contains(&token));

    // a dropped connection: offline at once, then back by itself
    hub.mux("nas").unwrap().close(1011, "test");
    until("the bridge to reconnect", || {
        h.status().connects >= 2 && hub.online("nas")
    })
    .await;

    // revoking the token disconnects it, and it is refused from then on
    let (code, _) = api::handle(
        &hub,
        &axum::http::Method::DELETE,
        &["nas"],
        &json!(null),
        &audit,
    )
    .await;
    assert_eq!(code, 200);
    until("the refusal", || {
        h.status()
            .last_error
            .unwrap_or_default()
            .contains("bad token")
    })
    .await;
    assert!(!hub.online("nas"));
    h.stop();
}

#[tokio::test]
async fn a_wrong_token_or_a_borrowed_one_is_refused() {
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    let hub = hub_in(rd.path());
    // a token minted for another bridge
    let other = tokens::mint(rd.path(), "pc", false).unwrap();
    let addr = router_with(hub.clone()).await;
    let h = client::spawn(
        bridge_config(bd.path(), addr, 1, &other),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    until("the refusal", || h.status().last_error.is_some()).await;
    assert!(
        h.status().last_error.unwrap().contains("bad token"),
        "{:?}",
        h.status()
    );
    assert!(!hub.online("nas") && !hub.online("pc"));
    h.stop();
}

#[test]
fn engines_the_admin_turns_off_leave_the_overlay() {
    let d = tempfile::tempdir().unwrap();
    let hub = hub_in(d.path());
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("nas", descriptor("nas"), "v", "", Some(mux));
    hub.set_settings(
        "nas",
        orca_bridge::proto::Settings {
            engines_off: vec!["chatterbox".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let c = effective_with(&hub, &base());
    assert!(!c.engines.contains_key("chatterbox"));
    assert!(c.engines.contains_key("kokoro"));
    let tts = &c.cards["NAS-GPU0"].engines.as_ref().unwrap()["tts"];
    assert_eq!(tts, &vec!["kokoro".to_string()]);
    assert_eq!(
        hub.view()["bridges"][0]["enginesOff"],
        json!(["chatterbox"])
    );
}

/// A desktop app's machine config, as the bridge's embedder answers for it (MACHINE.md 8).
struct FakeMachine {
    rev: std::sync::Mutex<u32>,
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl orca_bridge::client::Remote for FakeMachine {
    async fn call(
        &self,
        op: &str,
        args: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{op} {}", args["who"].as_str().unwrap_or("")));
        let rev = format!("r{}", *self.rev.lock().unwrap());
        Some(Ok(match op {
            "config.get" => json!({"status": 200, "body": {"rev": rev, "text": "version = 1\n"}}),
            "config.apply" if args["rev"].as_str() != Some(rev.as_str()) => {
                json!({"status": 412, "body": {"error": "stale", "currentRev": rev}})
            }
            "config.apply" => {
                *self.rev.lock().unwrap() += 1;
                json!({"status": 200, "body": {"ok": true}})
            }
            "machine.api" => {
                json!({"status": 200, "body": {"path": args["path"], "query": args["query"]}})
            }
            _ => return None,
        }))
    }
}

#[tokio::test]
async fn remote_config_goes_through_the_bridge_with_a_revision() {
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    let hub = hub_in(rd.path());
    let token = tokens::mint(rd.path(), "nas", false).unwrap();
    let addr = router_with(hub.clone()).await;
    let fake = Arc::new(FakeMachine {
        rev: std::sync::Mutex::new(1),
        seen: Default::default(),
    });
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a2 = allow.clone();
    let opts = orca_bridge::client::Options {
        remote: Some(fake.clone()),
        shape: Some(Arc::new(move |d: &mut Descriptor| {
            d.remote_config = a2.load(std::sync::atomic::Ordering::SeqCst);
        })),
        ..Default::default()
    };
    let h = client::spawn_with(
        bridge_config(bd.path(), addr, 9, &token),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
        opts,
    );
    until("the bridge to register", || {
        hub.online("nas") && h.status().connected
    })
    .await;
    let audit = |_: &str, _: &str| {};
    let call = |m: axum::http::Method,
                rest: &'static str,
                body: serde_json::Value,
                rev: Option<&'static str>| {
        let hub = hub.clone();
        async move {
            api::machine(
                &hub,
                "nas",
                &m,
                rest,
                "q=1",
                &body,
                rev,
                "admin:ann",
                &audit,
            )
            .await
        }
    };
    // off: read-only, and the bridge is never asked
    let (s, v) = call(axum::http::Method::GET, "config", json!(null), None).await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["readOnly"], json!(true));
    assert!(fake.seen.lock().unwrap().is_empty());

    allow.store(true, std::sync::atomic::Ordering::SeqCst);
    h.refresh();
    until("the new descriptor", || hub.remote_config("nas")).await;
    let (s, v) = call(axum::http::Method::GET, "config", json!(null), None).await;
    assert_eq!((s, v["rev"].as_str()), (200, Some("r1")));
    let (s, _) = call(
        axum::http::Method::PUT,
        "config",
        json!({"text": "x"}),
        Some("r1"),
    )
    .await;
    assert_eq!(s, 200);
    // the same revision again is stale now
    let (s, v) = call(
        axum::http::Method::PUT,
        "config",
        json!({"text": "x"}),
        Some("r1"),
    )
    .await;
    assert_eq!((s, v["currentRev"].as_str()), (412, Some("r2")));
    let (s, v) = call(axum::http::Method::GET, "inventory", json!(null), None).await;
    assert_eq!(
        (s, v["path"].as_str(), v["query"].as_str()),
        (200, Some("inventory"), Some("q=1"))
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen[0], "config.get admin:ann");
    assert!(seen.iter().all(|x| x.ends_with("admin:ann")), "{seen:?}");
    h.stop();
}

#[tokio::test]
async fn a_raw_tcp_stream_goes_through_the_bridge() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // a Wyoming-like engine: echoes with a prefix
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = c.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut out = b"echo:".to_vec();
                    out.extend_from_slice(&buf[..n]);
                    if c.write_all(&out).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    let hub = hub_in(rd.path());
    let token = tokens::mint(rd.path(), "nas", false).unwrap();
    let addr = router_with(hub.clone()).await;
    let h = client::spawn(
        bridge_config(bd.path(), addr, port, &token),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    until("the bridge to register", || {
        hub.online("nas") && h.status().connected
    })
    .await;
    let (mut ours, theirs) = tokio::io::duplex(4096);
    let hub2 = hub.clone();
    let fwd =
        tokio::spawn(async move { forward_tcp(&hub2, "nas", "kokoro", "NAS-CPU", theirs).await });
    ours.write_all(b"describe").await.unwrap();
    let mut got = vec![0u8; 13];
    tokio::time::timeout(Duration::from_secs(10), ours.read_exact(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got, b"echo:describe");
    drop(ours);
    tokio::time::timeout(Duration::from_secs(10), fwd)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // an engine that is not there: an error before any byte, so the caller can fail over
    let (_a, b) = tokio::io::duplex(64);
    assert!(
        forward_tcp(&hub, "nas", "nope", "NAS-CPU", b)
            .await
            .is_err()
    );
    h.stop();
}
