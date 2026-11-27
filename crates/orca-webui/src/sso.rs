//! Sign-in with Abhoth (D154, `docs/design/ROUTER-SSO.md`): this web UI as the identity
//! provider for routers' admin pages. A small OAuth2 authorization-code flow with PKCE (S256):
//!
//! 1. The router sends the browser to `/oauth/authorize?response_type=code&client_id=…&
//!    redirect_uri=…&state=…&code_challenge=…&code_challenge_method=S256`. That is the app's
//!    page: it signs the user in first (password, 2FA) when needed, then asks
//!    `GET /api/oauth/authorize` what is being asked and shows the consent screen.
//! 2. `POST /api/oauth/authorize {…, allow}` (the user's session and CSRF token): a denial goes
//!    back with `error=access_denied`; a non-admin is refused here whatever they choose; an
//!    admin's consent is remembered per router and user (revoked in the account dialog) and
//!    answered with a code: random, single use, 60 seconds, bound to the client, the redirect
//!    URI, the user and the PKCE challenge.
//! 3. The router exchanges it, server to server, at `POST /api/oauth/token` (its client id and
//!    secret, the code verifier) for `{sub, username, admin}`, and makes its own session.
//!
//! Clients: a linked router registers itself over the link (`POST /api/internal/sso/register`,
//! its link token); any other router asks (`POST /api/oauth/link-requests`, as `abhoth router
//! sso link` does) and an admin approves it once in Settings. Redirect URIs match exactly. The
//! client secret is stored as a SHA-256 hash and shown only to the router, once. Codes and link
//! requests live in memory. Every step is in the audit log; no secret, code or verifier is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use orca_core::router_link::fixed_time_eq;
use orca_core::sso::{
    AUTHORIZE_API, CODE_TTL_SECS, ClientCredentials, ClientRegistration, LINK_POLL_PATH,
    LINK_REQUESTS_PATH, LinkStatus, LinkTicket, REGISTER_INTERNAL_PATH, TOKEN_PATH, TokenRequest,
    TokenResponse, pkce_matches, pkce_ok, random, redirect_uri_ok, with_query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::login::db::{SsoClient, secret_hash};
use crate::login::endpoints::{body_json, no_store};
use crate::login::{Principal, Via, error, principal_of};
use crate::router_admin::link_check;
use crate::server::AppState;

/// Codes waiting to be exchanged, at most.
const MAX_CODES: usize = 500;
/// Link requests: how long one waits, how many at once, how many per address.
const LINK_TTL: i64 = 15 * 60;
const MAX_LINKS: usize = 20;
const MAX_LINKS_PER_IP: usize = 3;
/// Failed code exchanges per address in [`FAIL_WINDOW`] before it is refused for a while.
const FAILS_PER_IP: usize = 20;
const FAIL_WINDOW: i64 = 15 * 60;
/// Redirect URIs one client may have.
const MAX_REDIRECTS: usize = 16;

/// The POST paths that carry their own credentials (no session, no CSRF): the code exchange,
/// link requests and their poll, a linked router's registration (its link token).
pub fn is_public(path: &str) -> bool {
    path == TOKEN_PATH
        || path == LINK_REQUESTS_PATH
        || path == LINK_POLL_PATH
        || path == REGISTER_INTERNAL_PATH
}

#[derive(Debug, Clone)]
struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    user_id: i64,
    expires: i64,
}

#[derive(Debug, Clone)]
enum Stage {
    Pending,
    Approved(ClientCredentials),
    Denied,
}

#[derive(Debug, Clone)]
struct LinkReq {
    code: String,
    poll_hash: Vec<u8>,
    name: String,
    redirect_uris: Vec<String>,
    ip: String,
    created: i64,
    expires: i64,
    stage: Stage,
}

/// A waiting link request, as admins see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkView {
    pub id: String,
    pub code: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub ip: String,
    pub created: i64,
    pub expires_in: i64,
}

/// The provider's in-memory state: codes, link requests, failed exchanges.
#[derive(Default)]
pub struct Sso {
    codes: Mutex<HashMap<Vec<u8>, Code>>,
    links: Mutex<HashMap<String, LinkReq>>,
    fails: Mutex<HashMap<String, Vec<i64>>>,
}

impl Sso {
    fn sweep(&self, now: i64) -> Vec<(String, LinkReq)> {
        self.codes.lock().unwrap().retain(|_, c| c.expires > now);
        self.fails.lock().unwrap().retain(|_, v| {
            v.retain(|t| *t > now - FAIL_WINDOW);
            !v.is_empty()
        });
        let mut links = self.links.lock().unwrap();
        let gone: Vec<String> = links
            .iter()
            .filter(|(_, l)| l.expires <= now)
            .map(|(k, _)| k.clone())
            .collect();
        gone.into_iter()
            .filter_map(|k| links.remove(&k).map(|l| (k, l)))
            .collect()
    }

    fn issue(&self, c: Code) -> Option<String> {
        let mut codes = self.codes.lock().unwrap();
        if codes.len() >= MAX_CODES {
            return None;
        }
        let code = random(32);
        codes.insert(secret_hash(&code), c);
        Some(code)
    }

    /// Take a code (it never works twice, whatever happens next).
    fn take(&self, code: &str) -> Option<Code> {
        self.codes.lock().unwrap().remove(&secret_hash(code))
    }

    fn too_many(&self, ip: &str) -> bool {
        self.fails
            .lock()
            .unwrap()
            .get(ip)
            .is_some_and(|v| v.len() >= FAILS_PER_IP)
    }

    fn failed(&self, ip: &str, now: i64) {
        self.fails
            .lock()
            .unwrap()
            .entry(ip.to_string())
            .or_default()
            .push(now);
    }

    fn pending(&self, now: i64) -> Vec<LinkView> {
        let mut v: Vec<LinkView> = self
            .links
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| matches!(l.stage, Stage::Pending))
            .map(|(id, l)| LinkView {
                id: id.clone(),
                code: l.code.clone(),
                name: l.name.clone(),
                redirect_uris: l.redirect_uris.clone(),
                ip: l.ip.clone(),
                created: l.created,
                expires_in: (l.expires - now).max(0),
            })
            .collect();
        v.sort_by_key(|l| l.created);
        v
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(AUTHORIZE_API, get(ask).post(answer))
        .route(TOKEN_PATH, post(token))
        .route(REGISTER_INTERNAL_PATH, post(register_internal))
        .route(LINK_REQUESTS_PATH, post(link_create))
        .route(LINK_POLL_PATH, post(link_poll))
        .route("/api/admin/sso", get(admin_list))
        .route(
            "/api/admin/sso/link-requests/{id}/approve",
            post(link_approve),
        )
        .route("/api/admin/sso/link-requests/{id}/deny", post(link_deny))
        .route("/api/admin/sso/clients/{id}", delete(client_remove))
        .route("/api/account/sso", get(my_consents))
        .route("/api/account/sso/{client}", delete(revoke))
}

fn sweep(state: &AppState) {
    for (id, l) in state.sso.sweep(state.login.now()) {
        state.login.record(
            "sso.link-expired",
            None,
            Some(&l.name),
            Some(&l.ip),
            json!({"request": id}),
        );
    }
}

/// The caller's address (the public paths run before the login middleware notes it).
fn ip_of(state: &AppState, req: &Request<Body>) -> String {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    state
        .login
        .seen(peer, req.headers())
        .ip_text()
        .unwrap_or_else(|| "?".into())
}

/// Secrets travel only over HTTPS, loopback or the container network (as the router link).
fn transport_ok(state: &AppState, req: &Request<Body>) -> bool {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let seen = state.login.seen(peer, req.headers());
    let direct = peer.is_some_and(orca_core::router_link::link_peer_ok)
        && !peer.is_some_and(|p| state.login.options.trusted_proxies.contains(&p));
    seen.https || direct
}

fn clean_name(s: &str) -> Option<String> {
    let n: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    (!n.is_empty()).then_some(n)
}

/// "Lab Router" → "lab-router"; anything else → "-".
fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "router".into()
    } else {
        s.chars().take(40).collect()
    }
}

fn check_uris(uris: &[String]) -> Result<Vec<String>, String> {
    if uris.is_empty() || uris.len() > MAX_REDIRECTS {
        return Err(format!("1 to {MAX_REDIRECTS} redirect URIs"));
    }
    let mut out: Vec<String> = Vec::new();
    for u in uris {
        redirect_uri_ok(u).map_err(|e| format!("{u}: {e}"))?;
        if !out.contains(u) {
            out.push(u.clone());
        }
    }
    Ok(out)
}

/// A new secret for `client` (replacing any old one); the credentials to hand over once.
fn put_client(state: &AppState, client: &SsoClient) -> Option<ClientCredentials> {
    let secret = random(32);
    state
        .login
        .db
        .put_sso_client(client, &secret_hash(&secret))
        .then(|| ClientCredentials {
            client_id: client.id.clone(),
            client_secret: secret,
            redirect_uris: client.redirect_uris.clone(),
        })
}

// ---- the consent page's API ----

#[derive(Debug, Clone, Deserialize, Default)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
}

/// The client and its exact redirect URI, or why not. Nothing redirects on a failure here (a
/// redirect URI that is not the client's own would be an open redirect).
fn check_ask(state: &AppState, p: &AuthorizeParams) -> Result<SsoClient, &'static str> {
    let Some((client, _)) = state.login.db.sso_client(&p.client_id) else {
        return Err("This router is not registered here (unknown client_id).");
    };
    if !client.redirect_uris.iter().any(|u| u == &p.redirect_uri) {
        return Err("This router's address is not registered here (redirect_uri).");
    }
    if p.response_type != "code" {
        return Err("Only response_type=code is supported.");
    }
    if p.code_challenge_method != "S256" || !pkce_ok(&p.code_challenge) {
        return Err("A PKCE S256 code_challenge is required.");
    }
    if p.state.is_empty() || p.state.len() > 256 {
        return Err("A state of up to 256 characters is required.");
    }
    Ok(client)
}

/// Who may answer: a person in a browser (a session) or the desktop's launch token, not an
/// API key.
#[allow(clippy::result_large_err)]
fn person(req: &Request<Body>) -> Result<Principal, Response> {
    match principal_of(req) {
        Some(p) if !matches!(p.via, Via::ApiKey(_)) => Ok(p),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "Sign in in a browser to approve this (not with an API key).",
        )),
        None => Err(error(StatusCode::UNAUTHORIZED, "Sign in first.")),
    }
}

fn caller_ip(req: &Request<Body>) -> Option<String> {
    req.extensions()
        .get::<crate::login::Current>()
        .and_then(|c| c.seen.ip_text())
}

fn origin_of(uri: &str) -> String {
    let (scheme, rest) = uri.split_once("://").unwrap_or(("", uri));
    let host = rest.split('/').next().unwrap_or("");
    format!("{scheme}://{host}")
}

const REFUSED: &str =
    "Only Abhoth admins can sign in to a router's admin pages. Ask an admin if you need access.";

/// `GET /api/oauth/authorize?…`: what the consent page shows.
async fn ask(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AuthorizeParams>,
    req: Request<Body>,
) -> Response {
    let p = match person(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let client = match check_ask(&state, &q) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if !p.admin {
        state.login.record(
            "sso.refused",
            Some(&p),
            Some(&client.id),
            caller_ip(&req).as_deref(),
            json!({"why": "not an admin"}),
        );
    }
    no_store(json!({
        "client": {"id": client.id, "name": client.name},
        "redirectOrigin": origin_of(&q.redirect_uri),
        "user": p.username,
        "admin": p.admin,
        "refused": (!p.admin).then_some(REFUSED),
        "consented": p.admin && state.login.db.sso_consented(&client.id, p.user_id),
    }))
}

/// `POST /api/oauth/authorize {…the query…, allow}`: `{redirect}` (back to the router).
async fn answer(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let p = match person(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let ip = caller_ip(&req);
    let body = body_json(req).await;
    let q: AuthorizeParams = serde_json::from_value(body.clone()).unwrap_or_default();
    let client = match check_ask(&state, &q) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if body["allow"] != Value::Bool(true) {
        state.login.record(
            "sso.denied",
            Some(&p),
            Some(&client.id),
            ip.as_deref(),
            json!({}),
        );
        return no_store(json!({"redirect": with_query(
            &q.redirect_uri,
            &[("error", "access_denied"), ("state", &q.state)],
        )}));
    }
    if !p.admin {
        state.login.record(
            "sso.refused",
            Some(&p),
            Some(&client.id),
            ip.as_deref(),
            json!({"why": "not an admin"}),
        );
        return json_error(
            StatusCode::FORBIDDEN,
            json!({"error": REFUSED, "refused": true}),
        );
    }
    let now = state.login.now();
    if !state.login.db.sso_consented(&client.id, p.user_id) {
        state.login.db.add_sso_consent(&client.id, p.user_id, now);
        state.login.record(
            "sso.consent",
            Some(&p),
            Some(&client.id),
            ip.as_deref(),
            json!({"name": client.name}),
        );
    }
    sweep(&state);
    let Some(code) = state.sso.issue(Code {
        client_id: client.id.clone(),
        redirect_uri: q.redirect_uri.clone(),
        challenge: q.code_challenge.clone(),
        user_id: p.user_id,
        expires: now + CODE_TTL_SECS,
    }) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many sign-ins are waiting; try again in a minute.",
        );
    };
    state.login.record(
        "sso.authorized",
        Some(&p),
        Some(&client.id),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"redirect": with_query(
        &q.redirect_uri,
        &[("code", &code), ("state", &q.state)],
    )}))
}

// ---- the code exchange ----

fn json_error(code: StatusCode, v: Value) -> Response {
    let mut r = (code, axum::Json(v)).into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    r
}

fn oauth_error(code: StatusCode, what: &str, why: &str) -> Response {
    json_error(code, json!({"error": what, "error_description": why}))
}

/// `POST /api/oauth/token` (JSON [`TokenRequest`]) → [`TokenResponse`].
async fn token(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let ip = ip_of(&state, &req);
    if !transport_ok(&state, &req) {
        return error(
            StatusCode::FORBIDDEN,
            "The code exchange needs HTTPS, loopback or the container network.",
        );
    }
    sweep(&state);
    let now = state.login.now();
    if state.sso.too_many(&ip) {
        return oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Too many failed exchanges from this address; wait a few minutes.",
        );
    }
    let bytes = axum::body::to_bytes(req.into_body(), 8 * 1024)
        .await
        .unwrap_or_default();
    let Ok(t) = serde_json::from_slice::<TokenRequest>(&bytes) else {
        state.sso.failed(&ip, now);
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Expected a JSON token request.",
        );
    };
    // The code goes first: whatever is wrong below, it never works again.
    let code = state.sso.take(&t.code);
    let fail = |what: &str, why: &str, status: StatusCode| {
        state.sso.failed(&ip, now);
        state.login.record(
            "sso.exchange-refused",
            None,
            Some(&t.client_id.chars().take(80).collect::<String>()),
            Some(&ip),
            json!({"why": why}),
        );
        oauth_error(status, what, why)
    };
    let client_ok = state
        .login
        .db
        .sso_client(&t.client_id)
        .is_some_and(|(_, h)| fixed_time_eq(&secret_hash(&t.client_secret), &h));
    if !client_ok {
        return fail(
            "invalid_client",
            "unknown client or wrong secret",
            StatusCode::UNAUTHORIZED,
        );
    }
    if t.grant_type != "authorization_code" {
        return fail(
            "unsupported_grant_type",
            "only authorization_code",
            StatusCode::BAD_REQUEST,
        );
    }
    let Some(c) = code.filter(|c| c.expires > now) else {
        return fail(
            "invalid_grant",
            "unknown, used or expired code",
            StatusCode::BAD_REQUEST,
        );
    };
    if c.client_id != t.client_id || c.redirect_uri != t.redirect_uri {
        return fail(
            "invalid_grant",
            "the code was issued to another client or address",
            StatusCode::BAD_REQUEST,
        );
    }
    if !pkce_matches(&t.code_verifier, &c.challenge) {
        return fail(
            "invalid_grant",
            "the code verifier does not match",
            StatusCode::BAD_REQUEST,
        );
    }
    let Some(u) = state.login.db.user(c.user_id).filter(|u| !u.disabled) else {
        return fail(
            "invalid_grant",
            "the account is gone or disabled",
            StatusCode::BAD_REQUEST,
        );
    };
    state.login.db.touch_sso_consent(&c.client_id, u.id, now);
    state.login.record(
        "sso.sign-in",
        None,
        Some(&c.client_id),
        Some(&ip),
        json!({"user": u.username, "admin": u.admin}),
    );
    no_store(
        serde_json::to_value(TokenResponse {
            sub: u.id,
            username: u.username,
            admin: u.admin,
        })
        .unwrap_or(Value::Null),
    )
}

// ---- registration ----

/// `POST /api/internal/sso/register` [`ClientRegistration`] (a linked router, its link token)
/// → [`ClientCredentials`] (a new secret each time).
async fn register_internal(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = link_check(&state, &req, "router.sso-register") {
        return r;
    }
    let ip = ip_of(&state, &req);
    let bytes = axum::body::to_bytes(req.into_body(), 16 * 1024)
        .await
        .unwrap_or_default();
    let Ok(reg) = serde_json::from_slice::<ClientRegistration>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Expected {\"name\", \"redirect_uris\"}.",
        );
    };
    let Some(name) = clean_name(&reg.name) else {
        return error(StatusCode::BAD_REQUEST, "Name the router.");
    };
    let uris = match check_uris(&reg.redirect_uris) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let id = format!("router-{}", slug(&name));
    let created = state
        .login
        .db
        .sso_client(&id)
        .map(|(c, _)| c.created)
        .unwrap_or_else(|| state.login.now());
    let client = SsoClient {
        id: id.clone(),
        name: name.clone(),
        redirect_uris: uris.clone(),
        created,
        created_by: Some("router link".into()),
    };
    let Some(creds) = put_client(&state, &client) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the client.",
        );
    };
    state.login.record(
        "sso.client-registered",
        None,
        Some(&id),
        Some(&ip),
        json!({"name": name, "redirectUris": uris, "via": "link"}),
    );
    no_store(serde_json::to_value(creds).unwrap_or(Value::Null))
}

/// `POST /api/oauth/link-requests` [`ClientRegistration`] (anyone; rate limited) →
/// [`LinkTicket`]. Nothing is registered until an admin approves it.
async fn link_create(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let ip = ip_of(&state, &req);
    if !transport_ok(&state, &req) {
        return error(
            StatusCode::FORBIDDEN,
            "Linking needs HTTPS, loopback or the container network.",
        );
    }
    sweep(&state);
    let bytes = axum::body::to_bytes(req.into_body(), 16 * 1024)
        .await
        .unwrap_or_default();
    let Ok(reg) = serde_json::from_slice::<ClientRegistration>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Expected {\"name\", \"redirect_uris\"}.",
        );
    };
    let Some(name) = clean_name(&reg.name) else {
        return error(StatusCode::BAD_REQUEST, "Name the router.");
    };
    let uris = match check_uris(&reg.redirect_uris) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let now = state.login.now();
    let ticket = {
        let mut links = state.sso.links.lock().unwrap();
        if links.len() >= MAX_LINKS
            || links.values().filter(|l| l.ip == ip).count() >= MAX_LINKS_PER_IP
        {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many link requests are waiting.",
            );
        }
        let id = random(12);
        let poll = random(32);
        let code = crate::key_requests::new_code();
        links.insert(
            id.clone(),
            LinkReq {
                code: code.clone(),
                poll_hash: secret_hash(&poll),
                name: name.clone(),
                redirect_uris: uris.clone(),
                ip: ip.clone(),
                created: now,
                expires: now + LINK_TTL,
                stage: Stage::Pending,
            },
        );
        LinkTicket {
            id,
            code,
            poll,
            expires_in: LINK_TTL,
        }
    };
    state.login.record(
        "sso.link-requested",
        None,
        Some(&name),
        Some(&ip),
        json!({"request": ticket.id, "code": ticket.code, "redirectUris": uris}),
    );
    no_store(serde_json::to_value(ticket).unwrap_or(Value::Null))
}

/// `POST /api/oauth/link-requests/poll {id, poll}` → [`LinkStatus`] (the credentials once).
async fn link_poll(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if !transport_ok(&state, &req) {
        return error(
            StatusCode::FORBIDDEN,
            "Linking needs HTTPS, loopback or the container network.",
        );
    }
    sweep(&state);
    let v = body_json(req).await;
    let (id, poll) = (
        v["id"].as_str().unwrap_or(""),
        v["poll"].as_str().unwrap_or(""),
    );
    let mut links = state.sso.links.lock().unwrap();
    let Some(l) = links
        .get(id)
        .filter(|l| fixed_time_eq(&secret_hash(poll), &l.poll_hash))
    else {
        return no_store(json!({"status": "expired"}));
    };
    let status = match &l.stage {
        Stage::Pending => LinkStatus {
            status: "pending".into(),
            credentials: None,
        },
        Stage::Denied => LinkStatus {
            status: "denied".into(),
            credentials: None,
        },
        Stage::Approved(c) => LinkStatus {
            status: "approved".into(),
            credentials: Some(c.clone()),
        },
    };
    if !matches!(l.stage, Stage::Pending) {
        let l = links.remove(id).expect("present");
        drop(links);
        if let Stage::Approved(c) = &l.stage {
            state.login.record(
                "sso.link-collected",
                None,
                Some(&c.client_id),
                Some(&l.ip),
                json!({"request": id}),
            );
        }
    }
    no_store(serde_json::to_value(status).unwrap_or(Value::Null))
}

// ---- admins ----

#[allow(clippy::result_large_err)]
fn admin(req: &Request<Body>) -> Result<Principal, Response> {
    principal_of(req)
        .filter(|p| p.admin)
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "Only an admin can do that."))
}

/// `GET /api/admin/sso`: registered routers and waiting link requests.
async fn admin_list(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = admin(&req) {
        return r;
    }
    sweep(&state);
    no_store(json!({
        "clients": state.login.db.sso_clients(),
        "requests": state.sso.pending(state.login.now()),
    }))
}

async fn link_approve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    let p = match admin(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let ip = caller_ip(&req);
    sweep(&state);
    let now = state.login.now();
    let (name, uris) = {
        let links = state.sso.links.lock().unwrap();
        match links.get(&id).filter(|l| matches!(l.stage, Stage::Pending)) {
            Some(l) => (l.name.clone(), l.redirect_uris.clone()),
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    "That request is no longer waiting (expired, approved or denied).",
                );
            }
        }
    };
    // Its own id (not a linked router's): a request cannot take over a registered client.
    let client_id = format!("router-{}-{}", slug(&name), &random(6).to_lowercase()[..6]);
    let client = SsoClient {
        id: client_id.clone(),
        name: name.clone(),
        redirect_uris: uris.clone(),
        created: now,
        created_by: Some(p.username.clone()),
    };
    let Some(creds) = put_client(&state, &client) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the client.",
        );
    };
    if let Some(l) = state.sso.links.lock().unwrap().get_mut(&id) {
        l.stage = Stage::Approved(creds);
    }
    state.login.record(
        "sso.client-registered",
        Some(&p),
        Some(&client_id),
        ip.as_deref(),
        json!({"name": name, "redirectUris": uris, "via": "link request", "request": id}),
    );
    no_store(json!({"ok": true, "client": client}))
}

async fn link_deny(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    let p = match admin(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let ip = caller_ip(&req);
    let name = {
        let mut links = state.sso.links.lock().unwrap();
        match links
            .get_mut(&id)
            .filter(|l| matches!(l.stage, Stage::Pending))
        {
            Some(l) => {
                l.stage = Stage::Denied;
                l.name.clone()
            }
            None => return error(StatusCode::NOT_FOUND, "That request is no longer waiting."),
        }
    };
    state.login.record(
        "sso.link-denied",
        Some(&p),
        Some(&name),
        ip.as_deref(),
        json!({"request": id}),
    );
    no_store(json!({"ok": true}))
}

async fn client_remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    let p = match admin(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !state.login.db.delete_sso_client(&id) {
        return error(StatusCode::NOT_FOUND, "No such router.");
    }
    state.login.record(
        "sso.client-removed",
        Some(&p),
        Some(&id),
        caller_ip(&req).as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

// ---- the user's own consents ----

async fn my_consents(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let p = match person(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    no_store(json!({"consents": state.login.db.sso_consents_of(p.user_id)}))
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(client): Path<String>,
    req: Request<Body>,
) -> Response {
    let p = match person(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !state.login.db.delete_sso_consent(&client, p.user_id) {
        return error(StatusCode::NOT_FOUND, "No such consent.");
    }
    state.login.record(
        "sso.consent-revoked",
        Some(&p),
        Some(&client),
        caller_ip(&req).as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_origins() {
        assert_eq!(slug("Lab Router"), "lab-router");
        assert_eq!(slug("  My router #2 "), "my-router--2");
        assert_eq!(slug("!!!"), "router");
        assert_eq!(
            origin_of("https://h.example:8010/admin/auth/callback"),
            "https://h.example:8010"
        );
        assert!(is_public(TOKEN_PATH) && !is_public(AUTHORIZE_API));
    }

    #[test]
    fn codes_work_once_and_expire() {
        let s = Sso::default();
        let c = Code {
            client_id: "c".into(),
            redirect_uri: "u".into(),
            challenge: "x".into(),
            user_id: 1,
            expires: 100,
        };
        let code = s.issue(c.clone()).unwrap();
        assert!(s.take(&code).is_some());
        assert!(s.take(&code).is_none(), "single use");
        let code = s.issue(c).unwrap();
        s.sweep(100);
        assert!(s.take(&code).is_none(), "expired codes are swept");
    }

    #[test]
    fn failures_are_counted_per_address() {
        let s = Sso::default();
        for _ in 0..FAILS_PER_IP {
            assert!(!s.too_many("a"));
            s.failed("a", 10);
        }
        assert!(s.too_many("a") && !s.too_many("b"));
        s.sweep(10 + FAIL_WINDOW);
        assert!(!s.too_many("a"));
    }
}
