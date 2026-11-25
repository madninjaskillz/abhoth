//! Shared router state: the config snapshot (swapped atomically on reload), per-card runtime
//! counters (in flight, cooling, draining) and card choice.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::Snapshot;
use crate::metrics::Metrics;
use crate::reqlog::RequestLog;

#[derive(Debug, Default, Clone)]
pub struct CardRt {
    pub inflight: u32,
    pub cooling_until: Option<Instant>,
    pub picks: u64,
    pub draining: bool,
    /// Last time a request started or ended here (on-demand idleness).
    pub last_busy: Option<Instant>,
    pub served: u64,
    pub failures: u64,
}

pub struct AppState {
    snap: RwLock<Arc<Snapshot>>,
    pub config_path: Option<PathBuf>,
    pub config_error: Mutex<Option<String>>,
    pub http: reqwest::Client,
    cards: Mutex<HashMap<String, CardRt>>,
    pub started: Instant,
    pub request_ids: AtomicU64,
    pub metrics: Metrics,
    pub log: RequestLog,
    pub ext: crate::Extensions,
}

impl AppState {
    pub fn new(snap: Snapshot, config_path: Option<PathBuf>) -> Arc<Self> {
        let connect = Duration::from_secs(snap.config.settings.connect_timeout_seconds.max(1));
        let http = reqwest::Client::builder()
            .connect_timeout(connect)
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("http client");
        let log = RequestLog::open(snap.config.settings.request_log.as_deref());
        let ext = crate::Extensions::new(&snap);
        Arc::new(Self {
            snap: RwLock::new(Arc::new(snap)),
            config_path,
            config_error: Mutex::new(None),
            http,
            cards: Mutex::new(HashMap::new()),
            started: Instant::now(),
            request_ids: AtomicU64::new(1),
            metrics: Metrics::default(),
            log,
            ext,
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.read().unwrap().clone()
    }

    /// Swap in a new snapshot; requests in flight keep theirs.
    pub fn swap(&self, snap: Snapshot) {
        self.log.reopen(snap.config.settings.request_log.as_deref());
        *self.snap.write().unwrap() = Arc::new(snap);
        *self.config_error.lock().unwrap() = None;
    }

    pub fn next_id(&self) -> u64 {
        self.request_ids.fetch_add(1, Ordering::Relaxed)
    }

    pub fn cards(&self) -> HashMap<String, CardRt> {
        self.cards.lock().unwrap().clone()
    }

    pub fn card(&self, card: &str) -> CardRt {
        self.cards
            .lock()
            .unwrap()
            .get(card)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_draining(&self, card: &str, draining: bool) {
        self.cards
            .lock()
            .unwrap()
            .entry(card.to_string())
            .or_default()
            .draining = draining;
    }

    pub fn cool(&self, card: &str, secs: u64) {
        let mut m = self.cards.lock().unwrap();
        let c = m.entry(card.to_string()).or_default();
        c.cooling_until = Some(Instant::now() + Duration::from_secs(secs));
        c.failures += 1;
    }

    /// Cards that can take a request now, in preference order is not implied.
    pub fn eligible(
        &self,
        snap: &Snapshot,
        candidates: &[String],
        exclude: &[String],
    ) -> Vec<String> {
        let now = Instant::now();
        let m = self.cards.lock().unwrap();
        let usable: Vec<&String> = candidates
            .iter()
            .filter(|c| !exclude.contains(c))
            .filter(|c| snap.config.upstreams.get(*c).is_some_and(|u| u.online))
            .filter(|c| !m.get(*c).is_some_and(|r| r.draining))
            .collect();
        let warm: Vec<String> = usable
            .iter()
            .filter(|c| {
                !m.get(**c)
                    .and_then(|r| r.cooling_until)
                    .is_some_and(|t| t > now)
            })
            .map(|c| (*c).clone())
            .collect();
        // every candidate cooling: try them anyway rather than refuse (a pinned name has one)
        if warm.is_empty() {
            usable.into_iter().cloned().collect()
        } else {
            warm
        }
    }

    /// Weighted least-in-flight (nginx least_conn with weight = lanes); ties by fewest picks,
    /// then config order.
    pub fn pick(&self, snap: &Snapshot, eligible: &[String]) -> Option<String> {
        let m = self.cards.lock().unwrap();
        let mut best: Option<(u64, u64, usize, &String)> = None;
        for (i, c) in eligible.iter().enumerate() {
            let lanes = snap.config.upstreams.get(c).map_or(1, |u| u.lanes.max(1)) as u64;
            let rt = m.get(c);
            let load = rt.map_or(0, |r| r.inflight as u64) * 1_000_000 / lanes;
            let picks = rt.map_or(0, |r| r.picks) * 1_000_000 / lanes;
            let key = (load, picks, i, c);
            if best
                .as_ref()
                .is_none_or(|b| (key.0, key.1, key.2) < (b.0, b.1, b.2))
            {
                best = Some(key);
            }
        }
        best.map(|b| b.3.clone())
    }

    /// Count a request on `card` until the guard drops.
    pub fn begin(self: &Arc<Self>, card: &str) -> CardGuard {
        let mut m = self.cards.lock().unwrap();
        let c = m.entry(card.to_string()).or_default();
        c.inflight += 1;
        c.picks += 1;
        c.last_busy = Some(Instant::now());
        CardGuard {
            state: self.clone(),
            card: card.to_string(),
        }
    }

    pub fn inflight(&self, card: &str) -> u32 {
        self.card(card).inflight
    }
}

/// Decrements the card's in-flight count when the request (or its streamed body) ends.
pub struct CardGuard {
    state: Arc<AppState>,
    pub card: String,
}

impl Drop for CardGuard {
    fn drop(&mut self) {
        let mut m = self.state.cards.lock().unwrap();
        if let Some(c) = m.get_mut(&self.card) {
            c.inflight = c.inflight.saturating_sub(1);
            c.served += 1;
            c.last_busy = Some(Instant::now());
        }
        drop(m);
        self.state.ext.notify_released(&self.card);
    }
}
