//! `LoginThrottle` (brute-force brakes on sign-in and 2FA codes) and the clock tests move.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
