//! `BackgroundProcessManager` / `ManagedProcess`: shell commands whose output is captured
//! line by line (stderr lines prefixed `[stderr] `) and kept running past a tool timeout.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::watch;

const MAX_LINES: usize = 1000;

#[derive(Default)]
struct Output {
    lines: VecDeque<String>,
    total: usize,
}

pub struct ManagedProcess {
    pub id: String,
    pub command: String,
    pub working_directory: PathBuf,
    pub started: Instant,
    pub started_at: SystemTime,
    /// Whose it is: the tool registry (one per web UI session) that started it; `None`: the
    /// process's own (terminal, `--print`). Tools only see their own scope's processes (D140).
    pub scope: Option<Arc<str>>,
    pid: Option<u32>,
    output: Mutex<Output>,
    exit: watch::Receiver<Option<i32>>,
    readers_done: watch::Receiver<usize>,
    kill: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl ManagedProcess {
    fn append(&self, line: String) {
        let mut o = self.output.lock().unwrap();
        o.lines.push_back(line);
        o.total += 1;
        if o.lines.len() > MAX_LINES {
            o.lines.pop_front();
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit.borrow()
    }

    pub fn total_lines_captured(&self) -> usize {
        self.output.lock().unwrap().total
    }

    /// The last `count` lines kept.
    pub fn tail_lines(&self, count: usize) -> Vec<String> {
        let o = self.output.lock().unwrap();
        let start = o.lines.len().saturating_sub(count);
        o.lines.iter().skip(start).cloned().collect()
    }

    /// Lines appended since `cursor` (a running total), and the new cursor.
    pub fn new_lines(&self, cursor: usize) -> (Vec<String>, usize) {
        let o = self.output.lock().unwrap();
        if cursor >= o.total {
            return (Vec::new(), cursor);
        }
        let available = (o.total - cursor).min(o.lines.len());
        (
            o.lines
                .iter()
                .skip(o.lines.len() - available)
                .cloned()
                .collect(),
            o.total,
        )
    }

    /// Waits up to `timeout` for the process to exit.
    pub async fn wait_for_exit(&self, timeout: Duration) -> bool {
        let mut rx = self.exit.clone();
        tokio::time::timeout(timeout, rx.wait_for(|e| e.is_some()))
            .await
            .is_ok()
    }

    /// After exit, wait (briefly) until stdout and stderr hit EOF so the last lines are in.
    pub async fn wait_for_output(&self, timeout: Duration) {
        let mut rx = self.readers_done.clone();
        let _ = tokio::time::timeout(timeout, rx.wait_for(|n| *n >= 2)).await;
    }

    /// Kill the process and everything it started.
    pub fn kill(&self) {
        if self.has_exited() {
            return;
        }
        if let Some(pid) = self.pid {
            kill_tree(pid);
        }
        if let Some(tx) = self.kill.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    // The shell leads its own process group (see `start`), so this reaches its children too.
    let _ = std::process::Command::new("kill")
        .args(["-9", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

static PROCESSES: LazyLock<Mutex<HashMap<String, Arc<ManagedProcess>>>> =
    LazyLock::new(Default::default);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn new_id() -> String {
    // C# uses the first 4 hex digits of a GUID; mix the clock with a counter for the same shape.
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mixed = (nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0x2545_F491_4F6C_DD1D);
    format!("{:04x}", (mixed >> 48) & 0xFFFF)
}

/// Start `command` in `working_directory`: cmd.exe (with `@echo off`) on Windows, /bin/bash
/// elsewhere, the command written to stdin to avoid argument quoting problems.
pub fn start(command: &str, working_directory: &Path) -> std::io::Result<Arc<ManagedProcess>> {
    start_in(command, working_directory, None)
}

/// [`start`] in a scope (see [`ManagedProcess::scope`]).
pub fn start_in(
    command: &str,
    working_directory: &Path,
    scope: Option<Arc<str>>,
) -> std::io::Result<Arc<ManagedProcess>> {
    let windows = cfg!(windows);
    let mut cmd = tokio::process::Command::new(if windows { "cmd.exe" } else { "/bin/bash" });
    cmd.current_dir(working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn()?;

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let script = if windows {
        format!("@echo off\r\n{command}\r\n")
    } else {
        format!("{command}\n")
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (exit_tx, exit_rx) = watch::channel(None);
    let (done_tx, done_rx) = watch::channel(0usize);
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    let mut id = new_id();
    while PROCESSES.lock().unwrap().contains_key(&id) {
        id = new_id();
    }
    let managed = Arc::new(ManagedProcess {
        id: id.clone(),
        command: command.to_string(),
        working_directory: working_directory.to_path_buf(),
        started: Instant::now(),
        started_at: SystemTime::now(),
        scope,
        pid: child.id(),
        output: Mutex::new(Output::default()),
        exit: exit_rx,
        readers_done: done_rx,
        kill: Mutex::new(Some(kill_tx)),
    });

    tokio::spawn(async move {
        let _ = stdin.write_all(script.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let done_tx = Arc::new(done_tx);
    spawn_reader(stdout, managed.clone(), "", done_tx.clone());
    spawn_reader(stderr, managed.clone(), "[stderr] ", done_tx);
    tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = kill_rx => {
                let _ = child.start_kill();
                child.wait().await.ok()
            }
        };
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        let _ = exit_tx.send(Some(code));
    });

    PROCESSES.lock().unwrap().insert(id, managed.clone());
    Ok(managed)
}

fn spawn_reader(
    stream: impl AsyncRead + Unpin + Send + 'static,
    managed: Arc<ManagedProcess>,
    prefix: &'static str,
    done: Arc<watch::Sender<usize>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    managed.append(format!("{prefix}{}", String::from_utf8_lossy(&buf)));
                }
            }
        }
        done.send_modify(|n| *n += 1);
    });
}

pub fn get(id: &str) -> Option<Arc<ManagedProcess>> {
    get_in(id, None)
}

pub fn list_all() -> Vec<Arc<ManagedProcess>> {
    list_in(None)
}

pub fn stop(id: &str) -> bool {
    stop_in(id, None)
}

/// A process of `scope` (another scope's is "not found").
pub fn get_in(id: &str, scope: Option<&str>) -> Option<Arc<ManagedProcess>> {
    cleanup_exited();
    PROCESSES
        .lock()
        .unwrap()
        .get(id)
        .filter(|p| p.scope.as_deref() == scope)
        .cloned()
}

/// The processes of `scope`.
pub fn list_in(scope: Option<&str>) -> Vec<Arc<ManagedProcess>> {
    cleanup_exited();
    PROCESSES
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.scope.as_deref() == scope)
        .cloned()
        .collect()
}

/// Kill a process of `scope`.
pub fn stop_in(id: &str, scope: Option<&str>) -> bool {
    match get_in(id, scope) {
        Some(p) => {
            p.kill();
            true
        }
        None => false,
    }
}

/// Kill every process still running (the C# manager does this on process exit).
pub fn kill_all() {
    for p in PROCESSES.lock().unwrap().values() {
        p.kill();
    }
}

static LAST_CLEANUP: Mutex<Option<Instant>> = Mutex::new(None);

fn cleanup_exited() {
    {
        let mut last = LAST_CLEANUP.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some(Instant::now());
    }
    PROCESSES.lock().unwrap().retain(|_, p| !p.has_exited());
}
