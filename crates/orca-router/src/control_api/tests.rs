use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderValue, Method};
use orca_core::router_link::{ActingUser, Introspection, Role, key_digest, sign_acting_user};

use super::demo::{DEMO_CONFIG, DemoControl, demo_check};
use super::*;
use crate::access::keys::{CacheTimes, Introspector};
use crate::access::{AccessSettings, KeyCache};

struct Table(Mutex<HashMap<String, Introspection>>);

#[async_trait::async_trait]
impl Introspector for Table {
    async fn introspect(&self, d: &str) -> Result<Introspection, String> {
        Ok(self.0.lock().unwrap().get(d).cloned().unwrap_or_default())
    }
}

const ADMIN: &str = "abk_admin_aaaaaaaaaaaaaaaaaaaa";
const USER: &str = "abk_user_bbbbbbbbbbbbbbbbbbbbbb";
const HA: &str = "abk_svc_cccccccccccccccccccccc";
const DASH: &str = "abk_svc_dddddddddddddddddddddd";
const TOKEN: &[u8] = b"link-token-link-token-link-token-xx";

fn key(uid: i64, role: Role, kid: i64, name: &str, scopes: &[&str]) -> Introspection {
    Introspection {
        active: true,
        user_id: Some(uid),
        name: Some("someone".into()),
        role: Some(role),
        key_id: Some(kid),
        key_name: Some(name.into()),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        disabled: false,
    }
}

fn plane() -> ControlPlane {
    let mut t = HashMap::new();
    t.insert(key_digest(ADMIN), key(1, Role::Admin, 1, "cli", &[]));
    t.insert(key_digest(USER), key(2, Role::User, 2, "laptop", &[]));
    t.insert(key_digest(HA), key(1, Role::Service, 3, "ha", &["switch"]));
    t.insert(
        key_digest(DASH),
        key(1, Role::Service, 4, "dash", &["read"]),
    );
    let cache = KeyCache::new(
        Arc::new(Table(Mutex::new(t))),
        CacheTimes::default(),
        Arc::new(|| 0),
    );
    let demo = DemoControl::new();
    ControlPlane {
        access: crate::access::Access::with_parts(
            AccessSettings {
                accounts_url: Some("http://web".into()),
                accounts_token: Some("secret:accounts_token".into()),
                loopback_admin: false,
                ..Default::default()
            },
            Some(TOKEN.to_vec()),
            Some(cache),
            Arc::new(|| chrono::Utc::now().timestamp()),
        ),
        backend: demo,
    }
}

fn hk(k: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {k}")).unwrap(),
    );
    h
}

async fn call(cp: &ControlPlane, m: Method, pq: &str, h: &HeaderMap, body: Value) -> (u16, Value) {
    let raw = if body.is_null() {
        Bytes::new()
    } else {
        Bytes::from(body.to_string())
    };
    let r = handle(cp, &m, pq, h, Some("192.0.2.1".parse().unwrap()), &raw).await;
    let status = r.status().as_u16();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn only_admins_reach_the_admin_api() {
    let cp = plane();
    let none = HeaderMap::new();
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/state", &none, Value::Null)
            .await
            .0,
        401
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/state", &hk(USER), Value::Null)
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            &cp,
            Method::GET,
            "/admin/api/config",
            &hk(USER),
            Value::Null
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &cp,
            Method::GET,
            "/admin/api/state",
            &hk("abk_bad_eeeeeeeeeeeeeeeeeeeeee"),
            Value::Null
        )
        .await
        .0,
        401
    );
    let (s, v) = call(
        &cp,
        Method::GET,
        "/admin/api/state",
        &hk(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["servers"][0]["cards"][0]["id"], "A");
    // whoami works for anyone and says who
    let (_, v) = call(
        &cp,
        Method::GET,
        "/admin/api/whoami",
        &hk(USER),
        Value::Null,
    )
    .await;
    assert_eq!(v, json!({"principal": "user:someone#2", "admin": false}));
}

#[tokio::test]
async fn service_keys_get_exactly_their_scopes() {
    let cp = plane();
    // dash: read state and jobs, never config, audit or anything that changes
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/state", &hk(DASH), Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/jobs", &hk(DASH), Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        call(
            &cp,
            Method::GET,
            "/admin/api/config",
            &hk(DASH),
            Value::Null
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/audit", &hk(DASH), Value::Null)
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            &cp,
            Method::POST,
            "/admin/api/switch",
            &hk(DASH),
            json!({"server": "gpu-host", "layout": "flash-x2"})
        )
        .await
        .0,
        403
    );
    // HA: may plan and switch, not edit config or release leases
    assert_eq!(
        call(
            &cp,
            Method::GET,
            "/admin/api/plan?server=gpu-host&layout=flash-x2",
            &hk(HA),
            Value::Null
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &cp,
            Method::PUT,
            "/admin/api/config",
            &hk(HA),
            json!({"text": "x"})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &cp,
            Method::DELETE,
            "/admin/api/leases/bench-lead",
            &hk(HA),
            Value::Null
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn the_web_servers_signed_admin_is_an_admin_for_that_call_only() {
    let cp = plane();
    let u = ActingUser {
        user_id: 1,
        name: "admin".into(),
        admin: true,
    };
    let now = chrono::Utc::now().timestamp();
    let mut h = HeaderMap::new();
    h.insert(
        orca_core::router_link::ACTING_USER_HEADER,
        HeaderValue::from_str(&sign_acting_user(TOKEN, &u, now, "GET", "/admin/api/audit"))
            .unwrap(),
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/audit", &h, Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/config", &h, Value::Null)
            .await
            .0,
        401
    );
    let bob = ActingUser {
        user_id: 2,
        name: "bob".into(),
        admin: false,
    };
    let mut h = HeaderMap::new();
    h.insert(
        orca_core::router_link::ACTING_USER_HEADER,
        HeaderValue::from_str(&sign_acting_user(
            TOKEN,
            &bob,
            now,
            "GET",
            "/admin/api/state",
        ))
        .unwrap(),
    );
    assert_eq!(
        call(&cp, Method::GET, "/admin/api/state", &h, Value::Null)
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn config_saves_need_the_current_rev_and_a_clean_check() {
    let cp = plane();
    let a = hk(ADMIN);
    let (_, doc) = call(&cp, Method::GET, "/admin/api/config", &a, Value::Null).await;
    let rev = doc["rev"].as_str().unwrap().to_string();
    let edited = DEMO_CONFIG.replace(
        "cold    = \"reject\"",
        "cold    = \"fallback\"\nfallback = [\"qwen3.8-27b\"]",
    );
    // no If-Match
    assert_eq!(
        call(
            &cp,
            Method::PUT,
            "/admin/api/config",
            &a,
            json!({"text": edited})
        )
        .await
        .0,
        428
    );
    let mut h = a.clone();
    h.insert("if-match", HeaderValue::from_str("\"stale\"").unwrap());
    let (s, v) = call(
        &cp,
        Method::PUT,
        "/admin/api/config",
        &h,
        json!({"text": edited}),
    )
    .await;
    assert_eq!((s, v["currentRev"].as_str()), (412, Some(rev.as_str())));
    // an inline key is refused with the line
    let bad = format!("{DEMO_CONFIG}\n[upstreams.x]\nkey = \"sk-live-123\"\n");
    let (_, chk) = call(
        &cp,
        Method::POST,
        "/admin/api/config/check",
        &a,
        json!({"text": bad}),
    )
    .await;
    assert_eq!(chk["ok"], false);
    assert!(
        chk["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == "upstreams.x.key" && p["line"].as_u64().is_some())
    );
    let mut h = a.clone();
    h.insert(
        "if-match",
        HeaderValue::from_str(&format!("\"{rev}\"")).unwrap(),
    );
    assert_eq!(
        call(
            &cp,
            Method::PUT,
            "/admin/api/config",
            &h,
            json!({"text": bad})
        )
        .await
        .0,
        422
    );
    let (s, v) = call(
        &cp,
        Method::PUT,
        "/admin/api/config",
        &h,
        json!({"text": edited, "reason": "test"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["diff"]
            .as_str()
            .unwrap()
            .contains("+fallback = [\"qwen3.8-27b\"]")
    );
    let (_, audit) = call(&cp, Method::GET, "/admin/api/audit", &a, Value::Null).await;
    assert_eq!(audit[0]["action"], "config.save");
    assert_eq!(audit[0]["who"], "user:someone#1");
    assert_eq!(audit[0]["reason"], "test");
}

#[tokio::test]
async fn plans_show_blockers_and_switches_refuse_on_them() {
    let cp = plane();
    let a = hk(ADMIN);
    let (s, p) = call(
        &cp,
        Method::GET,
        "/admin/api/plan?server=gpu-host&layout=merged-inkling-x2",
        &a,
        Value::Null,
    )
    .await;
    assert_eq!((s, p["note"].as_str()), (200, Some("already live")));
    let (_, p) = call(
        &cp,
        Method::GET,
        "/admin/api/plan?server=gpu-host&layout=merged-a-inkling-b",
        &a,
        Value::Null,
    )
    .await;
    assert_eq!(p["cards"][0]["action"], "restart");
    assert_eq!(p["cards"][1]["action"], "keep");
    assert_eq!(
        p["blockers"][0]["kind"], "lease",
        "a lease on A refuses an A switch"
    );
    assert_eq!(p["runnable"], false);
    let (s, v) = call(
        &cp,
        Method::POST,
        "/admin/api/switch",
        &a,
        json!({"server": "gpu-host", "layout": "merged-a-inkling-b"}),
    )
    .await;
    assert_eq!(s, 409, "{v}");
    // release A's leases, then it runs
    for k in ["bench-lead", "agent-7f3"] {
        assert_eq!(
            call(
                &cp,
                Method::DELETE,
                &format!("/admin/api/leases/{k}"),
                &a,
                Value::Null
            )
            .await
            .0,
            200
        );
    }
    let (s, j) = call(
        &cp,
        Method::POST,
        "/admin/api/switch",
        &a,
        json!({"server": "gpu-host", "layout": "merged-a-inkling-b"}),
    )
    .await;
    assert_eq!((s, j["state"].as_str()), (202, Some("running")));
    let (s, _) = call(
        &cp,
        Method::GET,
        &format!("/admin/api/jobs/{}", j["id"].as_str().unwrap()),
        &a,
        Value::Null,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        call(
            &cp,
            Method::GET,
            "/admin/api/plan?server=nope&layout=x",
            &a,
            Value::Null
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn route_edits_are_validated() {
    let cp = plane();
    let a = hk(ADMIN);
    let patch = |name: &str, v: Value| {
        let cp = &cp;
        let a = a.clone();
        let p = format!("/admin/api/routes/{name}");
        async move { call(cp, Method::PATCH, &p, &a, v).await }
    };
    assert_eq!(
        patch("qwen3.8-27b", json!({"cold": "on_demand"})).await.0,
        422,
        "pool routes never switch"
    );
    assert_eq!(
        patch("dense-stock", json!({"cold": "fallback"})).await.0,
        422,
        "fallback needs a route"
    );
    assert_eq!(
        patch(
            "dense-stock",
            json!({"cold": "fallback", "fallback": ["nope"]})
        )
        .await
        .0,
        422
    );
    let (s, r) = patch(
        "dense-stock",
        json!({"cold": "fallback", "fallback": ["qwen3.8-27b"], "access": "keys:jarvis"}),
    )
    .await;
    assert_eq!(
        (s, r["cold"].as_str(), r["access"].as_str()),
        (200, Some("fallback"), Some("keys:jarvis"))
    );
    assert_eq!(
        patch("dense-stock", json!({"cold": "on_demand"})).await.0,
        200
    );
    assert_eq!(patch("missing", json!({})).await.0, 404);
}

#[tokio::test]
async fn the_event_stream_starts_with_the_state() {
    let cp = plane();
    let r = handle(
        &cp,
        &Method::GET,
        "/admin/api/events",
        &hk(ADMIN),
        None,
        &Bytes::new(),
    )
    .await;
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    use futures_util::StreamExt;
    let mut body = r.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&first);
    assert!(text.starts_with("event: state\ndata: {"), "{text}");
}

#[test]
fn metrics_cover_cards_engines_guards_and_keys() {
    let cp = plane();
    let m = metrics_text(&cp);
    for needle in [
        "abhoth_gpu_temperature_celsius{server=\"gpu-host\",card=\"A\"",
        "abhoth_gpu_sm_clock_max_mhz{server=\"gpu-host\",card=\"A\"",
        "abhoth_engine_state{server=\"gpu-host\",card=\"A\",engine=\"ninfer\",state=\"ready\"} 1",
        "abhoth_guard_active{guard=\"bench\"} 1",
        "abhoth_layout_active_info{server=\"gpu-host\",layout=\"merged-inkling-x2\"} 1",
        "abhoth_key_checks_total{outcome=\"cache_hit\"} 0",
        "# TYPE abhoth_gpu_power_watts gauge",
    ] {
        assert!(m.contains(needle), "missing {needle}\n{m}");
    }
}

#[test]
fn the_demo_check_places_problems() {
    assert!(
        demo_check(DEMO_CONFIG).is_empty(),
        "{:?}",
        demo_check(DEMO_CONFIG)
    );
    let p = demo_check("version = 1\n[routes.x\nname = \"open\n");
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].line, Some(2));
}

#[tokio::test]
async fn users_get_a_filtered_view() {
    let cp = Arc::new(plane());
    let view = |k: &str| {
        let h = hk(k);
        let cp = cp.clone();
        async move {
            let r = user_view::handle(
                &cp,
                "/v1/abhoth/view",
                &h,
                Some("192.0.2.1".parse().unwrap()),
            )
            .await;
            let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
            serde_json::from_slice::<Value>(&b).unwrap()
        }
    };
    let u = view(USER).await;
    let names: Vec<&str> = u["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"qwen3.8-27b") && !names.contains(&"claude-sonnet"),
        "{names:?}"
    );
    assert_eq!(u["actions"]["switch"], false);
    assert!(
        u.get("layouts").is_none() && u.get("cards").is_none(),
        "{u}"
    );
    assert!(
        u["models"][0].get("serving").is_none(),
        "no placement detail for users"
    );
    let dense = u["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "dense-stock")
        .unwrap();
    assert_eq!(dense["status"], "cold");
    // a switch key sees layouts, not cards; an admin sees everything
    let ha = view(HA).await;
    assert_eq!(ha["actions"]["switch"], true);
    assert!(ha["layouts"].as_array().unwrap().len() >= 3 && ha.get("cards").is_none());
    assert_eq!(
        ha["models"].as_array().map(|a| a.len()),
        Some(0),
        "switch-only keys have no chat"
    );
    let admin = view(ADMIN).await;
    assert!(admin["cards"].as_array().unwrap().len() >= 2);
    assert!(
        admin["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "claude-sonnet")
    );
    // no key from outside the LAN: nothing
    let r = user_view::handle(
        &cp,
        "/v1/abhoth/view",
        &HeaderMap::new(),
        Some("192.0.2.1".parse().unwrap()),
    )
    .await;
    assert_eq!(r.status().as_u16(), 401);
}
