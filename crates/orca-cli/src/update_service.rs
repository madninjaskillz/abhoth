//! Settings -> Updates (desktop app): the update check the tray uses, on demand, with its state
//! for the page. The background check (at start, then every 6 hours, unless
//! `desktop.autoUpdate` is off) and "Check for updates now" run the same [`Updates::check`];
//! a newer release is downloaded (with progress) and then offered: "Download and restart" on
//! the page, "Update available: restart into ..." in the tray.
//!
//! The feed is Velopack's GitHub source in an installed app ([`crate::updater`]); a dev build
//! has none and says so. Tests use a fake feed: nothing here ever applies a real update unless
//! the app's own apply hook is called.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

/// Where the release notes of a version are.
pub fn notes_url(version: &str) -> String {
    format!(
        "{}/releases/tag/abhoth-v{}",
        RELEASES_REPO,
        version.trim_start_matches('v')
    )
}

/// Where releases are published (`abhoth-v*` tags; CI attaches the Velopack feed files).
pub const RELEASES_REPO: &str = "https://github.com/madninjaskillz/abhoth";

/// The update source: Velopack in an installed app, a fake in tests.
pub trait Feed: Send + Sync + 'static {
    /// The running version.
    fn current(&self) -> String;
    /// The channel the feed follows (`stable`: GitHub releases that are not pre-releases).
    fn channel(&self) -> String;
    /// A newer version, if there is one (remembered for `download` and `apply`).
    fn check(&self) -> Result<Option<String>, String>;
    /// Downloads what `check` found; `progress` gets 0-100.
    fn download(&self, progress: Arc<dyn Fn(u8) + Send + Sync>) -> Result<(), String>;
    /// Hands over to the updater, which waits for this process to exit, applies the update
    /// and starts the new version.
    fn apply(&self) -> Result<(), String>;
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// `unavailable` (no feed: a dev build), `idle`, `checking`, `downloading`, `ready`,
    /// `upToDate`, `error`.
    pub phase: String,
    pub current: String,
    pub channel: String,
    /// The newer version found (downloading or ready).
    pub available: Option<String>,
    pub notes_url: Option<String>,
    /// Download progress, 0-100.
    pub progress: Option<u8>,
    /// Unix seconds of the last finished check, and what it found, in words.
    pub last_check: Option<i64>,
    pub last_result: Option<String>,
    pub error: Option<String>,
    /// Who started the check under way or last finished (`you` or `automatic`).
    pub by: Option<String>,
    /// Why there is no feed (a dev build).
    pub reason: Option<String>,
}

pub struct Updates {
    feed: Option<Arc<dyn Feed>>,
    state: Mutex<State>,
    on_ready: Box<dyn Fn(String) + Send + Sync>,
    apply: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// An error from the feed in words a user can act on.
pub fn friendly(e: &str) -> String {
    let l = e.to_lowercase();
    if [
        "dns",
        "resolve",
        "connect",
        "timed out",
        "timeout",
        "network",
        "unreachable",
        "no such host",
    ]
    .iter()
    .any(|k| l.contains(k))
    {
        format!("Couldn't reach GitHub: this PC looks offline, or GitHub can't be reached. ({e})")
    } else if ["404", "403", "429", "rate limit", "not found", "forbidden"]
        .iter()
        .any(|k| l.contains(k))
    {
        format!(
            "The releases on GitHub couldn't be read (not found, or rate-limited for now). ({e})"
        )
    } else if l.contains("checksum") || l.contains("sha") || l.contains("hash") {
        format!("The download was damaged; check again to retry. ({e})")
    } else {
        e.to_string()
    }
}

impl Updates {
    /// `feed`: none in a dev build. `on_ready`: called with the version once a download is
    /// ready (the tray's item).
    pub fn new(
        feed: Option<Arc<dyn Feed>>,
        on_ready: impl Fn(String) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let state = match &feed {
            Some(f) => State {
                phase: "idle".into(),
                current: f.current(),
                channel: f.channel(),
                ..Default::default()
            },
            None => State {
                phase: "unavailable".into(),
                current: env!("CARGO_PKG_VERSION").into(),
                channel: "stable".into(),
                reason: Some(
                    "This copy wasn't installed with the installer (a dev build), so it can't update itself. Install the app from the releases page to get updates."
                        .into(),
                ),
                ..Default::default()
            },
        };
        Arc::new(Self {
            feed,
            state: Mutex::new(state),
            on_ready: Box::new(on_ready),
            apply: Mutex::new(None),
        })
    }

    /// What the app does to apply a ready update (stop the host cleanly, then [`Feed::apply`]).
    pub fn set_apply(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.apply.lock().unwrap() = Some(Box::new(f));
    }

    pub fn status(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// Marks a check as started; false when there is no feed, one is under way, or an update
    /// is ready.
    fn begin(&self, by: &str) -> bool {
        if self.feed.is_none() {
            return false;
        }
        let mut s = self.state.lock().unwrap();
        if matches!(s.phase.as_str(), "checking" | "downloading" | "ready") {
            return false;
        }
        s.phase = "checking".into();
        s.error = None;
        s.progress = None;
        s.by = Some(by.into());
        true
    }

    /// One check (and download, when there is something newer), on this thread. Returns
    /// false when there is no feed, a check is already under way, or an update is ready.
    pub fn check(self: &Arc<Self>, by: &str) -> bool {
        if !self.begin(by) {
            return false;
        }
        self.run();
        true
    }

    fn run(self: &Arc<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        let found = feed.check();
        let version = match found {
            Err(e) => {
                let mut s = self.state.lock().unwrap();
                let msg = friendly(&e);
                s.phase = "error".into();
                s.last_check = Some(now());
                s.last_result = Some(format!("Check failed: {msg}"));
                s.error = Some(msg);
                return;
            }
            Ok(None) => {
                let mut s = self.state.lock().unwrap();
                s.phase = "upToDate".into();
                s.available = None;
                s.notes_url = None;
                s.last_check = Some(now());
                s.last_result = Some(format!("Up to date ({}).", s.current));
                return;
            }
            Ok(Some(v)) => v,
        };
        {
            let mut s = self.state.lock().unwrap();
            s.phase = "downloading".into();
            s.available = Some(version.clone());
            s.notes_url = Some(notes_url(&version));
            s.progress = Some(0);
        }
        let me = Arc::downgrade(self);
        let progress: Arc<dyn Fn(u8) + Send + Sync> = Arc::new(move |p| {
            if let Some(me) = me.upgrade() {
                let mut s = me.state.lock().unwrap();
                if s.phase == "downloading" {
                    s.progress = Some(p.min(100));
                }
            }
        });
        let r = feed.download(progress);
        let mut s = self.state.lock().unwrap();
        s.last_check = Some(now());
        match r {
            Ok(()) => {
                s.phase = "ready".into();
                s.progress = Some(100);
                s.last_result = Some(format!("{version} is downloaded and ready."));
                drop(s);
                (self.on_ready)(version);
            }
            Err(e) => {
                let msg = friendly(&e);
                s.phase = "error".into();
                s.progress = None;
                s.last_result = Some(format!("{version} was found, but the download failed."));
                s.error = Some(msg);
            }
        }
    }

    /// "Check for updates now": the check on its own thread. Answers at once with the state
    /// (already `checking`, so the page never shows the old result as new).
    pub fn check_now(self: &Arc<Self>) -> Result<State, String> {
        if self.feed.is_none() {
            return Err(self.status().reason.unwrap_or_default());
        }
        if self.begin("you") {
            let me = self.clone();
            std::thread::Builder::new()
                .name("abhoth-update-now".into())
                .spawn(move || me.run())
                .map_err(|e| e.to_string())?;
        }
        Ok(self.status())
    }

    /// The background check: now, then every `every`, while `auto()` says so.
    pub fn spawn_auto(self: &Arc<Self>, every: Duration, auto: impl Fn() -> bool + Send + 'static) {
        if self.feed.is_none() {
            tracing::debug!("updates: not an installed app; no update checks");
            return;
        }
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("abhoth-update-check".into())
            .spawn(move || {
                loop {
                    let Some(m) = me.upgrade() else { return };
                    if auto() {
                        m.check("automatic");
                        if m.status().phase == "ready" {
                            return;
                        }
                    }
                    drop(m);
                    std::thread::sleep(every);
                }
            });
    }

    /// "Download and restart": the app's apply hook (only once an update is ready).
    pub fn apply(&self) -> Result<(), String> {
        if self.status().phase != "ready" {
            return Err("No update is downloaded yet; check for updates first.".into());
        }
        match self.apply.lock().unwrap().as_ref() {
            Some(f) => {
                f();
                Ok(())
            }
            None => Err("The app can't restart itself here.".into()),
        }
    }

    /// Applies a ready update through the feed (the app calls this after stopping its host).
    pub fn apply_feed(&self) -> Result<(), String> {
        match &self.feed {
            Some(f) => f.apply(),
            None => Err("no update feed".into()),
        }
    }
}

fn json_response(status: u16, v: &Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        v.to_string(),
    )
        .into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}

/// The web host's handler for `/api/desktop/updates[/check|/apply]` (admins; checked first).
pub async fn handle(u: Arc<Updates>, req: Request<Body>) -> Response {
    let path = req.uri().path().trim_end_matches('/').to_string();
    let method = req.method().clone();
    match (method, path.as_str()) {
        (Method::GET, "/api/desktop/updates") => json_response(200, &json!(u.status())),
        (Method::POST, "/api/desktop/updates/check") => {
            let r = tokio::task::spawn_blocking(move || u.check_now()).await;
            match r {
                Ok(Ok(s)) => json_response(202, &json!(s)),
                Ok(Err(e)) => json_response(409, &json!({"error": e})),
                Err(e) => json_response(500, &json!({"error": e.to_string()})),
            }
        }
        (Method::POST, "/api/desktop/updates/apply") => match u.apply() {
            Ok(()) => json_response(202, &json!({"ok": true})),
            Err(e) => json_response(409, &json!({"error": e})),
        },
        _ => json_response(404, &json!({"error": "no such updates call"})),
    }
}

/// [`handle`] as the web host's hook.
pub fn hook(u: Arc<Updates>) -> orca_webui::server::MachineFn {
    Arc::new(move |req| {
        let u = u.clone();
        Box::pin(async move { handle(u, req).await })
    })
}

#[cfg(test)]
#[path = "update_service_tests.rs"]
mod tests;
