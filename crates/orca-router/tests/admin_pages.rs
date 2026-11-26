//! The router's own admin pages (D151), over HTTP on a real listener: the page, sign-in with a
//! router admin key (session cookie, CSRF, rate limit, revocation), the router.toml forms' edits
//! and saves, router.json read / check / save (backup, reload, audit), and that a proxied
//! request from loopback is not an admin.

use std::path::PathBuf;

use orca_router::config::Snapshot;
use orca_router::server::{self, Mode};
use orca_router::state::AppState;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

const TOML: &str = r#"# router.toml for the admin pages' tests (placeholders only)
version = 1

[router]
listen = "127.0.0.1:0"
state_dir = "STATE"

[control]
enabled = false

[secrets]
ssh_gpu = { file = "/run/secrets/ssh-gpu" }

[servers.gpu]
title = "GPU host"   # stays
host = "gpu-host"
exec = "ssh"
ssh = { user = "abhoth-exec", key = "secret:ssh_gpu", host_key = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }

[cards.A]
server = "gpu"
gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
vram_gib = 32.0
"#;

const JSON: &str = r#"{
  "upstreams": { "A": { "url": "http://127.0.0.1:9", "lanes": 2 } },
  "deployments": [ { "name": "dense", "cards": ["A"], "servedId": "dense-27b", "modelInfo": {} } ]
}
"#;

struct Router {
    url: String,
    dir: tempfile::TempDir,
    toml: PathBuf,
}

async fn start() -> Router {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let toml = dir.path().join("router.toml");
    std::fs::write(
        &toml,
        TOML.replace("STATE", &state.display().to_string().replace('\\', "/")),
    )
    .unwrap();
    let json = dir.path().join("router.json");
    std::fs::write(&json, JSON).unwrap();
    let st = AppState::new(Snapshot::load(&json).unwrap(), Some(json.clone()));
    let started = orca_router::control::startup::start(&st, &toml)
        .await
        .unwrap();
    assert!(
        started.message.contains("admin pages on"),
        "{}",
        started.message
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve(l, st, Mode::Gateway, std::future::pending()));
    Router { url, dir, toml }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// As if through a reverse proxy (so loopback does not make the caller an admin).
fn proxied() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.5"));
    h
}

/// `abhoth router admin-key create NAME --control FILE --out FILE`, read back.
async fn make_key(r: &Router, name: &str) -> String {
    let out = r.dir.path().join(format!("{name}.key"));
    let code = orca_router::cli::run(
        [
            "admin-key",
            "create",
            name,
            "--control",
            &r.toml.display().to_string(),
            "--out",
            &out.display().to_string(),
        ]
        .map(String::from)
        .to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    std::fs::read_to_string(out).unwrap().trim().to_string()
}

async fn login(r: &Router, key: &str, extra: &[(&str, &str)]) -> reqwest::Response {
    let mut h = proxied();
    for (k, v) in extra {
        h.insert(
            reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    http()
        .post(format!("{}/admin/login", r.url))
        .headers(h)
        .json(&json!({"key": key}))
        .send()
        .await
        .unwrap()
}

/// A signed-in session: (cookie header, CSRF header value).
async fn session(r: &Router, key: &str) -> (String, String) {
    let resp = login(r, key, &[]).await;
    assert_eq!(resp.status(), 200);
    let cookie = resp.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    assert!(!cookie.contains("Secure"), "plain http: no Secure");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["csrfHeader"], "x-router-csrf");
    (
        cookie.split(';').next().unwrap().to_string(),
        v["csrf"].as_str().unwrap().to_string(),
    )
}

fn with(c: &(String, String), csrf: bool) -> HeaderMap {
    let mut h = proxied();
    h.insert("cookie", HeaderValue::from_str(&c.0).unwrap());
    if csrf {
        h.insert("x-router-csrf", HeaderValue::from_str(&c.1).unwrap());
    }
    h
}

#[tokio::test]
async fn the_page_is_served_with_relative_urls_and_a_strict_policy() {
    let r = start().await;
    let resp = http().get(format!("{}/admin", r.url)).send().await.unwrap();
    assert_eq!(resp.status(), 308);
    assert_eq!(resp.headers()["location"], "admin/");
    let resp = http()
        .get(format!("{}/admin/", r.url))
        .send()
        .await
        .unwrap();
    if !orca_router::admin_ui::bundled() {
        assert_eq!(resp.status(), 503, "built without client/dist-router");
        return;
    }
    assert_eq!(resp.status(), 200);
    let csp = resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'self'") && csp.contains("default-src 'self'"));
    let html = resp.text().await.unwrap();
    assert!(
        html.contains("src=\"./assets/"),
        "relative asset URLs: {html}"
    );
    let asset = html
        .split("src=\"./")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let a = http()
        .get(format!("{}/admin/{asset}", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 200);
    assert!(
        a.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let bad = http()
        .get(format!("{}/admin/assets/../../router.toml", r.url))
        .send()
        .await
        .unwrap();
    assert_ne!(bad.status(), 200);
}

#[tokio::test]
async fn loopback_is_admin_but_not_through_a_proxy() {
    let r = start().await;
    let s: Value = http()
        .get(format!("{}/admin/session", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (s["available"].clone(), s["admin"].clone(), s["via"].clone()),
        (json!(true), json!(true), json!("loopback"))
    );
    assert_eq!(s["hasKeys"], false);
    assert_eq!(s["controlPlane"], false);
    let s: Value = http()
        .get(format!("{}/admin/session", r.url))
        .headers(proxied())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (s["signedIn"].clone(), s["admin"].clone()),
        (json!(false), json!(false))
    );
    let st = http()
        .get(format!("{}/admin/api/config", r.url))
        .headers(proxied())
        .send()
        .await
        .unwrap();
    assert_eq!(st.status(), 401);
}

#[tokio::test]
async fn sign_in_session_csrf_and_revocation() {
    let r = start().await;
    let key = make_key(&r, "ops").await;
    assert!(key.starts_with("abk_"));
    // Wrong key, not JSON
    assert_eq!(
        login(&r, "abk_00000000000000000000000000000000", &[])
            .await
            .status(),
        401
    );
    let form = http()
        .post(format!("{}/admin/login", r.url))
        .headers(proxied())
        .body(format!("key={key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 415);
    // HTTPS behind a proxy: Secure
    let secure = login(&r, &key, &[("x-forwarded-proto", "https")]).await;
    assert!(
        secure.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Secure")
    );

    let c = session(&r, &key).await;
    let s: Value = http()
        .get(format!("{}/admin/session", r.url))
        .headers(with(&c, false))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["via"], "session");
    assert_eq!(s["principal"], "router-key:ops#1");
    assert_eq!(s["csrf"], c.1);

    // A read needs no CSRF token; a change does.
    let rd = http()
        .get(format!("{}/admin/api/config", r.url))
        .headers(with(&c, false))
        .send()
        .await
        .unwrap();
    assert_eq!(rd.status(), 200);
    let no_csrf = http()
        .post(format!("{}/admin/api/admin-keys", r.url))
        .headers(with(&c, false))
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 401);
    let made: Value = http()
        .post(format!("{}/admin/api/admin-keys", r.url))
        .headers(with(&c, true))
        .json(&json!({"name": "laptop"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(made["key"].as_str().unwrap().starts_with("abk_"));
    let list: Value = http()
        .get(format!("{}/admin/api/admin-keys", r.url))
        .headers(with(&c, false))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["keys"].as_array().unwrap().len(), 2);
    assert!(
        !list.to_string().contains(made["key"].as_str().unwrap()),
        "never listed"
    );

    // A bearer admin key works without a session (scripts, the Abhoth app's proxy).
    let mut h = proxied();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", made["key"].as_str().unwrap())).unwrap(),
    );
    let who: Value = http()
        .get(format!("{}/admin/api/whoami", r.url))
        .headers(h)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["admin"], true);

    // Revoke the key the session was made from: the session ends.
    let rv = http()
        .delete(format!("{}/admin/api/admin-keys/1", r.url))
        .headers(with(&c, true))
        .send()
        .await
        .unwrap();
    assert_eq!(rv.status(), 200);
    let after = http()
        .get(format!("{}/admin/api/config", r.url))
        .headers(with(&c, false))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);

    // The audit has the sign-ins, the refusal and the keys (never a key).
    let mut h = proxied();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", made["key"].as_str().unwrap())).unwrap(),
    );
    let audit: Value = http()
        .get(format!("{}/admin/api/audit", r.url))
        .headers(h)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = audit.to_string();
    for a in [
        "admin.sign-in",
        "admin.sign-in-refused",
        "admin-key.create",
        "admin-key.revoke",
    ] {
        assert!(text.contains(a), "{a} in {text}");
    }
    assert!(!text.contains(&key) && !text.contains(made["key"].as_str().unwrap()));

    // Sign-in is rate limited per address.
    let mut last = 0;
    for _ in 0..12 {
        last = login(&r, "abk_ffffffffffffffffffffffffffffffff", &[])
            .await
            .status()
            .as_u16();
    }
    assert_eq!(last, 429);
}

async fn admin_headers(r: &Router) -> HeaderMap {
    let key = make_key(r, "ops").await;
    let mut h = proxied();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
    );
    h
}

#[tokio::test]
async fn form_edits_are_applied_checked_and_saved_like_text() {
    let r = start().await;
    let h = admin_headers(&r).await;
    let data: Value = http()
        .get(format!("{}/admin/api/config/json", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["data"]["cards"]["A"]["vram_gib"], 32.0);
    let rev = data["rev"].as_str().unwrap().to_string();
    let edit = |edits: Value| {
        let h = h.clone();
        let url = r.url.clone();
        async move {
            http()
                .post(format!("{url}/admin/api/config/edit"))
                .headers(h)
                .json(&json!({"edits": edits}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    // A card on a server that does not exist: the router's validator says so.
    let bad = edit(json!([{"path": ["cards", "B"], "value": {"server": "nowhere", "gpu_uuid": "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"}}])).await;
    assert_eq!(bad["check"]["ok"], false, "{bad}");
    let good = edit(json!([
        {"path": ["cards", "B"], "value": {"server": "gpu", "gpu_uuid": "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "vram_gib": 16}},
        {"path": ["servers", "gpu", "host"], "value": "gpu-2"}
    ]))
    .await;
    assert_eq!(good["check"]["ok"], true, "{good}");
    let text = good["text"].as_str().unwrap();
    assert!(text.contains("title = \"GPU host\"   # stays"));
    assert!(
        good["check"]["diff"]
            .as_str()
            .unwrap()
            .contains("+host = \"gpu-2\"")
    );
    // Nothing written yet.
    assert!(
        std::fs::read_to_string(&r.toml)
            .unwrap()
            .contains("host = \"gpu-host\"")
    );
    // Saved like a text save (If-Match).
    let mut hm = h.clone();
    hm.insert(
        "if-match",
        HeaderValue::from_str(&format!("\"{rev}\"")).unwrap(),
    );
    let saved = http()
        .put(format!("{}/admin/api/config", r.url))
        .headers(hm.clone())
        .json(&json!({"text": text, "reason": "card B"}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    let disk = std::fs::read_to_string(&r.toml).unwrap();
    assert!(disk.contains("[cards.B]") && disk.contains("# stays"));
    // The same rev again: the file moved.
    let stale = http()
        .put(format!("{}/admin/api/config", r.url))
        .headers(hm)
        .json(&json!({"text": text, "reason": "again"}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 412);
}

#[tokio::test]
async fn router_json_is_checked_backed_up_saved_and_reloaded() {
    let r = start().await;
    let h = admin_headers(&r).await;
    let doc: Value = http()
        .get(format!("{}/admin/api/gateway", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["data"]["deployments"][0]["name"], "dense");
    assert_eq!(doc["writable"], true);
    let rev = doc["rev"].as_str().unwrap().to_string();
    let mut next: Value = doc["data"].clone();
    next["deployments"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "small", "cards": ["Z"], "servedId": "s"}));
    let bad_text = serde_json::to_string_pretty(&next).unwrap();
    let check: Value = http()
        .post(format!("{}/admin/api/gateway/check", r.url))
        .headers(h.clone())
        .json(&json!({"text": bad_text}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(check["ok"], false);
    assert!(check["problems"].to_string().contains("unknown card Z"));
    let mut hm = h.clone();
    hm.insert(
        "if-match",
        HeaderValue::from_str(&format!("\"{rev}\"")).unwrap(),
    );
    let refused = http()
        .put(format!("{}/admin/api/gateway", r.url))
        .headers(hm.clone())
        .json(&json!({"text": bad_text, "reason": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 422);
    assert_eq!(
        std::fs::read_to_string(r.dir.path().join("router.json")).unwrap(),
        JSON,
        "never an invalid file"
    );
    // A literal key is refused too.
    let mut keyed = doc["data"].clone();
    keyed["deployments"][0]["params"] = json!({"api_key": "sk-secret-value"});
    let refused = http()
        .put(format!("{}/admin/api/gateway", r.url))
        .headers(hm.clone())
        .json(&json!({"text": keyed.to_string(), "reason": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 422);
    assert!(!refused.text().await.unwrap().contains("sk-secret-value"));

    next["deployments"][1]["cards"] = json!(["A"]);
    let good = serde_json::to_string_pretty(&next).unwrap();
    let saved: Value = http()
        .put(format!("{}/admin/api/gateway", r.url))
        .headers(hm.clone())
        .json(&json!({"text": good, "reason": "small model"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ok"], true, "{saved}");
    assert_eq!(saved["reloaded"], true);
    let models: Value = http()
        .get(format!("{}/v1/models", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models.to_string().contains("\"small\""),
        "hot reloaded: {models}"
    );
    let backups: Vec<_> = std::fs::read_dir(r.dir.path().join("backups"))
        .unwrap()
        .collect();
    assert_eq!(backups.len(), 1);
    let again = http()
        .put(format!("{}/admin/api/gateway", r.url))
        .headers(hm)
        .json(&json!({"text": good, "reason": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 412);
    let audit: Value = http()
        .get(format!("{}/admin/api/audit", r.url))
        .headers(h)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = audit
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == "gateway.save")
        .expect("audited");
    assert_eq!(row["who"], "router-key:ops#1");
    assert!(row.to_string().contains("small model"));
}
