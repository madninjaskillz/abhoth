//! The bridge against a fake router (axum) and a fake engine: hello and welcome, a command, a
//! proxied request in both directions, reconnecting after a drop, and a refused token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use orca_bridge::client::{self, Timing};
use orca_bridge::config::BridgeConfig;
use orca_bridge::mux::{Mux, Out};
use orca_bridge::proto::{self, Frame};
use serde_json::json;
use tokio::sync::mpsc;

const TOKEN: &str = "abb_testtokentesttokentesttokentesttokentestt";

/// What the fake router does with a connection.
#[derive(Clone, Copy, PartialEq)]
enum Script {
    /// Welcome, then run the test's checks through the channel.
    Serve,
    /// Welcome, then drop the connection.
    DropAfterWelcome,
}

#[derive(Clone)]
struct Fake {
    script: Script,
    connects: Arc<AtomicU32>,
    muxes: mpsc::UnboundedSender<(Arc<Mux>, Frame)>,
}

async fn ws(State(f): State<Fake>, headers: HeaderMap, up: WebSocketUpgrade) -> impl IntoResponse {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"));
    up.on_upgrade(move |socket| session(f, socket, ok))
}

async fn session(f: Fake, socket: WebSocket, token_ok: bool) {
    f.connects.fetch_add(1, Ordering::SeqCst);
    let (mut sink, mut stream) = socket.split();
    if !token_ok {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: proto::CLOSE_BAD_TOKEN,
                reason: "bad token".into(),
            })))
            .await;
        return;
    }
    let (mux, mut out) = Mux::new();
    tokio::spawn(async move {
        while let Some(o) = out.recv().await {
            let m = match o {
                Out::Text(t) => Message::Text(t.into()),
                Out::Binary(b) => Message::Binary(b.into()),
                Out::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(m)) = stream.next().await {
        match m {
            Message::Text(t) => {
                let Some(fr) = mux.on_text(Frame::parse(&t).unwrap()) else {
                    continue;
                };
                if let Frame::Hello { bridge, .. } = &fr {
                    mux.send(&Frame::Welcome {
                        v: 1,
                        server: bridge.clone(),
                        settings: Default::default(),
                    });
                    if f.script == Script::DropAfterWelcome {
                        mux.close(1011, "test drop");
                        return;
                    }
                    let _ = f.muxes.send((mux.clone(), fr));
                }
            }
            Message::Binary(b) => mux.on_binary(&b),
            Message::Close(_) => break,
            _ => mux.on_other(),
        }
    }
    mux.shutdown("closed");
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

async fn fake_router(
    script: Script,
) -> (SocketAddr, Fake, mpsc::UnboundedReceiver<(Arc<Mux>, Frame)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let f = Fake {
        script,
        connects: Arc::default(),
        muxes: tx,
    };
    let app = Router::new()
        .route(proto::WS_PATH, get(ws))
        .with_state(f.clone());
    (serve(app).await, f, rx)
}

/// An engine that answers /health and echoes /echo with a header.
async fn fake_engine() -> SocketAddr {
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|headers: HeaderMap, body: axum::body::Bytes| async move {
                let tag = headers
                    .get("x-tag")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                ([("x-echo-tag", tag)], body)
            }),
        )
        // the request is bigger than axum's default 2 MiB body limit on purpose
        .layer(axum::extract::DefaultBodyLimit::disable());
    serve(app).await
}

fn config(
    dir: &std::path::Path,
    router: SocketAddr,
    engine_port: u16,
    token: &str,
) -> BridgeConfig {
    let tf = dir.join("bridge.token");
    std::fs::write(&tf, token).unwrap();
    let text = format!(
        r#"
        [bridge]
        id = "testbox"
        router = "http://{router}"
        token_file = "{}"
        state_dir = "{}"
        [detect]
        nvidia = false
        [cards.CPU]
        cores = 2
        ram_gib = 4
        [engines.echo]
        kinds = ["tts"]
        cards = ["CPU"]
        port = {engine_port}
        lifecycle = "external"
        "#,
        tf.display().to_string().replace('\\', "/"),
        dir.display().to_string().replace('\\', "/"),
    );
    let c: BridgeConfig = toml::from_str(&text).unwrap();
    c.check().unwrap();
    c
}

fn fast() -> Timing {
    Timing {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        refused_wait: Duration::from_secs(30),
        ..Timing::default()
    }
}

#[tokio::test]
async fn hello_command_and_a_proxied_request() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fake_engine().await;
    let (addr, _f, mut rx) = fake_router(Script::Serve).await;
    let h = client::spawn(
        config(dir.path(), addr, engine.port(), TOKEN),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    let (mux, hello) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the bridge dials in")
        .unwrap();
    let Frame::Hello {
        v,
        bridge,
        descriptor,
        ..
    } = hello
    else {
        panic!("first frame is hello")
    };
    assert_eq!((v, bridge.as_str()), (1, "testbox"));
    assert!(descriptor.problems().is_empty());
    assert_eq!(descriptor.cards[0].id, "CPU");
    assert_eq!(descriptor.cards[0].engines["tts"], ["echo"]);
    assert_eq!(descriptor.engines[0].ports["CPU"], engine.port());

    // a command
    let st = mux
        .call("status", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(st["bridge"], "testbox");
    assert_eq!(st["engines"][0]["engine"], "echo");
    let e = mux
        .call("install", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(e.contains("not a bridge command yet"), "{e}");

    // a request through the tunnel, bigger than one window
    let body: Vec<u8> = (0..(2 * proto::WINDOW + 999))
        .map(|i| (i % 7) as u8)
        .collect();
    let (mut s, head) = mux
        .open(
            "echo",
            "CPU",
            "POST",
            "/echo",
            vec![
                ("x-tag".into(), "t1".into()),
                ("authorization".into(), "Bearer client".into()),
            ],
            false,
        )
        .unwrap();
    s.send(&body).await.unwrap();
    s.end();
    let head = tokio::time::timeout(Duration::from_secs(10), head)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    assert!(
        head.headers
            .iter()
            .any(|(k, v)| k == "x-echo-tag" && v == "t1")
    );
    let back = s.read_all(1 << 30).await.unwrap();
    assert!(
        back == body,
        "echoed {} of {} bytes",
        back.len(),
        body.len()
    );

    // an unknown engine is a 503 with a reason, not a hang
    let (mut s, head) = mux.open("nope", "CPU", "GET", "/", vec![], false).unwrap();
    s.end();
    assert_eq!(head.await.unwrap().status, 503);
    let msg = String::from_utf8(s.read_all(1 << 20).await.unwrap()).unwrap();
    assert!(msg.contains("no engine `nope`"), "{msg}");

    assert!(h.status().connected);
    h.stop();
}

#[tokio::test]
async fn the_bridge_redials_after_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, f, _rx) = fake_router(Script::DropAfterWelcome).await;
    let h = client::spawn(
        config(dir.path(), addr, 1, TOKEN),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while f.connects.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(f.connects.load(Ordering::SeqCst) >= 3, "it reconnected");
    assert!(h.status().connects >= 2);
    h.stop();
}

#[tokio::test]
async fn a_refused_token_is_reported_and_not_hammered() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, f, _rx) = fake_router(Script::Serve).await;
    let h = client::spawn(
        config(
            dir.path(),
            addr,
            1,
            "abb_wrongwrongwrongwrongwrongwrongwrongwrongw",
        ),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while h.status().last_error.is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = h.status().last_error.unwrap_or_default();
    assert!(err.contains("bad token"), "{err}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    // with a 50 ms backoff a lost connection would have redialled many times; refused waits
    assert_eq!(f.connects.load(Ordering::SeqCst), 1);
    assert!(!h.status().connected);
    h.stop();
}

#[tokio::test]
async fn a_missing_token_file_is_refused_without_dialling() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, f, _rx) = fake_router(Script::Serve).await;
    let mut c = config(dir.path(), addr, 1, TOKEN);
    c.bridge.token_file = dir.path().join("missing").display().to_string();
    let h = client::spawn(c, "test", Arc::new(|_: &str| {}), fast());
    // detection runs first (on Windows a PowerShell query: give it time)
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while h.status().last_error.is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        h.status()
            .last_error
            .unwrap_or_default()
            .contains("token file")
    );
    assert_eq!(f.connects.load(Ordering::SeqCst), 0);
    h.stop();
}
