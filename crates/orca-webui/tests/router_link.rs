//! The web server and `abhoth router` together (ROUTER-CONFIG.md 11.1-11.2): service keys
//! issued here and checked there by introspection, the admin pages' proxy with the signed
//! acting user, and revocation through the router's cache.

mod common;

use std::sync::Arc;

use common::{FakeSession, Host};
use orca_router::access::{Access, AccessSettings};
use orca_router::control_api::{ControlPlane, demo::DemoControl};
use orca_router::server::{self, Mode};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const LINK: &str = "link-token-0123456789abcdef0123456789";

struct Pair {
    web: Host,
    router: String,
    st: Arc<orca_router::state::AppState>,
    _work: tempfile::TempDir,
}

async fn pair() -> Pair {
    // The router first (its port goes into the web server's link), its control plane after.
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
    let r = router.clone();
    let web = Host::start_with(FakeSession::new(work.path()), move |o| {
        o.router = Some(orca_webui::router_admin::RouterLink::new(&r, LINK).unwrap());
    })
    .await;
    let settings = AccessSettings {
        accounts_url: Some(web.url("")),
        accounts_token: Some("secret:accounts_token".into()),
        // tests call from loopback: that must not stand for an admin here
        loopback_admin: false,
        ..Default::default()
    };
    let _ = st.control.set(Arc::new(ControlPlane {
        access: Access::new(settings, Some(LINK.as_bytes().to_vec())),
        backend: DemoControl::new(),
    }));
    Pair {
        web,
        router,
        st,
        _work: work,
    }
}

async fn at_router(p: &Pair, m: Method, path: &str, key: &str) -> (StatusCode, Value) {
    let r = reqwest::Client::new()
        .request(m, format!("{}{path}", p.router))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    let s = r.status();
    (s, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn service_keys_work_at_the_router_only_and_revoke_within_the_cache() {
    let p = pair().await;
    let r = p
        .web
        .api(Method::POST, "/api/admin/service-keys")
        .json(&json!({"name": "HA", "scopes": ["switch"], "note": "Home Assistant buttons"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let k: Value = r.json().await.unwrap();
    let key = k["key"].as_str().unwrap().to_string();
    assert_eq!(
        (k["name"].as_str(), k["kind"].as_str()),
        (Some("ha"), Some("service"))
    );
    // listed without the secret; a second "ha" is refused; scopes are checked
    let list: Value = p
        .web
        .api(Method::GET, "/api/admin/service-keys")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["keys"][0]["owner"], "admin");
    assert!(list["keys"][0].get("key").is_none());
    let dup = p
        .web
        .api(Method::POST, "/api/admin/service-keys")
        .json(&json!({"name": "ha", "scopes": ["chat"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    let bad = p
        .web
        .api(Method::POST, "/api/admin/service-keys")
        .json(&json!({"name": "x", "scopes": ["admin"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // the account page doesn't list it as the admin's own key
    let own: Value = p
        .web
        .api(Method::GET, "/api/account/keys")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(own.as_array().map(|a| a.len()), Some(0));

    // the router asks the web server: HA may plan, not read the config
    let (s, plan) = at_router(
        &p,
        Method::GET,
        "/admin/api/plan?server=gpu-host&layout=flash-x2",
        &key,
    )
    .await;
    assert_eq!(s, 200, "{plan}");
    assert_eq!(at_router(&p, Method::GET, "/admin/api/config", &key).await.0, 403);
    let (_, who) = at_router(&p, Method::GET, "/admin/api/whoami", &key).await;
    assert_eq!(who["principal"], format!("service:ha#{}", k["id"]));
    // ... and the web UI refuses the key outright
    let r = p
        .web
        .http()
        .get(p.web.url("/api/info"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // revoked: the router's cache keeps it for up to a minute, then it's gone
    let del = p
        .web
        .api(
            Method::DELETE,
            &format!("/api/admin/service-keys/{}", k["id"]),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    assert_eq!(at_router(&p, Method::GET, "/admin/api/whoami", &key).await.0, 200);
    p.st
        .control
        .get()
        .unwrap()
        .access
        .keys
        .as_ref()
        .unwrap()
        .clear();
    assert_eq!(
        at_router(
            &p,
            Method::GET,
            "/admin/api/plan?server=gpu-host&layout=flash-x2",
            &key
        )
        .await
        .0,
        401
    );
}

#[tokio::test]
async fn user_keys_are_users_at_the_router() {
    let p = pair().await;
    let k: Value = p
        .web
        .api(Method::POST, "/api/account/keys")
        .json(&json!({"name": "cli"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_key = k["key"].as_str().unwrap().to_string();
    let (s, v) = at_router(&p, Method::GET, "/admin/api/state", &admin_key).await;
    assert_eq!(s, 200, "the admin's own key is an admin at the router: {v}");
    // a user who is not an admin: the model view only
    let r = p
        .web
        .api(Method::POST, "/api/admin/users")
        .json(&json!({"username": "bob", "password": "bob password 1234", "admin": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let db = &p.web.server.state().login.db;
    let bob_id = db.user_by_name("bob").unwrap().id;
    let bob_key = "abk_bobs_key_0123456789abcdef";
    db.create_api_key(bob_id, "laptop", bob_key, "abk_bo", 0)
        .unwrap();
    assert_eq!(at_router(&p, Method::GET, "/admin/api/state", bob_key).await.0, 403);
    let (s, v) = at_router(&p, Method::GET, "/v1/abhoth/view", bob_key).await;
    assert_eq!(s, 200);
    assert_eq!(v["actions"]["switch"], false);
    assert!(
        v["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "claude-sonnet")
    );
}

#[tokio::test]
async fn the_admin_pages_reach_the_router_as_the_signed_in_admin() {
    let p = pair().await;
    let r = p
        .web
        .api(Method::GET, "/api/admin/router/state")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let st: Value = r.json().await.unwrap();
    assert_eq!(st["servers"][0]["id"], "gpu-host");
    let r = p
        .web
        .api(Method::PATCH, "/api/admin/router/routes/dense-stock")
        .json(&json!({"cold": "fallback", "fallback": ["qwen3.8-27b"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let audit: Value = p
        .web
        .api(Method::GET, "/api/admin/router/audit")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit[0]["action"], "route.edit");
    assert_eq!(audit[0]["who"], "web:admin#1");
    // the web server's own audit has the proxied change
    let own: Value = p
        .web
        .api(Method::GET, "/api/admin/audit")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        own.as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "router.admin")
    );
    // the SSE stream passes through
    let r = p
        .web
        .api(Method::GET, "/api/admin/router/events")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "text/event-stream");
}

#[tokio::test]
async fn introspection_needs_the_link_token() {
    let p = pair().await;
    let c = p.web.http();
    let url = p.web.url(orca_core::router_link::INTROSPECT_PATH);
    let body = json!({"key_sha256": orca_core::router_link::key_digest("abk_x")});
    assert_eq!(c.post(&url).json(&body).send().await.unwrap().status(), 401);
    assert_eq!(
        c.post(&url)
            .bearer_auth("wrong")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let r = c
        .post(&url)
        .bearer_auth(LINK)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["active"], false);
    // a GET goes through the sign-in check like any other call
    assert_eq!(c.get(&url).send().await.unwrap().status(), 401);
}
