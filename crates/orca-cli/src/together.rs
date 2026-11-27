//! The single-container mode (D148): the web UI, the router and the supervisor in one process.
//!
//! ```text
//! abhoth --hostui=0.0.0.0:8093 --hostui-login \
//!        --with-router "--config /data/router/router.json --port 8010 --control /data/router/router.toml" \
//!        --with-supervisor /data/supervisor/supervisor.toml
//! ```
//!
//! Each extra part runs exactly what `abhoth router ...` / `abhoth supervisor run --config ...`
//! run, on its own OS thread with its own tokio runtime, so a busy agent (builds, big tool
//! output) cannot starve the gateway's proxy threads, and each part keeps its own ports and
//! config.
//!
//! Self-healing: a part that fails (an error at startup such as a missing secret or a port in
//! use, an error or a stop later on, or a panic) is restarted in this process with exponential
//! backoff (1 s, doubling, at most 60 s; back to 1 s after a run of 5 minutes), each time on a
//! fresh runtime (the old one is shut down first, so its listeners are closed). It never takes
//! the web UI or the other part down, and it keeps being retried (a container restart would not
//! fix a missing secret or a taken port either). Its state, restart count and last error are on
//! `/api/parts` and in `/api/info` (admins). The web UI is the main part: if it stops, the
//! process exits as before. Only SIGTERM (docker stop) stops the parts for good: they stop on
//! it themselves and are not restarted, and the web UI (it only listens for Ctrl-C itself)
//! stops after they have had a moment to finish.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use orca_webui::parts::{PartState, PartStatus, now_ms};

pub const ROUTER_FLAG: &str = "--with-router";
pub const SUPERVISOR_FLAG: &str = "--with-supervisor";
/// The same flags under the components' names (docs/NAMES.md).
pub const SOTHOTH_FLAG: &str = "--with-sothoth";
pub const NODENS_FLAG: &str = "--with-nodens";

/// The flag a name stands for: `--with-sothoth` is `--with-router`, `--with-nodens` is
/// `--with-supervisor`; `None` for anything else.
fn part_flag(f: &str) -> Option<&'static str> {
    match f {
        ROUTER_FLAG | SOTHOTH_FLAG => Some(ROUTER_FLAG),
        SUPERVISOR_FLAG | NODENS_FLAG => Some(SUPERVISOR_FLAG),
        _ => None,
    }
}

/// The extra parts asked for on the command line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parts {
    /// The router's arguments (what follows `abhoth router`), split on whitespace.
    pub router: Option<Vec<String>>,
    /// The supervisor's config file.
    pub supervisor: Option<String>,
}

impl Parts {
    pub fn any(&self) -> bool {
        self.router.is_some() || self.supervisor.is_some()
    }
}

/// Removes `--with-router ARGS` / `--with-router=ARGS` and `--with-supervisor FILE` /
/// `--with-supervisor=FILE` from `raw` (the rest is parsed as usual). Router arguments are one
/// word: split on whitespace, no quoting (paths with spaces are not supported here; use the
/// separate `abhoth router` for those).
pub fn take(raw: &mut Vec<String>) -> Result<Parts, String> {
    let mut parts = Parts::default();
    let mut i = 0;
    while i < raw.len() {
        let a = raw[i].clone();
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if part_flag(f).is_some() => (f.to_string(), Some(v)),
            _ if part_flag(a.as_str()).is_some() => (a.clone(), None),
            _ => {
                i += 1;
                continue;
            }
        };
        let value = match inline {
            Some(v) => {
                raw.remove(i);
                v.to_string()
            }
            None => {
                if i + 1 >= raw.len() {
                    return Err(format!("{flag} needs a value"));
                }
                raw.remove(i);
                raw.remove(i)
            }
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(format!("{flag} needs a value"));
        }
        if part_flag(&flag) == Some(ROUTER_FLAG) {
            if parts.router.is_some() {
                return Err(format!("{flag} given twice"));
            }
            let words: Vec<String> = value.split_whitespace().map(str::to_string).collect();
            // `router` / `sothoth` itself is optional, like the image's abhoth-router.
            let words = match words.first().map(String::as_str) {
                Some("router" | "sothoth") => words[1..].to_vec(),
                _ => words,
            };
            // Only the gateway runs alongside the web UI, not a one-shot subcommand.
            if let Some(w) = words.first().filter(|w| !w.starts_with('-')) {
                return Err(format!(
                    "{flag}: '{w}' is a router subcommand; give the gateway's flags (--config ...)"
                ));
            }
            parts.router = Some(words);
        } else {
            if parts.supervisor.is_some() {
                return Err(format!("{flag} given twice"));
            }
            parts.supervisor = Some(value);
        }
    }
    Ok(parts)
}

/// The running extra parts.
pub struct Running {
    #[cfg_attr(not(unix), allow(dead_code))]
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Set on SIGTERM: a part that stops now is not restarted.
    #[cfg_attr(not(unix), allow(dead_code))]
    stopping: Arc<AtomicBool>,
}

/// How a failed part is restarted.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// The wait before the first restart.
    pub initial: Duration,
    /// The longest wait (the wait doubles after each failure, up to this).
    pub max: Duration,
    /// A run at least this long resets the wait to `initial`.
    pub stable: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            stable: Duration::from_secs(300),
        }
    }
}

/// Starts the parts (each on its own thread and runtime). Call from the web UI's runtime.
pub fn start(parts: &Parts) -> Running {
    let stopping = Arc::new(AtomicBool::new(false));
    #[allow(unused_mut)]
    let mut threads = Vec::new();
    #[cfg(feature = "router")]
    if let Some(args) = parts.router.clone() {
        threads.push(spawn_part(
            "router",
            Backoff::default(),
            stopping.clone(),
            move || {
                let args = args.clone();
                on_runtime("abhoth-router", async move {
                    match orca_router::cli::run(args).await {
                        Ok(0) => Ok(()),
                        Ok(code) => Err(format!("exit code {code}")),
                        Err(e) => Err(format!("{e:#}")),
                    }
                })
            },
        ));
    }
    #[cfg(not(feature = "router"))]
    if parts.router.is_some() {
        eprintln!("abhoth: this build has no router (feature `router`); not started");
        orca_webui::parts::set("router", stopped_with("this build has no router"));
    }
    #[cfg(feature = "supervisor")]
    if let Some(config) = parts.supervisor.clone() {
        threads.push(spawn_part(
            "supervisor",
            Backoff::default(),
            stopping.clone(),
            move || on_runtime("abhoth-supervisor", orca_supervisor::run::run_file(&config)),
        ));
    }
    #[cfg(not(feature = "supervisor"))]
    if parts.supervisor.is_some() {
        eprintln!("abhoth: this build has no supervisor (feature `supervisor`); not started");
        orca_webui::parts::set("supervisor", stopped_with("this build has no supervisor"));
    }
    let running = Running {
        threads: Arc::new(Mutex::new(threads)),
        stopping,
    };
    running.stop_on_sigterm();
    running
}

/// A part that is not running and will not be started again.
fn stopped_with(why: &str) -> PartStatus {
    PartStatus {
        state: PartState::Stopped,
        restarts: 0,
        last_error: Some(why.to_string()),
        last_error_at: Some(now_ms()),
        since: now_ms(),
        next_start_at: None,
    }
}

impl Running {
    /// SIGTERM (docker stop): the router and supervisor stop on it themselves (and are not
    /// restarted); wait up to 10 s for them, then stop the process (the web UI listens for
    /// Ctrl-C only).
    fn stop_on_sigterm(&self) {
        #[cfg(unix)]
        {
            let threads = self.threads.clone();
            let stopping = self.stopping.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let Ok(mut term) = signal(SignalKind::terminate()) else {
                    return;
                };
                term.recv().await;
                stopping.store(true, Ordering::SeqCst);
                tracing::info!("SIGTERM: stopping the other parts (if any), then the web UI");
                let waited = tokio::task::spawn_blocking(move || {
                    let handles: Vec<_> = threads
                        .lock()
                        .map(|mut t| t.drain(..).collect())
                        .unwrap_or_default();
                    for h in handles {
                        let _ = h.join();
                    }
                });
                let _ = tokio::time::timeout(Duration::from_secs(10), waited).await;
                orca_tools_shell::process::kill_all();
                std::process::exit(0);
            });
        }
    }
}

/// Runs `fut` on a fresh multi-threaded runtime named `name`, then shuts that runtime down
/// (at most 5 s for its blocking threads), so a restarted part starts clean: the old run's
/// listeners and tasks are gone. A panic in `fut` comes back as an error.
#[allow(dead_code)] // unused when neither the router nor the supervisor feature is on
fn on_runtime(
    name: &str,
    fut: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(name)
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| rt.block_on(fut)));
    rt.shutdown_timeout(Duration::from_secs(5));
    out.unwrap_or_else(|p| Err(format!("panicked: {}", panic_text(p.as_ref()))))
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string())
}

/// Runs `body` on its own thread, restarting it with `backoff` whenever it returns or
/// panics, until `stopping` is set. Its status is kept in `orca_webui::parts`.
#[allow(dead_code)] // unused when neither the router nor the supervisor feature is on
fn spawn_part(
    name: &'static str,
    backoff: Backoff,
    stopping: Arc<AtomicBool>,
    body: impl FnMut() -> Result<(), String> + Send + 'static,
) -> JoinHandle<()> {
    let thread = std::thread::Builder::new()
        .name(format!("abhoth-{name}"))
        .spawn(move || supervise(name, backoff, &stopping, body));
    match thread {
        Ok(t) => t,
        Err(e) => {
            // No thread for it: the web UI still runs; the part shows as stopped.
            eprintln!("abhoth: could not start the {name}: {e}");
            tracing::error!("could not start the {name}: {e}");
            orca_webui::parts::set(name, stopped_with(&format!("no thread: {e}")));
            std::thread::spawn(|| {})
        }
    }
}

/// The restart loop of one part (see the module docs). Returns once `stopping` is set.
fn supervise(
    name: &str,
    backoff: Backoff,
    stopping: &AtomicBool,
    mut body: impl FnMut() -> Result<(), String>,
) {
    let mut wait = backoff.initial;
    let mut restarts: u64 = 0;
    let mut last_error: Option<(String, u64)> = None;
    loop {
        let status = |state, restarts, last: &Option<(String, u64)>, next_start_at| PartStatus {
            state,
            restarts,
            last_error: last.as_ref().map(|e| e.0.clone()),
            last_error_at: last.as_ref().map(|e| e.1),
            since: now_ms(),
            next_start_at,
        };
        orca_webui::parts::set(
            name,
            status(PartState::Running, restarts, &last_error, None),
        );
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(&mut body))
            .unwrap_or_else(|p| Err(format!("panicked: {}", panic_text(p.as_ref()))));
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        if started.elapsed() >= backoff.stable {
            wait = backoff.initial;
        }
        let why = match outcome {
            Ok(()) => "stopped unexpectedly".to_string(),
            Err(e) => e,
        };
        tracing::error!(
            "the {name} failed ({why}); restarting it in {wait:.0?}; the web UI and other parts keep running"
        );
        eprintln!(
            "abhoth: the {name} failed ({why}); restarting it in {wait:.0?} (restart {})",
            restarts + 1
        );
        last_error = Some((why, now_ms()));
        let next = now_ms() + wait.as_millis() as u64;
        orca_webui::parts::set(
            name,
            status(PartState::Restarting, restarts, &last_error, Some(next)),
        );
        // Wait in small steps, so SIGTERM (which may reach the part first and make it stop
        // "cleanly") is seen before a restart.
        let until = Instant::now() + wait;
        while !stopping.load(Ordering::SeqCst) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        wait = (wait * 2).min(backoff.max);
        restarts += 1;
        tracing::info!("restarting the {name} (restart {restarts})");
        eprintln!("abhoth: restarting the {name} (restart {restarts})");
    }
    tracing::info!("the {name} stopped");
    eprintln!("abhoth: the {name} stopped");
    let mut st = orca_webui::parts::get(name).unwrap_or_else(|| stopped_with(""));
    st.state = PartState::Stopped;
    st.next_start_at = None;
    orca_webui::parts::set(name, st);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn takes_both_forms_and_leaves_the_rest() {
        let mut raw = v(&[
            "--hostui=0.0.0.0:8093",
            "--with-router",
            "--config /r/router.json  --port 8010 --control /r/router.toml",
            "--hostui-login",
            "--with-supervisor=/s/supervisor.toml",
        ]);
        let p = take(&mut raw).unwrap();
        assert_eq!(raw, v(&["--hostui=0.0.0.0:8093", "--hostui-login"]));
        assert_eq!(
            p.router.unwrap(),
            v(&[
                "--config",
                "/r/router.json",
                "--port",
                "8010",
                "--control",
                "/r/router.toml"
            ])
        );
        assert_eq!(p.supervisor.as_deref(), Some("/s/supervisor.toml"));
    }

    #[test]
    fn new_names_are_the_same_flags() {
        let mut raw = v(&[
            "--with-sothoth",
            "sothoth --config /r/sothoth.json",
            "--with-nodens=/s/nodens.toml",
        ]);
        let p = take(&mut raw).unwrap();
        assert!(raw.is_empty());
        assert_eq!(p.router.unwrap(), v(&["--config", "/r/sothoth.json"]));
        assert_eq!(p.supervisor.as_deref(), Some("/s/nodens.toml"));
        // An old and a new flag for the same part is one part given twice.
        assert!(
            take(&mut v(&[
                "--with-router=--config a",
                "--with-sothoth=--config b"
            ]))
            .is_err()
        );
        assert!(take(&mut v(&["--with-supervisor=a", "--with-nodens=b"])).is_err());
    }

    #[test]
    fn router_word_is_optional() {
        let mut raw = v(&["--with-router=router --config /r.json"]);
        assert_eq!(
            take(&mut raw).unwrap().router.unwrap(),
            v(&["--config", "/r.json"])
        );
    }

    #[test]
    fn nothing_asked_for() {
        let mut raw = v(&["--hostui", "-p", "hi"]);
        let p = take(&mut raw).unwrap();
        assert!(!p.any());
        assert_eq!(raw.len(), 3);
    }

    #[test]
    fn refuses_bad_values() {
        assert!(take(&mut v(&["--with-router"])).is_err());
        assert!(take(&mut v(&["--with-supervisor="])).is_err());
        assert!(take(&mut v(&["--with-router", "check --config x"])).is_err());
        assert!(take(&mut v(&["--with-supervisor=a", "--with-supervisor=b"])).is_err());
    }

    // --- self-healing (each test uses its own part names: the status map is process-wide) ---

    use std::sync::atomic::AtomicUsize;

    fn quick() -> Backoff {
        Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(40),
            stable: Duration::from_secs(60),
        }
    }

    /// Polls `f` for up to 5 s.
    fn eventually(what: &str, f: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for: {what}");
    }

    fn state(name: &str) -> Option<PartState> {
        orca_webui::parts::get(name).map(|s| s.state)
    }

    /// A part that fails at startup N times, then runs until stopped.
    fn flaky(
        fails: usize,
        calls: Arc<AtomicUsize>,
        stopping: Arc<AtomicBool>,
    ) -> impl FnMut() -> Result<(), String> + Send + 'static {
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < fails {
                return Err(format!("missing secret (try {})", n + 1));
            }
            while !stopping.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    }

    #[test]
    fn a_part_that_fails_then_succeeds_is_restarted_and_reported() {
        let stopping = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let h = spawn_part(
            "t-flaky",
            quick(),
            stopping.clone(),
            flaky(3, calls.clone(), stopping.clone()),
        );
        eventually("the fourth start", || calls.load(Ordering::SeqCst) == 4);
        eventually("running", || state("t-flaky") == Some(PartState::Running));
        let st = orca_webui::parts::get("t-flaky").unwrap();
        assert_eq!(st.restarts, 3);
        assert_eq!(st.last_error.as_deref(), Some("missing secret (try 3)"));
        assert!(st.last_error_at.is_some());
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert_eq!(state("t-flaky"), Some(PartState::Stopped));
        // A clean stop on SIGTERM is not a restart.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_panicking_part_is_caught_and_restarted() {
        let stopping = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (c, s) = (calls.clone(), stopping.clone());
        let h = spawn_part("t-panic", quick(), stopping.clone(), move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            let s = s.clone();
            // Through the same runtime wrapper the real parts use.
            on_runtime("t-panic", async move {
                if n == 0 {
                    panic!("boom in the part");
                }
                while !s.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(())
            })
        });
        eventually("restarted after the panic", || {
            calls.load(Ordering::SeqCst) == 2 && state("t-panic") == Some(PartState::Running)
        });
        let st = orca_webui::parts::get("t-panic").unwrap();
        assert_eq!(st.restarts, 1);
        assert_eq!(st.last_error.as_deref(), Some("panicked: boom in the part"));
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
    }

    #[test]
    fn a_panic_outside_the_runtime_wrapper_is_caught_too() {
        let stopping = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let h = spawn_part("t-rawpanic", quick(), stopping.clone(), move || {
            c.fetch_add(1, Ordering::SeqCst);
            panic!("{}", String::from("owned message"));
        });
        eventually("a few restarts", || calls.load(Ordering::SeqCst) >= 3);
        let st = orca_webui::parts::get("t-rawpanic").unwrap();
        assert_eq!(st.last_error.as_deref(), Some("panicked: owned message"));
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert_eq!(state("t-rawpanic"), Some(PartState::Stopped));
    }

    #[test]
    fn a_failing_part_does_not_affect_another() {
        let stopping = Arc::new(AtomicBool::new(false));
        let ticks = Arc::new(AtomicUsize::new(0));
        let (t, s) = (ticks.clone(), stopping.clone());
        let healthy = spawn_part("t-healthy", quick(), stopping.clone(), move || {
            while !s.load(Ordering::SeqCst) {
                t.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        });
        let fails = Arc::new(AtomicUsize::new(0));
        let f = fails.clone();
        let broken = spawn_part("t-broken", quick(), stopping.clone(), move || {
            f.fetch_add(1, Ordering::SeqCst);
            Err("exit code 1".into())
        });
        eventually("the broken part failing repeatedly", || {
            fails.load(Ordering::SeqCst) >= 4
        });
        let before = ticks.load(Ordering::SeqCst);
        eventually("the healthy part still working", || {
            ticks.load(Ordering::SeqCst) > before + 5
        });
        let h = orca_webui::parts::get("t-healthy").unwrap();
        assert_eq!(
            (h.state, h.restarts, h.last_error),
            (PartState::Running, 0, None)
        );
        let b = orca_webui::parts::get("t-broken").unwrap();
        assert!(b.restarts >= 3);
        stopping.store(true, Ordering::SeqCst);
        healthy.join().unwrap();
        broken.join().unwrap();
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let stopping = Arc::new(AtomicBool::new(false));
        let times = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let t = times.clone();
        let backoff = Backoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(80),
            stable: Duration::from_secs(60),
        };
        let h = spawn_part("t-backoff", backoff, stopping.clone(), move || {
            t.lock().unwrap().push(Instant::now());
            Err("down".into())
        });
        eventually("six starts", || times.lock().unwrap().len() >= 6);
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
        let t = times.lock().unwrap();
        let gaps: Vec<u128> = t.windows(2).map(|w| (w[1] - w[0]).as_millis()).collect();
        // 20, 40, 80, 80, 80 ms (and never shorter than the wait asked for).
        for (gap, want) in gaps.iter().zip([20u128, 40, 80, 80, 80]) {
            assert!(*gap >= want, "gaps {gaps:?}");
            assert!(*gap < want + 500, "gaps {gaps:?}");
        }
    }

    #[test]
    fn a_long_run_resets_the_backoff() {
        let stopping = Arc::new(AtomicBool::new(false));
        let times = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let t = times.clone();
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_secs(10),
            stable: Duration::from_millis(100),
        };
        let h = spawn_part("t-reset", backoff, stopping.clone(), move || {
            let n = {
                let mut t = t.lock().unwrap();
                t.push(Instant::now());
                t.len()
            };
            // Starts 1-4 fail at once (waits 10, 20, 40, 80 ms); start 5 runs "stably" then
            // fails, so the next wait is back to 10 ms, not 160 ms.
            if n == 5 {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err("down".into())
        });
        eventually("seven starts", || times.lock().unwrap().len() >= 7);
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
        let t = times.lock().unwrap();
        let after_stable = (t[5] - t[4]).as_millis() - 150;
        assert!(
            after_stable < 120,
            "wait after a stable run: {after_stable} ms"
        );
    }

    #[test]
    fn stopping_during_the_backoff_ends_the_loop_without_a_restart() {
        let stopping = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let backoff = Backoff {
            initial: Duration::from_secs(30),
            ..quick()
        };
        let h = spawn_part("t-stop", backoff, stopping.clone(), move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(()) // "stopped" before SIGTERM was seen
        });
        eventually("waiting to restart", || {
            state("t-stop") == Some(PartState::Restarting)
        });
        assert!(
            orca_webui::parts::get("t-stop")
                .unwrap()
                .next_start_at
                .is_some()
        );
        let at = Instant::now();
        stopping.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert!(at.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let st = orca_webui::parts::get("t-stop").unwrap();
        assert_eq!(st.state, PartState::Stopped);
        assert_eq!(st.last_error.as_deref(), Some("stopped unexpectedly"));
    }

    #[test]
    fn a_restart_gets_its_port_back() {
        // The failed run's listener is closed with its runtime, so the next run can bind the
        // same port (the router and supervisor feed after a crash).
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        for _ in 0..3 {
            let r = on_runtime("t-port", async move {
                let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .map_err(|e| format!("bind: {e}"))?;
                // A task still holding the listener when the run fails.
                tokio::spawn(async move {
                    let _l = l;
                    std::future::pending::<()>().await;
                });
                Err("failed after binding".to_string())
            });
            assert_eq!(r, Err("failed after binding".to_string()));
        }
    }
}
