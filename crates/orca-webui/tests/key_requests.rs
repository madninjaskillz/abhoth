//! D149 key requests, end to end: the app's server asks the router for a key, the router passes
//! the request to the accounts web server, an admin approves it there, the app's poll collects
//! the key once and saves it, and the router then takes it. Also the panel's key field.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeSession, Host};
use orca_router::access::{Access, AccessSettings};
use orca_router::control_api::{ControlPlane, demo::DemoControl};
use orca_router::server::{self, Mode};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const LINK: &str = "link-token-0123456789abcdef0123456789";

struct Setup {
    web: Host,
    router: String,
    home: tempfile::TempDir,
    _work: tempfile::TempDir,
}

/// A router linked to a web server that is both the accounts server and the app (its own
/// config.json, in a temp dir, is the app's).
async fn setup() -> Setup {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let router = format!("http://{}", l.local_addr().unwrap());
    let snap = orca_router::config::Snapshot::build(Default::default(), None).unwrap();
    let st = orca_router::state::AppState::new(snap, None);
    let st2 = st.clone();
    tokio::spawn(async move {
        server::serve(l, st2, Mode::Gateway, std::future::pending())
            .await
            .unwrap();
    });
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let r = router.clone();
    let h = home.path().to_path_buf();
    let web = Host::start_with(FakeSession::new(work.path()), move |o| {
        o.router = Some(orca_webui::router_admin::RouterLink::new(&r, LINK).unwrap());
        o.config_home = Some(h);
    })
    .await;
    let settings = AccessSettings {
        accounts_url: Some(web.url("")),
        accounts_token: Some("secret:accounts_token".into()),
        loopback_admin: false,
        ..Default::default()
    };
    let _ = st.control.set(Arc::new(ControlPlane {
        access: Access::new(settings, Some(LINK.as_bytes().to_vec())),
        backend: DemoControl::new(),
    }));
    Setup {
        web,
        router,
        home,
        _work: work,
    }
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

fn saved_key(s: &Setup) -> String {
    let mut m = orca_core::config::ConfigManager::new(Some(s.home.path()));
    m.load().unwrap();
    m.config.lm_studio.api_key.clone()
}

const P: &str = "/api/settings/connection";

#[tokio::test(flavor = "multi_thread")]
async fn a_key_request_is_approved_collected_once_saved_and_accepted() {
    let s = setup().await;
    // An old server key is configured: going to router mode keeps it for later, sends none.
    let (c, _) = call(
        &s.web,
        Method::PUT,
        "/api/settings",
        Some(json!({"key": "lmStudio.apiKey", "value": "sk-old-gateway-key"})),
    )
    .await;
    assert_eq!(c, StatusCode::OK);
    let (c, v) = call(
        &s.web,
        Method::POST,
        &format!("{P}/use"),
        Some(json!({"url": s.router})),
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{v}");
    assert_eq!(saved_key(&s), "");
    let (_, st) = call(&s.web, Method::GET, P, None).await;
    assert_eq!(st["apiKey"], "none");
    assert_eq!(st["router"]["auth"]["keyRequests"], true);
    assert_eq!(
        st["router"]["auth"]["required"], true,
        "no anonymous LAN here"
    );
    // Without a key this router refuses the models.
    let (c, _) = call(&s.web, Method::GET, &format!("{P}/models"), None).await;
    assert_eq!(c, StatusCode::BAD_GATEWAY);

    // Nothing to poll yet.
    let (_, v) = call(&s.web, Method::GET, &format!("{P}/key-request"), None).await;
    assert_eq!(v["status"], "none");
    let (c, v) = call(&s.web, Method::POST, &format!("{P}/key-request"), None).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    let code = v["request"]["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 9);
    let (_, st) = call(&s.web, Method::GET, P, None).await;
    assert_eq!(
        st["keyRequest"]["code"],
        code.as_str(),
        "the panel shows it again after a reload"
    );

    // The admin sees it, by its code.
    let (_, list) = call(&s.web, Method::GET, "/api/admin/key-requests", None).await;
    let reqs = list["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["code"], code.as_str());
    assert!(
        reqs[0]["client"]
            .as_str()
            .unwrap()
            .starts_with("Abhoth on ")
    );
    assert_eq!(reqs[0]["ip"], "127.0.0.1");
    let id = reqs[0]["id"].as_str().unwrap().to_string();
    assert!(!list.to_string().contains("poll"), "no poll secret: {list}");

    let (_, v) = call(&s.web, Method::GET, &format!("{P}/key-request"), None).await;
    assert_eq!(v["status"], "pending");

    // A name with a space is not a key name.
    let (c, ok) = call(
        &s.web,
        Method::POST,
        &format!("/api/admin/key-requests/{id}/approve"),
        Some(json!({"name": "My Laptop", "claude": true})),
    )
    .await;
    assert_eq!(c, StatusCode::BAD_REQUEST, "{ok}");
    let (c, ok) = call(
        &s.web,
        Method::POST,
        &format!("/api/admin/key-requests/{id}/approve"),
        Some(json!({"name": "laptop", "claude": true})),
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{ok}");
    assert_eq!(ok["name"], "laptop");
    assert!(!ok.to_string().contains("abk_"), "the admin never sees it");
    assert_eq!(
        ok["scopes"],
        json!(["chat", "leases", "models", "group:claude"])
    );

    // The app's poll collects and saves it; the page never sees it.
    let (c, v) = call(&s.web, Method::GET, &format!("{P}/key-request"), None).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "approved");
    assert_eq!(v["claude"], true);
    assert!(!v.to_string().contains("abk_"), "{v}");
    let key = saved_key(&s);
    assert!(key.starts_with("abk_") && key.len() > 30);
    let (_, v) = call(&s.web, Method::GET, &format!("{P}/key-request"), None).await;
    assert_eq!(v["status"], "none", "collected once");

    // The router takes it.
    let (c, m) = call(&s.web, Method::GET, &format!("{P}/models"), None).await;
    assert_eq!(c, StatusCode::OK, "{m}");
    assert!(
        m["principal"]
            .as_str()
            .unwrap()
            .starts_with("service:laptop#"),
        "{m}"
    );
    let (_, st) = call(&s.web, Method::GET, P, None).await;
    assert_eq!(st["apiKey"], "abhoth");
    // It is a service key like any other: listed, revocable.
    let (_, keys) = call(&s.web, Method::GET, "/api/admin/service-keys", None).await;
    assert_eq!(keys["keys"][0]["name"], "laptop");
    assert!(
        keys["keys"][0]["note"]
            .as_str()
            .unwrap()
            .starts_with("Requested by Abhoth on ")
    );

    // The audit has every step and never the key.
    let (_, audit) = call(&s.web, Method::GET, "/api/admin/audit?limit=200", None).await;
    let text = audit.to_string();
    for ev in [
        "keyrequest.created",
        "keyrequest.approved",
        "keyrequest.collected",
    ] {
        assert!(text.contains(ev), "{ev} missing: {text}");
    }
    assert!(!text.contains(&key), "a key in the audit");

    // Switching back to server mode puts the old key back.
    let (c, _) = call(
        &s.web,
        Method::PUT,
        "/api/settings",
        Some(json!({"key": "lmStudio.connection", "value": "server"})),
    )
    .await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(saved_key(&s), "sk-old-gateway-key");
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_requests_say_so_and_the_link_is_needed_inside() {
    let s = setup().await;
    let (c, _) = call(&s.web, Method::POST, &format!("{P}/key-request"), None).await;
    assert_eq!(c, StatusCode::CONFLICT, "only in router mode");
    call(
        &s.web,
        Method::POST,
        &format!("{P}/use"),
        Some(json!({"url": s.router})),
    )
    .await;
    let (c, _) = call(&s.web, Method::POST, &format!("{P}/key-request"), None).await;
    assert_eq!(c, StatusCode::OK);
    let (_, list) = call(&s.web, Method::GET, "/api/admin/key-requests", None).await;
    let id = list["requests"][0]["id"].as_str().unwrap().to_string();
    let (c, _) = call(
        &s.web,
        Method::POST,
        &format!("/api/admin/key-requests/{id}/deny"),
        None,
    )
    .await;
    assert_eq!(c, StatusCode::OK);
    let (c, _) = call(
        &s.web,
        Method::POST,
        &format!("/api/admin/key-requests/{id}/approve"),
        Some(json!({})),
    )
    .await;
    assert_eq!(c, StatusCode::NOT_FOUND, "denied is final");
    let (_, v) = call(&s.web, Method::GET, &format!("{P}/key-request"), None).await;
    assert_eq!(v["status"], "denied");
    assert_eq!(saved_key(&s), "");
    let (_, keys) = call(&s.web, Method::GET, "/api/admin/service-keys", None).await;
    assert!(
        keys["keys"].as_array().unwrap().is_empty(),
        "nothing was made"
    );

    // The internal paths want the link token.
    let r = reqwest::Client::new()
        .post(s.web.url("/api/internal/key-requests"))
        .json(&json!({"client": "x", "ip": "1.2.3.4"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = reqwest::Client::new()
        .post(s.web.url("/api/internal/key-requests/poll"))
        .bearer_auth("wrong")
        .json(&json!({"id": id, "poll": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_field_saves_only_abhoth_keys_and_removes_them() {
    let s = setup().await;
    call(
        &s.web,
        Method::POST,
        &format!("{P}/use"),
        Some(json!({"url": s.router})),
    )
    .await;
    let (c, v) = call(
        &s.web,
        Method::POST,
        &format!("{P}/key"),
        Some(json!({"key": "sk-ant-not-ours-0123456789"})),
    )
    .await;
    assert_eq!(c, StatusCode::BAD_REQUEST, "{v}");
    assert!(!v.to_string().contains("not-ours"), "the key is not echoed");
    let k = "abk_0123456789abcdefghijklmnop";
    let (c, v) = call(
        &s.web,
        Method::POST,
        &format!("{P}/key"),
        Some(json!({"key": format!(" {k} ")})),
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{v}");
    assert_eq!(saved_key(&s), k);
    let (_, st) = call(&s.web, Method::GET, P, None).await;
    assert_eq!(st["apiKey"], "abhoth");
    assert!(!st.to_string().contains(k));
    let (c, _) = call(&s.web, Method::DELETE, &format!("{P}/key"), None).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(saved_key(&s), "");
}
