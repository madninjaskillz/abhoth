//! Key requests (D149, `docs/design/ROUTER-MODE.md` 3.1): a client without a key asks the router
//! for one, and an admin approves it in the web UI.
//!
//! - `POST /v1/abhoth/key-requests` `{client}`: unauthenticated (a client with no key is the
//!   point), rate-limited per IP. Passed on to the web server over the link
//!   (`POST /api/internal/key-requests` with the link token and the TCP peer's IP), which answers
//!   `{id, code, poll, expiresIn, interval}`. Nothing usable exists until an admin approves.
//! - `POST /v1/abhoth/key-requests/poll` `{id, poll}`: where it is (`pending`, `approved` with
//!   the key once, `denied`, `expired`), also rate-limited per IP.
//!
//! Both exist only with the control plane on and the accounts link set (the identification
//! document says so: `auth.keyRequests`). Neither touches a card, a lease or a layout; no body
//! (client name, poll secret, key) is ever logged.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::Response;
use orca_core::router_link::{
    KEY_REQUESTS_INTERNAL_PATH, KEY_REQUESTS_POLL_INTERNAL_PATH, KeyRequestCreate,
};
use orca_core::router_mode::{KeyRequestPoll, KeyRequestStart};
use serde_json::Value;

use crate::access::keys::{Clock, system_clock};
use crate::proxy::{error, json_response};
use crate::state::AppState;

/// Passes requests on to the web server. `Ok((status, body))` is its answer.
#[async_trait::async_trait]
pub trait KeyRequestLink: Send + Sync {
    async fn create(&self, ask: &KeyRequestCreate) -> Result<(u16, Value), String>;
    async fn poll(&self, ask: &KeyRequestPoll) -> Result<(u16, Value), String>;
}

/// The real one: the web server at `access.accounts_url`, with the link token.
pub struct HttpKeyRequestLink {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HttpKeyRequestLink {
    pub fn new(base_url: &str, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("http client"),
        }
    }

    async fn post<T: serde::Serialize + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<(u16, Value), String> {
        let r = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header("x-abhoth-no-switch", "1")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "accounts server: timeout".to_string()
                } else {
                    "accounts server unreachable".to_string()
                }
            })?;
        let st = r.status().as_u16();
        Ok((st, r.json().await.unwrap_or(Value::Null)))
    }
}

#[async_trait::async_trait]
impl KeyRequestLink for HttpKeyRequestLink {
    async fn create(&self, ask: &KeyRequestCreate) -> Result<(u16, Value), String> {
        self.post(KEY_REQUESTS_INTERNAL_PATH, ask).await
    }
    async fn poll(&self, ask: &KeyRequestPoll) -> Result<(u16, Value), String> {
        self.post(KEY_REQUESTS_POLL_INTERNAL_PATH, ask).await
    }
}

/// New requests one IP may make per [`CREATE_WINDOW`].
pub const CREATE_LIMIT: usize = 5;
pub const CREATE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Polls one IP may make per [`POLL_WINDOW`] (the app polls every 3 s).
pub const POLL_LIMIT: usize = 60;
pub const POLL_WINDOW: Duration = Duration::from_secs(60);

/// A sliding-window count per IP.
pub struct Limiter {
    clock: Clock,
    hits: Mutex<HashMap<(bool, IpAddr), VecDeque<u64>>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(system_clock())
    }
}

impl Limiter {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Count one hit for `ip` (`create`: a new request, else a poll); false when over the limit
    /// (the hit is then not counted).
    pub fn allow(&self, create: bool, ip: IpAddr) -> bool {
        let (limit, window) = if create {
            (CREATE_LIMIT, CREATE_WINDOW)
        } else {
            (POLL_LIMIT, POLL_WINDOW)
        };
        let now = (self.clock)();
        let w = window.as_millis() as u64;
        let mut m = self.hits.lock().unwrap();
        // Keep the map small: forget IPs whose window has passed.
        if m.len() > 4096 {
            m.retain(|_, q| q.back().is_some_and(|t| now.saturating_sub(*t) < w));
        }
        let q = m.entry((create, ip)).or_default();
        while q.front().is_some_and(|t| now.saturating_sub(*t) >= w) {
            q.pop_front();
        }
        if q.len() >= limit {
            return false;
        }
        q.push_back(now);
        true
    }
}

fn link(st: &AppState) -> Option<Arc<dyn KeyRequestLink>> {
    st.control
        .get()
        .and_then(|cp| cp.access.key_requests.clone())
}

/// Whether this router takes key requests (the identification document's `auth.keyRequests`).
pub fn available(st: &AppState) -> bool {
    link(st).is_some()
}

fn not_here() -> Response {
    error(
        404,
        "this router does not take key requests (ask its admin for a key)",
        "not_found",
        None,
    )
}

fn too_many() -> Response {
    error(
        429,
        "too many key requests from this address; wait a few minutes",
        "rate_limit_error",
        None,
    )
}

/// The web server's answer, as the client's (its error text, or the body as is).
fn relay(res: Result<(u16, Value), String>) -> Response {
    match res {
        Ok((200, v)) => {
            let mut r = json_response(200, &v);
            r.headers_mut().insert(
                "cache-control",
                axum::http::HeaderValue::from_static("no-store"),
            );
            r
        }
        Ok((st, v)) => {
            let msg = v["error"].as_str().unwrap_or("the accounts server refused");
            let st = if st == 429 {
                429
            } else if st >= 500 {
                502
            } else {
                st
            };
            error(st, msg, "key_request_error", None)
        }
        Err(e) => error(503, &e, "unavailable", None),
    }
}

/// `POST /v1/abhoth/key-requests`.
pub async fn start(st: &AppState, peer: Option<IpAddr>, raw: &[u8]) -> Response {
    let Some(link) = link(st) else {
        return not_here();
    };
    let cp = st.control.get().expect("link implies the control plane");
    let ip = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !cp.access.key_request_limits.allow(true, ip) {
        tracing::info!(ip = %ip, "key request refused: rate limit");
        return too_many();
    }
    let Ok(ask) = serde_json::from_slice::<KeyRequestStart>(raw) else {
        return error(
            400,
            "expected {\"client\": \"a name\"}",
            "invalid_request_error",
            None,
        );
    };
    tracing::info!(ip = %ip, "key request passed to the accounts server");
    relay(
        link.create(&KeyRequestCreate {
            client: ask.client.chars().take(200).collect(),
            ip: ip.to_string(),
        })
        .await,
    )
}

/// `POST /v1/abhoth/key-requests/poll`.
pub async fn poll(st: &AppState, peer: Option<IpAddr>, raw: &[u8]) -> Response {
    let Some(link) = link(st) else {
        return not_here();
    };
    let cp = st.control.get().expect("link implies the control plane");
    let ip = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !cp.access.key_request_limits.allow(false, ip) {
        return too_many();
    }
    let Ok(ask) = serde_json::from_slice::<KeyRequestPoll>(raw) else {
        return error(
            400,
            "expected {\"id\", \"poll\"}",
            "invalid_request_error",
            None,
        );
    };
    if ask.id.len() > 64 || ask.poll.len() > 128 {
        return error(400, "not a key request", "invalid_request_error", None);
    }
    relay(link.poll(&ask).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn the_limiter_counts_per_ip_in_a_sliding_window() {
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        let l = Limiter::new(Arc::new(move || tc.load(Ordering::SeqCst)));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..CREATE_LIMIT {
            assert!(l.allow(true, a));
        }
        assert!(!l.allow(true, a));
        assert!(l.allow(true, b), "another IP");
        assert!(l.allow(false, a), "polls are counted apart");
        t.store(CREATE_WINDOW.as_millis() as u64, Ordering::SeqCst);
        assert!(l.allow(true, a), "the window passed");
    }
}
