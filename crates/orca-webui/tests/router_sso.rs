//! Sign in with Abhoth (D154), end to end over HTTP: the router's admin pages and this web UI
//! as its identity provider. A linked router registers itself, an admin signs in with a
//! password, consents once and gets the router's admin session; a non-admin is refused whatever
//! they answer; replayed codes, unknown or reused states and a missing binding cookie are
//! refused; a router that is not linked asks to be registered and an admin approves it.

mod common;

use std::sync::Arc;

use common::{ADMIN, ADMIN_PASSWORD, FakeSession, Host};
use orca_router::access::{Access, AccessSettings};
use orca_router::control_api::{ControlPlane, demo::DemoControl};
use orca_router::server::{self, Mode};
use orca_webui::login::CSRF_HEADER;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const LINK: &str = "link-token-0123456789abcdef0123456789";

struct Pair {
    web: Host,
    router: String,
    _work: tempfile::TempDir,
}

async fn pair() -> Pair {
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
        loopback_admin: false,
        sso_url: Some(web.url("")),
        ..Default::default()
    };
    let _ = st.control.set(Arc::new(ControlPlane {
        access: Access::new(settings, Some(LINK.as_bytes().to_vec())).with_sso(work.path()),
        backend: DemoControl::new(),
    }));
    Pair {
        web,
        router,
        _work: work,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// The first `name=value` of a Set-Cookie for `name`.
fn set_cookie(r: &reqwest::Response, name: &str) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .map(str::to_string)
}

fn first(c: &str) -> String {
    c.split(';').next().unwrap().to_string()
}

/// One browser on the web UI: its session cookie and CSRF token.
struct Web<'a> {
    host: &'a Host,
    cookie: Option<String>,
    csrf: Option<String>,
}

impl<'a> Web<'a> {
    fn new(host: &'a Host) -> Self {
        Self {
            host,
            cookie: None,
            csrf: None,
        }
    }

    async fn call(&mut self, m: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = client().request(m.clone(), self.host.url(path));
        if m != Method::GET {
            r = r.header("Origin", self.host.url(""));
            if let Some(c) = &self.csrf {
                r = r.header(CSRF_HEADER, c);
            }
        }
        if let Some(c) = &self.cookie {
            r = r.header("Cookie", c);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        if let Some(c) = r
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.contains("abhoth_session="))
        {
            self.cookie = Some(first(c));
        }
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if let Some(c) = v.get("csrf").and_then(Value::as_str) {
            self.csrf = Some(c.to_string());
        }
        (s, v)
    }

    async fn sign_in(&mut self, user: &str, password: &str) {
        let (s, v) = self
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({"username": user, "password": password})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["stage"], "ok", "{v}");
    }
}

/// A started sign-in at the router: the authorize URL's query and the binding cookie.
struct Started {
    query: String,
    state: String,
    binding: String,
}

async fn start(p: &Pair) -> Started {
    let r = client()
        .get(format!("{}/admin/sso", p.router))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let binding = set_cookie(&r, "abhoth_router_sso").expect("the binding cookie");
    assert!(
        binding.contains("HttpOnly") && binding.contains("SameSite=Lax"),
        "{binding}"
    );
    let (base, query) = loc.split_once('?').unwrap();
    assert_eq!(base, p.web.url("/oauth/authorize"));
    let state = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap()
        .to_string();
    assert!(query.contains("code_challenge_method=S256"));
    Started {
        query: format!("?{query}"),
        state,
        binding: first(&binding),
    }
}

fn params(query: &str) -> Value {
    let mut m = serde_json::Map::new();
    for kv in query.trim_start_matches('?').split('&') {
        let (k, v) = kv.split_once('=').unwrap();
        m.insert(k.into(), Value::String(decode(v)));
    }
    Value::Object(m)
}

fn decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            out.push(
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap(),
            );
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// Ask and answer on the web UI; the redirect back to the router.
async fn consent(web: &mut Web<'_>, s: &Started, allow: bool) -> (StatusCode, Value) {
    let mut body = params(&s.query);
    body["allow"] = Value::Bool(allow);
    web.call(Method::POST, "/api/oauth/authorize", Some(body))
        .await
}

/// The router's callback (the URL the web UI sent back), with a binding cookie or none.
async fn callback(redirect: &str, binding: Option<&str>) -> reqwest::Response {
    let mut r = client().get(redirect);
    if let Some(b) = binding {
        r = r.header("Cookie", b);
    }
    r.send().await.unwrap()
}

async fn router_session(p: &Pair, cookie: &str) -> Value {
    client()
        .get(format!("{}/admin/session", p.router))
        .header("Cookie", cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_signs_in_to_the_router_with_their_abhoth_account() {
    let p = pair().await;
    // The sign-in page offers the button.
    let s0: Value = client()
        .get(format!("{}/admin/session", p.router))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (s0["sso"].as_bool(), s0["admin"].as_bool()),
        (Some(true), Some(false))
    );

    // The router registers itself over the link at the first start.
    let s = start(&p).await;
    let clients = p.web.server.state().login.db.sso_clients();
    assert_eq!(clients.len(), 1);
    assert_eq!(
        clients[0].redirect_uris,
        vec![format!("{}/admin/sso-callback", p.router)]
    );

    // Not signed in on the web UI: the API wants a session (the page shows the sign-in).
    let mut web = Web::new(&p.web);
    let (st, _) = web
        .call(
            Method::GET,
            &format!("/api/oauth/authorize{}", s.query),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    web.sign_in(ADMIN, ADMIN_PASSWORD).await;
    let (st, ask) = web
        .call(
            Method::GET,
            &format!("/api/oauth/authorize{}", s.query),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{ask}");
    assert_eq!(
        (
            ask["admin"].as_bool(),
            ask["consented"].as_bool(),
            ask["user"].as_str()
        ),
        (Some(true), Some(false), Some(ADMIN))
    );
    assert_eq!(ask["redirectOrigin"], p.router);

    // Allow: back to the router with a code; the callback makes the admin session.
    let (st, v) = consent(&mut web, &s, true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let redirect = v["redirect"].as_str().unwrap().to_string();
    assert!(redirect.starts_with(&format!("{}/admin/sso-callback?code=", p.router)));
    let r = callback(&redirect, Some(&s.binding)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let session = set_cookie(&r, "abhoth_router_session").expect("a session");
    assert!(
        session.contains("HttpOnly") && session.contains("SameSite=Strict"),
        "{session}"
    );
    assert!(r.text().await.unwrap().contains("http-equiv=\"refresh\""));
    let me = router_session(&p, &first(&session)).await;
    assert_eq!(me["admin"], true, "{me}");
    assert_eq!(me["via"], "session");
    assert_eq!(me["principal"], format!("webui:{ADMIN}"));
    assert!(me["csrf"].as_str().is_some_and(|c| !c.is_empty()));
    // The session works for the admin API.
    let cookie = first(&session);
    let ok = client()
        .get(format!("{}/admin/api/whoami", p.router))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    // The same callback again: the state is used up.
    let again = callback(&redirect, Some(&s.binding)).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&again, "abhoth_router_session").is_none());

    // The code replayed on a fresh flow: the web UI refuses it (single use).
    let s2 = start(&p).await;
    let code = redirect
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let replay = callback(
        &format!(
            "{}/admin/sso-callback?code={code}&state={}",
            p.router, s2.state
        ),
        Some(&s2.binding),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
    assert!(set_cookie(&replay, "abhoth_router_session").is_none());
    assert!(replay.text().await.unwrap().contains("refused"));

    // Consent is remembered: the next sign-in needs no question.
    let s3 = start(&p).await;
    let (_, ask) = web
        .call(
            Method::GET,
            &format!("/api/oauth/authorize{}", s3.query),
            None,
        )
        .await;
    assert_eq!(ask["consented"], true);
    // ...and a callback from another browser (no binding cookie) is refused.
    let (_, v) = consent(&mut web, &s3, true).await;
    let other = callback(v["redirect"].as_str().unwrap(), None).await;
    assert_eq!(other.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&other, "abhoth_router_session").is_none());

    // An unknown state is refused.
    let s4 = start(&p).await;
    let bad = callback(
        &format!(
            "{}/admin/sso-callback?code=x&state=not-{}",
            p.router, s4.state
        ),
        Some(&s4.binding),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // Revoking the consent: asked again next time.
    let (st, list) = web.call(Method::GET, "/api/account/sso", None).await;
    assert_eq!(st, StatusCode::OK);
    let id = list["consents"][0]["clientId"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = web
        .call(Method::DELETE, &format!("/api/account/sso/{id}"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let s5 = start(&p).await;
    let (_, ask) = web
        .call(
            Method::GET,
            &format!("/api/oauth/authorize{}", s5.query),
            None,
        )
        .await;
    assert_eq!(ask["consented"], false);

    // Denying goes back with access_denied; the router refuses without a session.
    let (_, v) = consent(&mut web, &s5, false).await;
    let denied = v["redirect"].as_str().unwrap();
    assert!(denied.contains("error=access_denied"));
    let r = callback(denied, Some(&s5.binding)).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(set_cookie(&r, "abhoth_router_session").is_none());

    // Every step is in the web UI's audit.
    let events: Vec<String> = p
        .web
        .server
        .state()
        .login
        .db
        .audit_entries(200, None)
        .into_iter()
        .map(|e| e.event)
        .collect();
    for e in [
        "sso.client-registered",
        "sso.consent",
        "sso.authorized",
        "sso.sign-in",
        "sso.exchange-refused",
        "sso.consent-revoked",
        "sso.denied",
    ] {
        assert!(events.iter().any(|x| x == e), "{e} in {events:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_admin_is_refused_whatever_they_answer() {
    let p = pair().await;
    let mut admin = Web::new(&p.web);
    admin.sign_in(ADMIN, ADMIN_PASSWORD).await;
    let (st, v) = admin
        .call(
            Method::POST,
            "/api/admin/users",
            Some(json!({"username": "bob", "password": "user password 123", "admin": false})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s = start(&p).await;
    let mut bob = Web::new(&p.web);
    bob.sign_in("bob", "user password 123").await;
    let (st, ask) = bob
        .call(
            Method::GET,
            &format!("/api/oauth/authorize{}", s.query),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ask["admin"], false);
    assert!(
        ask["refused"]
            .as_str()
            .unwrap()
            .contains("Only Abhoth admins")
    );
    let (st, v) = consent(&mut bob, &s, true).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["refused"], true);
    assert!(v.get("redirect").is_none());
    let db = &p.web.server.state().login.db;
    let bob_id = db.user_by_name("bob").unwrap().id;
    assert!(db.sso_consents_of(bob_id).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_never_redirect_and_the_exchange_checks_everything() {
    let p = pair().await;
    let s = start(&p).await;
    let mut web = Web::new(&p.web);
    web.sign_in(ADMIN, ADMIN_PASSWORD).await;
    // Another redirect URI: an error here, never a redirect there.
    let evil = s.query.replace(
        &orca_core::sso::enc(&format!("{}/admin/sso-callback", p.router)),
        &orca_core::sso::enc("http://evil.example/admin/sso-callback"),
    );
    assert_ne!(evil, s.query);
    let (st, v) = web
        .call(Method::GET, &format!("/api/oauth/authorize{evil}"), None)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let mut body = params(&evil);
    body["allow"] = Value::Bool(true);
    let (st, v) = web
        .call(Method::POST, "/api/oauth/authorize", Some(body))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v.get("redirect").is_none());
    // No PKCE: refused.
    let plain = s
        .query
        .replace("code_challenge_method=S256", "code_challenge_method=plain");
    let (st, _) = web
        .call(Method::GET, &format!("/api/oauth/authorize{plain}"), None)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // A real code, exchanged with a wrong secret or verifier: refused, and used up.
    let (_, v) = consent(&mut web, &s, true).await;
    let redirect = v["redirect"].as_str().unwrap();
    let code = redirect
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let creds: orca_core::sso::ClientCredentials =
        serde_json::from_slice(&std::fs::read(p._work.path().join("sso-client.json")).unwrap())
            .unwrap();
    let exchange = |secret: &str, verifier: &str| {
        client()
            .post(p.web.url("/api/oauth/token"))
            .json(&json!({
                "grant_type": "authorization_code",
                "code": decode(code),
                "redirect_uri": format!("{}/admin/sso-callback", p.router),
                "client_id": creds.client_id,
                "client_secret": secret,
                "code_verifier": verifier,
            }))
            .send()
    };
    let r = exchange("wrong", &"v".repeat(43)).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = exchange(&creds.client_secret, &"v".repeat(43))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_grant");
    // The router's own callback with that code now fails too (it was used up above).
    let r = callback(redirect, Some(&s.binding)).await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_router_that_is_not_linked_asks_and_an_admin_approves() {
    let p = pair().await;
    let reg =
        json!({"name": "Spare box", "redirect_uris": ["http://192.0.2.9:8010/admin/sso-callback"]});
    let r = client()
        .post(p.web.url("/api/oauth/link-requests"))
        .json(&reg)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let t: orca_core::sso::LinkTicket = r.json().await.unwrap();
    let poll = || {
        client()
            .post(p.web.url("/api/oauth/link-requests/poll"))
            .json(&json!({"id": t.id, "poll": t.poll}))
            .send()
    };
    let s: Value = poll().await.unwrap().json().await.unwrap();
    assert_eq!(s["status"], "pending");
    // A wrong poll secret learns nothing.
    let s: Value = client()
        .post(p.web.url("/api/oauth/link-requests/poll"))
        .json(&json!({"id": t.id, "poll": "x"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["status"], "expired");
    let mut admin = Web::new(&p.web);
    admin.sign_in(ADMIN, ADMIN_PASSWORD).await;
    let (_, list) = admin.call(Method::GET, "/api/admin/sso", None).await;
    assert_eq!(list["requests"][0]["code"], t.code);
    let (st, v) = admin
        .call(
            Method::POST,
            &format!("/api/admin/sso/link-requests/{}/approve", t.id),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s: orca_core::sso::LinkStatus = poll().await.unwrap().json().await.unwrap();
    assert_eq!(s.status, "approved");
    let c = s.credentials.unwrap();
    assert!(c.client_id.starts_with("router-spare-box-"));
    // Collected once.
    let s: Value = poll().await.unwrap().json().await.unwrap();
    assert_eq!(s["status"], "expired");
    // Removing the client: gone from the list.
    let (st, _) = admin
        .call(
            Method::DELETE,
            &format!("/api/admin/sso/clients/{}", c.client_id),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (_, list) = admin.call(Method::GET, "/api/admin/sso", None).await;
    assert!(
        list["clients"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["id"] != c.client_id)
    );
}
