//! Key requests (D149, `docs/design/ROUTER-MODE.md` 3.1): how someone without an Abhoth key gets
//! one, device-flow style.
//!
//! 1. The app asks the router (`POST /v1/abhoth/key-requests`, unauthenticated, rate-limited per
//!    IP); the router passes it on here over the link (`POST /api/internal/key-requests`, link
//!    token). The request gets a random id, a short code ("ABCD-2345") the user reads to an admin,
//!    and a poll secret only that client has. Nothing usable exists yet.
//! 2. An admin sees the pending requests in Settings > Service keys
//!    (`GET /api/admin/key-requests`) and approves one (`POST …/{id}/approve {name, claude}`:
//!    a service key with the scopes of a user key, `chat`, `models` and `leases`, plus the
//!    `group:claude` scope when the admin lets it use Claude) or denies it (`POST …/{id}/deny`).
//! 3. The app's poll (`POST /v1/abhoth/key-requests/poll` → `…/internal/key-requests/poll`)
//!    gets the new key once; the request is then gone. A key approved but never collected is
//!    revoked when the request expires.
//!
//! Requests live in memory only (15 minutes; a restart forgets them, the app asks again). The
//! database stores the key's hash, as for every key. Codes and poll secrets are random,
//! single-use and short-lived; at most 50 requests wait at once, 3 per IP. Every step is in the
//! audit log; no key, poll secret or link token ever is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use orca_core::router_link::{
    CLAUDE_KEY_GROUP, KEY_REQUESTS_INTERNAL_PATH, KEY_REQUESTS_POLL_INTERNAL_PATH, KeyRequestCreate,
};
use orca_core::router_mode::{
    KEY_REQUEST_TTL_SECS, KeyRequestPoll, KeyRequestStatus, KeyRequestTicket,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::login::endpoints::{body_json, no_store};
use crate::login::{API_KEY_PREFIX, Current, error, principal_of};
use crate::router_admin::{check_key_name, link_check};
use crate::server::AppState;

/// How long a request waits (and an approved key waits to be collected).
pub const TTL: i64 = KEY_REQUEST_TTL_SECS as i64;
/// How often the client should poll.
pub const POLL_INTERVAL: u64 = 3;
/// Requests waiting at once, in all.
pub const MAX_PENDING: usize = 50;
/// Requests waiting at once from one IP.
pub const MAX_PER_IP: usize = 3;
/// The scopes an approved request's key gets: what a user key may do at the router.
pub const USER_SCOPES: &[&str] = &["chat", "leases", "models"];

/// Letters and digits that read aloud without confusion (no 0/O, 1/I/L).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone)]
enum Stage {
    Pending,
    Approved {
        key: String,
        key_id: i64,
        name: String,
        claude: bool,
    },
    Denied,
}

#[derive(Debug, Clone)]
struct Req {
    id: String,
    code: String,
    poll_hash: Vec<u8>,
    client: String,
    ip: String,
    created: i64,
    expires: i64,
    stage: Stage,
}

/// A waiting request, as admins see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    pub id: String,
    pub code: String,
    pub client: String,
    pub ip: String,
    pub created: i64,
    pub expires_in: i64,
    /// A key name made from the client's ("desktop-laptop").
    pub suggested_name: String,
}

/// What happened to a request that left the store (for the audit, and to revoke an approved key
/// nobody collected).
#[derive(Debug, Clone, PartialEq)]
pub struct Gone {
    pub id: String,
    pub client: String,
    pub ip: String,
    /// Approved but never collected: this key is to be revoked.
    pub uncollected_key: Option<i64>,
}

/// The waiting requests (in memory).
#[derive(Default)]
pub struct KeyRequests {
    inner: Mutex<HashMap<String, Req>>,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("random bytes");
    b
}

fn new_code() -> String {
    // 31 symbols; rejection sampling keeps every symbol equally likely.
    let mut out = String::new();
    while out.len() < 9 {
        for b in random_bytes::<16>() {
            if out.len() >= 9 {
                break;
            }
            if (b as usize) < CODE_ALPHABET.len() * 8 {
                if out.len() == 4 {
                    out.push('-');
                }
                out.push(CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char);
            }
        }
    }
    out
}

fn hash(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

/// The client's name as given: trimmed, control characters out, at most 80 characters.
pub fn clean_client(s: &str) -> String {
    let t: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect::<String>()
        .trim()
        .to_string();
    if t.is_empty() {
        "an unnamed client".into()
    } else {
        t
    }
}

/// A key name from a client's ("Abhoth desktop on Laptop" -> "desktop-laptop").
pub fn suggest_name(client: &str) -> String {
    let lower = client.to_lowercase();
    let lower = lower.strip_prefix("abhoth ").unwrap_or(&lower);
    let words: Vec<String> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty() && *w != "on")
        .map(str::to_string)
        .collect();
    let mut n = words.join("-");
    n.truncate(36);
    let n = n.trim_matches('-').to_string();
    if n.is_empty() { "app".into() } else { n }
}

impl KeyRequests {
    /// A new request from `client` at `ip`.
    pub fn create(
        &self,
        client: &str,
        ip: &str,
        now: i64,
    ) -> Result<KeyRequestTicket, &'static str> {
        let mut m = self.inner.lock().unwrap();
        let waiting = |r: &&Req| matches!(r.stage, Stage::Pending) && r.expires > now;
        if m.values().filter(waiting).count() >= MAX_PENDING {
            return Err("Too many key requests are waiting; try again later.");
        }
        if m.values().filter(waiting).filter(|r| r.ip == ip).count() >= MAX_PER_IP {
            return Err("This address already has key requests waiting for an admin.");
        }
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let id = b64.encode(random_bytes::<12>());
        let poll = b64.encode(random_bytes::<32>());
        let code = loop {
            let c = new_code();
            if !m.values().any(|r| r.code == c) {
                break c;
            }
        };
        m.insert(
            id.clone(),
            Req {
                id: id.clone(),
                code: code.clone(),
                poll_hash: hash(&poll),
                client: clean_client(client),
                ip: ip.to_string(),
                created: now,
                expires: now + TTL,
                stage: Stage::Pending,
            },
        );
        Ok(KeyRequestTicket {
            id,
            code,
            poll,
            expires_in: TTL as u64,
            interval: POLL_INTERVAL,
        })
    }

    /// The client's poll. An approved key is in the answer once (the request is then gone).
    /// A wrong poll secret reads like an unknown request.
    pub fn poll(&self, id: &str, poll: &str, now: i64) -> (KeyRequestStatus, Option<Req2>) {
        let mut m = self.inner.lock().unwrap();
        let unknown = KeyRequestStatus {
            status: "expired".into(),
            ..Default::default()
        };
        let Some(r) = m.get(id) else {
            return (unknown, None);
        };
        if !orca_core::router_link::fixed_time_eq(&r.poll_hash, &hash(poll)) {
            return (unknown, None);
        }
        if r.expires <= now {
            return (unknown, None);
        }
        match r.stage.clone() {
            Stage::Pending => (
                KeyRequestStatus {
                    status: "pending".into(),
                    expires_in: Some((r.expires - now) as u64),
                    ..Default::default()
                },
                None,
            ),
            Stage::Denied => {
                let r = m.remove(id).expect("there");
                (
                    KeyRequestStatus {
                        status: "denied".into(),
                        ..Default::default()
                    },
                    Some(Req2::Denied(r.client)),
                )
            }
            Stage::Approved {
                key,
                key_id,
                name,
                claude,
            } => {
                let r = m.remove(id).expect("there");
                (
                    KeyRequestStatus {
                        status: "approved".into(),
                        key: Some(key),
                        name: Some(name.clone()),
                        claude,
                        expires_in: None,
                    },
                    Some(Req2::Collected {
                        key_id,
                        name,
                        client: r.client,
                        ip: r.ip,
                    }),
                )
            }
        }
    }

    /// The requests waiting for an admin, oldest first.
    pub fn pending(&self, now: i64) -> Vec<PendingView> {
        let m = self.inner.lock().unwrap();
        let mut v: Vec<PendingView> = m
            .values()
            .filter(|r| matches!(r.stage, Stage::Pending) && r.expires > now)
            .map(|r| PendingView {
                id: r.id.clone(),
                code: r.code.clone(),
                client: r.client.clone(),
                ip: r.ip.clone(),
                created: r.created,
                expires_in: r.expires - now,
                suggested_name: suggest_name(&r.client),
            })
            .collect();
        v.sort_by(|a, b| a.created.cmp(&b.created).then(a.id.cmp(&b.id)));
        v
    }

    /// A waiting request's client and IP.
    pub fn waiting(&self, id: &str, now: i64) -> Option<(String, String)> {
        let m = self.inner.lock().unwrap();
        m.get(id)
            .filter(|r| matches!(r.stage, Stage::Pending) && r.expires > now)
            .map(|r| (r.client.clone(), r.ip.clone()))
    }

    /// Hand `key` to the waiting request `id` (its client collects it). False when it is no
    /// longer waiting (the caller revokes the key).
    pub fn approve(
        &self,
        id: &str,
        key: String,
        key_id: i64,
        name: &str,
        claude: bool,
        now: i64,
    ) -> bool {
        let mut m = self.inner.lock().unwrap();
        let Some(r) = m
            .get_mut(id)
            .filter(|r| matches!(r.stage, Stage::Pending) && r.expires > now)
        else {
            return false;
        };
        r.stage = Stage::Approved {
            key,
            key_id,
            name: name.to_string(),
            claude,
        };
        // Time to collect it, whenever the admin got to it.
        r.expires = now + TTL;
        true
    }

    /// Deny a waiting request: its client's next poll says so. Its client, or None.
    pub fn deny(&self, id: &str, now: i64) -> Option<(String, String)> {
        let mut m = self.inner.lock().unwrap();
        let r = m
            .get_mut(id)
            .filter(|r| matches!(r.stage, Stage::Pending) && r.expires > now)?;
        r.stage = Stage::Denied;
        Some((r.client.clone(), r.ip.clone()))
    }

    /// Drop the expired requests; those approved and never collected name their key.
    pub fn sweep(&self, now: i64) -> Vec<Gone> {
        let mut m = self.inner.lock().unwrap();
        let ids: Vec<String> = m
            .values()
            .filter(|r| r.expires <= now)
            .map(|r| r.id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| m.remove(id))
            .map(|r| Gone {
                uncollected_key: match r.stage {
                    Stage::Approved { key_id, .. } => Some(key_id),
                    _ => None,
                },
                id: r.id,
                client: r.client,
                ip: r.ip,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a poll did, for the audit.
#[derive(Debug, Clone, PartialEq)]
pub enum Req2 {
    Collected {
        key_id: i64,
        name: String,
        client: String,
        ip: String,
    },
    Denied(String),
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(KEY_REQUESTS_INTERNAL_PATH, post(internal_create))
        .route(KEY_REQUESTS_POLL_INTERNAL_PATH, post(internal_poll))
        .route("/api/admin/key-requests", get(list))
        .route("/api/admin/key-requests/{id}/approve", post(approve))
        .route("/api/admin/key-requests/{id}/deny", post(deny))
}

/// Expired requests out; an approved key nobody collected is revoked.
fn sweep(state: &AppState) {
    for g in state.key_requests.sweep(state.login.now()) {
        if let Some(key_id) = g.uncollected_key {
            state.login.db.delete_service_key(key_id);
        }
        state.login.record(
            "keyrequest.expired",
            None,
            Some(&g.client),
            Some(&g.ip),
            json!({"request": g.id, "revokedKey": g.uncollected_key}),
        );
    }
}

/// `POST /api/internal/key-requests` `{client, ip}` (the router, link token).
async fn internal_create(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = link_check(&state, &req, "router.keyrequest") {
        return r;
    }
    sweep(&state);
    let body = axum::body::to_bytes(req.into_body(), 4096)
        .await
        .unwrap_or_default();
    let Ok(ask) = serde_json::from_slice::<KeyRequestCreate>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Expected {\"client\", \"ip\"}.");
    };
    let ip: String = ask.ip.chars().take(64).collect();
    match state
        .key_requests
        .create(&ask.client, &ip, state.login.now())
    {
        Ok(t) => {
            state.login.record(
                "keyrequest.created",
                None,
                Some(&clean_client(&ask.client)),
                Some(&ip),
                json!({"request": t.id, "code": t.code}),
            );
            no_store(serde_json::to_value(t).unwrap_or(Value::Null))
        }
        Err(e) => error(StatusCode::TOO_MANY_REQUESTS, e),
    }
}

/// `POST /api/internal/key-requests/poll` `{id, poll}` (the router, link token).
async fn internal_poll(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = link_check(&state, &req, "router.keyrequest") {
        return r;
    }
    sweep(&state);
    let body = axum::body::to_bytes(req.into_body(), 4096)
        .await
        .unwrap_or_default();
    let Ok(ask) = serde_json::from_slice::<KeyRequestPoll>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Expected {\"id\", \"poll\"}.");
    };
    let (status, done) = state
        .key_requests
        .poll(&ask.id, &ask.poll, state.login.now());
    match done {
        Some(Req2::Collected {
            key_id,
            name,
            client,
            ip,
        }) => state.login.record(
            "keyrequest.collected",
            None,
            Some(&client),
            Some(&ip),
            json!({"request": ask.id, "key": key_id, "name": name}),
        ),
        Some(Req2::Denied(client)) => state.login.record(
            "keyrequest.denial-seen",
            None,
            Some(&client),
            None,
            json!({"request": ask.id}),
        ),
        None => {}
    }
    no_store(serde_json::to_value(status).unwrap_or(Value::Null))
}

#[allow(clippy::result_large_err)]
fn admin_of(req: &Request<Body>) -> Result<(crate::login::Principal, Option<String>), Response> {
    let Some(p) = principal_of(req).filter(|p| p.admin) else {
        return Err(error(StatusCode::FORBIDDEN, "Only an admin can do that."));
    };
    let ip = req
        .extensions()
        .get::<Current>()
        .and_then(|c| c.seen.ip_text());
    Ok((p, ip))
}

/// `GET /api/admin/key-requests`: the waiting requests.
async fn list(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Err(r) = admin_of(&req) {
        return r;
    }
    sweep(&state);
    no_store(json!({
        "requests": state.key_requests.pending(state.login.now()),
        "router": state.options.router.is_some(),
        "claudeGroup": CLAUDE_KEY_GROUP,
        "scopes": USER_SCOPES,
    }))
}

/// A free service-key name from `want` ("desktop", "desktop-2", …).
fn free_name(state: &AppState, want: &str) -> String {
    let taken: Vec<String> = state
        .login
        .db
        .service_keys()
        .into_iter()
        .map(|k| k.name)
        .collect();
    if !taken.contains(&want.to_string()) {
        return want.to_string();
    }
    (2..)
        .map(|i| format!("{want}-{i}"))
        .find(|n| !taken.contains(n))
        .expect("some name is free")
}

/// `POST /api/admin/key-requests/{id}/approve` `{name?, claude?}`.
async fn approve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = match admin_of(&req) {
        Ok(x) => x,
        Err(r) => return r,
    };
    sweep(&state);
    let body = body_json(req).await;
    let now = state.login.now();
    let Some((client, from)) = state.key_requests.waiting(&id, now) else {
        return error(
            StatusCode::NOT_FOUND,
            "That request is no longer waiting (expired, approved or denied).",
        );
    };
    let want = body["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| suggest_name(&client));
    let name = match check_key_name(&want) {
        Ok(n) => free_name(&state, &n),
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let claude = body["claude"].as_bool().unwrap_or(false);
    let mut scopes: Vec<String> = USER_SCOPES.iter().map(|s| s.to_string()).collect();
    if claude {
        scopes.push(format!("group:{CLAUDE_KEY_GROUP}"));
    }
    let db = &state.login.db;
    if db.service_keys().len() >= 200 {
        return error(StatusCode::BAD_REQUEST, "200 service keys at most.");
    }
    let key = format!("{API_KEY_PREFIX}{}", crate::login::store_token());
    let prefix: String = key.chars().take(API_KEY_PREFIX.len() + 6).collect();
    let note: String = format!("Requested by {client} from {from}")
        .chars()
        .take(200)
        .collect();
    let Some(k) = db.create_service_key(p.user_id, &name, &key, &prefix, now, &scopes, Some(&note))
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save the key.");
    };
    if !state
        .key_requests
        .approve(&id, key, k.id, &name, claude, now)
    {
        db.delete_service_key(k.id);
        return error(
            StatusCode::NOT_FOUND,
            "That request stopped waiting just now.",
        );
    }
    state.login.record(
        "keyrequest.approved",
        Some(&p),
        Some(&client),
        ip.as_deref(),
        json!({"request": id, "key": k.id, "name": name, "scopes": scopes, "from": from}),
    );
    no_store(json!({"ok": true, "name": name, "scopes": scopes, "id": k.id}))
}

/// `POST /api/admin/key-requests/{id}/deny`.
async fn deny(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = match admin_of(&req) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some((client, from)) = state.key_requests.deny(&id, state.login.now()) else {
        return error(StatusCode::NOT_FOUND, "That request is no longer waiting.");
    };
    state.login.record(
        "keyrequest.denied",
        Some(&p),
        Some(&client),
        ip.as_deref(),
        json!({"request": id, "from": from}),
    );
    no_store(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_short_readable_and_random() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let c = new_code();
            assert_eq!(c.len(), 9, "{c}");
            assert_eq!(&c[4..5], "-");
            assert!(
                c.chars()
                    .filter(|&x| x != '-')
                    .all(|x| CODE_ALPHABET.contains(&(x as u8))),
                "{c}"
            );
            seen.insert(c);
        }
        assert!(seen.len() > 495, "codes repeat too often");
    }

    #[test]
    fn a_request_waits_for_an_admin_and_its_key_is_collected_once() {
        let k = KeyRequests::default();
        let t = k
            .create("Abhoth desktop on Laptop", "10.0.0.5", 1000)
            .unwrap();
        assert_eq!(t.expires_in, 900);
        let (s, _) = k.poll(&t.id, &t.poll, 1010);
        assert_eq!(s.status, "pending");
        assert!(s.key.is_none());
        // Someone with the id but not the poll secret learns nothing.
        let (s, _) = k.poll(&t.id, "guess", 1010);
        assert_eq!(s.status, "expired");
        let p = k.pending(1010);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].code, t.code);
        assert_eq!(p[0].suggested_name, "desktop-laptop");
        assert!(k.approve(&t.id, "abk_new".into(), 7, "desktop-laptop", true, 1500));
        assert!(k.pending(1500).is_empty(), "no longer waiting");
        let (s, done) = k.poll(&t.id, &t.poll, 1501);
        assert_eq!(
            (s.status.as_str(), s.key.as_deref(), s.claude),
            ("approved", Some("abk_new"), true)
        );
        assert!(matches!(done, Some(Req2::Collected { key_id: 7, .. })));
        let (s, _) = k.poll(&t.id, &t.poll, 1502);
        assert_eq!(s.status, "expired", "single retrieval");
        assert!(s.key.is_none());
        assert!(k.is_empty());
    }

    #[test]
    fn denied_and_expired_requests_say_so_and_uncollected_keys_are_named() {
        let k = KeyRequests::default();
        let a = k.create("a", "10.0.0.1", 0).unwrap();
        let b = k.create("b", "10.0.0.2", 0).unwrap();
        assert!(k.deny(&a.id, 10).is_some());
        assert!(k.deny(&a.id, 11).is_none(), "once");
        assert_eq!(k.poll(&a.id, &a.poll, 12).0.status, "denied");
        assert!(k.approve(&b.id, "abk_x".into(), 9, "b", false, 100));
        // Approval restarts the clock: still collectable after the original 15 minutes.
        assert!(k.sweep(TTL + 1).is_empty());
        let gone = k.sweep(100 + TTL);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].uncollected_key, Some(9));
        // A pending one just goes.
        let c = k.create("c", "10.0.0.3", 0).unwrap();
        assert_eq!(k.poll(&c.id, &c.poll, TTL).0.status, "expired");
        assert!(
            !k.approve(&c.id, "abk_y".into(), 3, "c", false, TTL),
            "too late"
        );
        assert_eq!(k.sweep(TTL)[0].uncollected_key, None);
    }

    #[test]
    fn waiting_requests_are_capped_per_ip_and_in_all() {
        let k = KeyRequests::default();
        for _ in 0..MAX_PER_IP {
            k.create("x", "10.0.0.9", 0).unwrap();
        }
        assert!(k.create("x", "10.0.0.9", 0).is_err());
        assert!(k.create("x", "10.0.0.8", 0).is_ok());
        // Expired ones don't count.
        assert!(k.create("x", "10.0.0.9", TTL).is_ok());
        let k = KeyRequests::default();
        for i in 0..MAX_PENDING {
            k.create("x", &format!("10.1.{}.{}", i / 3, i % 3), 0)
                .unwrap();
        }
        assert!(k.create("x", "10.9.9.9", 0).is_err());
    }

    #[test]
    fn client_names_are_cleaned_and_names_suggested() {
        assert_eq!(
            clean_client("  \u{7}Abhoth desktop\n on X "),
            "Abhoth desktop on X"
        );
        assert_eq!(clean_client(""), "an unnamed client");
        assert_eq!(clean_client(&"y".repeat(200)).len(), 80);
        assert_eq!(
            suggest_name("Abhoth desktop on DESKTOP-ABC12"),
            "desktop-desktop-abc12"
        );
        assert_eq!(suggest_name("Abhoth on gpu.lan"), "gpu-lan");
        assert_eq!(suggest_name("!!!"), "app");
        assert!(check_key_name(&suggest_name(&"long words ".repeat(20))).is_ok());
    }
}
