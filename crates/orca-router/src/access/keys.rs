//! Checking `abk_…` keys by asking the Abhoth web server (ROUTER-CONFIG.md 11.1, D-RC6), with
//! a cache: answers are kept 60 s (unknown keys 10 s), so a revoked key stops working within a
//! minute. If the web server can't be reached, a key it vouched for in the last 15 minutes keeps
//! working; any other key gets 503 (never "allowed by default").
//!
//! Only the key's SHA-256 is sent and kept; the key itself never leaves the request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_core::router_link::{
    API_KEY_PREFIX, INTROSPECT_PATH, IntrospectRequest, Introspection, Role, key_digest,
};

/// Who a valid key belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub user_id: i64,
    /// The user (or, for a service key, its owning admin).
    pub name: String,
    pub role: Role,
    pub key_id: i64,
    pub key_name: String,
    pub scopes: Vec<String>,
}

impl Identity {
    fn from(i: &Introspection) -> Option<Self> {
        if !i.active || i.disabled {
            return None;
        }
        Some(Self {
            user_id: i.user_id?,
            name: i.name.clone().unwrap_or_default(),
            role: i.role?,
            key_id: i.key_id?,
            key_name: i.key_name.clone().unwrap_or_default(),
            scopes: i.scopes.clone(),
        })
    }
}

/// Why a key was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Not a key the web server knows (or revoked, or its account disabled): 401.
    Invalid,
    /// The web server could not be asked and the key is not cached: 503.
    Unavailable(String),
}

/// Asks the web server about one key digest. `Ok` is its answer (active or not); `Err` means it
/// could not be asked.
#[async_trait::async_trait]
pub trait Introspector: Send + Sync {
    async fn introspect(&self, key_sha256: &str) -> Result<Introspection, String>;
}

/// The real one: `POST <base>/api/internal/keys/introspect` with the link token.
pub struct HttpIntrospector {
    pub base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HttpIntrospector {
    pub fn new(base_url: &str, token: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(4))
            .build()
            .expect("http client");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            http,
        }
    }
}

#[async_trait::async_trait]
impl Introspector for HttpIntrospector {
    async fn introspect(&self, key_sha256: &str) -> Result<Introspection, String> {
        let r = self
            .http
            .post(format!("{}{INTROSPECT_PATH}", self.base_url))
            .bearer_auth(&self.token)
            .header("x-abhoth-no-switch", "1")
            .json(&IntrospectRequest {
                key_sha256: key_sha256.to_string(),
            })
            .send()
            .await
            .map_err(|e| format!("accounts server unreachable: {}", without_url(&e)))?;
        let status = r.status();
        if !status.is_success() {
            return Err(format!("accounts server answered {}", status.as_u16()));
        }
        r.json::<Introspection>()
            .await
            .map_err(|_| "accounts server sent an unreadable answer".to_string())
    }
}

/// reqwest errors name the URL; ours carries no secret, but keep messages short.
pub(crate) fn without_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connection refused or unreachable".into()
    } else {
        "request failed".into()
    }
}

/// Cache lifetimes.
#[derive(Debug, Clone, Copy)]
pub struct CacheTimes {
    pub positive: Duration,
    pub negative: Duration,
    /// How long a known-good key keeps working while the web server is down.
    pub stale: Duration,
}

impl Default for CacheTimes {
    fn default() -> Self {
        Self {
            positive: Duration::from_secs(60),
            negative: Duration::from_secs(10),
            stale: Duration::from_secs(15 * 60),
        }
    }
}

/// Milliseconds on a monotonic clock (tests move it).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_millis() as u64)
}

#[derive(Clone)]
struct Entry {
    identity: Option<Identity>,
    fetched_ms: u64,
}

/// Counters for `/metrics`.
#[derive(Default)]
pub struct KeyStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub stale_served: AtomicU64,
    pub failures: AtomicU64,
    pub invalid: AtomicU64,
}

/// At most this many digests are cached (expired ones are dropped first).
const MAX_ENTRIES: usize = 10_000;

pub struct KeyCache {
    backend: Arc<dyn Introspector>,
    times: CacheTimes,
    clock: Clock,
    entries: Mutex<HashMap<String, Entry>>,
    pub stats: KeyStats,
}

impl KeyCache {
    pub fn new(backend: Arc<dyn Introspector>, times: CacheTimes, clock: Clock) -> Self {
        Self {
            backend,
            times,
            clock,
            entries: Mutex::new(HashMap::new()),
            stats: KeyStats::default(),
        }
    }

    /// The principal of `key`, from the cache or the web server.
    pub async fn check(&self, key: &str) -> Result<Identity, KeyError> {
        if !key.starts_with(API_KEY_PREFIX) || key.len() < API_KEY_PREFIX.len() + 16 {
            self.stats.invalid.fetch_add(1, Ordering::Relaxed);
            return Err(KeyError::Invalid);
        }
        self.check_digest(&key_digest(key)).await
    }

    /// [`Self::check`] by the key's digest (a signed-in session keeps only the digest).
    pub async fn check_digest(&self, digest: &str) -> Result<Identity, KeyError> {
        let digest = digest.to_string();
        let now = (self.clock)();
        let cached = self.entries.lock().unwrap().get(&digest).cloned();
        if let Some(e) = &cached {
            let ttl = if e.identity.is_some() {
                self.times.positive
            } else {
                self.times.negative
            };
            if now.saturating_sub(e.fetched_ms) < ttl.as_millis() as u64 {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                return e.identity.clone().ok_or_else(|| {
                    self.stats.invalid.fetch_add(1, Ordering::Relaxed);
                    KeyError::Invalid
                });
            }
        }
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        match self.backend.introspect(&digest).await {
            Ok(answer) => {
                let identity = Identity::from(&answer);
                self.store(digest, identity.clone(), now);
                identity.ok_or_else(|| {
                    self.stats.invalid.fetch_add(1, Ordering::Relaxed);
                    KeyError::Invalid
                })
            }
            Err(why) => {
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                match cached {
                    Some(Entry {
                        identity: Some(id),
                        fetched_ms,
                    }) if now.saturating_sub(fetched_ms) < self.times.stale.as_millis() as u64 => {
                        self.stats.stale_served.fetch_add(1, Ordering::Relaxed);
                        Ok(id)
                    }
                    _ => Err(KeyError::Unavailable(why)),
                }
            }
        }
    }

    fn store(&self, digest: String, identity: Option<Identity>, now: u64) {
        let mut m = self.entries.lock().unwrap();
        if m.len() >= MAX_ENTRIES {
            let keep = self.times.stale.max(self.times.positive).as_millis() as u64;
            m.retain(|_, e| now.saturating_sub(e.fetched_ms) < keep);
            if m.len() >= MAX_ENTRIES {
                m.clear();
            }
        }
        m.insert(
            digest,
            Entry {
                identity,
                fetched_ms: now,
            },
        );
    }

    /// Forget everything (an admin revoked keys and wants it now, or tests).
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A web server in memory: a key table, a call counter and an "is it up" switch.
    #[derive(Default)]
    struct Fake {
        keys: Mutex<HashMap<String, Introspection>>,
        calls: AtomicU64,
        down: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Introspector for Fake {
        async fn introspect(&self, d: &str) -> Result<Introspection, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err("down".into());
            }
            Ok(self
                .keys
                .lock()
                .unwrap()
                .get(d)
                .cloned()
                .unwrap_or_default())
        }
    }

    const KEY: &str = "abk_0123456789abcdef0123456789";

    fn user() -> Introspection {
        Introspection {
            active: true,
            user_id: Some(2),
            name: Some("bob".into()),
            role: Some(Role::User),
            key_id: Some(5),
            key_name: Some("laptop".into()),
            ..Default::default()
        }
    }

    fn setup() -> (Arc<Fake>, KeyCache, Arc<AtomicU64>) {
        let fake = Arc::new(Fake::default());
        fake.keys.lock().unwrap().insert(key_digest(KEY), user());
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        let cache = KeyCache::new(
            fake.clone(),
            CacheTimes::default(),
            Arc::new(move || tc.load(Ordering::SeqCst)),
        );
        (fake, cache, t)
    }

    #[tokio::test]
    async fn a_good_key_is_asked_once_a_minute() {
        let (fake, cache, t) = setup();
        let id = cache.check(KEY).await.unwrap();
        assert_eq!(
            (id.user_id, id.role, id.key_name.as_str()),
            (2, Role::User, "laptop")
        );
        t.store(59_000, Ordering::SeqCst);
        cache.check(KEY).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        t.store(60_000, Ordering::SeqCst);
        cache.check(KEY).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_revoked_key_stops_within_a_minute() {
        let (fake, cache, t) = setup();
        cache.check(KEY).await.unwrap();
        fake.keys.lock().unwrap().clear(); // revoked
        t.store(30_000, Ordering::SeqCst);
        assert!(cache.check(KEY).await.is_ok(), "still cached");
        t.store(60_001, Ordering::SeqCst);
        assert_eq!(cache.check(KEY).await, Err(KeyError::Invalid));
    }

    #[tokio::test]
    async fn unknown_keys_are_cached_for_ten_seconds() {
        let (fake, cache, t) = setup();
        let other = "abk_ffffffffffffffffffffffffffff";
        assert_eq!(cache.check(other).await, Err(KeyError::Invalid));
        t.store(9_000, Ordering::SeqCst);
        assert_eq!(cache.check(other).await, Err(KeyError::Invalid));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        // issued meanwhile: found after the negative entry expires
        fake.keys.lock().unwrap().insert(key_digest(other), user());
        t.store(10_000, Ordering::SeqCst);
        assert!(cache.check(other).await.is_ok());
    }

    #[tokio::test]
    async fn disabled_accounts_and_non_keys_are_refused_without_asking() {
        let (fake, cache, _) = setup();
        assert_eq!(cache.check("sk-not-ours").await, Err(KeyError::Invalid));
        assert_eq!(cache.check("abk_short").await, Err(KeyError::Invalid));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        let mut u = user();
        u.disabled = true;
        fake.keys.lock().unwrap().insert(key_digest(KEY), u);
        assert_eq!(cache.check(KEY).await, Err(KeyError::Invalid));
    }

    #[tokio::test]
    async fn web_server_down_keeps_known_keys_for_fifteen_minutes_only() {
        let (fake, cache, t) = setup();
        cache.check(KEY).await.unwrap();
        fake.down.store(true, Ordering::SeqCst);
        t.store(5 * 60_000, Ordering::SeqCst);
        assert!(cache.check(KEY).await.is_ok(), "stale but known");
        assert_eq!(cache.stats.stale_served.load(Ordering::SeqCst), 1);
        t.store(15 * 60_000, Ordering::SeqCst);
        assert!(matches!(
            cache.check(KEY).await,
            Err(KeyError::Unavailable(_))
        ));
        // an uncached key while down: 503, not allowed
        assert!(matches!(
            cache.check("abk_eeeeeeeeeeeeeeeeeeeeeeeeeeee").await,
            Err(KeyError::Unavailable(_))
        ));
    }
}
