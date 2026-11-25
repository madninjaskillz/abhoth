//! Sign-in mode (`--hostui-login`), ported from the C# LoginAuthTests.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{FakeSession, Host};
use orca_webui::login::{CSRF_HEADER, LoginOptions, accounts, totp};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const USER: &str = "orca-admin";
const PASSWORD: &str = "a long and secret password";

struct Site {
    host: Host,
    state: tempfile::TempDir,
    _work: tempfile::TempDir,
    _home: tempfile::TempDir,
    now: Arc<AtomicU64>,
}

impl Site {
    async fn start(require_totp: bool, proxies: Vec<std::net::IpAddr>) -> Self {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Few PBKDF2 iterations so the debug build signs in quickly (the count is in the hash).
        let account = accounts::Account {
            username: USER.into(),
            password_hash: accounts::hash(PASSWORD, 1000),
            ..Default::default()
        };
        accounts::AccountStore::new(state.path())
            .save(&account)
            .unwrap();
        let now = Arc::new(AtomicU64::new(1_800_000_000));
        let clock_now = now.clone();
        let mut login = LoginOptions::new(state.path().to_path_buf());
        login.require_totp = require_totp;
        login.trusted_proxies = proxies;
        login.clock = Some(Arc::new(move || clock_now.load(Ordering::SeqCst) as f64));
        let home_dir = home.path().to_path_buf();
        let host = Host::start_with(FakeSession::new(work.path()), move |o| {
            o.login = Some(login);
            o.config_home = Some(home_dir);
        })
        .await;
        Self {
            host,
            state,
            _work: work,
            _home: home,
            now,
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.host.server.port())
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.state.path().join("audit.log")).unwrap_or_default()
    }
}

/// A browser: keeps the session cookie, sends this site's Origin.
struct Browser<'a> {
    site: &'a Site,
    cookie: Option<String>,
}

impl<'a> Browser<'a> {
    fn new(site: &'a Site) -> Self {
        Self { site, cookie: None }
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .site
            .host
            .http()
            .request(method.clone(), self.site.host.url(path));
        if method != Method::GET {
            r = r.header("Origin", self.site.origin());
        }
        if let Some(c) = &self.cookie {
            r = r.header("Cookie", c);
        }
        r
    }

    fn keep(&mut self, r: &reqwest::Response) {
        if let Some(v) = r.headers().get("set-cookie") {
            let first = v.to_str().unwrap().split(';').next().unwrap().to_string();
            self.cookie = (!first.ends_with('=')).then_some(first);
        }
    }

    async fn send(&mut self, r: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let r = r.send().await.unwrap();
        self.keep(&r);
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn sign_in(&mut self, password: &str) -> (StatusCode, Value) {
        let r = self
            .req(Method::POST, "/api/auth/login")
            .json(&json!({"username": USER, "password": password}));
        self.send(r).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_but_the_sign_in_page_without_a_session() {
    let site = Site::start(false, vec![]).await;
    let b = Browser::new(&site);
    let page = b.req(Method::GET, "/").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    let r = b.req(Method::GET, "/api/projects").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // The URL token means nothing in sign-in mode.
    let t = site
        .host
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap();
    assert_eq!(t.status(), StatusCode::UNAUTHORIZED);
    let state: Value = b
        .req(Method::GET, "/api/auth/state")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["mode"], "login");
    assert_eq!(state["stage"], "signedOut");
    assert_eq!(state["user"], Value::Null);
    assert_eq!(state["idleMinutes"], 60);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_sets_a_strict_http_only_cookie_and_opens_the_api() {
    let site = Site::start(false, vec![]).await;
    let mut b = Browser::new(&site);
    let r = b
        .req(Method::POST, "/api/auth/login")
        .json(&json!({"username": USER, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let set = r.headers()["set-cookie"].to_str().unwrap().to_lowercase();
    assert!(set.starts_with("openorca_session="), "{set}");
    assert!(
        set.contains("httponly") && set.contains("samesite=strict"),
        "{set}"
    );
    b.keep(&r);
    let state: Value = r.json().await.unwrap();
    assert_eq!(state["stage"], "ok");
    assert_eq!(state["user"], USER);
    let csrf = state["csrf"].as_str().unwrap().to_string();

    let info: Value = b
        .req(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["auth"], "login");

    // State-changing requests need the CSRF header and this Origin.
    let put = |b: &Browser, csrf: Option<&str>, origin: Option<&str>| {
        let mut r = site
            .host
            .http()
            .put(site.host.url("/api/settings"))
            .header("Cookie", b.cookie.clone().unwrap())
            .json(&json!({"key": "lmStudio.temperature", "value": 0.4}));
        if let Some(c) = csrf {
            r = r.header(CSRF_HEADER, c);
        }
        if let Some(o) = origin {
            r = r.header("Origin", o);
        }
        r.send()
    };
    let origin = site.origin();
    assert_eq!(
        put(&b, None, Some(&origin)).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        put(&b, Some(&csrf), None).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        put(&b, Some(&csrf), Some("http://evil.example"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        put(&b, Some(&csrf), Some(&origin)).await.unwrap().status(),
        StatusCode::OK
    );
    // The audit log has the signed-in user on the change.
    let audit = site.audit();
    let line: Value = audit
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|l| l["event"] == "settings.global")
        .unwrap();
    assert_eq!(line["user"], USER);
    assert!(audit.contains("\"csrf.rejected\"") && audit.contains("\"origin.rejected\""));

    // Signing out ends the session.
    let r = b
        .req(Method::POST, "/api/auth/logout")
        .header(CSRF_HEADER, &csrf);
    let (status, state) = b.send(r).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["stage"], "signedOut");
    assert!(b.cookie.is_none());
    let old = site
        .host
        .http()
        .get(site.host.url("/api/projects"))
        .header("Cookie", "openorca_session=x")
        .send()
        .await
        .unwrap();
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn websockets_from_another_origin_or_without_a_session_are_refused() {
    let site = Site::start(false, vec![]).await;
    let mut b = Browser::new(&site);
    b.sign_in(PASSWORD).await;
    let ws = |origin: Option<String>, cookie: Option<String>| {
        let mut r = site
            .host
            .http()
            .get(site.host.url("/hub/ws"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(o) = origin {
            r = r.header("Origin", o);
        }
        if let Some(c) = cookie {
            r = r.header("Cookie", c);
        }
        r.send()
    };
    let other = ws(Some("http://evil.example".into()), b.cookie.clone())
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
    let anon = ws(Some(site.origin()), None).await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    let ok = ws(Some(site.origin()), b.cookie.clone()).await.unwrap();
    assert_eq!(ok.status(), StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_failures_lock_the_account_even_with_the_right_password() {
    let site = Site::start(false, vec![]).await;
    let mut b = Browser::new(&site);
    for i in 0..5 {
        let (status, body) = b.sign_in(&format!("wrong-{i}-password")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "Wrong username or password.");
    }
    let locked = b
        .req(Method::POST, "/api/auth/login")
        .json(&json!({"username": USER, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(locked.headers()["retry-after"], "60");
    let audit = site.audit();
    for e in ["\"login.fail\"", "\"login.lockout\"", "\"login.locked\""] {
        assert!(audit.contains(e), "{e}: {audit}");
    }
    assert!(!audit.contains(PASSWORD) && !audit.contains("wrong-0-password"));
    // The lock passes.
    site.now.fetch_add(61, Ordering::SeqCst);
    assert_eq!(b.sign_in(PASSWORD).await.0, StatusCode::OK);
}

fn code_now(site: &Site, secret: &str) -> String {
    totp::code(
        secret,
        totp::step_at(site.now.load(Ordering::SeqCst) as i64),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn totp_when_required_is_set_up_at_first_sign_in_then_asked_every_time_and_codes_work_once() {
    let site = Site::start(true, vec![]).await;
    let mut b = Browser::new(&site);
    let (_, first) = b.sign_in(PASSWORD).await;
    assert_eq!(first["stage"], "enroll");
    let r = b.req(Method::GET, "/api/projects").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "not yet");
    let enroll: Value = b
        .req(Method::GET, "/api/auth/enroll")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        enroll["qr"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    let secret = enroll["secret"].as_str().unwrap().to_string();
    assert!(
        enroll["uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/OpenOrca:orca-admin?secret=")
    );
    let csrf = first["csrf"].as_str().unwrap().to_string();
    let wrong = b
        .req(Method::POST, "/api/auth/totp")
        .header(CSRF_HEADER, &csrf)
        .json(&json!({"code": "000000"}));
    let (status, body) = b.send(wrong).await;
    if code_now(&site, &secret) != "000000" {
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "That code didn't work.");
    }
    let code = code_now(&site, &secret);
    let ok = b
        .req(Method::POST, "/api/auth/totp")
        .header(CSRF_HEADER, &csrf)
        .json(&json!({"code": code}));
    let (status, state) = b.send(ok).await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["stage"], "ok");
    assert_eq!(state["totp"], true);
    let saved = std::fs::read_to_string(site.state.path().join("account.json")).unwrap();
    assert!(saved.contains("\"totpConfirmed\": true"));
    assert!(site.audit().contains("\"totp.enrolled\""));
    assert!(!site.audit().contains(&secret));

    // Next time: asked for a code; the one just used doesn't work again.
    let mut b2 = Browser::new(&site);
    let (_, again) = b2.sign_in(PASSWORD).await;
    assert_eq!(again["stage"], "totp");
    let csrf2 = again["csrf"].as_str().unwrap().to_string();
    let replay = b2
        .req(Method::POST, "/api/auth/totp")
        .header(CSRF_HEADER, &csrf2)
        .json(&json!({"code": code}));
    assert_eq!(b2.send(replay).await.0, StatusCode::UNAUTHORIZED);
    site.now.fetch_add(30, Ordering::SeqCst);
    let next = b2
        .req(Method::POST, "/api/auth/totp")
        .header(CSRF_HEADER, &csrf2)
        .json(&json!({"code": code_now(&site, &secret)}));
    assert_eq!(b2.send(next).await.0, StatusCode::OK);
    let r = b2.req(Method::GET, "/api/projects").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn behind_the_trusted_proxy_client_ip_and_https_come_from_its_headers() {
    let site = Site::start(false, vec!["127.0.0.1".parse().unwrap()]).await;
    let plain = site
        .host
        .http()
        .get(site.host.url("/api/auth/state"))
        .header("X-Forwarded-For", "203.0.113.9")
        .header("X-Forwarded-Proto", "http")
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::FORBIDDEN);
    let r = site
        .host
        .http()
        .post(site.host.url("/api/auth/login"))
        .header("X-Forwarded-For", "203.0.113.9")
        .header("X-Forwarded-Proto", "https")
        .header("X-Forwarded-Host", "orca.example.test")
        .header("Origin", "https://orca.example.test")
        .json(&json!({"username": USER, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set.starts_with("__Host-openorca_session="), "{set}");
    assert!(set.to_lowercase().contains("secure"));
    assert!(
        site.audit().contains("\"ip\":\"203.0.113.9\""),
        "{}",
        site.audit()
    );
}

#[test]
fn a_bootstrap_secret_creates_the_account_once() {
    let state = tempfile::tempdir().unwrap();
    let pw = state.path().join("pw.txt");
    std::fs::write(&pw, "bootstrap password here\n").unwrap();
    let mut o = LoginOptions::new(state.path().to_path_buf());
    o.bootstrap_user = Some("boot".into());
    o.bootstrap_password_file = Some(pw);
    let audit = orca_webui::audit::AuditLog::new(Some(state.path().join("audit.log")));
    let auth = orca_webui::login::LoginAuth::new(o.clone(), &audit).unwrap();
    let a = auth.accounts.load().unwrap();
    assert_eq!(a.username, "boot");
    assert!(accounts::verify_password(
        "bootstrap password here",
        &a.password_hash
    ));
    let text = std::fs::read_to_string(Path::new(state.path()).join("audit.log")).unwrap();
    assert!(text.contains("account.created") && !text.contains("bootstrap password"));
    o.bootstrap_password_file = Some(state.path().join("missing.txt"));
    let other = tempfile::tempdir().unwrap();
    o.state_dir = other.path().to_path_buf();
    assert!(orca_webui::login::LoginAuth::new(o, &audit).is_err());
}
