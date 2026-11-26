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
//! config. If a part stops with a non-zero code (a bad config, a port in use, a panic), the
//! whole process exits with that code, so the container's restart policy restarts all of them;
//! a part that stops cleanly (SIGTERM) is only logged. SIGTERM also stops the web UI (it only
//! listens for Ctrl-C itself), after the other parts have had a moment to finish.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::Duration;

pub const ROUTER_FLAG: &str = "--with-router";
pub const SUPERVISOR_FLAG: &str = "--with-supervisor";

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
            Some((f, v)) if f == ROUTER_FLAG || f == SUPERVISOR_FLAG => (f.to_string(), Some(v)),
            _ if a == ROUTER_FLAG || a == SUPERVISOR_FLAG => (a.clone(), None),
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
        if flag == ROUTER_FLAG {
            if parts.router.is_some() {
                return Err(format!("{ROUTER_FLAG} given twice"));
            }
            let words: Vec<String> = value.split_whitespace().map(str::to_string).collect();
            // `router` itself is optional, like the image's abhoth-router.
            let words = match words.first().map(String::as_str) {
                Some("router") => words[1..].to_vec(),
                _ => words,
            };
            // Only the gateway runs alongside the web UI, not a one-shot subcommand.
            if let Some(w) = words.first().filter(|w| !w.starts_with('-')) {
                return Err(format!(
                    "{ROUTER_FLAG}: '{w}' is a router subcommand; give the gateway's flags (--config ...)"
                ));
            }
            parts.router = Some(words);
        } else {
            if parts.supervisor.is_some() {
                return Err(format!("{SUPERVISOR_FLAG} given twice"));
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
}

/// Starts the parts (each on its own thread and runtime). Call from the web UI's runtime.
pub fn start(parts: &Parts) -> Running {
    let mut threads = Vec::new();
    #[cfg(feature = "router")]
    if let Some(args) = parts.router.clone() {
        threads.push(spawn_part("router", move || {
            let rt = match runtime("abhoth-router") {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("abhoth router: runtime: {e}");
                    return 2;
                }
            };
            match rt.block_on(orca_router::cli::run(args)) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("abhoth router: {e:#}");
                    2
                }
            }
        }));
    }
    #[cfg(not(feature = "router"))]
    if parts.router.is_some() {
        threads.push(spawn_part("router", || {
            eprintln!("abhoth: this build has no router (feature `router`)");
            2
        }));
    }
    #[cfg(feature = "supervisor")]
    if let Some(config) = parts.supervisor.clone() {
        threads.push(spawn_part("supervisor", move || {
            let rt = match runtime("abhoth-supervisor") {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("abhoth supervisor: runtime: {e}");
                    return 1;
                }
            };
            let args = ["run".to_string(), "--config".to_string(), config];
            rt.block_on(orca_supervisor::run::main(&args))
        }));
    }
    #[cfg(not(feature = "supervisor"))]
    if parts.supervisor.is_some() {
        threads.push(spawn_part("supervisor", || {
            eprintln!("abhoth: this build has no supervisor (feature `supervisor`)");
            2
        }));
    }
    let running = Running {
        threads: Arc::new(Mutex::new(threads)),
    };
    running.stop_on_sigterm();
    running
}

impl Running {
    /// SIGTERM (docker stop): the router and supervisor stop on it themselves; wait up to 10 s
    /// for them, then stop the process (the web UI listens for Ctrl-C only).
    fn stop_on_sigterm(&self) {
        #[cfg(unix)]
        {
            let threads = self.threads.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let Ok(mut term) = signal(SignalKind::terminate()) else {
                    return;
                };
                term.recv().await;
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

#[allow(dead_code)] // unused when neither the router nor the supervisor feature is on
fn runtime(name: &str) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(name)
        .build()
}

fn spawn_part(name: &'static str, body: impl FnOnce() -> i32 + Send + 'static) -> JoinHandle<()> {
    let thread = std::thread::Builder::new()
        .name(format!("abhoth-{name}"))
        .spawn(move || {
            let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(101);
            if code == 0 {
                tracing::info!("the {name} stopped");
                eprintln!("abhoth: the {name} stopped");
            } else {
                // One part down means the container restarts all of them (D148).
                tracing::error!("the {name} stopped with exit code {code}; exiting");
                eprintln!(
                    "abhoth: the {name} stopped with exit code {code}; exiting so all parts restart"
                );
                orca_tools_shell::process::kill_all();
                std::process::exit(code);
            }
        });
    match thread {
        Ok(t) => t,
        Err(e) => {
            eprintln!("abhoth: could not start the {name}: {e}");
            std::process::exit(2);
        }
    }
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
}
