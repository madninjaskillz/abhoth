//! `SessionStore` (in-memory browser sessions) and `LoginThrottle` (brute-force brakes).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Seconds since the Unix epoch; tests move it.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default()
    })
}

/// A browser session: after the password (stage "totp"/"enroll") or signed in ("ok").
#[derive(Debug, Clone)]
pub struct WebSession {
    pub id: String,
    pub user: String,
    pub stage: String,
    /// Sent back on every state-changing request (header X-OpenOrca-CSRF).
    pub csrf: String,
    pub created: f64,
    pub last_seen: f64,
    /// A TOTP secret shown for setup, not yet confirmed.
    pub pending_secret: Option<String>,
}

/// 256-bit random, URL-safe base64.
pub fn new_id() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random bytes");
    URL_SAFE_NO_PAD.encode(b)
}

/// In-memory sessions (a restart signs everyone out). A session ends after `idle` without
/// requests or `absolute` after sign-in, whichever is first; a half-signed-in one after
/// `pending`.
pub struct SessionStore {
    sessions: Mutex<HashMap<String, WebSession>>,
    clock: Clock,
    pub idle: Duration,
    pub absolute: Duration,
    pub pending: Duration,
}

impl SessionStore {
    pub fn new(clock: Clock, idle: Duration, absolute: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            clock,
            idle,
            absolute,
            pending: Duration::from_secs(5 * 60),
        }
    }

    fn expired(&self, s: &WebSession, now: f64) -> bool {
        let limit = if s.stage == "ok" {
            self.absolute
        } else {
            self.pending
        };
        now - s.created > limit.as_secs_f64() || now - s.last_seen > self.idle.as_secs_f64()
    }

    pub fn create(&self, user: &str, stage: &str) -> WebSession {
        let now = (self.clock)();
        let s = WebSession {
            id: new_id(),
            user: user.into(),
            stage: stage.into(),
            csrf: new_id(),
            created: now,
            last_seen: now,
            pending_secret: None,
        };
        let mut all = self.sessions.lock().unwrap();
        all.retain(|_, v| !self.expired(v, now));
        all.insert(s.id.clone(), s.clone());
        s
    }

    /// The live session for a cookie value (touching it), or `None`.
    pub fn get(&self, id: Option<&str>) -> Option<WebSession> {
        let id = id.filter(|i| !i.is_empty())?;
        let now = (self.clock)();
        let mut all = self.sessions.lock().unwrap();
        let s = all.get_mut(id)?;
        if self.expired(s, now) {
            all.remove(id);
            return None;
        }
        s.last_seen = now;
        Some(s.clone())
    }

    /// Change a live session in place (its pending secret).
    pub fn update(&self, id: &str, f: impl FnOnce(&mut WebSession)) -> Option<WebSession> {
        let mut all = self.sessions.lock().unwrap();
        let s = all.get_mut(id)?;
        f(s);
        Some(s.clone())
    }

    /// Signed in: a new id (no fixation), stage "ok", the absolute timeout restarted.
    pub fn promote(&self, pending: &WebSession) -> WebSession {
        self.remove(Some(&pending.id));
        self.create(&pending.user, "ok")
    }

    pub fn remove(&self, id: Option<&str>) {
        if let Some(id) = id {
            self.sessions.lock().unwrap().remove(id);
        }
    }

    pub fn clear(&self) {
        self.sessions.lock().unwrap().clear();
    }

    pub fn count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

#[derive(Default)]
struct Entry {
    failures: Vec<f64>,
    locked_until: f64,
    locks: u32,
}

/// Per client IP and per account: failures in a sliding window; past the free attempts the key
/// is locked for a backoff that doubles with every further failure (capped). A success clears it.
pub struct LoginThrottle {
    entries: Mutex<HashMap<String, Entry>>,
    clock: Clock,
    pub ip_free_failures: usize,
    pub account_free_failures: usize,
    pub window: Duration,
    pub first_lock: Duration,
    pub max_lock: Duration,
}

impl LoginThrottle {
    pub fn new(clock: Clock) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            clock,
            ip_free_failures: 10,
            account_free_failures: 5,
            window: Duration::from_secs(15 * 60),
            first_lock: Duration::from_secs(60),
            max_lock: Duration::from_secs(3600),
        }
    }

    pub fn ip_key(ip: Option<&str>) -> String {
        format!("ip:{}", ip.unwrap_or("?"))
    }

    pub fn account_key(user: &str) -> String {
        format!("user:{}", user.to_lowercase())
    }

    /// How long the key is still locked, or `None`.
    pub fn locked_for(&self, key: &str) -> Option<Duration> {
        let all = self.entries.lock().unwrap();
        let left = all.get(key)?.locked_until - (self.clock)();
        (left > 0.0).then(|| Duration::from_secs_f64(left))
    }

    /// Count a failure; the lock it caused, if any.
    pub fn fail(&self, key: &str, free_failures: usize) -> Option<Duration> {
        let now = (self.clock)();
        let mut all = self.entries.lock().unwrap();
        let e = all.entry(key.to_string()).or_default();
        let window = self.window.as_secs_f64();
        e.failures.retain(|t| now - t <= window);
        e.failures.push(now);
        if e.failures.len() < free_failures {
            return None;
        }
        let lock = self
            .first_lock
            .saturating_mul(1u32 << e.locks.min(20))
            .min(self.max_lock);
        e.locks += 1;
        e.locked_until = now + lock.as_secs_f64();
        Some(lock)
    }

    pub fn succeed(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clock() -> (Clock, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(1_000_000));
        let c = t.clone();
        (Arc::new(move || c.load(Ordering::SeqCst) as f64), t)
    }

    #[test]
    fn sessions_end_when_idle_or_too_old_and_promotion_changes_the_id() {
        let (c, t) = clock();
        let store = SessionStore::new(c, Duration::from_secs(3600), Duration::from_secs(12 * 3600));
        let pending = store.create("me", "totp");
        assert!(store.get(Some(&pending.id)).is_some());
        let full = store.promote(&pending);
        assert_ne!(full.id, pending.id);
        assert_ne!(full.csrf, pending.csrf);
        assert!(store.get(Some(&pending.id)).is_none());
        assert_eq!(store.get(Some(&full.id)).unwrap().stage, "ok");
        t.fetch_add(3000, Ordering::SeqCst);
        assert!(store.get(Some(&full.id)).is_some(), "touched");
        t.fetch_add(3601, Ordering::SeqCst);
        assert!(store.get(Some(&full.id)).is_none(), "idle");
        let p = store.create("me", "enroll");
        t.fetch_add(301, Ordering::SeqCst);
        assert!(store.get(Some(&p.id)).is_none(), "pending lasts 5 min");
        assert!(store.get(None).is_none());
    }

    #[test]
    fn failures_past_the_free_ones_lock_with_a_doubling_backoff() {
        let (c, t) = clock();
        let th = LoginThrottle::new(c);
        for _ in 0..4 {
            assert!(th.fail("k", 5).is_none());
        }
        assert_eq!(th.fail("k", 5), Some(Duration::from_secs(60)));
        assert!(th.locked_for("k").is_some());
        assert_eq!(th.fail("k", 5), Some(Duration::from_secs(120)));
        t.fetch_add(121, Ordering::SeqCst);
        assert!(th.locked_for("k").is_none());
        th.succeed("k");
        assert!(th.fail("k", 5).is_none());
        assert_eq!(LoginThrottle::account_key("Me"), "user:me");
    }
}
