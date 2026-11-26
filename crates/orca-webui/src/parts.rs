//! The status of the extra parts running in this process alongside the web UI (the router and
//! the supervisor in the single-container mode, `orca-cli`'s `together.rs`). The part runner
//! writes it; `/api/info` (admins) and `/api/parts` read it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartState {
    /// Started (or restarted) and still running.
    Running,
    /// Stopped or failed; waiting out its backoff before the next start.
    Restarting,
    /// Stopped for good (the process is shutting down).
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartStatus {
    pub state: PartState,
    /// Restarts so far (0 = the first start is still the current one).
    pub restarts: u64,
    /// The last failure, e.g. "exit code 1: supervisor.toml: router_key: …" or "panicked: …".
    pub last_error: Option<String>,
    /// When the last failure happened (Unix ms).
    pub last_error_at: Option<u64>,
    /// When the current run started (Unix ms).
    pub since: u64,
    /// While restarting: the next start (Unix ms).
    pub next_start_at: Option<u64>,
}

static PARTS: Mutex<BTreeMap<String, PartStatus>> = Mutex::new(BTreeMap::new());

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Records `name`'s status (replacing the previous one).
pub fn set(name: &str, status: PartStatus) {
    if let Ok(mut m) = PARTS.lock() {
        m.insert(name.to_string(), status);
    }
}

/// One part's status.
pub fn get(name: &str) -> Option<PartStatus> {
    PARTS.lock().ok().and_then(|m| m.get(name).cloned())
}

/// Every part's status, by name (empty when the web UI runs alone).
pub fn all() -> BTreeMap<String, PartStatus> {
    PARTS.lock().map(|m| m.clone()).unwrap_or_default()
}

/// Drops a part's status (tests).
pub fn remove(name: &str) {
    if let Ok(mut m) = PARTS.lock() {
        m.remove(name);
    }
}
