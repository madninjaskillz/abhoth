//! Browser sessions for the router's own admin pages (D151): an admin signs in with an admin key
//! once and gets a short-lived, HttpOnly, SameSite=Strict cookie; state-changing calls also need
//! the session's CSRF token in a header. Sessions live in memory (a restart signs everyone out).
//!
//! Sign-in attempts are rate limited per client address and in total, and the key a session was
//! made from is re-checked on use (a revoked key ends its sessions).

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;

use super::keys::{Clock, system_clock};
use super::{Identity, Principal};

/// The session cookie (no `Path`: the browser scopes it to the folder the sign-in was posted
/// from, `/admin` or a reverse proxy's prefix).
pub const COOKIE: &str = "abhoth_router_session";
/// The header state-changing calls carry the session's CSRF token in.
pub const CSRF_HEADER: &str = "x-router-csrf";
/// A session ends this long after sign-in, whatever happens.
pub const MAX_AGE: Duration = Duration::from_secs(8 * 3600);
/// ...or after this long unused.
pub const IDLE: Duration = Duration::from_secs(2 * 3600);
/// Failed sign-ins allowed per address in [`FAIL_WINDOW`] (then 429 until the window passes).
pub const FAILS_PER_IP: usize = 10;
/// Failed sign-ins allowed from everyone together in [`FAIL_WINDOW`].
pub const FAILS_TOTAL: usize = 100;
pub const FAIL_WINDOW: Duration = Duration::from_secs(15 * 60);
/// At most this many sessions (the oldest go first).
const MAX_SESSIONS: usize = 1000;

/// Where a session's key is checked again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A router-local admin key (`router.db`), by digest.
    Local(String),
    /// A key the web server vouches for (introspection, cached), by digest.
    Remote(String),
    /// Sign-in with Abhoth (D154): a web UI admin, by user id.
    Sso(i64),
}

#[derive(Debug, Clone)]
struct Session {
    identity: Identity,
    source: KeySource,
    csrf: String,
    created_ms: u64,
    used_ms: u64,
}

/// A session as the page sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub principal: Principal,
    pub source: KeySource,
    pub csrf: String,
}

pub struct Sessions {
    clock: Clock,
    map: Mutex<HashMap<String, Session>>,
    fails: Mutex<HashMap<IpAddr, VecDeque<u64>>>,
    fails_total: Mutex<VecDeque<u64>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new(system_clock())
    }
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random bytes");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Sessions {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            map: Mutex::new(HashMap::new()),
            fails: Mutex::new(HashMap::new()),
            fails_total: Mutex::new(VecDeque::new()),
        }
    }

    /// May `ip` try to sign in now? (Over either limit: no, and nothing is checked.)
    pub fn may_try(&self, ip: Option<IpAddr>) -> bool {
        let now = (self.clock)();
        let w = FAIL_WINDOW.as_millis() as u64;
        let mut total = self.fails_total.lock().unwrap();
        while total.front().is_some_and(|t| now.saturating_sub(*t) >= w) {
            total.pop_front();
        }
        if total.len() >= FAILS_TOTAL {
            return false;
        }
        let Some(ip) = ip else { return true };
        let mut m = self.fails.lock().unwrap();
        let q = m.entry(ip).or_default();
        while q.front().is_some_and(|t| now.saturating_sub(*t) >= w) {
            q.pop_front();
        }
        q.len() < FAILS_PER_IP
    }

    /// Count a failed sign-in.
    pub fn failed(&self, ip: Option<IpAddr>) {
        let now = (self.clock)();
        self.fails_total.lock().unwrap().push_back(now);
        if let Some(ip) = ip {
            let mut m = self.fails.lock().unwrap();
            if m.len() > 4096 {
                let w = FAIL_WINDOW.as_millis() as u64;
                m.retain(|_, q| q.back().is_some_and(|t| now.saturating_sub(*t) < w));
            }
            m.entry(ip).or_default().push_back(now);
        }
    }

    /// A new session for `identity`; returns (session id, CSRF token).
    pub fn create(&self, identity: Identity, source: KeySource) -> (String, String) {
        let now = (self.clock)();
        let id = random_token();
        let csrf = random_token();
        let mut m = self.map.lock().unwrap();
        self.expire(&mut m, now);
        if m.len() >= MAX_SESSIONS
            && let Some(oldest) = m
                .iter()
                .min_by_key(|(_, s)| s.created_ms)
                .map(|(k, _)| k.clone())
        {
            m.remove(&oldest);
        }
        m.insert(
            id.clone(),
            Session {
                identity,
                source,
                csrf: csrf.clone(),
                created_ms: now,
                used_ms: now,
            },
        );
        (id, csrf)
    }

    fn expire(&self, m: &mut HashMap<String, Session>, now: u64) {
        let (max, idle) = (MAX_AGE.as_millis() as u64, IDLE.as_millis() as u64);
        m.retain(|_, s| {
            now.saturating_sub(s.created_ms) < max && now.saturating_sub(s.used_ms) < idle
        });
    }

    /// The session `id`, if it is still good (marks it used).
    pub fn get(&self, id: &str) -> Option<SessionInfo> {
        let now = (self.clock)();
        let mut m = self.map.lock().unwrap();
        self.expire(&mut m, now);
        let s = m.get_mut(id)?;
        s.used_ms = now;
        Some(SessionInfo {
            principal: Principal::Key(s.identity.clone()),
            source: s.source.clone(),
            csrf: s.csrf.clone(),
        })
    }

    pub fn remove(&self, id: &str) {
        self.map.lock().unwrap().remove(id);
    }

    /// End every session made from a key with this digest (it was revoked).
    pub fn remove_key(&self, digest: &str) {
        self.map.lock().unwrap().retain(|_, s| match &s.source {
            KeySource::Local(d) | KeySource::Remote(d) => d != digest,
            KeySource::Sso(_) => true,
        });
    }

    /// End the sessions of local key `id`.
    pub fn remove_local_id(&self, id: i64) {
        self.map
            .lock()
            .unwrap()
            .retain(|_, s| !(matches!(s.source, KeySource::Local(_)) && s.identity.key_id == id));
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The session cookie's value in a request.
pub fn cookie(headers: &HeaderMap) -> Option<&str> {
    named_cookie(headers, COOKIE)
}

/// A cookie's value in a request.
pub fn named_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for v in headers.get_all("cookie") {
        let Ok(s) = v.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=')
                && k.trim() == name
                && !v.trim().is_empty()
            {
                return Some(v.trim());
            }
        }
    }
    None
}

/// Was the request made over HTTPS (directly, or through a proxy that says so)?
pub fn https(headers: &HeaderMap) -> bool {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
    };
    h("x-forwarded-proto").is_some_and(|p| p.split(',').next().map(str::trim) == Some("https"))
        || h("forwarded").is_some_and(|f| f.contains("proto=https"))
}

/// `Set-Cookie` for a new session (`Secure` when the page came over HTTPS).
pub fn set_cookie(id: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Max-Age={}{}",
        MAX_AGE.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` that ends the session in the browser.
pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clocked() -> (Sessions, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        (
            Sessions::new(Arc::new(move || tc.load(Ordering::SeqCst))),
            t,
        )
    }

    fn admin() -> Identity {
        super::super::local_keys::identity(1, "ops")
    }

    #[test]
    fn sessions_end_after_eight_hours_or_two_idle() {
        let (s, t) = clocked();
        let (id, csrf) = s.create(admin(), KeySource::Local("d".into()));
        assert_ne!(id, csrf);
        assert_eq!(id.len(), 64);
        for h in 1..8 {
            t.store(h * 3_600_000 - 1000, Ordering::SeqCst);
            assert!(s.get(&id).is_some(), "used every hour: alive at {h} h");
        }
        t.store(8 * 3_600_000, Ordering::SeqCst);
        assert!(s.get(&id).is_none(), "8 h at most");
        let (id2, _) = s.create(admin(), KeySource::Local("d".into()));
        t.store(8 * 3_600_000 + 2 * 3_600_000, Ordering::SeqCst);
        assert!(s.get(&id2).is_none(), "2 h idle");
    }

    #[test]
    fn sign_in_failures_are_limited_per_address_and_in_total() {
        let (s, t) = clocked();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..FAILS_PER_IP {
            assert!(s.may_try(Some(a)));
            s.failed(Some(a));
        }
        assert!(!s.may_try(Some(a)));
        assert!(s.may_try(Some(b)), "another address still may");
        t.store(FAIL_WINDOW.as_millis() as u64, Ordering::SeqCst);
        assert!(s.may_try(Some(a)), "the window passed");
        for i in 0..FAILS_TOTAL {
            s.failed(Some(IpAddr::from([
                10,
                1,
                (i / 200) as u8,
                (i % 200) as u8,
            ])));
        }
        assert!(!s.may_try(Some(b)), "too many failures from everyone");
    }

    #[test]
    fn revoking_a_key_ends_its_sessions() {
        let (s, _) = clocked();
        let (a, _) = s.create(admin(), KeySource::Local("d1".into()));
        let (b, _) = s.create(admin(), KeySource::Remote("d2".into()));
        s.remove_key("d1");
        assert!(s.get(&a).is_none() && s.get(&b).is_some());
        s.remove_local_id(1);
        assert!(
            s.get(&b).is_some(),
            "a remote key's session is not a local id's"
        );
    }

    #[test]
    fn cookies_parse_and_are_strict() {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            format!("other=1; {COOKIE}=abc ; x=y").parse().unwrap(),
        );
        assert_eq!(cookie(&h), Some("abc"));
        let c = set_cookie("abc", true);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.ends_with("Secure"));
        assert!(!c.contains("Path="), "scoped to the sign-in folder");
        assert!(!set_cookie("abc", false).contains("Secure"));
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(https(&h));
    }
}
