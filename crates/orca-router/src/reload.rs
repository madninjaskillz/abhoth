//! Atomic hot reload of `router.json` (ROUTER.md 4.3): a watcher (every 2 s, mtime + size, as
//! effort_hook) and `POST /admin/reload`. A half-written or invalid file keeps the old snapshot
//! and `/status` shows `configError`.

use std::sync::Arc;
use std::time::Duration;

use crate::config::{FileSig, Snapshot};
use crate::state::AppState;

/// Reload now. Ok(message) when swapped or unchanged.
pub fn reload_now(st: &Arc<AppState>) -> Result<String, String> {
    let Some(path) = st.config_path.clone() else {
        return Err("no config file (started without --config)".into());
    };
    match Snapshot::load(&path) {
        Ok(s) => {
            let rev = s.config.rev.clone();
            st.swap(s);
            tracing::info!("router: config reloaded (rev {rev})");
            Ok(format!("reloaded rev {rev}"))
        }
        Err(e) => {
            tracing::warn!("router: config reload failed, keeping the old one: {e}");
            *st.config_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}

/// Watch the config file; returns when `stop` fires.
pub async fn watch(st: Arc<AppState>, every: Duration) {
    let Some(path) = st.config_path.clone() else {
        return;
    };
    let mut last_bad: Option<FileSig> = None;
    loop {
        tokio::time::sleep(every).await;
        let sig = FileSig::of(&path);
        if sig.is_none() || sig == st.snapshot().sig || sig == last_bad {
            continue;
        }
        if reload_now(&st).is_err() {
            last_bad = sig;
        } else {
            last_bad = None;
        }
    }
}
