//! The settings' connection panel (D149) in both scopes: router mode on a project's running
//! session (as /config in its terminal would) and on the server's config.json, and switching
//! back to the OpenAI-compatible server it had.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::routing::get;
use common::Host;
use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, ToolRegistry};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::{ConfigManager, OrcaConfig};
use orca_core::live::LiveEventBus;
use orca_webui::native::{AgentLiveSession, NativeOptions};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// What the fake router saw: (path, x-abhoth-no-switch, authorization).
type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// A router that answers identify and models, and records what it was sent.
async fn fake_router() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = |seen: Seen, path: &'static str| {
        move |h: HeaderMap| {
            let seen = seen.clone();
            async move {
                let hv = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
                seen.lock().unwrap().push((
                    path.into(),
                    hv("x-abhoth-no-switch"),
                    hv("authorization"),
                ));
            }
        }
    };
    let s1 = seen.clone();
    let s2 = seen.clone();
    let app = axum::Router::new()
        .route(
            "/.well-known/abhoth-router",
            get(move |h: HeaderMap| {
                let r = record(s1.clone(), "identify");
                async move {
                    r(h).await;
                    axum::Json(json!({
                        "service": "abhoth-router", "api": 1, "name": "Test router", "version": "0.16.8",
                        "capabilities": ["chat", "models", "model-meta"],
                        "auth": {"required": false, "keys": true, "keyPrefix": "abk_"},
                        "paths": {"openai": "/v1", "models": "/v1/abhoth/models"}
                    }))
                }
            }),
        )
        .route(
            "/v1/abhoth/models",
            get(move |h: HeaderMap| {
                let r = record(s2.clone(), "models");
                async move {
                    r(h).await;
                    axum::Json(json!({
                        "principal": "anonymous-lan",
                        "models": [{"id": "big-model", "contextWindow": 131072, "tools": true, "status": "live"}],
                        "claude": false
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// LM Studio's defaults: the URL (nothing listens on it here) and its placeholder key.
const LM_URL: &str = "http://127.0.0.1:9/v1";

fn lm_config() -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = LM_URL.into();
    config.lm_studio.api_key = "lm-studio".into();
    config.lm_studio.models = vec!["local-model".into()];
    config
}

/// A native session on LM Studio's settings, saving to `home`.
fn session(cwd: &std::path::Path, home: &std::path::Path) -> Arc<AgentLiveSession> {
    let config = lm_config();
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let s = AgentSession::new(config, client, ToolRegistry::new(), ToolContext::new(cwd));
    AgentLiveSession::start_with(
        s,
        Arc::new(LiveEventBus::new()),
        "headless",
        NativeOptions {
            config_home: Some(home.to_path_buf()),
            ..Default::default()
        },
    )
}

async fn call(host: &Host, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = host.api(method, path).timeout(Duration::from_secs(20));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.unwrap();
    let status = r.status();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn value_of(doc: &Value, key: &str) -> Value {
    doc["sections"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["settings"].as_array().unwrap().iter())
        .find(|s| s["key"] == key)
        .map(|s| s["value"].clone())
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn router_mode_in_the_project_scope_changes_that_session_and_switches_back() {
    let (router, seen) = fake_router().await;
    let work = tempfile::tempdir().unwrap();
    let session_home = tempfile::tempdir().unwrap();
    // The server's own config.json already points at the router (the reported case: the panel
    // said "Connected to <router>" while the project's session was on LM Studio).
    let server_home = tempfile::tempdir().unwrap();
    let mut server = ConfigManager::new(Some(server_home.path()));
    server.config = lm_config();
    server.config.lm_studio.connection = Some("router".into());
    server.config.lm_studio.base_url = format!("{router}/v1");
    server.save().unwrap();
    let server_before = std::fs::read_to_string(server.file_path()).unwrap();

    let sh = server_home.path().to_path_buf();
    let host = Host::start_with(session(work.path(), session_home.path()), move |o| {
        o.config_home = Some(sh);
    })
    .await;
    let pid = host.primary();
    let conn = format!("/api/projects/{pid}/settings/connection");

    // The project's status is its session's, not the server's.
    let (code, st) = call(&host, Method::GET, &conn, None).await;
    assert_eq!(code, StatusCode::OK, "{st}");
    assert_eq!(st["connection"], "server");
    assert_eq!(st["baseUrl"], LM_URL);
    assert!(st["router"].is_null(), "{st}");
    let (_, global) = call(&host, Method::GET, "/api/settings/connection", None).await;
    assert_eq!(global["connection"], "router");
    assert_eq!(global["router"]["name"], "Test router");

    // "Use it" on the router: the session's settings switch.
    let (code, used) = call(
        &host,
        Method::POST,
        &format!("{conn}/use"),
        Some(json!({"url": router})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{used}");
    assert_eq!(used["baseUrl"], format!("{router}/v1"));
    let (_, doc) = call(
        &host,
        Method::GET,
        &format!("/api/projects/{pid}/settings"),
        None,
    )
    .await;
    assert_eq!(
        doc["connection"], "router",
        "the settings page now shows router mode"
    );
    assert_eq!(
        value_of(&doc, "lmStudio.baseUrl"),
        json!(format!("{router}/v1"))
    );
    let (_, st) = call(&host, Method::GET, &conn, None).await;
    assert_eq!(st["connection"], "router");
    assert_eq!(st["router"]["name"], "Test router");
    assert_eq!(st["apiKey"], "placeholder");

    // The default-model picker's list, as the session's key sees it: the placeholder isn't sent.
    let (code, models) = call(&host, Method::GET, &format!("{conn}/models"), None).await;
    assert_eq!(code, StatusCode::OK, "{models}");
    assert_eq!(models["models"][0]["id"], "big-model");
    {
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|(p, _, _)| p == "models"));
        for (path, no_switch, auth) in seen.iter() {
            assert_eq!(no_switch.as_deref(), Some("1"), "{path}");
            assert!(
                auth.is_none(),
                "{path}: the lm-studio placeholder is never sent"
            );
        }
    }
    // Saved to the session's config.json, with what to put back.
    let saved = std::fs::read_to_string(session_home.path().join("config.json")).unwrap();
    assert!(saved.contains("\"connection\": \"router\""), "{saved}");
    assert!(saved.contains("serverMode"), "{saved}");
    // The server defaults are the server scope's business.
    assert_eq!(
        std::fs::read_to_string(server.file_path()).unwrap(),
        server_before
    );

    // Switching back: LM Studio's URL, key and models again.
    let (code, back) = call(
        &host,
        Method::PUT,
        &format!("/api/projects/{pid}/settings"),
        Some(json!({"key": "lmStudio.connection", "value": "server"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{back}");
    let (_, doc) = call(
        &host,
        Method::GET,
        &format!("/api/projects/{pid}/settings"),
        None,
    )
    .await;
    assert_eq!(doc["connection"], "server");
    assert_eq!(value_of(&doc, "lmStudio.baseUrl"), json!(LM_URL));
    assert_eq!(value_of(&doc, "lmStudio.models"), json!(["local-model"]));
    let saved = std::fs::read_to_string(session_home.path().join("config.json")).unwrap();
    assert!(!saved.contains("serverMode"), "{saved}");

    // Not a router: refused with a reason the panel shows.
    let (code, err) = call(
        &host,
        Method::POST,
        &format!("{conn}/use"),
        Some(json!({"url": "http://127.0.0.1:9"})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(
        err["error"].as_str().unwrap().contains("did not answer"),
        "{err}"
    );
    let (code, _) = call(
        &host,
        Method::GET,
        "/api/projects/nope/settings/connection",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn router_mode_in_the_server_scope_saves_config_json_and_switches_back() {
    let (router, _) = fake_router().await;
    let work = tempfile::tempdir().unwrap();
    let session_home = tempfile::tempdir().unwrap();
    let server_home = tempfile::tempdir().unwrap();
    let mut server = ConfigManager::new(Some(server_home.path()));
    server.config = lm_config();
    server.save().unwrap();
    let sh = server_home.path().to_path_buf();
    let host = Host::start_with(session(work.path(), session_home.path()), move |o| {
        o.config_home = Some(sh);
    })
    .await;

    let (code, used) = call(
        &host,
        Method::POST,
        "/api/settings/connection/use",
        Some(json!({"url": router, "model": "big-model"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{used}");
    let (_, doc) = call(&host, Method::GET, "/api/settings", None).await;
    assert_eq!(doc["connection"], "router");
    assert_eq!(value_of(&doc, "lmStudio.models"), json!(["big-model"]));

    let (code, _) = call(
        &host,
        Method::PUT,
        "/api/settings",
        Some(json!({"key": "lmStudio.connection", "value": "server"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, doc) = call(&host, Method::GET, "/api/settings", None).await;
    assert_eq!(doc["connection"], "server");
    assert_eq!(value_of(&doc, "lmStudio.baseUrl"), json!(LM_URL));
    assert_eq!(value_of(&doc, "lmStudio.models"), json!(["local-model"]));
    // The project's session was not touched.
    let pid = host.primary();
    let (_, st) = call(
        &host,
        Method::GET,
        &format!("/api/projects/{pid}/settings/connection"),
        None,
    )
    .await;
    assert_eq!(st["connection"], "server");
}
