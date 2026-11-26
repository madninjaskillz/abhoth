//! The web server's side of `abhoth router`'s control plane (ROUTER-CONFIG.md 11.1-11.2):
//!
//! - `POST /api/internal/keys/introspect`: the router asks who an `abk_…` key belongs to
//!   (by its SHA-256), authenticated with the link token; it caches the answer for 60 s.
//! - `/api/admin/service-keys`: admins issue and revoke **service keys**, named keys for
//!   clients such as Jarvis, the video pipelines, Home Assistant and the dash buttons. They are
//!   owned by the admin who issued them, carry scopes (`chat`, `models`, `switch`, `leases`,
//!   `read`, `group:<name>`) and work only at the router, never in this web UI.
//! - `/api/admin/router/*`: the admin pages' calls, proxied to the router's `/admin/api/*` with
//!   the acting user in a signed header (the router writes it into its own audit).
//!
//! The link (router URL and token) comes from `ABHOTH_ROUTER_URL` and
//! `ABHOTH_ROUTER_TOKEN_FILE`; without it only the service-key pages exist.

use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use orca_core::router_link::{
    self, ActingUser, INTROSPECT_PATH, IntrospectRequest, Introspection, Role, fixed_time_eq,
    sign_acting_user, valid_scope,
};
use serde_json::{Value, json};

use crate::login::db::{ApiKey, KEY_SERVICE};
use crate::login::endpoints::{body_json, no_store};
use crate::login::{API_KEY_PREFIX, Current, error, principal_of};
use crate::server::AppState;

/// How the web server reaches the router.
#[derive(Clone)]
pub struct RouterLink {
    /// The router's base URL (its admin API is under `/admin/api/`).
    pub url: String,
    /// The shared link token (the router's `secrets.accounts_token`).
    pub token: Vec<u8>,
}

impl RouterLink {
    /// A signed call to the router's admin API from the web server itself (no browser), as
    /// `acting` (e.g. the bench queue). Returns the status and the JSON body.
    pub async fn call(
        &self,
        method: &str,
        rest: &str,
        acting: &ActingUser,
        body: &Value,
    ) -> Result<(u16, Value), String> {
        let path = format!("/admin/api/{rest}");
        let sig = sign_acting_user(
            &self.token,
            acting,
            chrono::Utc::now().timestamp(),
            method,
            &path,
        );
        let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let r = http_client()
            .request(m, format!("{}{path}", self.url))
            .header(router_link::ACTING_USER_HEADER, sig)
            .header("x-abhoth-no-switch", "1")
            .timeout(Duration::from_secs(15))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("router: {}", e.without_url()))?;
        let st = r.status().as_u16();
        Ok((st, r.json().await.unwrap_or(Value::Null)))
    }

    /// From `ABHOTH_ROUTER_URL` and `ABHOTH_ROUTER_TOKEN_FILE` (both needed; the token file must
    /// hold at least 32 characters).
    pub fn from_env() -> Result<Option<Self>, String> {
        let url = orca_core::brand::env_var("ROUTER_URL").filter(|s| !s.trim().is_empty());
        let file = orca_core::brand::env_var("ROUTER_TOKEN_FILE").filter(|s| !s.trim().is_empty());
        match (url, file) {
            (None, None) => Ok(None),
            (Some(url), Some(file)) => Self::load(&url, FsPath::new(&file)).map(Some),
            _ => Err("ABHOTH_ROUTER_URL and ABHOTH_ROUTER_TOKEN_FILE go together.".into()),
        }
    }

    pub fn load(url: &str, token_file: &FsPath) -> Result<Self, String> {
        let token = std::fs::read_to_string(token_file)
            .map_err(|e| format!("Could not read the router token file: {e}"))?;
        Self::new(url, token.trim())
    }

    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        if token.len() < 32 {
            return Err("The router link token must be at least 32 characters.".into());
        }
        let url = url.trim().trim_end_matches('/').to_string();
        if let Err(e) = orca_core::router_link::link_url_ok(&url) {
            return Err(format!("ABHOTH_ROUTER_URL: {e}"));
        }
        Ok(Self {
            url,
            token: token.as_bytes().to_vec(),
        })
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(INTROSPECT_PATH, post(introspect))
        .route("/api/admin/service-keys", get(list_keys).post(create_key))
        .route("/api/admin/service-keys/{id}", delete(revoke_key))
        .merge(crate::key_requests::routes())
        .route("/api/admin/router", any(proxy_root))
        .route("/api/admin/router/{*rest}", any(proxy))
}

/// Is this the introspection path (the login middleware lets it through to its own check)?
pub fn is_internal(path: &str) -> bool {
    path == INTROSPECT_PATH
        || path == router_link::KEY_REQUESTS_INTERNAL_PATH
        || path == router_link::KEY_REQUESTS_POLL_INTERNAL_PATH
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let a = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, v) = a.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| v.trim())
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 {
        return None;
    }
    (0..32)
        .map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

/// The router's own calls (introspection, key requests): the link token, only over loopback,
/// the container network, or HTTPS (via a trusted proxy). Refused before the token is looked at,
/// so a LAN client learns nothing. `what`: the audit event's prefix (`router.introspect`).
#[allow(clippy::result_large_err)]
pub(crate) fn link_check(
    state: &AppState,
    req: &Request<Body>,
    what: &str,
) -> Result<(), Response> {
    if state.options.router.is_none() {
        return Err(error(StatusCode::NOT_FOUND, "Not found."));
    }
    let link = state.options.router.as_ref().expect("checked");
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let seen = state.login.seen(peer, req.headers());
    let direct = peer.is_some_and(orca_core::router_link::link_peer_ok)
        && !peer.is_some_and(|p| state.login.options.trusted_proxies.contains(&p));
    if !(seen.https || direct) {
        state.login.record(
            &format!("{what}.rejected"),
            None,
            None,
            seen.ip.map(|i| i.to_string()).as_deref(),
            json!({"why": "plain http from outside loopback and the container network"}),
        );
        return Err(error(
            StatusCode::FORBIDDEN,
            "The router link needs HTTPS, loopback or the container network.",
        ));
    }
    let ok = bearer(req.headers()).is_some_and(|t| fixed_time_eq(t.as_bytes(), &link.token));
    if !ok {
        let ip = peer.map(|i| i.to_string());
        state.login.record(
            &format!("{what}.rejected"),
            None,
            None,
            ip.as_deref(),
            json!({}),
        );
        return Err(error(StatusCode::UNAUTHORIZED, "Not the router."));
    }
    Ok(())
}

/// `POST /api/internal/keys/introspect` `{key_sha256}` with `Authorization: Bearer <link token>`.
async fn introspect(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = link_check(&state, &req, "router.introspect") {
        return r;
    }
    let body = axum::body::to_bytes(req.into_body(), 4096)
        .await
        .unwrap_or_default();
    let Ok(ask) = serde_json::from_slice::<IntrospectRequest>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Expected {\"key_sha256\": hex}.");
    };
    let Some(hash) = from_hex(&ask.key_sha256.to_ascii_lowercase()) else {
        return error(StatusCode::BAD_REQUEST, "key_sha256 must be 64 hex digits.");
    };
    let db = &state.login.db;
    let answer = db
        .api_key_by_hash(&hash)
        .and_then(|k| db.user(k.user_id).map(|u| (k, u)))
        .map(|(k, u)| {
            db.touch_api_key(k.id, state.login.now());
            let service = k.kind == KEY_SERVICE;
            // A service key needs its owner to still be an admin.
            let disabled = u.disabled || (service && !u.admin);
            Introspection {
                active: !disabled,
                user_id: Some(u.id),
                name: Some(u.username.clone()),
                role: Some(if service {
                    Role::Service
                } else if u.admin {
                    Role::Admin
                } else {
                    Role::User
                }),
                key_id: Some(k.id),
                key_name: Some(k.name.clone()),
                scopes: if service { k.scopes.clone() } else { vec![] },
                disabled,
            }
        })
        .unwrap_or_default();
    no_store(serde_json::to_value(answer).unwrap_or(Value::Null))
}

fn describe(state: &AppState, k: &ApiKey) -> Value {
    let mut v = json!(k);
    v["owner"] = json!(state.login.db.user(k.user_id).map(|u| u.username));
    v
}

/// The service keys (admins).
async fn list_keys(State(state): State<Arc<AppState>>) -> Response {
    let list: Vec<Value> = state
        .login
        .db
        .service_keys()
        .iter()
        .map(|k| describe(&state, k))
        .collect();
    no_store(json!({
        "keys": list,
        "scopes": router_link::SERVICE_SCOPES,
        "router": state.options.router.is_some(),
    }))
}

/// A service key's name: what `access = "keys:<name>"` matches, so a simple lower-case word.
pub fn check_key_name(name: &str) -> Result<String, &'static str> {
    let n = name.trim().to_lowercase();
    if n.is_empty() || n.len() > 40 {
        return Err("Name the key (1 to 40 characters).");
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Use letters, digits, '-' and '_' only (routes name keys this way).");
    }
    Ok(n)
}

/// `POST /api/admin/service-keys` `{name, scopes, note}`: the key is shown once.
async fn create_key(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let Some(p) = principal_of(&req).filter(|p| p.admin) else {
        return error(StatusCode::FORBIDDEN, "Only an admin can do that.");
    };
    let ip = req
        .extensions()
        .get::<Current>()
        .and_then(|c| c.seen.ip_text());
    let body = body_json(req).await;
    let name = match check_key_name(body["name"].as_str().unwrap_or("")) {
        Ok(n) => n,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let scopes: Vec<String> = body["scopes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    if scopes.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Give the key at least one scope.");
    }
    if let Some(bad) = scopes.iter().find(|s| !valid_scope(s)) {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("Unknown scope: {bad} (chat, models, switch, leases, read or group:<name>)."),
        );
    }
    let mut scopes = scopes;
    scopes.sort();
    scopes.dedup();
    let note = body["note"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(200).collect::<String>());
    let db = &state.login.db;
    if db.service_keys().iter().any(|k| k.name == name) {
        return error(
            StatusCode::CONFLICT,
            "A service key has that name; revoke it first or pick another.",
        );
    }
    if db.service_keys().len() >= 200 {
        return error(StatusCode::BAD_REQUEST, "200 service keys at most.");
    }
    let key = format!("{API_KEY_PREFIX}{}", crate::login::store_token());
    let prefix: String = key.chars().take(API_KEY_PREFIX.len() + 6).collect();
    let Some(k) = db.create_service_key(
        p.user_id,
        &name,
        &key,
        &prefix,
        state.login.now(),
        &scopes,
        note.as_deref(),
    ) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save the key.");
    };
    state.login.record(
        "servicekey.created",
        Some(&p),
        Some(&name),
        ip.as_deref(),
        json!({"key": k.id, "scopes": scopes}),
    );
    let mut v = describe(&state, &k);
    v["key"] = json!(key);
    no_store(v)
}

async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let Some(p) = principal_of(&req).filter(|p| p.admin) else {
        return error(StatusCode::FORBIDDEN, "Only an admin can do that.");
    };
    let ip = req
        .extensions()
        .get::<Current>()
        .and_then(|c| c.seen.ip_text());
    let db = &state.login.db;
    let Some(k) = db.service_key(id) else {
        return error(StatusCode::NOT_FOUND, "No such service key.");
    };
    db.delete_service_key(id);
    state.login.record(
        "servicekey.revoked",
        Some(&p),
        Some(&k.name),
        ip.as_deref(),
        json!({"key": id, "note": "the router stops accepting it within a minute (its cache)"}),
    );
    no_store(json!({"ok": true}))
}

async fn proxy_root(state: State<Arc<AppState>>, req: Request<Body>) -> Response {
    forward(state, String::new(), req).await
}

async fn proxy(
    state: State<Arc<AppState>>,
    Path(rest): Path<String>,
    req: Request<Body>,
) -> Response {
    forward(state, rest, req).await
}

/// Headers passed to the router and back.
const FORWARD_IN: &[&str] = &["content-type", "if-match", "accept", "last-event-id"];
const FORWARD_OUT: &[&str] = &["content-type", "etag", "retry-after", "cache-control"];

fn http_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("http client")
    })
}

/// `/api/admin/router/<rest>` -> `<router>/admin/api/<rest>`, signed as the acting admin.
async fn forward(State(state): State<Arc<AppState>>, rest: String, req: Request<Body>) -> Response {
    let Some(link) = state.options.router.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "No router is linked (ABHOTH_ROUTER_URL and ABHOTH_ROUTER_TOKEN_FILE).",
        );
    };
    let Some(p) = principal_of(&req).filter(|p| p.admin) else {
        return error(StatusCode::FORBIDDEN, "Only an admin can do that.");
    };
    if rest.split('/').any(|s| s == ".." || s == ".") {
        return error(StatusCode::BAD_REQUEST, "Bad path.");
    }
    let ip = req
        .extensions()
        .get::<Current>()
        .and_then(|c| c.seen.ip_text());
    let method = req.method().clone();
    let path = if rest.is_empty() {
        "/admin/api".to_string()
    } else {
        format!("/admin/api/{rest}")
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let mut out = reqwest::header::HeaderMap::new();
    for h in FORWARD_IN {
        if let Some(v) = req.headers().get(*h)
            && let Ok(v) = reqwest::header::HeaderValue::from_bytes(v.as_bytes())
        {
            out.insert(*h, v);
        }
    }
    let acting = ActingUser {
        user_id: p.user_id,
        name: p.username.clone(),
        admin: p.admin,
    };
    let sig = sign_acting_user(
        &link.token,
        &acting,
        chrono::Utc::now().timestamp(),
        method.as_str(),
        &path,
    );
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&sig) {
        out.insert(router_link::ACTING_USER_HEADER, v);
    }
    out.insert(
        "x-abhoth-no-switch",
        reqwest::header::HeaderValue::from_static("1"),
    );
    let body = match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request too large."),
    };
    let changes = method != Method::GET && method != Method::HEAD;
    let rm =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let sent = http_client()
        .request(rm, format!("{}{path}{query}", link.url))
        .headers(out)
        .body(body.clone())
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let why = if e.is_connect() {
                "The router is not reachable."
            } else {
                "The router did not answer."
            };
            return error(StatusCode::BAD_GATEWAY, why);
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if changes {
        state.login.record(
            "router.admin",
            Some(&p),
            Some(&path),
            ip.as_deref(),
            json!({"method": method.as_str(), "status": status.as_u16()}),
        );
    }
    let mut builder = Response::builder().status(status);
    for h in FORWARD_OUT {
        if let Some(v) = resp.headers().get(*h)
            && let Ok(v) = HeaderValue::from_bytes(v.as_bytes())
        {
            builder = builder.header(*h, v);
        }
    }
    let stream = resp.bytes_stream();
    builder
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Bench protection through the router's calendar (SUPERVISOR.md 9, Q10): a run of the bench
/// queue is a one-off calendar entry that blocks on-demand switches (and shows on the
/// calendar) until the queue ends.
pub struct RouterBenchProtect {
    pub link: RouterLink,
}

fn bench_queue_user() -> ActingUser {
    ActingUser {
        user_id: 0,
        name: "bench-queue".into(),
        admin: true,
    }
}

fn router_error(st: u16, v: &Value) -> String {
    format!(
        "router {st}: {}",
        v.get("error")
            .and_then(Value::as_str)
            .unwrap_or("no detail")
    )
}

#[async_trait::async_trait]
impl crate::bench::queue::BenchProtect for RouterBenchProtect {
    async fn begin(&self, run: &str, seconds: i64) -> Result<(String, i64), String> {
        let body = json!({"run": run, "seconds": seconds});
        match self
            .link
            .call("POST", "calendar/bench", &bench_queue_user(), &body)
            .await?
        {
            // The event key in the reply is dropped: the queue's runs use no reserved card.
            (200 | 201, v) => Ok((
                v["id"].as_str().unwrap_or_default().to_string(),
                chrono::Utc::now().timestamp() + seconds + 1800,
            )),
            (st, v) => Err(router_error(st, &v)),
        }
    }

    async fn extend(&self, id: &str, seconds: i64) -> Result<i64, String> {
        let body = json!({"seconds": seconds});
        let path = format!("calendar/bench/{id}/extend");
        match self
            .link
            .call("POST", &path, &bench_queue_user(), &body)
            .await?
        {
            (200, v) => Ok(v["until"].as_i64().unwrap_or_default()),
            (st, v) => Err(router_error(st, &v)),
        }
    }

    async fn end(&self, id: &str) -> Result<(), String> {
        let path = format!("calendar/bench/{id}/end");
        match self
            .link
            .call("POST", &path, &bench_queue_user(), &Value::Null)
            .await?
        {
            (200 | 404, _) => Ok(()),
            (st, v) => Err(router_error(st, &v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_names_are_checked() {
        assert!(RouterLink::new("http://router:8010/", "short").is_err());
        let l = RouterLink::new("http://router:8010/", &"t".repeat(40)).unwrap();
        assert_eq!(l.url, "http://router:8010");
        assert!(RouterLink::new("router:8010", &"t".repeat(40)).is_err());
        assert_eq!(check_key_name(" Jarvis ").unwrap(), "jarvis");
        assert!(check_key_name("video pipeline").is_err());
        assert!(check_key_name("").is_err());
        assert_eq!(from_hex(&"ab".repeat(32)).unwrap().len(), 32);
        assert!(from_hex("zz").is_none());
    }
}
