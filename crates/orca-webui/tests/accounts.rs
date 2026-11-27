//! Web UI accounts (D140): first-run setup, sign-in, sessions, CSRF and Origin, 2FA with
//! recovery codes, API keys, admin user management, the audit log, the old account's import,
//! and per-user isolation of projects across every API family and the hub.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::{ADMIN, ADMIN_PASSWORD, FakeSession, Host, TOKEN, Ws};
use orca_webui::live::LiveSession;
use orca_webui::login::{CSRF_HEADER, accounts, password, totp};
use orca_webui::{SessionFactory, WebUiOptions};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const T0: u64 = 1_800_000_000;

struct Site {
    host: Host,
    _work: tempfile::TempDir,
    /// Where users' projects may live (`user_roots`).
    roots: tempfile::TempDir,
    now: Arc<AtomicU64>,
    /// The session made for each project folder (tests publish on their buses).
    sessions: Arc<Mutex<HashMap<String, Arc<FakeSession>>>>,
}

#[derive(Default)]
struct Setup {
    no_admin: bool,
    exposed: bool,
    require_totp: bool,
    legacy_dir: Option<PathBuf>,
    proxies: Vec<std::net::IpAddr>,
    /// The desktop app (D142).
    local_admin: bool,
}

impl Site {
    async fn start(cfg: Setup) -> Self {
        let work = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(T0));
        let sessions: Arc<Mutex<HashMap<String, Arc<FakeSession>>>> = Default::default();
        let (clock_now, made, root_path) =
            (now.clone(), sessions.clone(), roots.path().to_path_buf());
        let host = Host::start_with(
            FakeSession::new(work.path()),
            move |o: &mut WebUiOptions| {
                if cfg.no_admin {
                    o.login.bootstrap_user = None;
                    o.login.bootstrap_password = None;
                }
                o.login.exposed = cfg.exposed;
                o.login.require_totp = cfg.require_totp;
                o.login.legacy_dir = cfg.legacy_dir;
                o.login.trusted_proxies = cfg.proxies;
                o.login.local_admin = cfg.local_admin;
                o.login.user_roots = Some(vec![root_path]);
                o.login.clock = Some(Arc::new(move || clock_now.load(Ordering::SeqCst) as f64));
                let factory: SessionFactory =
                    Arc::new(move |folder: &str| -> Arc<dyn LiveSession> {
                        let s = FakeSession::new(Path::new(folder));
                        made.lock().unwrap().insert(folder.to_string(), s.clone());
                        s
                    });
                o.session_factory = Some(factory);
            },
        )
        .await;
        Self {
            host,
            _work: work,
            roots,
            now,
            sessions,
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.host.server.port())
    }

    fn browser(&self) -> Browser<'_> {
        Browser {
            site: self,
            cookie: None,
            csrf: None,
        }
    }

    /// A browser signed in as the bootstrap admin.
    async fn admin(&self) -> Browser<'_> {
        let mut b = self.browser();
        let (st, _) = b.sign_in(ADMIN, ADMIN_PASSWORD).await;
        assert_eq!(st, StatusCode::OK);
        b
    }

    /// A folder under the users' projects location.
    fn folder(&self, name: &str) -> String {
        let p = self.roots.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("secret.txt"), format!("{name}'s secret")).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn advance(&self, secs: u64) {
        self.now.fetch_add(secs, Ordering::SeqCst);
    }

    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst) as i64
    }

    fn db(&self) -> &orca_webui::login::db::UserDb {
        &self.host.server.state().login.db
    }
}

/// A browser: keeps the session cookie and CSRF token, sends this site's Origin.
struct Browser<'a> {
    site: &'a Site,
    cookie: Option<String>,
    csrf: Option<String>,
}

impl Browser<'_> {
    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.req_from(method, path, &self.site.origin())
    }

    /// A request sent with this Origin (unsafe methods) instead of the site's own.
    fn req_from(&self, method: Method, path: &str, origin: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .site
            .host
            .http()
            .request(method.clone(), self.site.host.url(path));
        if method != Method::GET {
            r = r.header("Origin", origin);
            if let Some(c) = &self.csrf {
                r = r.header(CSRF_HEADER, c);
            }
        }
        if let Some(c) = &self.cookie {
            r = r.header("Cookie", c);
        }
        r
    }

    fn keep(&mut self, r: &reqwest::Response) {
        for v in r.headers().get_all("set-cookie") {
            let first = v.to_str().unwrap().split(';').next().unwrap().to_string();
            if first.starts_with("abhoth_session") || first.starts_with("__Host-abhoth_session") {
                self.cookie = (!first.ends_with('=')).then_some(first);
            }
        }
    }

    async fn send(&mut self, r: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let r = r.send().await.unwrap();
        self.keep(&r);
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if let Some(c) = v.get("csrf").and_then(Value::as_str) {
            self.csrf = Some(c.to_string());
        }
        (status, v)
    }

    async fn get(&mut self, path: &str) -> (StatusCode, Value) {
        let r = self.req(Method::GET, path);
        self.send(r).await
    }

    async fn post(&mut self, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self.req(Method::POST, path).json(&body);
        self.send(r).await
    }

    async fn call(&mut self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self.req(method, path).json(&body);
        self.send(r).await
    }

    async fn sign_in(&mut self, user: &str, password: &str) -> (StatusCode, Value) {
        self.post(
            "/api/auth/login",
            json!({"username": user, "password": password}),
        )
        .await
    }
}

/// An API key request.
fn keyed(site: &Site, key: &str, method: Method, path: &str) -> reqwest::RequestBuilder {
    site.host
        .http()
        .request(method, site.host.url(path))
        .header("Authorization", format!("Bearer {key}"))
}

async fn status_of(r: reqwest::RequestBuilder) -> StatusCode {
    r.send().await.unwrap().status()
}

// ---- first run ----

#[tokio::test(flavor = "multi_thread")]
async fn with_no_users_everything_leads_to_the_setup() {
    let site = Site::start(Setup {
        no_admin: true,
        ..Default::default()
    })
    .await;
    assert!(site.host.server.setup_needed());
    let mut b = site.browser();
    let page = b.req(Method::GET, "/").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::FOUND);
    assert_eq!(page.headers()["location"], "/setup");
    let deep = b.req(Method::GET, "/projects/x").send().await.unwrap();
    assert_eq!(deep.headers()["location"], "/setup");
    assert_eq!(
        b.req(Method::GET, "/setup").send().await.unwrap().status(),
        StatusCode::OK
    );
    let (st, v) = b.get("/api/projects").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["setup"], true);
    // The launch token opens nothing but the setup either.
    assert_eq!(
        status_of(site.host.api(Method::GET, "/api/info")).await,
        StatusCode::FORBIDDEN
    );
    let (st, state) = b.get("/api/auth/state").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(state["setup"], true);
    assert_eq!(state["setupCodeNeeded"], true);
    // Without the setup code: refused.
    let (st, _) = b
        .post(
            "/api/auth/setup",
            json!({"username": "root", "password": "a long enough password", "code": "nope"}),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // With it: a short password is refused, a good one makes the admin and signs in.
    let (st, v) = b
        .post(
            "/api/auth/setup",
            json!({"username": "root", "password": "short", "code": TOKEN}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, v) = b
        .post(
            "/api/auth/setup",
            json!({"username": "root", "password": "a long enough password", "code": TOKEN}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["stage"], "ok");
    assert_eq!(v["admin"], true);
    assert_eq!(v["setup"], false);
    assert!(b.cookie.is_some());
    let (st, _) = b.get("/api/projects").await;
    assert_eq!(st, StatusCode::OK);
    // Setup is closed now.
    let (st, _) = site
        .browser()
        .post(
            "/api/auth/setup",
            json!({"username": "evil", "password": "a long enough password", "code": TOKEN}),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(site.db().user_count(), 1);
    let page = site
        .browser()
        .req(Method::GET, "/setup")
        .send()
        .await
        .unwrap();
    assert_eq!(page.headers()["location"], "/");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_launch_url_carries_the_setup_code_in_a_cookie() {
    let site = Site::start(Setup {
        no_admin: true,
        ..Default::default()
    })
    .await;
    let r = site
        .browser()
        .req(Method::GET, &format!("/?token={TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(r.headers()["location"], "/setup");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        set.starts_with("abhoth_setup=") && set.contains("httponly"),
        "{set}"
    );
    let mut b = site.browser();
    b.cookie = Some(set.split(';').next().unwrap().to_string());
    let (_, state) = b.get("/api/auth/state").await;
    assert_eq!(state["setupCodeNeeded"], false);
    let setup_cookie = b.cookie.clone();
    let (st, _) = b
        .post(
            "/api/auth/setup",
            json!({"username": "root", "password": "a long enough password"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_ne!(b.cookie, setup_cookie, "a session cookie now");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_setups_have_exactly_one_winner() {
    let site = Site::start(Setup {
        no_admin: true,
        ..Default::default()
    })
    .await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let req = site
            .host
            .http()
            .post(site.host.url("/api/auth/setup"))
            .json(&json!({"username": format!("u{i}"), "password": "a long enough password", "code": TOKEN}));
        tasks.push(tokio::spawn(
            async move { req.send().await.unwrap().status() },
        ));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(site.db().user_count(), 1);
}

// ---- sign-in, sessions, CSRF ----

/// Settings → This Machine is /machine.html in a frame of the app's own page: it must allow
/// same-origin framing (WebView2 showed "content blocked" under the default DENY), while
/// every other page still refuses to be framed.
#[tokio::test(flavor = "multi_thread")]
async fn machine_page_may_be_framed_by_the_app_and_nothing_else_may() {
    let site = Site::start(Setup::default()).await;
    let b = site.admin().await;
    let page = b.req(Method::GET, "/machine.html").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["x-frame-options"], "SAMEORIGIN");
    let csp = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
    let index = b.req(Method::GET, "/").send().await.unwrap();
    assert_eq!(index.headers()["x-frame-options"], "DENY");
    let api = b.req(Method::GET, "/api/projects").send().await.unwrap();
    assert_eq!(api.headers()["x-frame-options"], "DENY");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_sets_a_lax_httponly_cookie_and_wrong_answers_look_alike() {
    let site = Site::start(Setup::default()).await;
    let mut b = site.browser();
    let page = b.req(Method::GET, "/").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK, "the sign-in page");
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    let (st, _) = b.get("/api/projects").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st1, wrong) = b.sign_in(ADMIN, "not the password!").await;
    let (st2, unknown) = b.sign_in("nobody-here", "not the password!").await;
    assert_eq!(
        (st1, st2),
        (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED)
    );
    assert_eq!(wrong, unknown, "no user enumeration");
    let r = b
        .req(Method::POST, "/api/auth/login")
        .json(&json!({"username": ADMIN, "password": ADMIN_PASSWORD}))
        .send()
        .await
        .unwrap();
    let set = r.headers()["set-cookie"].to_str().unwrap().to_lowercase();
    assert!(set.starts_with("abhoth_session="), "{set}");
    assert!(
        set.contains("httponly") && set.contains("samesite=lax"),
        "{set}"
    );
    assert!(!set.contains("secure"), "plain HTTP here");
    b.keep(&r);
    let v: Value = r.json().await.unwrap();
    b.csrf = v["csrf"].as_str().map(str::to_string);
    assert_eq!(v["user"], ADMIN);
    let (st, _) = b.get("/api/projects").await;
    assert_eq!(st, StatusCode::OK);
    // The cookie is stored hashed: the raw value is not in the database.
    let raw = b.cookie.clone().unwrap();
    let raw = raw.split_once('=').unwrap().1;
    assert!(site.db().session_by_token(raw).is_some());
    // State changes need the CSRF header and this site's Origin.
    let no_csrf = b
        .site
        .host
        .http()
        .post(site.host.url("/api/account/keys"))
        .header("Cookie", b.cookie.clone().unwrap())
        .header("Origin", site.origin())
        .json(&json!({"name": "x"}));
    assert_eq!(status_of(no_csrf).await, StatusCode::FORBIDDEN);
    let cross = b
        .req_from(Method::POST, "/api/account/keys", "http://evil.example")
        .json(&json!({"name": "x"}));
    assert_eq!(status_of(cross).await, StatusCode::FORBIDDEN);
    let (st, _) = b.post("/api/account/keys", json!({"name": "ok"})).await;
    assert_eq!(st, StatusCode::OK);
    // Sign out: the cookie no longer works.
    let old = b.cookie.clone();
    let (st, _) = b.post("/api/auth/logout", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    b.cookie = old;
    let (st, _) = b.get("/api/projects").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_expire_and_can_be_revoked_everywhere() {
    let site = Site::start(Setup::default()).await;
    let mut one = site.admin().await;
    let mut two = site.admin().await;
    let (_, list) = one.get("/api/account/sessions").await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .filter(|s| s["current"] == true)
            .count(),
        1
    );
    // Revoke the other one.
    let other = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == false)
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (st, _) = one
        .call(
            Method::DELETE,
            &format!("/api/account/sessions/{other}"),
            Value::Null,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(two.get("/api/projects").await.0, StatusCode::UNAUTHORIZED);
    // Everywhere: this one too.
    let mut three = site.admin().await;
    let (st, _) = one
        .post("/api/account/sessions/revoke-all", json!({}))
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(three.get("/api/projects").await.0, StatusCode::UNAUTHORIZED);
    // Idle expiry (12 h by default).
    let mut idle = site.admin().await;
    assert_eq!(idle.get("/api/projects").await.0, StatusCode::OK);
    site.advance(13 * 3600);
    assert_eq!(idle.get("/api/projects").await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_failures_lock_the_account_and_the_ip() {
    let site = Site::start(Setup::default()).await;
    let mut b = site.browser();
    for _ in 0..4 {
        assert_eq!(
            b.sign_in(ADMIN, "wrong password!!").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        b.sign_in(ADMIN, "wrong password!!").await.0,
        StatusCode::UNAUTHORIZED
    );
    // Locked now, even with the right password.
    let (st, v) = b.sign_in(ADMIN, ADMIN_PASSWORD).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{v}");
    site.advance(61);
    assert_eq!(b.sign_in(ADMIN, ADMIN_PASSWORD).await.0, StatusCode::OK);
    let audit = site.db().audit_entries(100, None);
    assert!(audit.iter().any(|e| e.event == "login.lockout"));
    assert!(audit.iter().any(|e| e.event == "login.fail"));
    assert!(audit.iter().any(|e| e.event == "login.ok"));
}

// ---- 2FA ----

#[tokio::test(flavor = "multi_thread")]
async fn totp_enrolment_sign_in_and_single_use_recovery_codes() {
    let site = Site::start(Setup::default()).await;
    let mut b = site.admin().await;
    let (st, e) = b.post("/api/account/totp/start", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        e["qr"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    let secret = e["secret"].as_str().unwrap().to_string();
    assert!(e["uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    let (st, _) = b
        .post("/api/account/totp/confirm", json!({"code": "000000"}))
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let code = totp::code(&secret, totp::step_at(site.now())).unwrap();
    let (st, v) = b
        .post("/api/account/totp/confirm", json!({"code": code}))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let codes: Vec<String> = serde_json::from_value(v["recoveryCodes"].clone()).unwrap();
    assert_eq!(codes.len(), 10);
    let (_, me) = b.get("/api/account").await;
    assert_eq!(me["totp"], true);
    assert_eq!(me["recoveryCodesLeft"], 10);

    // Sign-in now asks for a code; the same code doesn't work twice.
    let mut c = site.browser();
    let (_, v) = c.sign_in(ADMIN, ADMIN_PASSWORD).await;
    assert_eq!(v["stage"], "totp");
    assert_eq!(c.get("/api/projects").await.0, StatusCode::UNAUTHORIZED);
    let (st, _) = c.post("/api/auth/totp", json!({"code": code})).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "a used code");
    site.advance(30);
    let fresh = totp::code(&secret, totp::step_at(site.now())).unwrap();
    let (st, v) = c.post("/api/auth/totp", json!({"code": fresh})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["stage"], "ok");

    // A recovery code works once (any case, any separator).
    let mut d = site.browser();
    d.sign_in(ADMIN, ADMIN_PASSWORD).await;
    let typed = codes[0].to_uppercase().replace('-', " ");
    assert_eq!(
        d.post("/api/auth/totp", json!({"code": typed})).await.0,
        StatusCode::OK
    );
    let mut e2 = site.browser();
    e2.sign_in(ADMIN, ADMIN_PASSWORD).await;
    assert_eq!(
        e2.post("/api/auth/totp", json!({"code": codes[0]})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, me) = b.get("/api/account").await;
    assert_eq!(me["recoveryCodesLeft"], 9);

    // Turning it off needs the password.
    assert_eq!(
        b.post(
            "/api/account/totp/disable",
            json!({"password": "nope nope nope"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        b.post(
            "/api/account/totp/disable",
            json!({"password": ADMIN_PASSWORD})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, v) = site.browser().sign_in(ADMIN, ADMIN_PASSWORD).await;
    assert_eq!(v["stage"], "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn required_2fa_enrols_at_sign_in_and_hands_out_recovery_codes() {
    let site = Site::start(Setup {
        require_totp: true,
        ..Default::default()
    })
    .await;
    let mut b = site.browser();
    let (_, v) = b.sign_in(ADMIN, ADMIN_PASSWORD).await;
    assert_eq!(v["stage"], "enroll");
    let (st, e) = b.get("/api/auth/enroll").await;
    assert_eq!(st, StatusCode::OK);
    let code = totp::code(e["secret"].as_str().unwrap(), totp::step_at(site.now())).unwrap();
    let (st, v) = b.post("/api/auth/totp", json!({"code": code})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["stage"], "ok");
    assert_eq!(v["recoveryCodes"].as_array().unwrap().len(), 10);
}

// ---- API keys ----

#[tokio::test(flavor = "multi_thread")]
async fn api_keys_are_shown_once_stored_hashed_and_revocable() {
    let site = Site::start(Setup::default()).await;
    let mut b = site.admin().await;
    let (st, k) = b.post("/api/account/keys", json!({"name": "ci"})).await;
    assert_eq!(st, StatusCode::OK);
    let key = k["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("abk_"));
    let (_, list) = b.get("/api/account/keys").await;
    assert_eq!(list[0]["name"], "ci");
    assert!(list[0].get("key").is_none(), "never shown again");
    assert!(!list.to_string().contains(&key));
    assert_eq!(
        status_of(keyed(&site, &key, Method::GET, "/api/projects")).await,
        StatusCode::OK
    );
    // No CSRF for a key (not a cookie): a state change works.
    assert_eq!(
        status_of(
            keyed(&site, &key, Method::POST, "/api/account/keys").json(&json!({"name": "two"}))
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status_of(keyed(&site, "abk_wrong", Method::GET, "/api/projects")).await,
        StatusCode::UNAUTHORIZED
    );
    let id = k["id"].as_i64().unwrap();
    let (st, _) = b
        .call(
            Method::DELETE,
            &format!("/api/account/keys/{id}"),
            Value::Null,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        status_of(keyed(&site, &key, Method::GET, "/api/projects")).await,
        StatusCode::UNAUTHORIZED
    );
    let audit = site.db().audit_entries(100, None);
    assert!(audit.iter().any(|e| e.event == "apikey.created"));
    assert!(audit.iter().any(|e| e.event == "apikey.revoked"));
    assert!(!serde_json::to_string(&audit).unwrap().contains(&key));
}

// ---- admin ----

/// The admin makes a user and an API key for them (signing in as them).
async fn user_with_key(site: &Site, admin: &mut Browser<'_>, name: &str) -> String {
    let (st, v) = admin
        .post(
            "/api/admin/users",
            json!({"username": name, "password": "user password 123", "admin": false}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let mut b = site.browser();
    assert_eq!(b.sign_in(name, "user password 123").await.0, StatusCode::OK);
    let (_, k) = b.post("/api/account/keys", json!({"name": "test"})).await;
    k["key"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn admins_manage_users_and_others_cannot() {
    let site = Site::start(Setup::default()).await;
    let mut admin = site.admin().await;
    let bob = user_with_key(&site, &mut admin, "bob").await;
    for (m, path) in [
        (Method::GET, "/api/admin/users"),
        (Method::GET, "/api/admin/audit"),
        (Method::GET, "/api/settings"),
        (Method::PUT, "/api/settings"),
        (Method::GET, "/api/bench"),
        (Method::GET, "/api/github/app"),
        (Method::POST, "/api/profiles/switch"),
        (Method::POST, "/api/admin/users"),
    ] {
        let st = status_of(keyed(&site, &bob, m.clone(), path).json(&json!({}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {path}");
    }
    let (_, users) = admin.get("/api/admin/users").await;
    let bob_id = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        admin
            .post(
                "/api/admin/users",
                json!({"username": "BOB", "password": "user password 123"})
            )
            .await
            .0,
        StatusCode::CONFLICT,
        "names are case-insensitive"
    );
    // Disable: no sign-in, keys stop working.
    let (st, _) = admin
        .call(
            Method::PATCH,
            &format!("/api/admin/users/{bob_id}"),
            json!({"disabled": true}),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        status_of(keyed(&site, &bob, Method::GET, "/api/projects")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        site.browser().sign_in("bob", "user password 123").await.0,
        StatusCode::UNAUTHORIZED
    );
    admin
        .call(
            Method::PATCH,
            &format!("/api/admin/users/{bob_id}"),
            json!({"disabled": false}),
        )
        .await;
    // Reset the password.
    let (st, _) = admin
        .post(
            &format!("/api/admin/users/{bob_id}/password"),
            json!({"password": "brand new password"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        site.browser().sign_in("bob", "brand new password").await.0,
        StatusCode::OK
    );
    // Make admin, then the last-admin guard.
    let (_, v) = admin
        .call(
            Method::PATCH,
            &format!("/api/admin/users/{bob_id}"),
            json!({"admin": true}),
        )
        .await;
    assert_eq!(v["admin"], true);
    let me = site.db().user_by_name(ADMIN).unwrap().id;
    assert_eq!(
        admin
            .call(
                Method::PATCH,
                &format!("/api/admin/users/{me}"),
                json!({"admin": false})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST,
        "not yourself"
    );
    assert_eq!(
        admin
            .call(
                Method::DELETE,
                &format!("/api/admin/users/{me}"),
                Value::Null
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // Delete bob.
    let (st, _) = admin
        .call(
            Method::DELETE,
            &format!("/api/admin/users/{bob_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert!(site.db().user_by_name("bob").is_none());
    let audit = site.db().audit_entries(200, None);
    for ev in [
        "user.created",
        "user.disabled",
        "user.enabled",
        "user.password_reset",
        "user.admin_granted",
        "user.deleted",
    ] {
        assert!(audit.iter().any(|e| e.event == ev), "{ev}");
    }
    // The page reads it newest first, in pages.
    let (_, page) = admin.get("/api/admin/audit?limit=2").await;
    let page = page.as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert!(page[0]["id"].as_i64() > page[1]["id"].as_i64());
}

// ---- isolation ----

#[tokio::test(flavor = "multi_thread")]
async fn one_user_cannot_reach_another_users_projects_through_any_api() {
    let site = Site::start(Setup::default()).await;
    let mut admin = site.admin().await;
    let ann = user_with_key(&site, &mut admin, "ann").await;
    let bob = user_with_key(&site, &mut admin, "bob").await;
    let folder = site.folder("ann-project");
    let r = keyed(&site, &ann, Method::POST, "/api/projects")
        .json(&json!({"path": folder}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let pid = r.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Ann sees and reads it.
    let list: Value = keyed(&site, &ann, Method::GET, "/api/projects")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list["projects"].as_array().unwrap().len(),
        1,
        "not the server's own"
    );
    assert_eq!(list["primary"], "");
    assert_eq!(
        status_of(keyed(
            &site,
            &ann,
            Method::GET,
            &format!("/api/projects/{pid}/file?path=secret.txt")
        ))
        .await,
        StatusCode::OK
    );

    // Bob: not in his list, and every route under it is "no such project".
    let list: Value = keyed(&site, &bob, Method::GET, "/api/projects")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!list.to_string().contains(&pid));
    let p = format!("/api/projects/{pid}");
    let attempts: Vec<(Method, String, Value)> = vec![
        (Method::GET, format!("{p}/tree"), Value::Null),
        (
            Method::GET,
            format!("{p}/file?path=secret.txt"),
            Value::Null,
        ),
        (
            Method::PUT,
            format!("{p}/file?path=secret.txt"),
            json!({"content": "x"}),
        ),
        (Method::GET, format!("{p}/changes"), Value::Null),
        (Method::GET, format!("{p}/markers"), Value::Null),
        (Method::GET, format!("{p}/worktrees"), Value::Null),
        (Method::GET, format!("{p}/raw?path=secret.txt"), Value::Null),
        (
            Method::POST,
            format!("{p}/preview"),
            json!({"path": "secret.txt"}),
        ),
        (Method::POST, format!("{p}/attachments"), Value::Null),
        (Method::GET, format!("{p}/git/status"), Value::Null),
        (
            Method::GET,
            format!("{p}/git/diff?path=secret.txt"),
            Value::Null,
        ),
        (
            Method::POST,
            format!("{p}/git/stage"),
            json!({"paths": ["secret.txt"]}),
        ),
        (Method::GET, format!("{p}/github"), Value::Null),
        (Method::GET, format!("{p}/pty"), Value::Null),
        (
            Method::POST,
            format!("{p}/profile"),
            json!({"profile": "x"}),
        ),
        (Method::POST, format!("{p}/mode"), json!({"mode": "x"})),
        (Method::DELETE, p.clone(), Value::Null),
    ];
    for (m, path, body) in attempts {
        let mut r = keyed(&site, &bob, m.clone(), &path);
        if !body.is_null() {
            r = r.json(&body);
        }
        let resp = r.send().await.unwrap();
        let st = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            st == StatusCode::NOT_FOUND || st == StatusCode::FORBIDDEN,
            "{m} {path}: {st} {text}"
        );
        assert!(!text.contains("ann-project's secret"), "{m} {path}");
    }
    // Still there for Ann, untouched.
    assert_eq!(
        std::fs::read_to_string(Path::new(&folder).join("secret.txt")).unwrap(),
        "ann-project's secret"
    );
    assert!(site.host.server.projects().info(&pid).is_some());

    // The hub: no snapshot, no prompt, no events of Ann's project for Bob.
    let mut wb = Ws::connect_as(site.host.server.port(), &bob, None).await;
    assert_eq!(
        wb.invoke("GetSnapshot", json!([pid])).await.unwrap_err(),
        "No such project."
    );
    assert_eq!(
        wb.invoke("SendPrompt", json!([pid, "rm -rf"]))
            .await
            .unwrap_err(),
        "No such project."
    );
    assert_eq!(
        wb.invoke("Cancel", json!([pid])).await.unwrap_err(),
        "No such project."
    );
    let mut wa = Ws::connect_as(site.host.server.port(), &ann, None).await;
    assert!(wa.invoke("GetSnapshot", json!([pid])).await.is_ok());
    let session = site
        .sessions
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .unwrap();
    session
        .bus
        .publish("notice", json!({"level": "info", "text": "ann only"}));
    let got = wa.events_until(|e| e["data"]["text"] == "ann only").await;
    assert!(!got.is_empty());
    // Bob's socket got nothing of it: his next message is the answer to his own call.
    let id = wb.send_invoke("GetSnapshot", json!(["nope"])).await;
    let next = wb.next().await;
    assert_eq!(next["type"], "result", "{next}");
    assert_eq!(next["id"], id.as_str());
    // Nor can he take over Ann's connection by resuming it.
    let wb2 = Ws::connect_as(
        site.host.server.port(),
        &bob,
        Some((wa.connection_id.as_str(), 0)),
    )
    .await;
    assert!(!wb2.resumed);
    assert_ne!(wb2.connection_id, wa.connection_id);

    // The admin sees everything.
    let all: Value = admin.get("/api/projects?all=1").await.1;
    let ann_entry = all["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == pid.as_str())
        .cloned()
        .unwrap();
    assert_eq!(ann_entry["owner"], "ann");
    assert_eq!(admin.get(&format!("{p}/tree")).await.0, StatusCode::OK);
    // A user doesn't see the server's own (primary) project.
    let primary = site.host.primary();
    assert_eq!(
        status_of(keyed(
            &site,
            &bob,
            Method::GET,
            &format!("/api/projects/{primary}/tree")
        ))
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        wb.invoke("GetSnapshot", json!([primary]))
            .await
            .unwrap_err(),
        "No such project."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn users_open_folders_only_inside_the_projects_locations() {
    let site = Site::start(Setup::default()).await;
    let mut admin = site.admin().await;
    let bob = user_with_key(&site, &mut admin, "bob").await;
    let outside = tempfile::tempdir().unwrap();
    let state_home = site.host.state_dir.path().to_string_lossy().into_owned();
    for path in [
        outside.path().to_string_lossy().into_owned(),
        state_home.clone(),
    ] {
        let st = status_of(
            keyed(&site, &bob, Method::POST, "/api/projects").json(&json!({"path": path})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}");
        let st = status_of(keyed(
            &site,
            &bob,
            Method::GET,
            &format!("/api/fs/browse?path={}", urlencode(&path)),
        ))
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}");
        let st = status_of(
            keyed(&site, &bob, Method::POST, "/api/fs/mkdir")
                .json(&json!({"parent": path, "name": "x"})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}");
        let st = status_of(
            keyed(&site, &bob, Method::POST, "/api/projects/clone")
                .json(&json!({"url": "https://example.invalid/r.git", "parent": path})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}");
    }
    // Browsing starts in the projects location; `..` out of it is refused.
    let r: Value = keyed(&site, &bob, Method::GET, "/api/fs/browse")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["parent"].is_null(), "{r}");
    let escape = format!("{}/..", site.roots.path().to_string_lossy());
    let st = status_of(keyed(
        &site,
        &bob,
        Method::GET,
        &format!("/api/fs/browse?path={}", urlencode(&escape)),
    ))
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Inside: fine. Two users opening one folder get two projects.
    let shared = site.folder("shared");
    let a: Value = keyed(&site, &bob, Method::POST, "/api/projects")
        .json(&json!({"path": shared}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (_, b) = admin.post("/api/projects", json!({"path": shared})).await;
    assert_ne!(a["id"], b["id"]);
}

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_out_closes_the_users_hub_sockets() {
    let site = Site::start(Setup::default()).await;
    let mut admin = site.admin().await;
    let bob = user_with_key(&site, &mut admin, "bob").await;
    let mut ws = Ws::connect_as(site.host.server.port(), &bob, None).await;
    let users: Value = admin.get("/api/admin/users").await.1;
    let id = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    admin
        .call(
            Method::PATCH,
            &format!("/api/admin/users/{id}"),
            json!({"disabled": true}),
        )
        .await;
    // The socket ends.
    assert!(ws.closed_within(std::time::Duration::from_secs(10)).await);
}

// ---- the launch token ----

#[tokio::test(flavor = "multi_thread")]
async fn the_launch_url_signs_in_as_the_first_admin_locally_but_not_when_exposed() {
    let site = Site::start(Setup::default()).await;
    let r = site
        .browser()
        .req(Method::GET, &format!("/?token={TOKEN}&x=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(
        r.headers()["location"],
        "/?x=1",
        "the token leaves the address bar"
    );
    let mut b = site.browser();
    b.keep(&r);
    assert!(b.cookie.is_some());
    let (_, state) = b.get("/api/auth/state").await;
    assert_eq!(state["user"], ADMIN);
    assert_eq!(state["admin"], true);
    // The header works for scripts too (no cookie).
    assert_eq!(
        status_of(site.host.api(Method::GET, "/api/info")).await,
        StatusCode::OK
    );

    let exposed = Site::start(Setup {
        exposed: true,
        ..Default::default()
    })
    .await;
    assert_eq!(
        status_of(exposed.host.api(Method::GET, "/api/info")).await,
        StatusCode::UNAUTHORIZED
    );
    let r = exposed
        .browser()
        .req(Method::GET, &format!("/?token={TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "just the sign-in page");
    assert!(r.headers().get("set-cookie").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exposed_server_trusts_its_proxy_for_https_and_marks_cookies_secure() {
    let site = Site::start(Setup {
        exposed: true,
        proxies: vec!["127.0.0.1".parse().unwrap()],
        ..Default::default()
    })
    .await;
    let via_proxy = |proto: &str, client: &str| {
        site.browser()
            .req_from(
                Method::POST,
                "/api/auth/login",
                &format!("{proto}://chat.example.org"),
            )
            .header("X-Forwarded-Proto", proto)
            .header("X-Forwarded-For", client)
            .header("X-Forwarded-Host", "chat.example.org")
            .json(&json!({"username": ADMIN, "password": ADMIN_PASSWORD}))
    };
    // Plain HTTP from another machine: refused.
    assert_eq!(
        status_of(via_proxy("http", "203.0.113.9")).await,
        StatusCode::FORBIDDEN
    );
    // HTTPS through the proxy: a __Host- cookie, Secure.
    let r = via_proxy("https", "203.0.113.9").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let set = r.headers()["set-cookie"].to_str().unwrap().to_lowercase();
    assert!(
        set.starts_with("__host-abhoth_session=") && set.contains("; secure"),
        "{set}"
    );
    // The client IP behind the proxy is what the audit log records.
    assert!(
        site.db()
            .audit_entries(20, None)
            .iter()
            .any(|e| e.event == "login.ok" && e.ip.as_deref() == Some("203.0.113.9"))
    );
}

// ---- the old account ----

#[tokio::test(flavor = "multi_thread")]
async fn the_old_single_account_becomes_the_first_admin_with_its_2fa() {
    let legacy = tempfile::tempdir().unwrap();
    let store = accounts::AccountStore::new(legacy.path());
    let mut acc = accounts::Account {
        username: "orca-admin".into(),
        password_hash: accounts::hash("the old long password", 1000),
        ..Default::default()
    };
    acc.totp_secret = Some(totp::new_secret());
    acc.totp_confirmed = true;
    store.save(&acc).unwrap();
    let site = Site::start(Setup {
        no_admin: true,
        legacy_dir: Some(legacy.path().to_path_buf()),
        ..Default::default()
    })
    .await;
    assert!(!site.host.server.setup_needed());
    let mut b = site.browser();
    let (st, v) = b.sign_in("orca-admin", "the old long password").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["stage"], "totp", "2FA came along");
    let code = totp::code(
        acc.totp_secret.as_deref().unwrap(),
        totp::step_at(site.now()),
    )
    .unwrap();
    let (_, v) = b.post("/api/auth/totp", json!({"code": code})).await;
    assert_eq!(v["admin"], true);
    // Re-hashed with Argon2id at that sign-in; account.json left as it was (rollback).
    let u = site.db().user_by_name("orca-admin").unwrap();
    assert!(
        u.password_hash.starts_with("$argon2id$"),
        "{}",
        u.password_hash
    );
    assert!(password::verify("the old long password", &u.password_hash));
    assert_eq!(store.load().unwrap(), acc);
    assert!(
        site.db()
            .audit_entries(50, None)
            .iter()
            .any(|e| e.event == "user.imported")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_admin_can_come_from_a_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pw");
    std::fs::write(&file, "from a file, long enough\n").unwrap();
    let work = tempfile::tempdir().unwrap();
    let f2 = file.clone();
    let host = Host::start_with(FakeSession::new(work.path()), move |o| {
        o.login.bootstrap_user = Some("ops".into());
        o.login.bootstrap_password = None;
        o.login.bootstrap_password_file = Some(f2);
    })
    .await;
    let u = host.server.state().login.db.user_by_name("ops").unwrap();
    assert!(u.admin);
    assert!(password::verify(
        "from a file, long enough",
        &u.password_hash
    ));
}

// ---- the desktop's local admin (D142) ----

#[tokio::test(flavor = "multi_thread")]
async fn the_desktop_signs_in_as_its_local_admin_which_has_no_password_sign_in() {
    let site = Site::start(Setup {
        no_admin: true,
        local_admin: true,
        ..Default::default()
    })
    .await;
    assert!(!site.host.server.setup_needed(), "no setup screen");
    let admin = site.db().first_admin().unwrap();
    assert!(admin.local_only());
    // The launch URL (what the desktop window opens) signs in.
    let r = site
        .browser()
        .req(Method::GET, &format!("/?token={TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    let mut b = site.browser();
    b.keep(&r);
    let (_, state) = b.get("/api/auth/state").await;
    assert_eq!(state["user"], admin.username.as_str());
    assert_eq!(state["admin"], true);
    let (_, me) = b.get("/api/account").await;
    assert_eq!(me["localOnly"], true);
    // Password sign-in refuses it, with the answer an unknown user gets.
    let (st, v) = site.browser().sign_in(&admin.username, "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st2, unknown) = site.browser().sign_in("nobody-here", "").await;
    assert_eq!((st2, &v), (StatusCode::UNAUTHORIZED, &unknown));
    assert_eq!(
        site.browser()
            .sign_in(&admin.username, "any long password at all")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // It sets a first password without a current one; then password sign-in works.
    let (_, _) = b.get("/api/auth/state").await;
    let (st, v) = b
        .post(
            "/api/account/password",
            json!({"password": "a first real password"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!site.db().user(admin.id).unwrap().local_only());
    assert_eq!(
        site.browser()
            .sign_in(&admin.username, "a first real password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exposed_server_on_the_desktops_home_asks_for_the_local_admins_password_first() {
    // The same home served exposed: only the passwordless local admin exists.
    let site = Site::start(Setup {
        no_admin: true,
        local_admin: true,
        exposed: true,
        ..Default::default()
    })
    .await;
    let admin = site.db().first_admin().unwrap();
    assert!(admin.local_only());
    assert!(site.host.server.setup_needed());
    let mut b = site.browser();
    let page = b.req(Method::GET, "/").send().await.unwrap();
    assert_eq!(page.headers()["location"], "/setup");
    assert_eq!(
        status_of(site.host.api(Method::GET, "/api/info")).await,
        StatusCode::FORBIDDEN,
        "the token opens nothing"
    );
    let (_, state) = b.get("/api/auth/state").await;
    assert_eq!(state["setup"], true);
    assert_eq!(state["setupUser"], admin.username.as_str());
    assert_eq!(
        b.sign_in(&admin.username, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    // The setup (with the code) sets its password: no second admin.
    let (st, _) = b
        .post(
            "/api/auth/setup",
            json!({"password": "a long enough password", "code": "wrong"}),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, v) = b
        .post(
            "/api/auth/setup",
            json!({"username": "someone-else", "password": "a long enough password", "code": TOKEN}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["user"], admin.username.as_str());
    assert_eq!(site.db().user_count(), 1);
    assert!(!site.host.server.setup_needed());
    assert_eq!(
        site.browser()
            .sign_in(&admin.username, "a long enough password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_server_still_shows_the_setup() {
    let site = Site::start(Setup {
        no_admin: true,
        ..Default::default()
    })
    .await;
    assert!(site.host.server.setup_needed());
    assert_eq!(site.db().user_count(), 0);
}

/// D137: the tab a user switches to becomes their last opened project (the desktop app opens it
/// when started from its install folder); another user can't mark it, and has their own.
#[tokio::test(flavor = "multi_thread")]
async fn switching_to_a_project_makes_it_that_users_last_opened_one() {
    let site = Site::start(Setup::default()).await;
    let mut admin = site.admin().await;
    let ann = user_with_key(&site, &mut admin, "ann").await;
    let bob = user_with_key(&site, &mut admin, "bob").await;
    let add = async |key: &str, name: &str| -> (String, String) {
        let folder = site.folder(name);
        let v: Value = keyed(&site, key, Method::POST, "/api/projects")
            .json(&json!({"path": folder}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (v["id"].as_str().unwrap().to_string(), folder)
    };
    let (first, first_dir) = add(&ann, "ann-first").await;
    let (_, second_dir) = add(&ann, "ann-second").await;
    let (_, bob_dir) = add(&bob, "bob-own").await;
    let ann_id = site.db().user_by_name("ann").unwrap().id;
    let bob_id = site.db().user_by_name("bob").unwrap().id;
    let store = site.host.server.projects().store();
    let last = |id: i64| store.last_opened(&[Some(id), None]).unwrap();
    let same = |a: PathBuf, b: &str| orca_webui::paths::same_path(&a.to_string_lossy(), b);
    assert!(same(last(ann_id), &second_dir), "adding opens it");

    let opened = format!("/api/projects/{first}/opened");
    assert_eq!(
        status_of(keyed(&site, &ann, Method::POST, &opened)).await,
        StatusCode::OK
    );
    assert!(
        same(last(ann_id), &first_dir),
        "switching back to the first"
    );
    // Bob can't reach Ann's project, and his own last project is untouched.
    assert_eq!(
        status_of(keyed(&site, &bob, Method::POST, &opened)).await,
        StatusCode::NOT_FOUND
    );
    assert!(same(last(bob_id), &bob_dir));
}
