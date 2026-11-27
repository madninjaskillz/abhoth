//! Supervises engines that run as host processes (pip, uv, binary, git, existing programs):
//! restart on exit with backoff, captured and rotated logs, stop by exact PID, and a memory
//! limit where the OS allows one (ROUTER-MEDIA-PLAN.md 13.3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::render::Limits;
use crate::state::{Phase, State};

use super::proc;

/// Restart policy.
#[derive(Debug, Clone)]
pub struct Policy {
    pub first_backoff: Duration,
    pub max_backoff: Duration,
    /// A run this long resets the backoff and the restart window.
    pub healthy_after: Duration,
    /// At most this many restarts within `window`, then `Failed`.
    pub max_restarts: u32,
    pub window: Duration,
    /// Rotate the log at this size (one old file kept as `.1`).
    pub log_max_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            first_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            healthy_after: Duration::from_secs(600),
            max_restarts: 5,
            window: Duration::from_secs(600),
            log_max_bytes: 8 * 1024 * 1024,
        }
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub struct ProcSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub limits: Limits,
    /// Applies the memory limit (Linux `prlimit --as`); false on GPU cards, where CUDA reserves
    /// far more address space than it uses.
    pub limit_memory: bool,
    pub log: PathBuf,
}

struct Entry {
    state: Arc<Mutex<State>>,
    pid: Arc<Mutex<Option<(u32, u64)>>>,
    stop: watch::Sender<Option<Duration>>,
    done: watch::Receiver<bool>,
}

#[derive(Default, Clone)]
pub struct Supervisor {
    procs: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    policy: Arc<Policy>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Supervisor {
    pub fn new(policy: Policy) -> Supervisor {
        Supervisor {
            procs: Default::default(),
            policy: Arc::new(policy),
        }
    }

    /// Start supervising. Returns the first PID and its start time once it has started.
    pub async fn spawn(&self, spec: ProcSpec) -> Result<(u32, u64), String> {
        if let Some(e) = self.procs.lock().expect("lock").get(&spec.name)
            && !*e.done.borrow()
        {
            return Err(format!("{} is already running", spec.name));
        }
        let state = Arc::new(Mutex::new(State::of(Phase::Starting)));
        let pid = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = watch::channel(None);
        let (done_tx, done_rx) = watch::channel(false);
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let entry = Arc::new(Entry {
            state: state.clone(),
            pid: pid.clone(),
            stop: stop_tx,
            done: done_rx,
        });
        self.procs
            .lock()
            .expect("lock")
            .insert(spec.name.clone(), entry);
        let policy = self.policy.clone();
        tokio::spawn(run_loop(
            spec, policy, state, pid, stop_rx, done_tx, first_tx,
        ));
        match first_rx.await {
            Ok(r) => r,
            Err(_) => Err("the supervisor stopped before the process started".into()),
        }
    }

    pub fn state(&self, name: &str) -> Option<State> {
        self.procs
            .lock()
            .expect("lock")
            .get(name)
            .map(|e| e.state.lock().expect("lock").clone())
    }

    pub fn pid(&self, name: &str) -> Option<(u32, u64)> {
        self.procs
            .lock()
            .expect("lock")
            .get(name)
            .and_then(|e| *e.pid.lock().expect("lock"))
    }

    pub fn supervises(&self, name: &str) -> bool {
        self.procs
            .lock()
            .expect("lock")
            .get(name)
            .is_some_and(|e| !*e.done.borrow())
    }

    /// Stop: SIGTERM, then kill after `grace`. Waits until the loop has ended.
    pub async fn stop(&self, name: &str, grace: Duration) -> bool {
        let e = self.procs.lock().expect("lock").get(name).cloned();
        let Some(e) = e else { return false };
        let _ = e.stop.send(Some(grace));
        let mut done = e.done.clone();
        let _ = tokio::time::timeout(grace + Duration::from_secs(10), async {
            while !*done.borrow() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        true
    }
}

/// The argv with the memory limit applied where the OS allows it.
fn wrapped(spec: &ProcSpec, notes: &mut Vec<String>) -> (String, Vec<String>) {
    let mut program = spec.program.clone();
    let mut args = spec.args.clone();
    if let Some(g) = spec.limits.mem_gib {
        if cfg!(target_os = "linux")
            && spec.limit_memory
            && crate::host::find_on_path("prlimit").is_some()
        {
            let bytes = (g * 1024.0 * 1024.0 * 1024.0) as u64;
            let mut a = vec![format!("--as={bytes}"), "--".to_string(), program];
            a.extend(args);
            program = "prlimit".into();
            args = a;
        } else {
            notes.push(format!(
                "the {g} GiB memory limit is not applied here ({})",
                if cfg!(windows) {
                    "Windows job objects are not used yet"
                } else if !spec.limit_memory {
                    "GPU card"
                } else {
                    "no prlimit"
                }
            ));
        }
    }
    if spec.limits.cpus.is_some() {
        notes.push("the CPU limit applies to containers only".into());
    }
    (program, args)
}

async fn run_loop(
    spec: ProcSpec,
    policy: Arc<Policy>,
    state: Arc<Mutex<State>>,
    pid: Arc<Mutex<Option<(u32, u64)>>>,
    mut stop: watch::Receiver<Option<Duration>>,
    done: watch::Sender<bool>,
    first: tokio::sync::oneshot::Sender<Result<(u32, u64), String>>,
) {
    let mut first = Some(first);
    let mut backoff = policy.first_backoff;
    let mut restarts_in_window: Vec<tokio::time::Instant> = vec![];
    let mut total_restarts = 0u32;
    loop {
        let mut notes = vec![];
        let (program, args) = wrapped(&spec, &mut notes);
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(c) = &spec.cwd {
            cmd.current_dir(c);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let started_at = tokio::time::Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{program}: {e}");
                append_log(
                    &spec.log,
                    &format!("[abhoth] cannot start: {msg}\n"),
                    policy.log_max_bytes,
                )
                .await;
                {
                    let mut s = state.lock().expect("lock");
                    s.phase = Phase::Failed;
                    s.notes = vec![msg.clone()];
                }
                if let Some(f) = first.take() {
                    let _ = f.send(Err(msg));
                }
                let _ = done.send(true);
                return;
            }
        };
        let id = child.id().unwrap_or(0);
        let st = proc::start_time(id).await.unwrap_or(0);
        *pid.lock().expect("lock") = Some((id, st));
        {
            let mut s = state.lock().expect("lock");
            s.phase = Phase::Running;
            s.pid = Some(id);
            s.since = Some(now());
            s.restarts = total_restarts;
            s.notes = notes;
        }
        append_log(
            &spec.log,
            &format!("[abhoth] started {} (pid {id})\n", spec.name),
            policy.log_max_bytes,
        )
        .await;
        if let Some(f) = first.take() {
            let _ = f.send(Ok((id, st)));
        }
        let out_task = child
            .stdout
            .take()
            .map(|o| tokio::spawn(pump(o, spec.log.clone(), policy.log_max_bytes)));
        let err_task = child
            .stderr
            .take()
            .map(|o| tokio::spawn(pump(o, spec.log.clone(), policy.log_max_bytes)));
        let exit = tokio::select! {
            r = child.wait() => Some(r),
            _ = stop.changed() => None,
        };
        let code = match exit {
            Some(r) => r.ok().and_then(|s| s.code()),
            None => {
                // asked to stop
                let grace = (*stop.borrow()).unwrap_or(Duration::from_secs(10));
                proc::terminate(id).await;
                let code = match tokio::time::timeout(grace, child.wait()).await {
                    Ok(r) => r.ok().and_then(|s| s.code()),
                    Err(_) => {
                        let _ = child.kill().await;
                        child.wait().await.ok().and_then(|s| s.code())
                    }
                };
                for t in [out_task, err_task].into_iter().flatten() {
                    let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
                }
                append_log(
                    &spec.log,
                    &format!("[abhoth] stopped {}\n", spec.name),
                    policy.log_max_bytes,
                )
                .await;
                let mut s = state.lock().expect("lock");
                s.phase = Phase::Stopped;
                s.pid = None;
                s.last_exit = code;
                *pid.lock().expect("lock") = None;
                drop(s);
                let _ = done.send(true);
                return;
            }
        };
        for t in [out_task, err_task].into_iter().flatten() {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
        *pid.lock().expect("lock") = None;
        append_log(
            &spec.log,
            &format!(
                "[abhoth] {} exited ({})\n",
                spec.name,
                code.map(|c| c.to_string()).unwrap_or("signal".into())
            ),
            policy.log_max_bytes,
        )
        .await;
        let ran = started_at.elapsed();
        if ran >= policy.healthy_after {
            backoff = policy.first_backoff;
            restarts_in_window.clear();
        }
        let t = tokio::time::Instant::now();
        restarts_in_window.retain(|x| t.duration_since(*x) < policy.window);
        if restarts_in_window.len() as u32 >= policy.max_restarts {
            let mut s = state.lock().expect("lock");
            s.phase = Phase::Failed;
            s.pid = None;
            s.last_exit = code;
            s.notes.push(format!(
                "exited {} times within {} s; not restarted again",
                policy.max_restarts + 1,
                policy.window.as_secs()
            ));
            drop(s);
            let _ = done.send(true);
            return;
        }
        restarts_in_window.push(t);
        total_restarts += 1;
        {
            let mut s = state.lock().expect("lock");
            s.phase = Phase::Backoff;
            s.pid = None;
            s.last_exit = code;
            s.restarts = total_restarts;
        }
        let wait = backoff;
        backoff = (backoff * 2).min(policy.max_backoff);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => {
                let mut s = state.lock().expect("lock");
                s.phase = Phase::Stopped;
                drop(s);
                let _ = done.send(true);
                return;
            }
        }
    }
}

async fn pump<R: AsyncRead + Unpin>(mut r: R, log: PathBuf, max: u64) {
    let mut buf = vec![0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                append_log(&log, &text, max).await;
            }
        }
    }
}

static LOG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Append to a log, rotating it to `.1` when it passes `max`. Secrets are scrubbed on write.
pub async fn append_log(path: &Path, text: &str, max: u64) {
    let _g = LOG_LOCK.lock().await;
    if let Some(p) = path.parent() {
        let _ = tokio::fs::create_dir_all(p).await;
    }
    if let Ok(m) = tokio::fs::metadata(path).await
        && m.len() > max
    {
        let old = path.with_extension("log.1");
        let _ = tokio::fs::rename(path, old).await;
    }
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        let _ = f.write_all(crate::exec::scrub(text).as_bytes()).await;
    }
}

/// The last `lines` lines of a log.
pub fn tail(path: &Path, lines: u32) -> String {
    let t = std::fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = t.lines().collect();
    let n = lines as usize;
    v[v.len().saturating_sub(n)..].join("\n")
}
