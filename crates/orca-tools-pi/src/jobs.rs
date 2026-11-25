//! Background jobs started by bash `background: true` (`PiJob`, `PiJobManager`): one shell
//! process each, its combined output streamed to a log file and kept as lines for `job wait`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

use crate::bash::{create_shell_command, kill_tree, write_command_if_stdin};
use crate::output_accumulator::Utf8Stream;
use crate::shell::PiShellConfig;

/// Lines kept in memory; older ones stay in the log file only.
pub const MAX_LINES_IN_MEMORY: usize = 20_000;

/// How a [`PiJobManager::wait`] call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Exited,
    Matched,
    TimedOut,
}

/// The result of waiting on a job.
#[derive(Debug, Clone)]
pub struct WaitResult {
    pub outcome: WaitOutcome,
    pub matched_line: Option<String>,
    pub matched_line_number: Option<u64>,
    pub waited: Duration,
}

struct JobState {
    log: Option<File>,
    decoder: Utf8Stream,
    partial: String,
    lines: Vec<String>,
    /// Absolute number (0-based) of `lines[0]`.
    first_line: u64,
    /// Absolute number of the first line the next wait looks at.
    cursor: u64,
    exit_code: Option<i32>,
    exited: bool,
    killed: bool,
    ended: Option<Instant>,
}

/// A background command (`PiJob`).
pub struct PiJob {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub working_directory: PathBuf,
    pub log_path: PathBuf,
    pub started: Instant,
    state: Mutex<JobState>,
    changed: watch::Sender<u64>,
}

impl PiJob {
    pub fn has_exited(&self) -> bool {
        self.state.lock().unwrap().exited
    }

    /// `None` while running, or when the exit code could not be read.
    pub fn exit_code(&self) -> Option<i32> {
        self.state.lock().unwrap().exit_code
    }

    /// True once [`PiJobManager::kill`] stopped it.
    pub fn killed(&self) -> bool {
        self.state.lock().unwrap().killed
    }

    pub fn total_lines(&self) -> u64 {
        let s = self.state.lock().unwrap();
        s.first_line + s.lines.len() as u64
    }

    pub fn runtime(&self) -> Duration {
        let ended = self.state.lock().unwrap().ended;
        ended.unwrap_or_else(Instant::now) - self.started
    }

    /// "running", "exited 0", "killed".
    pub fn status(&self) -> String {
        let s = self.state.lock().unwrap();
        if !s.exited {
            "running".into()
        } else if s.killed {
            "killed".into()
        } else {
            match s.exit_code {
                Some(c) => format!("exited {c}"),
                None => "exited".into(),
            }
        }
    }

    fn signal(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    fn append(&self, data: &[u8]) {
        {
            let mut s = self.state.lock().unwrap();
            if let Some(log) = s.log.as_mut() {
                let _ = log.write_all(data);
                let _ = log.flush();
            }
            let text = s.decoder.decode(data, false);
            append_text(&mut s, &text);
        }
        self.signal();
    }

    fn mark_exited(&self, exit_code: Option<i32>) {
        {
            let mut s = self.state.lock().unwrap();
            if s.exited {
                return;
            }
            let rest = s.decoder.decode(&[], true);
            append_text(&mut s, &rest);
            if !s.partial.is_empty() {
                let line = std::mem::take(&mut s.partial);
                add_line(&mut s, line.trim_end_matches('\r').to_string());
            }
            s.exit_code = exit_code;
            s.exited = true;
            s.ended = Some(Instant::now());
            s.log = None;
        }
        self.signal();
    }

    /// Scans lines after the wait cursor for `pattern`, moving the cursor past what it
    /// scanned. Returns the match (line and 1-based number) and whether the job has exited.
    fn poll(&self, pattern: Option<&Regex>) -> (Option<(String, u64)>, bool) {
        let mut s = self.state.lock().unwrap();
        let end = s.first_line + s.lines.len() as u64;
        if s.cursor < s.first_line {
            s.cursor = s.first_line;
        }
        if let Some(p) = pattern {
            for n in s.cursor..end {
                let line = &s.lines[(n - s.first_line) as usize];
                if p.is_match(line) {
                    let line = line.clone();
                    s.cursor = n + 1;
                    return (Some((line, n + 1)), s.exited);
                }
            }
        }
        s.cursor = end;
        (None, s.exited)
    }

    /// The whole output kept in memory (the tail when it is very long).
    pub fn output(&self) -> String {
        let s = self.state.lock().unwrap();
        let text = s.lines.join("\n");
        if s.partial.is_empty() {
            text
        } else if text.is_empty() {
            s.partial.clone()
        } else {
            format!("{text}\n{}", s.partial)
        }
    }
}

fn append_text(s: &mut JobState, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            let line = std::mem::take(&mut s.partial);
            add_line(s, line.trim_end_matches('\r').to_string());
        } else {
            s.partial.push(c);
        }
    }
}

fn add_line(s: &mut JobState, line: String) {
    s.lines.push(line);
    if s.lines.len() > MAX_LINES_IN_MEMORY {
        let drop = s.lines.len() - MAX_LINES_IN_MEMORY;
        s.lines.drain(..drop);
        s.first_line += drop as u64;
    }
}

/// Background jobs of one OpenOrca process, shared by the Lead and every sub-agent.
pub struct PiJobManager {
    jobs: Mutex<HashMap<String, Arc<PiJob>>>,
    next_id: AtomicU64,
    /// Directory for job logs (%TEMP%/openorca-jobs).
    pub log_directory: PathBuf,
}

impl Default for PiJobManager {
    fn default() -> Self {
        Self::new()
    }
}

static SHARED: LazyLock<Arc<PiJobManager>> = LazyLock::new(|| Arc::new(PiJobManager::new()));

impl PiJobManager {
    pub fn new() -> Self {
        Self::with_log_directory(crate::temp_dir().join("openorca-jobs"))
    }

    pub fn with_log_directory(dir: impl Into<PathBuf>) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            log_directory: dir.into(),
        }
    }

    /// The process-wide manager the Pi tools use by default.
    pub fn shared() -> Arc<PiJobManager> {
        SHARED.clone()
    }

    /// Jobs in start order.
    pub fn list(&self) -> Vec<Arc<PiJob>> {
        let mut v: Vec<Arc<PiJob>> = self.jobs.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|j| j.started);
        v
    }

    /// A job by id ("3", "#3", "job 3", "job-3").
    pub fn get(&self, id: Option<&str>) -> Option<Arc<PiJob>> {
        let id = id?.trim();
        if id.is_empty() {
            return None;
        }
        let key = if id.len() >= 3 && id[..3].eq_ignore_ascii_case("job") {
            id[3..].trim_start_matches([' ', '-', '_', '#'])
        } else {
            id.strip_prefix('#').unwrap_or(id)
        };
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, j)| j.clone())
    }

    /// Starts `command` in bash; its output goes to the job's log file.
    pub async fn start(
        &self,
        shell: &PiShellConfig,
        command: &str,
        cwd: &Path,
        environment: &[(String, String)],
    ) -> Result<Arc<PiJob>, String> {
        let id = (self.next_id.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        std::fs::create_dir_all(&self.log_directory).map_err(|e| e.to_string())?;
        let log_path = self
            .log_directory
            .join(format!("job-{}-{id}.log", std::process::id()));
        let log = File::create(&log_path).map_err(|e| e.to_string())?;

        let script = format!("exec 2>&1\n{command}");
        let mut child = create_shell_command(shell, &script, cwd, environment)
            .spawn()
            .map_err(|_| format!("spawn {} ENOENT", shell.path))?;
        write_command_if_stdin(&mut child, shell, &script).await;

        let (changed, _) = watch::channel(0u64);
        let job = Arc::new(PiJob {
            id: id.clone(),
            pid: child.id().unwrap_or(0),
            command: command.to_string(),
            working_directory: cwd.to_path_buf(),
            log_path,
            started: Instant::now(),
            state: Mutex::new(JobState {
                log: Some(log),
                decoder: Utf8Stream::default(),
                partial: String::new(),
                lines: Vec::new(),
                first_line: 0,
                cursor: 0,
                exit_code: None,
                exited: false,
                killed: false,
                ended: None,
            }),
            changed,
        });
        self.jobs.lock().unwrap().insert(id, job.clone());

        let monitored = job.clone();
        tokio::spawn(async move {
            let pumps = [
                child.stdout.take().map(|s| pump(s, monitored.clone())),
                child.stderr.take().map(|s| pump(s, monitored.clone())),
            ];
            let status = child.wait().await.ok();
            // A background descendant can hold the pipes open: drain briefly, then report.
            let deadline = Instant::now() + Duration::from_secs(1);
            for p in pumps.into_iter().flatten() {
                let left = deadline.saturating_duration_since(Instant::now());
                let _ = tokio::time::timeout(left, p).await;
            }
            monitored.mark_exited(status.and_then(|s| s.code()));
        });
        Ok(job)
    }

    /// Blocks until the job exits, a line after the previous wait matches `until`, or
    /// `timeout` passes.
    pub async fn wait(&self, job: &PiJob, until: Option<&Regex>, timeout: Duration) -> WaitResult {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut rx = job.changed.subscribe();
            let (hit, exited) = job.poll(until);
            if let Some((line, n)) = hit {
                return matched(line, n, started);
            }
            if exited {
                return finished(WaitOutcome::Exited, started);
            }
            if tokio::time::timeout_at(deadline, rx.changed())
                .await
                .is_err()
            {
                // One last look: output that arrived with the deadline still counts.
                let (hit, exited) = job.poll(until);
                if let Some((line, n)) = hit {
                    return matched(line, n, started);
                }
                return finished(
                    if exited {
                        WaitOutcome::Exited
                    } else {
                        WaitOutcome::TimedOut
                    },
                    started,
                );
            }
        }
    }

    /// Kills the job's whole process tree. False when it had already exited.
    pub async fn kill(&self, job: &PiJob) -> bool {
        if job.has_exited() {
            return false;
        }
        job.state.lock().unwrap().killed = true;
        kill_tree(job.pid);
        let waited = Instant::now();
        while !job.has_exited() && waited.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Kills every running job (session end).
    pub fn kill_all(&self) {
        for job in self.jobs.lock().unwrap().values() {
            if job.has_exited() {
                continue;
            }
            job.state.lock().unwrap().killed = true;
            kill_tree(job.pid);
        }
    }
}

fn matched(line: String, n: u64, started: Instant) -> WaitResult {
    WaitResult {
        outcome: WaitOutcome::Matched,
        matched_line: Some(line),
        matched_line_number: Some(n),
        waited: started.elapsed(),
    }
}

fn finished(outcome: WaitOutcome, started: Instant) -> WaitResult {
    WaitResult {
        outcome,
        matched_line: None,
        matched_line_number: None,
        waited: started.elapsed(),
    }
}

fn pump<R>(mut stream: R, job: Arc<PiJob>) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 16 * 1024];
        while let Ok(n) = stream.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            job.append(&buffer[..n]);
        }
    })
}
