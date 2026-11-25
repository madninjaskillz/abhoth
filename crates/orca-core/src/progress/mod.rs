//! progress.js buckets (port of `OpenOrca.Core.Progress`, `ProgressBuckets.cs`): the bars the
//! progress-bar script writes, read as OpenOrca bars, and a watcher that follows one bucket.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::orchestration::custom_agents::files_with_extension;
use crate::orchestration::palette;

/// One bar written by `progress.js` (the progress-bar script the Claude Code footer and
/// tools/progress use): `~/.claude/progress/<session>/<key>.json` holding
/// `{label, pct, colour, started, updated, source}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressFileBar {
    pub key: String,
    pub label: String,
    /// 0–100.
    pub pct: f64,
    /// "blue" … "white"; its letter is fixed (blue A … white H).
    pub colour: String,
    /// Unix milliseconds.
    pub started: i64,
    /// Unix milliseconds.
    pub updated: i64,
    pub source: Option<String>,
}

impl ProgressFileBar {
    /// The colour's index in the agent palette (blue 0 … white 7; unknown = 0), so the badge
    /// letter matches progress.js.
    pub fn color_slot(&self) -> i32 {
        palette::NAMES
            .iter()
            .position(|n| *n == self.colour)
            .map_or(0, |i| i as i32)
    }

    /// The badge letter (A … H).
    pub fn letter(&self) -> char {
        palette::letter_of(self.color_slot())
    }
}

/// Files not updated for this long are skipped (progress.js's staleness rule).
pub fn stale_after() -> Duration {
    Duration::from_secs(30 * 60)
}

/// `~/.claude/progress` then `%TEMP%/claude-progress` (the sandbox fallback).
pub fn default_roots() -> Vec<PathBuf> {
    vec![
        crate::config::manager::user_profile()
            .join(".claude")
            .join("progress"),
        std::env::temp_dir().join("claude-progress"),
    ]
}

/// A session id made safe the way progress.js does it (so both sides name the same folder):
/// every char but `A-Za-z0-9_.-` becomes `_` (per UTF-16 unit, as in C#), leading dots become
/// one `_`, and the result is cut to 80 chars.
pub fn safe_session(session: &str) -> String {
    let mut s = String::with_capacity(session.len());
    for c in session.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            s.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                s.push('_');
            }
        }
    }
    let trimmed = s.trim_start_matches('.');
    if trimmed.len() != s.len() {
        s = format!("_{trimmed}");
    }
    if s.len() > 80 {
        s.truncate(80);
    }
    s
}

/// The bucket a bridged OpenOrca session's CLI writes to: `openorca-<id>`.
pub fn bridge_session(id: &str) -> String {
    safe_session(&format!("openorca-{id}"))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Live bars of a session across the roots (`None` = [`default_roots`]), oldest first (by
/// `started`, then key). A key in both roots: the newer wins. `now_ms` (Unix milliseconds,
/// default now) decides staleness.
pub fn read(session: &str, roots: Option<&[PathBuf]>, now_ms: Option<i64>) -> Vec<ProgressFileBar> {
    let now_ms = now_ms.unwrap_or_else(now_unix_ms);
    let defaults;
    let roots = match roots {
        Some(r) => r,
        None => {
            defaults = default_roots();
            &defaults
        }
    };
    let stale_ms = stale_after().as_millis() as i64;
    let mut by_key: Vec<ProgressFileBar> = Vec::new();
    for root in roots {
        let dir = root.join(safe_session(session));
        for file in files_with_extension(&dir, "json") {
            let Some(bar) = try_parse(&file) else {
                continue;
            };
            if now_ms - bar.updated > stale_ms {
                continue;
            }
            match by_key.iter_mut().find(|b| b.key == bar.key) {
                Some(seen) if seen.updated < bar.updated => *seen = bar,
                Some(_) => {}
                None => by_key.push(bar),
            }
        }
    }
    by_key.sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.key.cmp(&b.key)));
    by_key
}

/// Parse one state file; `None` when unreadable (mid-write, malformed).
pub fn try_parse(file: &Path) -> Option<ProgressFileBar> {
    let bytes = std::fs::read(file).ok()?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(bytes);
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    let obj = root.as_object()?;
    let key = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let str_of = |name: &str| obj.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let long_of = |name: &str| obj.get(name).and_then(|v| v.as_i64()).unwrap_or(0);
    let pct = obj
        .get("pct")
        .and_then(|v| v.as_f64())
        .map_or(0.0, |p| p.clamp(0.0, 100.0));
    Some(ProgressFileBar {
        label: str_of("label").unwrap_or_else(|| key.clone()),
        pct,
        colour: str_of("colour").unwrap_or_else(|| "blue".to_string()),
        started: long_of("started"),
        updated: long_of("updated"),
        source: str_of("source"),
        key,
    })
}

/// Remove a session's bucket in every root (`None` = [`default_roots`]) — progress.js clear.
pub fn clear(session: &str, roots: Option<&[PathBuf]>) {
    let roots = roots.map_or_else(default_roots, <[PathBuf]>::to_vec);
    for root in roots {
        let _ = std::fs::remove_dir_all(root.join(safe_session(session)));
    }
}

/// A listener for bar changes of a [`ProgressBucketWatcher`].
pub type ChangedFn = dyn Fn(&[ProgressFileBar]) + Send + Sync;

type ChangedListener = Arc<ChangedFn>;

struct WatcherInner {
    session: String,
    roots: Vec<PathBuf>,
    bars: Mutex<Vec<ProgressFileBar>>,
    listeners: Mutex<Vec<ChangedListener>>,
    disposed: AtomicBool,
    stop: Mutex<bool>,
    wake: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl WatcherInner {
    fn refresh(&self) {
        if self.disposed.load(Ordering::Acquire) {
            return;
        }
        let now = read(&self.session, Some(&self.roots), None);
        {
            let mut bars = lock(&self.bars);
            if *bars == now {
                return;
            }
            *bars = now.clone();
        }
        let listeners = lock(&self.listeners).clone();
        for l in listeners {
            // A subscriber must not stop the watcher.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| l(&now)));
        }
    }
}

/// Follows one progress.js bucket and calls the [`on_changed`](Self::on_changed) listeners
/// when its bars change (set, update, done, clear).
///
/// Unlike the C# watcher (a `FileSystemWatcher` per root plus a poll as fallback), this one
/// only polls, on a background thread (default every second): no file-system watcher crate is
/// available and a one-second poll of a small folder is cheap. The thread stops when the
/// watcher is dropped.
pub struct ProgressBucketWatcher {
    inner: Arc<WatcherInner>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ProgressBucketWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressBucketWatcher")
            .field("session", &self.inner.session)
            .field("roots", &self.inner.roots)
            .finish()
    }
}

impl ProgressBucketWatcher {
    /// Start following `session`'s bucket in `roots` (`None` = [`default_roots`]), re-reading
    /// it every `poll_interval` (default 1 s) starting now.
    pub fn new(
        session: &str,
        roots: Option<Vec<PathBuf>>,
        poll_interval: Option<Duration>,
    ) -> Self {
        let inner = Arc::new(WatcherInner {
            session: safe_session(session),
            roots: roots.unwrap_or_else(default_roots),
            bars: Mutex::new(Vec::new()),
            listeners: Mutex::new(Vec::new()),
            disposed: AtomicBool::new(false),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let interval = poll_interval.unwrap_or(Duration::from_secs(1));
        let worker = Arc::clone(&inner);
        let thread = std::thread::Builder::new()
            .name("progress-bucket-watcher".to_string())
            .spawn(move || {
                loop {
                    worker.refresh();
                    let stopped = lock(&worker.stop);
                    let (stopped, _) = worker
                        .wake
                        .wait_timeout_while(stopped, interval, |s| !*s)
                        .unwrap_or_else(|e| e.into_inner());
                    if *stopped {
                        return;
                    }
                }
            })
            .ok();
        Self { inner, thread }
    }

    /// The bucket name ("openorca-…").
    pub fn session(&self) -> &str {
        &self.inner.session
    }

    /// The bars now, oldest first.
    pub fn bars(&self) -> Vec<ProgressFileBar> {
        lock(&self.inner.bars).clone()
    }

    /// Register a listener called (on the poll thread, or the thread calling
    /// [`refresh`](Self::refresh)) with the new bar list when anything changed.
    pub fn on_changed(&self, callback: Box<ChangedFn>) {
        lock(&self.inner.listeners).push(Arc::from(callback));
    }

    /// Re-read the bucket now; calls the listeners if it differs.
    pub fn refresh(&self) {
        self.inner.refresh();
    }

    /// Delete the bucket (the session ended) and drop the bars.
    pub fn clear_bucket(&self) {
        clear(&self.inner.session, Some(&self.inner.roots));
        self.inner.refresh();
    }
}

impl Drop for ProgressBucketWatcher {
    fn drop(&mut self) {
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        *lock(&self.inner.stop) = true;
        self.inner.wake.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
