//! The media routes against fake engines (HTTP, Wyoming, WebSocket) and a real bridge.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::routing::{any, get, post};
use orca_bridge::client::{self, Timing};
use orca_bridge::config::BridgeConfig;
use orca_bridge::proto::Descriptor;
use serde_json::{Value, json};

use super::place::{self, Want};
use super::*;
use crate::bridges::{self, Hub};
use crate::config::{RouterConfig, Snapshot};

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

/// A port nothing listens on.
fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn config(toml_text: &str) -> ControlConfig {
    toml::from_str(toml_text).unwrap()
}

/// DeepThought as it is live: speech on D, ComfyUI on C, music on the CPU, the image shim.
fn live_like(chatterbox: u16, whisper: u16, comfy: u16, sa3: u16, shim: u16) -> String {
    format!(
        r#"
        version = 1
        [servers.deepthought]
        host = "127.0.0.1"
        [cards.D]
        server = "deepthought"
        gpu_uuid = "GPU-d"
        vram_gib = 8.0
        engines = {{ llm = [], tts = ["chatterbox", "kokoro"], stt = ["whisper"] }}
        [cards.C]
        server = "deepthought"
        gpu_uuid = "GPU-c"
        vram_gib = 16.0
        engines = {{ image = ["comfyui"], video = ["comfyui"] }}
        [cards.DT-CPU]
        server = "deepthought"
        device = "cpu"
        cores = 96
        ram_gib = 64.0
        managed = false
        on_demand = false
        engines = {{ music = ["sa3"], image = ["image-api"] }}
        [engines.comfyui]
        driver = "external"
        port = {{ C = {comfy} }}
        [engines.sa3]
        driver = "external"
        port = {{ DT-CPU = {sa3} }}
        [engines.chatterbox]
        driver = "external"
        media_api = "synth"
        port = {{ D = {chatterbox} }}
        [engines.whisper]
        driver = "external"
        port = {{ D = {whisper} }}
        [engines.image-api]
        driver = "external"
        media_api = "openai-images"
        port = {{ DT-CPU = {shim} }}
        [models.chatterbox-nano]
        kind = "tts"
        primary = "chatterbox"
        servers = ["deepthought"]
        [models.qwen-image]
        kind = "image"
        primary = "comfyui"
        needs = ["qwen21-vae"]
        aliases = ["juggernaut"]
        [models.qwen21-vae]
        kind = "image"
        role = "vae"
        "#
    )
}

fn nas_descriptor() -> Descriptor {
    serde_json::from_value(json!({ "v": 1,
      "server": { "id": "nas", "title": "truenas.local", "os": "linux", "arch": "amd64" },
      "cards": [
        { "id": "GPU0", "device": "gpu", "gpu_uuid": "GPU-m4000", "vram_gib": 8.0,
          "engines": { "tts": ["chatterbox"], "stt": ["whisper"] }, "shared": true },
        { "id": "CPU", "device": "cpu", "cores": 4, "ram_gib": 6.0, "engines": { "tts": ["kokoro"] } } ],
      "engines": [
        { "id": "chatterbox", "kinds": ["tts"], "api": "openai-speech", "ports": { "GPU0": 10204 } },
        { "id": "whisper", "kinds": ["stt"], "api": "wyoming", "ports": { "GPU0": 10300 } },
        { "id": "kokoro", "kinds": ["tts"], "caps": { "inherits": "kokoro" }, "ports": { "CPU": 18890 } } ] }))
    .unwrap()
}

fn keys(v: &[place::Target]) -> Vec<String> {
    v.iter().map(|t| t.key()).collect()
}

#[test]
fn routes_and_their_limits() {
    let p = Method::POST;
    assert_eq!(route_of(&p, "/v1/audio/speech"), Some(Route::Speech));
    assert_eq!(route_of(&Method::GET, "/v1/audio/speech"), None);
    assert_eq!(
        route_of(&p, "/v1/audio/transcriptions"),
        Some(Route::Transcribe)
    );
    assert_eq!(route_of(&p, "/v1/audio/music"), Some(Route::Music));
    assert_eq!(
        route_of(&Method::GET, "/v1/audio/music/abc.mp3"),
        Some(Route::Music)
    );
    assert_eq!(route_of(&p, "/v1/images/generations"), Some(Route::Images));
    assert_eq!(route_of(&Method::GET, "/comfyui"), Some(Route::Comfy));
    assert_eq!(route_of(&Method::GET, "/comfyui/ws"), Some(Route::Comfy));
    assert_eq!(route_of(&Method::GET, "/comfyuix"), None);
    assert_eq!(route_of(&p, "/v1/chat/completions"), None);
    assert!(
        Route::Comfy.limit() >= 100 << 20,
        "uploads of 50 MB and more"
    );
    assert!(Route::Speech.limit() <= 1 << 20);
}

#[test]
fn media_apis_come_from_the_catalog_unless_set() {
    let c = config(&live_like(1, 2, 3, 4, 5));
    assert_eq!(place::api_of(&c, "comfyui"), Some(MediaApi::Comfyui));
    assert_eq!(place::api_of(&c, "sa3"), Some(MediaApi::Sa3));
    assert_eq!(place::api_of(&c, "whisper"), Some(MediaApi::Wyoming));
    assert_eq!(place::api_of(&c, "chatterbox"), Some(MediaApi::Synth));
    assert_eq!(place::api_of(&c, "image-api"), Some(MediaApi::OpenaiImages));
    assert_eq!(place::api_of(&c, "nope"), None);
}

#[test]
fn speech_goes_to_deepthought_first_then_the_bridge() {
    let d = tempfile::tempdir().unwrap();
    let base = config(&live_like(1, 2, 3, 4, 5));
    let hub = Hub::new(d.path(), Arc::new(base));
    let (mux, _out) = orca_bridge::mux::Mux::new();
    hub.upsert("nas", nas_descriptor(), "v", "", Some(mux));
    let c = hub.config();
    let cx = Cx {
        http: &reqwest::Client::new(),
        hub: Some(&hub),
        c: &c,
    };
    let any_tts = Want {
        kinds: &[ModelKind::Tts],
        apis: &[MediaApi::OpenaiSpeech, MediaApi::Synth],
        ..Default::default()
    };
    let got = place::candidates(&c, &any_tts, &cx.bridge_priority());
    assert_eq!(
        keys(&got),
        [
            "chatterbox/D@deepthought",
            "chatterbox/NAS-GPU0@nas",
            "kokoro/NAS-CPU@nas"
        ]
    );
    // the bridge's chatterbox is the same engine: router.toml's `media_api` holds there too
    assert_eq!(got[1].api, MediaApi::Synth);
    assert_eq!(got[2].api, MediaApi::OpenaiSpeech);
    assert!(got[1].bridge && !got[0].bridge);

    // a model in the inventory: its engines only (not Kokoro)
    let m = Want {
        model: Some("chatterbox-nano"),
        ..any_tts.clone()
    };
    assert_eq!(
        keys(&place::candidates(&c, &m, &cx.bridge_priority())),
        ["chatterbox/D@deepthought", "chatterbox/NAS-GPU0@nas"]
    );
    // a pin
    let p = Want {
        server: Some("nas"),
        ..any_tts.clone()
    };
    assert_eq!(
        keys(&place::candidates(&c, &p, &cx.bridge_priority())),
        ["chatterbox/NAS-GPU0@nas", "kokoro/NAS-CPU@nas"]
    );
    // disabled: never placed on
    hub.set_settings(
        "nas",
        orca_bridge::proto::Settings {
            enabled: false,
            priority: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let c = hub.config();
    let cx = Cx {
        http: cx.http,
        hub: Some(&hub),
        c: &c,
    };
    assert_eq!(
        keys(&place::candidates(&c, &any_tts, &cx.bridge_priority())),
        ["chatterbox/D@deepthought"]
    );
}

#[test]
fn a_failed_target_is_tried_last() {
    let c = config(
        r#"
        version = 1
        [servers.a]
        host = "127.0.0.1"
        [servers.b]
        host = "127.0.0.1"
        [cards.A1]
        server = "a"
        gpu_uuid = "GPU-a"
        engines = { tts = ["kokoro"] }
        [cards.B1]
        server = "b"
        gpu_uuid = "GPU-b"
        engines = { tts = ["kokoro"] }
        [engines.kokoro]
        driver = "external"
        port = { A1 = 1, B1 = 2 }
        "#,
    );
    let w = Want {
        kinds: &[ModelKind::Tts],
        apis: &[MediaApi::OpenaiSpeech],
        ..Default::default()
    };
    let none = |_: &str| None;
    let v = place::candidates(&c, &w, &none);
    assert_eq!(keys(&v), ["kokoro/A1@a", "kokoro/B1@b"]);
    place::cool(&v[0]);
    assert_eq!(
        keys(&place::candidates(&c, &w, &none)),
        ["kokoro/B1@b", "kokoro/A1@a"]
    );
    // busy goes after idle
    let busy = place::begin(&v[1]);
    let w2 = Want {
        kinds: &[ModelKind::Tts],
        apis: &[MediaApi::OpenaiSpeech],
        server: None,
        model: Some("tts-1"),
    };
    assert_eq!(place::inflight(&v[1]), 1);
    drop(busy);
    assert_eq!(place::inflight(&v[1]), 0);
    assert_eq!(place::candidates(&c, &w2, &none).len(), 2);
}

#[test]
fn speech_requests_per_engine_api() {
    let c = config(&live_like(1, 2, 3, 4, 5));
    let a = speech::parse(
        br#"{"model":"tts-1","input":"Hello there, sir.","voice":"alloy","response_format":"mp3"}"#,
    )
    .unwrap();
    let t = place::Target {
        server: "deepthought".into(),
        card: "D".into(),
        engine: "chatterbox".into(),
        api: MediaApi::Synth,
        bridge: false,
        priority: 0,
        port: 1,
    };
    let p = speech::prepare(&c, &t, &a).unwrap();
    assert_eq!(p.method, "GET");
    // OpenAI's voice names mean the engine's default
    assert_eq!(p.path_q, "/synth?mode=whole&text=Hello%20there%2C%20sir.");
    let a = speech::parse(br#"{"input":"Hi","voice":"jarvis-bettany-a"}"#).unwrap();
    let p = speech::prepare(&c, &t, &a).unwrap();
    assert!(
        p.path_q.ends_with("&voice=jarvis-bettany-a"),
        "{}",
        p.path_q
    );
    // Kokoro: a voice it has, or its default
    let k = place::Target {
        engine: "kokoro".into(),
        api: MediaApi::OpenaiSpeech,
        ..t.clone()
    };
    let p = speech::prepare(&c, &k, &a).unwrap();
    let v: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(v["voice"], "bm_fable");
    assert_eq!(v["response_format"], "wav");
    let a = speech::parse(br#"{"input":"Hi","voice":"af_bella"}"#).unwrap();
    let v: Value = serde_json::from_slice(&speech::prepare(&c, &k, &a).unwrap().body).unwrap();
    assert_eq!(v["voice"], "af_bella");
    assert!(speech::parse(br#"{"input":"  "}"#).is_err());
    let a = speech::parse(br#"{"input":"x","model":"chatterbox-nano@nas"}"#).unwrap();
    assert_eq!(a.model.as_deref(), Some("chatterbox-nano"));
    assert_eq!(a.server.as_deref(), Some("nas"));
}

#[test]
fn wav_is_mixed_down_and_resampled() {
    // 8 kHz stereo, 0.5 s
    let mut b = super::wav::encode(&[0; 0], 8000);
    let n = 4000usize;
    let mut data = Vec::new();
    for i in 0..n {
        let s = ((i as f32 / 8.0).sin() * 10000.0) as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    // rewrite the header as stereo
    b[22] = 2;
    b[32] = 4;
    let len = data.len() as u32;
    b[40..44].copy_from_slice(&len.to_le_bytes());
    b.extend_from_slice(&data);
    let p = super::wav::decode(&b, 16_000).unwrap();
    assert_eq!(p.rate, 16_000);
    assert_eq!(p.data.len(), 2 * 8000, "0.5 s at 16 kHz mono");
    assert!((p.seconds() - 0.5).abs() < 1e-6);
    assert!(super::wav::decode(b"ID3....", 16_000).is_err());
}

fn form(boundary: &str, fields: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, file, data) in fields {
        b.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file {
            Some(f) => b.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
            ),
            None => b.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    b
}

#[test]
fn multipart_forms_parse() {
    let body = Bytes::from(form(
        "XyZ",
        &[
            ("model", None, b"whisper-1"),
            ("file", Some("a.wav"), b"RIFF\r\n--Xy binary\r\n"),
        ],
    ));
    let parts = super::multipart::parse("multipart/form-data; boundary=\"XyZ\"", &body).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].text(), "whisper-1");
    assert_eq!(parts[1].filename.as_deref(), Some("a.wav"));
    assert_eq!(&parts[1].data[..], b"RIFF\r\n--Xy binary\r\n");
    assert!(super::multipart::parse("application/json", &body).is_err());
}

#[test]
fn comfyui_paths_split_into_pin_and_rest() {
    use super::comfy::split;
    assert_eq!(split("/comfyui"), (None, "/".into()));
    assert_eq!(split("/comfyui/"), (None, "/".into()));
    assert_eq!(split("/comfyui/prompt?x=1"), (None, "/prompt?x=1".into()));
    assert_eq!(split("/comfyui/@pc"), (Some("pc".into()), "/".into()));
    assert_eq!(
        split("/comfyui/@pc/ws?clientId=a"),
        (Some("pc".into()), "/ws?clientId=a".into())
    );
}

#[test]
fn media_models_list_recipes_not_components() {
    let c = config(&live_like(1, 2, 3, 4, 5));
    let v = models(&c, "image");
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["qwen-image"]);
    assert_eq!(v["data"][0]["kind"], "image");
    assert_eq!(models(&c, "media")["data"].as_array().unwrap().len(), 2);
}

// ── over HTTP: the router's dispatcher, fake engines ────────────────────────────────────────

async fn router_for(c: ControlConfig, dir: &std::path::Path) -> (SocketAddr, Arc<Hub>) {
    let st = crate::state::AppState::new(
        Snapshot::build(RouterConfig::default(), None).unwrap(),
        None,
    );
    let hub = Hub::new(dir, Arc::new(c));
    st.bridges.set(hub.clone()).ok();
    (
        serve(crate::server::app(st, crate::server::Mode::Gateway)).await,
        hub,
    )
}

async fn synth_engine() -> SocketAddr {
    serve(Router::new().route(
        "/synth",
        get(|u: axum::http::Uri| async move {
            let q = u.query().unwrap_or_default().to_string();
            ([("content-type", "audio/wav")], format!("synth:{q}"))
        }),
    ))
    .await
}

async fn openai_speech_engine(tag: &'static str) -> SocketAddr {
    serve(Router::new().route(
        "/v1/audio/speech",
        post(move |h: HeaderMap, body: Bytes| async move {
            assert!(h.get("authorization").is_none(), "no client key upstream");
            let v: Value = serde_json::from_slice(&body).unwrap();
            (
                [("content-type", "audio/wav")],
                format!("{tag}:{}:{}", v["input"].as_str().unwrap(), v["voice"]),
            )
        }),
    ))
    .await
}

#[tokio::test]
async fn speech_answers_from_the_engine_and_fails_over() {
    let synth = synth_engine().await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&live_like(synth.port(), 2, 3, 4, 5)), d.path()).await;
    let http = reqwest::Client::new();
    let r = http
        .post(format!("http://{addr}/v1/audio/speech"))
        .header("authorization", "Bearer client")
        .json(&json!({"model": "tts-1", "input": "Good evening", "voice": "alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-engine"], "chatterbox");
    assert_eq!(
        r.text().await.unwrap(),
        "synth:mode=whole&text=Good%20evening"
    );

    // DeepThought's engine down: the next server's (a second direct server here)
    let kokoro = openai_speech_engine("kokoro").await;
    let text = format!(
        "{}\n[servers.pc]\nhost = \"127.0.0.1\"\n[cards.P1]\nserver = \"pc\"\ngpu_uuid = \"GPU-p\"\nengines = {{ tts = [\"kokoro\"] }}\n[engines.kokoro]\ndriver = \"external\"\nport = {{ P1 = {} }}\n",
        live_like(dead_port(), 2, 3, 4, 5),
        kokoro.port()
    );
    let d2 = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&text), d2.path()).await;
    let r = http
        .post(format!("http://{addr}/v1/audio/speech"))
        .json(&json!({"input": "Fallback", "voice": "jarvis-bettany-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-server"], "pc");
    assert_eq!(r.headers()["x-abhoth-failovers"], "1");
    assert_eq!(r.text().await.unwrap(), "kokoro:Fallback:\"bm_fable\"");

    // a model only the dead engine runs: 503 naming why
    let r = http
        .post(format!("http://{addr}/v1/audio/speech"))
        .json(&json!({"input": "x", "model": "chatterbox-nano"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("chatterbox/D@deepthought"),
        "{v}"
    );
}

#[tokio::test]
async fn transcriptions_speak_wyoming() {
    let w = super::wyoming::fake_engine("hello sir").await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&live_like(1, w.port(), 3, 4, 5)), d.path()).await;
    // 0.25 s at 8 kHz: 4000 samples at 16 kHz reach the engine (8000 bytes)
    let wav = super::wav::encode(&vec![100i16; 2000], 8000);
    let body = form(
        "b0und",
        &[("model", None, b"whisper-1"), ("file", Some("a.wav"), &wav)],
    );
    let http = reqwest::Client::new();
    let r = http
        .post(format!("http://{addr}/v1/audio/transcriptions"))
        .header("content-type", "multipart/form-data; boundary=b0und")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-engine"], "whisper");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["text"], "hello sir 8000");

    // not WAV: the client's to fix
    let bad = form("b0und", &[("file", Some("a.mp3"), b"ID3 not wav")]);
    let r = http
        .post(format!("http://{addr}/v1/audio/transcriptions"))
        .header("content-type", "multipart/form-data; boundary=b0und")
        .body(bad)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
}

async fn sa3_engine() -> SocketAddr {
    serve(
        Router::new()
            .route(
                "/generate",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    assert!(v.get("model").is_none() && v.get("async").is_none());
                    if v["wait"] == true {
                        axum::response::Response::builder()
                            .header("content-type", "audio/mpeg")
                            .header("x-job-id", "job1")
                            .body(axum::body::Body::from("ID3mp3"))
                            .unwrap()
                    } else {
                        let b = json!({"id": "job2", "status": "queued", "download_url": null});
                        axum::response::Response::builder()
                            .status(202)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(b.to_string()))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/jobs/{id}",
                get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
                    axum::Json(json!({"id": id, "status": "done", "download_url": format!("/audio/{id}.mp3")}))
                }),
            )
            .route(
                "/audio/{f}",
                get(|| async { ([("content-type", "audio/mpeg")], "ID3job2") }),
            ),
    )
    .await
}

#[tokio::test]
async fn music_waits_or_runs_as_a_job() {
    let sa3 = sa3_engine().await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&live_like(1, 2, 3, sa3.port(), 5)), d.path()).await;
    let http = reqwest::Client::new();
    let r = http
        .post(format!("http://{addr}/v1/audio/music"))
        .json(&json!({"prompt": "calm synth", "seconds": 10, "model": "stable-audio-3-medium"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-card"], "DT-CPU");
    assert_eq!(r.text().await.unwrap(), "ID3mp3");

    let r = http
        .post(format!("http://{addr}/v1/audio/music?async=1"))
        .json(&json!({"prompt": "calm synth", "seconds": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["id"], "job2");
    assert_eq!(v["status_url"], "/v1/audio/music/job2");
    assert!(v["download_url"].is_null());
    let v: Value = http
        .get(format!("http://{addr}/v1/audio/music/job2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["download_url"], "/v1/audio/music/job2.mp3");
    let r = http
        .get(format!("http://{addr}/v1/audio/music/job2.mp3"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ID3job2");
    let r = http
        .get(format!("http://{addr}/v1/audio/music/..%2Fetc"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

async fn comfy_engine() -> SocketAddr {
    serve(
        Router::new()
            .route(
                "/prompt",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    assert!(v["prompt"].is_object());
                    axum::Json(json!({"prompt_id": "p-1", "number": 3}))
                }),
            )
            .route(
                "/history/{id}",
                get(
                    |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        axum::Json(json!({id: {"status": {"completed": true}}}))
                    },
                ),
            )
            .route(
                "/view",
                get(|| async {
                    (
                        [("content-type", "image/png")],
                        vec![0x89u8, b'P', b'N', b'G'],
                    )
                }),
            )
            .route(
                "/ws",
                any(|up: WebSocketUpgrade| async move {
                    up.on_upgrade(|mut s| async move {
                        let _ = s.send(Message::Text(r#"{"type":"status"}"#.into())).await;
                        while let Some(Ok(m)) = s.recv().await {
                            if let Message::Text(t) = m {
                                let _ = s.send(Message::Text(format!("echo:{t}").into())).await;
                            }
                        }
                    })
                }),
            )
            .route(
                "/old",
                get(|| async { axum::response::Redirect::to("/new") }),
            ),
    )
    .await
}

#[tokio::test]
async fn comfyui_passes_through_with_its_websocket() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as T;
    let comfy = comfy_engine().await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&live_like(1, 2, comfy.port(), 4, 5)), d.path()).await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = http
        .post(format!("http://{addr}/comfyui/prompt"))
        .json(&json!({"prompt": {"1": {"class_type": "KSampler"}}, "client_id": "c-9"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-card"], "C");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["prompt_id"], "p-1");
    let v: Value = http
        .get(format!("http://{addr}/comfyui/history/p-1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["p-1"]["status"]["completed"], true);
    let r = http
        .get(format!(
            "http://{addr}/comfyui/@deepthought/view?filename=a.png"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(&r.bytes().await.unwrap()[..], &[0x89, b'P', b'N', b'G']);
    // a redirect stays under the prefix
    let r = http
        .get(format!("http://{addr}/comfyui/@deepthought/old"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["location"], "/comfyui/@deepthought/new");
    // an unknown pin: nothing serves it
    let r = http
        .get(format!("http://{addr}/comfyui/@nowhere/view"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    // another site's page with a session would be refused; no Origin from tools is fine
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/comfyui/ws?clientId=c-9"))
            .await
            .unwrap();
    match ws.next().await.unwrap().unwrap() {
        T::Text(t) => assert_eq!(t.as_str(), r#"{"type":"status"}"#),
        m => panic!("{m:?}"),
    }
    ws.send(T::Text("hi".into())).await.unwrap();
    match ws.next().await.unwrap().unwrap() {
        T::Text(t) => assert_eq!(t.as_str(), "echo:hi"),
        m => panic!("{m:?}"),
    }
}

#[tokio::test]
async fn images_go_to_the_shim_and_bodies_have_limits() {
    let shim = serve(Router::new().route(
        "/v1/images/generations",
        post(|body: Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap();
            axum::Json(json!({"data": [{"b64_json": "AAAA"}], "model": v["model"]}))
        }),
    ))
    .await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&live_like(1, 2, 3, 4, shim.port())), d.path()).await;
    let http = reqwest::Client::new();
    let v: Value = http
        .post(format!("http://{addr}/v1/images/generations"))
        .json(&json!({"model": "qwen-image@pc", "prompt": "a cat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["model"], "qwen-image@pc", "the shim keeps its own pins");
    let r = http
        .post(format!("http://{addr}/v1/audio/speech"))
        .body(vec![b'x'; (1 << 20) + 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let v: Value = http
        .get(format!("http://{addr}/v1/models?kind=tts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["data"][0]["id"], "chatterbox-nano");
}

// ── speech through a real bridge ────────────────────────────────────────────────────────────

fn fast() -> Timing {
    Timing {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        refused_wait: Duration::from_secs(30),
        ..Timing::default()
    }
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
        engines = {{ tts = ["kokoro"] }}
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

#[tokio::test]
async fn speech_falls_back_to_the_bridge() {
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    // DeepThought's Chatterbox is down
    let (addr, hub) = router_for(config(&live_like(dead_port(), 2, 3, 4, 5)), rd.path()).await;
    let token = bridges::tokens::mint(rd.path(), "nas", false).unwrap();
    let eng = openai_speech_engine("nas-kokoro").await;
    let _h = client::spawn(
        bridge_config(bd.path(), addr, eng.port(), &token),
        "test",
        Arc::new(|_: &str| {}),
        fast(),
    );
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !hub.online("nas") {
        assert!(
            std::time::Instant::now() < end,
            "the bridge never registered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/audio/speech"))
        .header("authorization", "Bearer client")
        .json(&json!({"input": "Through the tunnel", "voice": "alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-server"], "nas");
    assert_eq!(r.headers()["x-abhoth-bridge"], "nas");
    assert_eq!(
        r.text().await.unwrap(),
        "nas-kokoro:Through the tunnel:\"bm_fable\""
    );
}

#[tokio::test]
async fn transcription_falls_back_to_wyoming_on_the_bridge() {
    let rd = tempfile::tempdir().unwrap();
    let bd = tempfile::tempdir().unwrap();
    // DeepThought's whisper is down; the bridge's answers over its raw TCP stream (13.1)
    let (addr, hub) = router_for(config(&live_like(1, dead_port(), 3, 4, 5)), rd.path()).await;
    let token = bridges::tokens::mint(rd.path(), "nas", false).unwrap();
    let w = super::wyoming::fake_engine("from nas").await;
    let tf = bd.path().join("t");
    std::fs::write(&tf, &token).unwrap();
    let cfg: BridgeConfig = toml::from_str(&format!(
        r#"
        [bridge]
        id = "nas"
        router = "http://{addr}"
        token_file = "{}"
        state_dir = "{}"
        [detect]
        nvidia = false
        [cards.CPU]
        cores = 2
        ram_gib = 4
        engines = {{ stt = ["whisper"] }}
        [engines.whisper]
        kinds = ["stt"]
        api = "wyoming"
        cards = ["CPU"]
        port = {}
        health = "tcp"
        lifecycle = "external"
        "#,
        tf.display().to_string().replace('\\', "/"),
        bd.path().display().to_string().replace('\\', "/"),
        w.port()
    ))
    .unwrap();
    let _h = client::spawn(cfg, "test", Arc::new(|_: &str| {}), fast());
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !hub.online("nas") {
        assert!(
            std::time::Instant::now() < end,
            "the bridge never registered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let wav = super::wav::encode(&vec![100i16; 4000], 16000);
    let body = form(
        "b0und",
        &[("model", None, b"whisper-1"), ("file", Some("a.wav"), &wav)],
    );
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/audio/transcriptions"))
        .header("content-type", "multipart/form-data; boundary=b0und")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let text = r.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert_eq!(headers["x-abhoth-server"], "nas");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["text"], "from nas 8000");
}

// ── phase 4: images in the router ───────────────────────────────────────────────────────────

fn image_config(comfy_a: u16, comfy_b: u16) -> String {
    format!(
        r#"
        version = 1
        [servers.deepthought]
        host = "127.0.0.1"
        [servers.spare]
        host = "127.0.0.1"
        [cards.C]
        server = "deepthought"
        gpu_uuid = "GPU-c"
        engines = {{ image = ["comfyui"] }}
        [cards.S1]
        server = "spare"
        gpu_uuid = "GPU-s"
        engines = {{ image = ["comfyui"] }}
        [engines.comfyui]
        driver = "external"
        port = {{ C = {comfy_a}, S1 = {comfy_b} }}
        [models.qwen-image]
        kind = "image"
        arch = "qwen-image"
        primary = "comfyui"
        needs = ["unet", "te", "vae"]
        aliases = ["juggernaut", "qwen-image-2.1"]
        [models.qwen-image-uncensored]
        kind = "image"
        arch = "qwen-image"
        primary = "comfyui"
        needs = ["unet2", "te", "vae", "lora"]
        recipe = {{ sigmas = "1.0, 0.5, 0", edits = false }}
        [models.unet]
        kind = "image"
        role = "diffusion"
        artifacts.comfyui = {{ file = "turbo-Q4_K_M.gguf" }}
        [models.unet2]
        kind = "image"
        role = "diffusion"
        servers = ["deepthought"]
        artifacts.comfyui = {{ file = "omni-Q4_K_M.gguf" }}
        [models.te]
        kind = "image"
        role = "text_encoder"
        artifacts.comfyui = {{ file = "qwen3vl.safetensors" }}
        [models.vae]
        kind = "image"
        role = "vae"
        artifacts.comfyui = {{ file = "vae.safetensors" }}
        [models.lora]
        kind = "image"
        role = "lora"
        servers = ["deepthought"]
        artifacts.comfyui = {{ file = "turbo-lora.safetensors" }}
        "#
    )
}

#[test]
fn recipes_resolve_by_name_alias_or_default() {
    use super::images::{bucket, resolve};
    let c = config(&image_config(1, 2));
    let (p, r) = resolve(&c, Some("Juggernaut")).unwrap();
    assert_eq!((p.model.as_str(), r), ("qwen-image", None));
    assert_eq!(p.unet, "turbo-Q4_K_M.gguf");
    assert_eq!(p.sigmas, super::images::QWEN21_SIGMAS);
    assert!(p.lora.is_none() && p.edits);
    let (p, r) = resolve(&c, Some("sdxl")).unwrap();
    assert_eq!(p.model, "qwen-image");
    assert_eq!(r.as_deref(), Some("sdxl"));
    let (p, _) = resolve(&c, Some("qwen-image-uncensored")).unwrap();
    assert_eq!(p.lora.as_deref(), Some("turbo-lora.safetensors"));
    assert_eq!(p.sigmas, "1.0, 0.5, 0");
    assert!(!p.edits);
    assert_eq!(
        resolve(&c, Some("unet")).unwrap().0.model,
        "qwen-image",
        "a component name is not a recipe: the default"
    );
    assert_eq!(bucket(Some("1920x1080")), (1376, 768));
    assert_eq!(bucket(Some("512x512")), (1024, 1024));
    assert_eq!(bucket(Some("1080x1920")), (768, 1376));
    assert_eq!(bucket(Some("junk")), (1024, 1024));
    assert_eq!(bucket(None), (1024, 1024));
}

#[test]
fn workflows_match_the_shims() {
    use super::images::{edit_workflow, plan, workflow};
    let c = config(&image_config(1, 2));
    let p = plan(&c, "qwen-image").unwrap();
    let wf = workflow(&p, "a cat", 1184, 896, 7);
    assert_eq!(wf["1"]["class_type"], "UnetLoaderGGUF");
    assert_eq!(wf["5"]["inputs"]["width"], 1184);
    assert_eq!(wf["9"]["inputs"]["noise_seed"], 7);
    assert_eq!(wf["6"]["inputs"]["model"], json!(["1", 0]));
    assert!(wf.get("13").is_none());
    let p = plan(&c, "qwen-image-uncensored").unwrap();
    let wf = workflow(&p, "a cat", 1024, 1024, 1);
    assert_eq!(wf["13"]["inputs"]["lora_name"], "turbo-lora.safetensors");
    assert_eq!(wf["6"]["inputs"]["model"], json!(["13", 0]));
    assert_eq!(wf["8"]["inputs"]["sigmas"], "1.0, 0.5, 0");
    let wf = edit_workflow(&p, "make it blue", &["a.png".into(), "b.png".into()], 1);
    assert_eq!(wf["21"]["inputs"]["image"], "a.png");
    assert_eq!(wf["4"]["inputs"]["images.image_2"], json!(["22", 0]));
    assert_eq!(wf["10"]["inputs"]["latent_image"], json!(["4", 2]));
    // the files a raw workflow loads, for /comfyui/prompt
    let body = json!({ "prompt": wf }).to_string();
    let files = super::comfy::workflow_files(body.as_bytes());
    assert!(files.contains(&"turbo-lora.safetensors".to_string()));
    assert!(files.contains(&"omni-Q4_K_M.gguf".to_string()));
    assert!(!files.iter().any(|f| f.ends_with(".png")));
}

fn history(id: String, fail: bool) -> Value {
    if fail {
        let msg = json!(["execution_error", {"exception_type": "OutOfMemoryError",
                                             "exception_message": "CUDA out of memory"}]);
        return json!({ id: {"status": {"status_str": "error", "messages": [msg]}} });
    }
    let img = json!({"filename": "o.png", "subfolder": "", "type": "output"});
    json!({ id: {"status": {"status_str": "success"}, "outputs": {"12": {"images": [img]}}} })
}

/// A ComfyUI that renders a fixed PNG, or fails every job.
async fn fake_comfy(fail: bool) -> SocketAddr {
    serve(
        Router::new()
            .route(
                "/queue",
                get(|| async { axum::Json(json!({"queue_running": [], "queue_pending": []})) }),
            )
            .route(
                "/upload/image",
                post(|body: Bytes| async move {
                    assert!(body.windows(5).any(|w| w == b"IMAGE"));
                    axum::Json(json!({"name": "up.png", "subfolder": "", "type": "input"}))
                }),
            )
            .route(
                "/prompt",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(v["prompt"]["12"]["class_type"], "SaveImage");
                    let id = if v["prompt"].get("21").is_some() {
                        "edit-1"
                    } else {
                        "p-1"
                    };
                    axum::Json(json!({"prompt_id": id, "number": 1}))
                }),
            )
            .route(
                "/history/{id}",
                get(
                    move |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        axum::Json(history(id, fail))
                    },
                ),
            )
            .route(
                "/view",
                get(|u: axum::http::Uri| async move {
                    assert!(u.query().unwrap().contains("filename=o.png"));
                    ([("content-type", "image/png")], b"PNGDATA".to_vec())
                }),
            ),
    )
    .await
}

#[tokio::test]
async fn images_render_in_the_router_and_fail_over() {
    let bad = fake_comfy(true).await;
    let good = fake_comfy(false).await;
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&image_config(bad.port(), good.port())), d.path()).await;
    let http = reqwest::Client::new();
    let r = http
        .post(format!("http://{addr}/v1/images/generations"))
        .json(&json!({"model": "juggernaut", "prompt": "a red cube", "size": "1920x1080"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-server"], "spare");
    assert_eq!(r.headers()["x-abhoth-failovers"], "1");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["model"], "qwen-image");
    assert_eq!(
        v["data"][0]["b64_json"],
        crate::control::ssh::b64_encode(b"PNGDATA")
    );
    // the uncensored recipe's files are on DeepThought only (which fails every job): 500
    let r = http
        .post(format!("http://{addr}/v1/images/generations"))
        .json(&json!({"model": "qwen-image-uncensored", "prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OutOfMemoryError"),
        "{v}"
    );
    // an edit, pinned to the healthy server, from JSON base64
    let r = http
        .post(format!("http://{addr}/v1/images/edits"))
        .json(
            &json!({"model": "qwen-image@spare", "prompt": "make it blue",
                      "image": crate::control::ssh::b64_encode(b"IMAGE bytes")}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-abhoth-server"], "spare");
    // URLs are never fetched
    let r = http
        .post(format!("http://{addr}/v1/images/edits"))
        .json(&json!({"prompt": "x", "image": "http://169.254.169.254/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = http
        .get(format!("http://{addr}/compat/image-api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["backends"].as_array().unwrap().len(), 2);
}

#[test]
fn engine_reasons_are_read_from_their_bodies() {
    use super::send::reason_in;
    let r = |b: &str| reason_in(b.as_bytes());
    assert_eq!(
        r(r#"{"error":{"message":"image X is not on this host and pulling failed"}}"#).as_deref(),
        Some("image X is not on this host and pulling failed")
    );
    assert_eq!(r(r#"{"detail":"busy"}"#).as_deref(), Some("busy"));
    assert_eq!(r(r#"{"error":"no RAM"}"#).as_deref(), Some("no RAM"));
    assert_eq!(r("starting up\nmore").as_deref(), Some("starting up"));
    assert_eq!(r(""), None);
    assert_eq!(r("<html>bad gateway</html>"), None);
    assert_eq!(r(r#"{"other":1}"#), None);
    let long = "x".repeat(400);
    assert_eq!(r(&long).unwrap().chars().count(), 301);
}

async fn refusing_speech_engine(reason: &'static str, status: u16) -> SocketAddr {
    serve(Router::new().route(
        "/v1/audio/speech",
        post(move || async move {
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                [("retry-after", "7")],
                reason,
            )
        }),
    ))
    .await
}

#[tokio::test]
async fn when_every_engine_fails_the_answer_names_each_reason() {
    let a =
        refusing_speech_engine("image kokoro is not on this host and pulling failed", 503).await;
    let b = refusing_speech_engine(r#"{"error":{"message":"too many at once"}}"#, 429).await;
    let text = format!(
        "{}\n[servers.pc]\nhost = \"127.0.0.1\"\n[cards.P1]\nserver = \"pc\"\ngpu_uuid = \"GPU-p\"\nengines = {{ tts = [\"kokoro\"] }}\n[servers.pc2]\nhost = \"127.0.0.1\"\n[cards.P2]\nserver = \"pc2\"\ngpu_uuid = \"GPU-q\"\nengines = {{ tts = [\"kokoro\"] }}\n[engines.kokoro]\ndriver = \"external\"\nport = {{ P1 = {}, P2 = {} }}\n",
        live_like(dead_port(), 2, 3, 4, 5),
        a.port(),
        b.port()
    );
    let d = tempfile::tempdir().unwrap();
    let (addr, _hub) = router_for(config(&text), d.path()).await;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/audio/speech"))
        .json(&json!({"input": "x", "voice": "bm_fable"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["x-abhoth-failovers"], "3");
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    let m = v["error"]["message"].as_str().unwrap().to_string();
    assert!(m.contains("chatterbox/D@deepthought"), "{m}");
    assert!(m.contains("pulling failed"), "{m}");
    assert!(m.contains("too many at once"), "{m}");
    assert!(status == 503 || status == 429, "{status}");
}

async fn wyoming_port_for(c: ControlConfig, dir: &std::path::Path) -> SocketAddr {
    let st = crate::state::AppState::new(
        Snapshot::build(RouterConfig::default(), None).unwrap(),
        None,
    );
    st.bridges.set(Hub::new(dir, Arc::new(c))).ok();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(super::wyoming_listen::serve(l, st, ModelKind::Stt));
    addr
}

#[test]
fn wyoming_flags_parse() {
    use super::wyoming_listen::parse_flag;
    assert_eq!(
        parse_flag("stt=0.0.0.0:10310"),
        Some((ModelKind::Stt, "0.0.0.0:10310".into()))
    );
    assert_eq!(parse_flag("tts=:10213").map(|x| x.0), Some(ModelKind::Tts));
    assert_eq!(parse_flag("llm=:1"), None);
    assert_eq!(parse_flag("stt="), None);
}

#[tokio::test]
async fn the_wyoming_port_passes_through_and_fails_over() {
    let w = super::wyoming::fake_engine("hello sir").await;
    let pcm = vec![0u8; 32_000 * 2];
    // DeepThought's whisper answers
    let d = tempfile::tempdir().unwrap();
    let port = wyoming_port_for(config(&live_like(1, w.port(), 3, 4, 5)), d.path()).await;
    let got = super::wyoming::transcribe(
        &port.to_string(),
        &pcm,
        16_000,
        None,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(got, "hello sir 64000");

    // DeepThought's whisper down: the next server's
    let dead = dead_port();
    let text = format!(
        "{}
[servers.pc]
host = \"127.0.0.1\"
[cards.P1]
server = \"pc\"
gpu_uuid = \"GPU-p\"
engines = {{ stt = [\"whisper\"] }}
",
        live_like(1, dead, 3, 4, 5).replace(
            &format!("D = {dead} }}"),
            &format!("D = {dead}, P1 = {} }}", w.port())
        ),
    );
    assert!(text.contains("P1 = "), "{text}");
    let d1 = tempfile::tempdir().unwrap();
    let port = wyoming_port_for(config(&text), d1.path()).await;
    let got = super::wyoming::transcribe(
        &port.to_string(),
        &pcm,
        16_000,
        None,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(got, "hello sir 64000");

    // nothing takes it: a Wyoming error naming why
    let d2 = tempfile::tempdir().unwrap();
    let port = wyoming_port_for(config(&live_like(2, dead_port(), 3, 4, 5)), d2.path()).await;
    let err = super::wyoming::transcribe(
        &port.to_string(),
        &pcm,
        16_000,
        None,
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(err.contains("whisper/D@deepthought"), "{err}");
}
