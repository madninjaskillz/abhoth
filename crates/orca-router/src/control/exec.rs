//! Executors (ROUTER-CONFIG.md 6.1): run a command or a hostctl builtin on a server, with a
//! timeout, captured and capped output (scrubbed of every loaded secret value), the exit code,
//! the duration and cancellation.
//!
//! - [`LocalExecutor`]: `/bin/sh -c <text>` on this machine; builtins run in-process.
//! - [`super::ssh::SshExecutor`]: the same request framed as JSON over a forced-command SSH key
//!   to `abhoth hostctl serve` on the host (strict mode sends template ids, never text).
//! - [`FakeExecutor`]: records requests and answers from a script (tests).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

/// Output cap per stream (64 KiB), as hostctl applies on the host.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// What to run.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// A command template. `id` names it in the host's allowlist; `text` is the rendered command
    /// (run as is by local and trusted executors, shown in plans and logs); `params` are the
    /// runtime values a strict host validates and renders into its own copy of the template.
    Command {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        params: BTreeMap<String, String>,
    },
    /// A hostctl builtin (`flock`, `gpu-free-wait`, `numa-room`, `json-set`, `restore-file`,
    /// `oom-kills`, `proc-scan`, `secret-put`, `docker-rm-robust`, `telemetry`, …).
    Builtin { name: String, args: Value },
    /// A call to the host daemon (`abhoth hostd`, SUPERVISOR.md: telemetry, fans, clocks),
    /// carried by the same channel as commands: the forced command hands it to the daemon's
    /// socket. `method` is one of the daemon's own allowlisted methods.
    Hostd { method: String, params: Value },
}

impl Op {
    pub fn label(&self) -> &str {
        match self {
            Op::Command { id, .. } => id,
            Op::Builtin { name, .. } => name,
            Op::Hostd { method, .. } => method,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ExecRequest {
    pub server: String,
    #[serde(flatten)]
    pub op: Op,
    #[serde(with = "ms")]
    pub timeout: Duration,
    /// Run through the host's single sudo entry (allowlist entries marked `root = true`).
    #[serde(default)]
    pub root: bool,
    /// Bytes for the command's stdin (`secret-put` gets the key this way, never in argv).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<Vec<u8>>,
}

impl ExecRequest {
    pub fn command(server: &str, id: &str, text: impl Into<String>, timeout: Duration) -> Self {
        Self {
            server: server.into(),
            op: Op::Command {
                id: id.into(),
                text: text.into(),
                params: BTreeMap::new(),
            },
            timeout,
            root: false,
            stdin: None,
        }
    }
    pub fn builtin(server: &str, name: &str, args: Value, timeout: Duration) -> Self {
        Self {
            server: server.into(),
            op: Op::Builtin {
                name: name.into(),
                args,
            },
            timeout,
            root: false,
            stdin: None,
        }
    }
    pub fn as_root(mut self) -> Self {
        self.root = true;
        self
    }
}

mod ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ExecOutput {
    /// `None` when killed (timeout, cancel, signal).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Either stream was cut at [`OUTPUT_CAP`].
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    pub duration_ms: u64,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0)
    }
    pub fn ok_with(stdout: &str) -> Self {
        Self {
            exit_code: Some(0),
            stdout: stdout.into(),
            ..Default::default()
        }
    }
    pub fn fail(code: i32, stderr: &str) -> Self {
        Self {
            exit_code: Some(code),
            stderr: stderr.into(),
            ..Default::default()
        }
    }
    /// One line for job steps and errors: exit status plus the tail of stderr (or stdout).
    pub fn summary(&self) -> String {
        let status = match (self.exit_code, self.timed_out, self.cancelled) {
            (_, true, _) => "timed out".to_string(),
            (_, _, true) => "cancelled".to_string(),
            (Some(c), _, _) => format!("exit {c}"),
            (None, _, _) => "killed".to_string(),
        };
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let tail: Vec<&str> = text.trim().lines().rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        if tail.is_empty() {
            status
        } else {
            format!("{status}: {}", tail.join(" | "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ExecError {
    /// The server could not be reached (SSH connect, auth, host key mismatch).
    Unreachable(String),
    /// The host refused the request (not in the allowlist, bad variable value, stale text).
    Refused(String),
    /// Spawning or I/O failed locally.
    Io(String),
    Cancelled,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Unreachable(m) => write!(f, "server unreachable: {m}"),
            ExecError::Refused(m) => write!(f, "refused by host: {m}"),
            ExecError::Io(m) => write!(f, "exec failed: {m}"),
            ExecError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for ExecError {}

/// A cancellation flag that wakes waiters.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<(AtomicBool, Notify)>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }
    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        loop {
            let n = self.0.1.notified();
            if self.is_cancelled() {
                return;
            }
            n.await;
        }
    }
}

/// Runs requests on one or more servers. Implementations must enforce `timeout`, honour
/// `cancel`, cap output at [`OUTPUT_CAP`] and scrub secret values.
#[async_trait]
pub trait Executor: Send + Sync {
    async fn run(&self, req: ExecRequest, cancel: &Cancel) -> Result<ExecOutput, ExecError>;
}

// ── secret scrubbing ─────────────────────────────────────────────────────────────────────────

/// Replaces every loaded secret value in captured text with `***`.
#[derive(Debug, Clone, Default)]
pub struct Scrubber {
    values: Arc<Vec<String>>,
}

impl Scrubber {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        // Longest first, so a secret containing another is replaced whole. Very short values
        // would mangle ordinary text; they are not secrets worth the name.
        let mut v: Vec<String> = values
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= 6)
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v.dedup();
        Self {
            values: Arc::new(v),
        }
    }
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in self.values.iter() {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "***");
            }
        }
        out
    }
    pub fn scrub_output(&self, mut o: ExecOutput) -> ExecOutput {
        o.stdout = self.scrub(&o.stdout);
        o.stderr = self.scrub(&o.stderr);
        o
    }
}

/// Cap a captured stream at [`OUTPUT_CAP`] bytes on a char boundary.
pub fn cap(bytes: &[u8]) -> (String, bool) {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= OUTPUT_CAP {
        return (s.into_owned(), false);
    }
    let mut end = OUTPUT_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

// ── local ────────────────────────────────────────────────────────────────────────────────────

/// Runs commands on this machine with `/bin/sh -c`. Builtins run in-process through
/// [`super::builtins`]. `root` requests are refused unless the process already is root (the
/// router never escalates by itself).
pub struct LocalExecutor {
    pub scrubber: Scrubber,
    pub shell: String,
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self {
            scrubber: Scrubber::default(),
            shell: "sh".into(),
        }
    }
}

impl LocalExecutor {
    pub fn new(scrubber: Scrubber) -> Self {
        Self {
            scrubber,
            ..Default::default()
        }
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn run(&self, req: ExecRequest, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
        if cancel.is_cancelled() {
            return Err(ExecError::Cancelled);
        }
        let out = match &req.op {
            Op::Command { text, .. } if cfg!(windows) && !has_program(&self.shell) => {
                // A Windows PC without `sh` (the desktop app's own server, D155): a plain
                // command line runs directly; anything needing a shell is refused.
                let (cwd, argv) = simple_argv(text).ok_or_else(|| {
                    ExecError::Refused(format!(
                        "this command needs a POSIX shell, and there is no `{}` here: {text}",
                        self.shell
                    ))
                })?;
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| ExecError::Refused("an empty command".into()))?;
                run_program_in(
                    program,
                    args,
                    cwd.as_deref(),
                    req.stdin.as_deref(),
                    req.timeout,
                    cancel,
                )
                .await?
            }
            Op::Command { text, .. } => {
                run_shell(&self.shell, text, req.stdin.as_deref(), req.timeout, cancel).await?
            }
            Op::Builtin { name, args } => {
                super::builtins::run(name, args, req.stdin.as_deref(), req.timeout, cancel).await?
            }
            Op::Hostd { method, .. } => {
                return Err(ExecError::Refused(format!(
                    "hostd call {method}: no host daemon on the local executor yet"
                )));
            }
        };
        Ok(self.scrubber.scrub_output(out))
    }
}

/// Spawn `<shell> -c text`, feed stdin, wait with timeout and cancellation; on either, kill
/// the whole process group.
pub async fn run_shell(
    shell: &str,
    text: &str,
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    run_program(
        shell,
        &["-c".to_string(), text.to_string()],
        stdin,
        timeout,
        cancel,
    )
    .await
}

/// Spawn `program args…` without a shell (builtins whose argv is fixed: `nvidia-smi`, so they
/// also work on a host with no `sh`, a Windows PC), with the same timeout, cancel and capture
/// as [`run_shell`].
pub async fn run_program(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    run_program_in(program, args, None, stdin, timeout, cancel).await
}

/// Whether `name` is on the PATH (or is a path that exists).
pub fn has_program(name: &str) -> bool {
    if std::path::Path::new(name).is_absolute() {
        return std::path::Path::new(name).exists();
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| exts.iter().any(|e| d.join(format!("{name}{e}")).is_file()))
    })
}

/// A command line with no shell features (pipes, redirects, `$`, globs, `;`, `&&` other than a
/// leading `cd DIR &&`) as `(cwd, argv)`, with POSIX quoting undone. `None`: it needs a shell.
pub fn simple_argv(text: &str) -> Option<(Option<String>, Vec<String>)> {
    let words = split_words(text)?;
    let mut cwd = None;
    let mut w: &[Word] = &words;
    if w.len() >= 3 && w[0] == Word::Plain("cd".into()) && w[2] == Word::Op("&&".into()) {
        let Word::Plain(d) = &w[1] else { return None };
        cwd = Some(d.clone());
        w = &w[3..];
    }
    let mut argv = Vec::new();
    for x in w {
        match x {
            Word::Plain(s) => argv.push(s.clone()),
            Word::Op(_) => return None,
        }
    }
    (!argv.is_empty()).then_some((cwd, argv))
}

#[derive(Debug, PartialEq)]
enum Word {
    Plain(String),
    Op(String),
}

fn split_words(text: &str) -> Option<Vec<Word>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if in_word {
                    out.push(Word::Plain(std::mem::take(&mut cur)));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        x => cur.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            x @ ('"' | '\\' | '$' | '`') => cur.push(x),
                            x => {
                                cur.push('\\');
                                cur.push(x);
                            }
                        },
                        '$' | '`' => return None,
                        x => cur.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(it.next()?);
            }
            '&' if it.peek() == Some(&'&') && !in_word => {
                it.next();
                out.push(Word::Op("&&".into()));
            }
            '#' | '~' if in_word => cur.push(c),
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | '~' | '#' => {
                return None;
            }
            x => {
                in_word = true;
                cur.push(x);
            }
        }
    }
    if in_word {
        out.push(Word::Plain(cur));
    }
    Some(out)
}

/// [`run_program`] in a working folder.
pub async fn run_program_in(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    use tokio::io::AsyncWriteExt;
    let start = Instant::now();
    let shell = program;
    let mut cmd = tokio::process::Command::new(program);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    #[cfg(windows)]
    {
        // no console window flashing up from the desktop app
        cmd.creation_flags(0x0800_0000);
    }
    cmd.args(args)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecError::Io(format!("spawn {shell}: {e}")))?;
    let pid = child.id();
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        tokio::spawn(async move {
            let _ = si.write_all(&data).await;
            let _ = si.shutdown().await;
        });
    }
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let read_out = tokio::spawn(async move { read_capped(so.as_mut()).await });
    let read_err = tokio::spawn(async move { read_capped(se.as_mut()).await });

    enum End {
        Exit(std::io::Result<std::process::ExitStatus>),
        Timeout,
        Cancel,
    }
    let end = tokio::select! {
        r = child.wait() => End::Exit(r),
        _ = tokio::time::sleep(timeout) => End::Timeout,
        _ = cancel.cancelled() => End::Cancel,
    };
    let mut out = ExecOutput::default();
    match end {
        End::Exit(r) => {
            let st = r.map_err(|e| ExecError::Io(e.to_string()))?;
            out.exit_code = st.code();
        }
        End::Timeout | End::Cancel => {
            kill_group(pid);
            let _ = child.kill().await;
            let _ = child.wait().await;
            out.timed_out = matches!(end, End::Timeout);
            out.cancelled = matches!(end, End::Cancel);
        }
    }
    // After a kill, a grandchild that escaped the group could hold the pipes open: take what
    // was read within a second and stop waiting.
    let killed = out.timed_out || out.cancelled;
    let collect = |h: tokio::task::JoinHandle<(String, bool)>| async move {
        if killed {
            let abort = h.abort_handle();
            match tokio::time::timeout(Duration::from_secs(1), h).await {
                Ok(r) => r.unwrap_or_default(),
                Err(_) => {
                    abort.abort();
                    (String::new(), false)
                }
            }
        } else {
            h.await.unwrap_or_default()
        }
    };
    let (o, t1) = collect(read_out).await;
    let (e, t2) = collect(read_err).await;
    out.stdout = o;
    out.stderr = e;
    out.truncated = t1 || t2;
    out.duration_ms = start.elapsed().as_millis() as u64;
    Ok(out)
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: Option<&mut R>) -> (String, bool) {
    use tokio::io::AsyncReadExt;
    let Some(r) = r else {
        return (String::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut over = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < OUTPUT_CAP + 4 {
                    buf.extend_from_slice(&chunk[..n]);
                } else {
                    over = true; // keep draining so the child never blocks on a full pipe
                }
            }
        }
    }
    let (s, t) = cap(&buf);
    (s, t || over)
}

#[cfg(test)]
mod simple_argv_tests {
    use super::simple_argv;

    #[test]
    fn plain_command_lines_split_and_shell_features_are_refused() {
        assert_eq!(
            simple_argv("docker pull 'img:1' --quiet"),
            Some((
                None,
                vec![
                    "docker".into(),
                    "pull".into(),
                    "img:1".into(),
                    "--quiet".into()
                ]
            ))
        );
        assert_eq!(
            simple_argv("cd '/a b' && docker compose up -d"),
            Some((
                Some("/a b".into()),
                vec!["docker".into(), "compose".into(), "up".into(), "-d".into()]
            ))
        );
        assert_eq!(
            simple_argv(r#"echo "a\"b" x#y"#).unwrap().1,
            vec!["echo", "a\"b", "x#y"]
        );
        for bad in [
            "a | b",
            "a > f",
            "a && b",
            "echo $HOME",
            "ls *",
            "a; b",
            "echo `x`",
            "x 'open",
        ] {
            assert_eq!(simple_argv(bad), None, "{bad}");
        }
    }
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(p) = pid {
        // `-s KILL -- -<pgid>`: without `--`, procps reads `-<pgid>` as an option and the
        // group (the shell's children holding the pipes) survives.
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{p}")])
            .status();
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

// ── fake ─────────────────────────────────────────────────────────────────────────────────────

/// A scripted answer: requests whose label (template id or builtin name) starts with
/// `prefix` get the next response in `responses` (the last one repeats).
#[derive(Debug, Clone)]
pub struct FakeRule {
    pub prefix: String,
    pub responses: Vec<Result<ExecOutput, ExecError>>,
    pub delay: Duration,
    used: usize,
}

/// Records every request and answers from rules (first matching prefix wins); unmatched
/// requests succeed with empty output. Optionally delegates to a handler (a fake host model).
#[derive(Default, Clone)]
pub struct FakeExecutor {
    inner: Arc<Mutex<FakeInner>>,
}

type Handler = Arc<dyn Fn(&ExecRequest) -> Option<Result<ExecOutput, ExecError>> + Send + Sync>;

#[derive(Default)]
struct FakeInner {
    rules: Vec<FakeRule>,
    log: Vec<ExecRequest>,
    handler: Option<Handler>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self::default()
    }
    /// Answer requests starting with `prefix` with these outputs in turn.
    pub fn on(&self, prefix: &str, responses: Vec<Result<ExecOutput, ExecError>>) -> &Self {
        self.on_delayed(prefix, responses, Duration::ZERO)
    }
    pub fn on_delayed(
        &self,
        prefix: &str,
        responses: Vec<Result<ExecOutput, ExecError>>,
        delay: Duration,
    ) -> &Self {
        self.inner.lock().unwrap().rules.push(FakeRule {
            prefix: prefix.into(),
            responses,
            delay,
            used: 0,
        });
        self
    }
    /// A host model consulted before the rules.
    pub fn with_handler(
        &self,
        h: impl Fn(&ExecRequest) -> Option<Result<ExecOutput, ExecError>> + Send + Sync + 'static,
    ) -> &Self {
        self.inner.lock().unwrap().handler = Some(Arc::new(h));
        self
    }
    pub fn requests(&self) -> Vec<ExecRequest> {
        self.inner.lock().unwrap().log.clone()
    }
    /// Labels of every request so far, in order.
    pub fn labels(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r.op.label().to_string())
            .collect()
    }
    pub fn clear(&self) {
        self.inner.lock().unwrap().log.clear();
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn run(&self, req: ExecRequest, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
        if cancel.is_cancelled() {
            return Err(ExecError::Cancelled);
        }
        let (resp, delay, handler) = {
            let mut g = self.inner.lock().unwrap();
            g.log.push(req.clone());
            let handler = g.handler.clone();
            let label = req.op.label().to_string();
            let mut found = None;
            for r in g.rules.iter_mut() {
                if label.starts_with(&r.prefix) && !r.responses.is_empty() {
                    let i = r.used.min(r.responses.len() - 1);
                    r.used += 1;
                    found = Some((r.responses[i].clone(), r.delay));
                    break;
                }
            }
            match found {
                Some((resp, d)) => (Some(resp), d, handler),
                None => (None, Duration::ZERO, handler),
            }
        };
        if let Some(h) = handler
            && resp.is_none()
            && let Some(r) = h(&req)
        {
            return r;
        }
        if !delay.is_zero() {
            if delay >= req.timeout {
                tokio::select! {
                    _ = tokio::time::sleep(req.timeout) => {}
                    _ = cancel.cancelled() => return Err(ExecError::Cancelled),
                }
                return Ok(ExecOutput {
                    timed_out: true,
                    duration_ms: req.timeout.as_millis() as u64,
                    ..Default::default()
                });
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => return Err(ExecError::Cancelled),
            }
        }
        resp.unwrap_or_else(|| Ok(ExecOutput::ok_with("")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubber_replaces_longest_first() {
        let s = Scrubber::new(vec![
            "secret-value-1".into(),
            "secret-value-12345".into(),
            "ab".into(),
        ]);
        assert_eq!(
            s.scrub("k=secret-value-12345 x=secret-value-1 ab"),
            "k=*** x=*** ab"
        );
    }

    #[test]
    fn cap_cuts_on_char_boundary() {
        let big = "é".repeat(OUTPUT_CAP);
        let (s, t) = cap(big.as_bytes());
        assert!(t);
        assert!(s.len() <= OUTPUT_CAP);
    }

    #[test]
    fn summary_reads_well() {
        assert_eq!(
            ExecOutput::fail(2, "a\nb\nc\nd").summary(),
            "exit 2: b | c | d"
        );
        assert_eq!(
            ExecOutput {
                timed_out: true,
                ..Default::default()
            }
            .summary(),
            "timed out"
        );
    }

    #[tokio::test]
    async fn fake_scripts_and_records() {
        let f = FakeExecutor::new();
        f.on(
            "docker-run",
            vec![
                Ok(ExecOutput::fail(1, "boom")),
                Ok(ExecOutput::ok_with("id")),
            ],
        );
        let c = Cancel::new();
        let r = |id: &str| ExecRequest::command("s", id, "x", Duration::from_secs(1));
        assert_eq!(
            f.run(r("docker-run:A"), &c).await.unwrap().exit_code,
            Some(1)
        );
        assert!(f.run(r("docker-run:A"), &c).await.unwrap().ok());
        assert!(f.run(r("docker-run:A"), &c).await.unwrap().ok());
        assert!(f.run(r("other"), &c).await.unwrap().ok());
        assert_eq!(
            f.labels(),
            vec!["docker-run:A", "docker-run:A", "docker-run:A", "other"]
        );
    }

    #[tokio::test]
    async fn fake_hang_times_out_and_cancels() {
        let f = FakeExecutor::new();
        f.on_delayed(
            "hang",
            vec![Ok(ExecOutput::ok_with(""))],
            Duration::from_secs(60),
        );
        let c = Cancel::new();
        let o = f
            .run(
                ExecRequest::command("s", "hang", "x", Duration::from_millis(20)),
                &c,
            )
            .await
            .unwrap();
        assert!(o.timed_out);
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            c2.cancel();
        });
        let e = f
            .run(
                ExecRequest::command("s", "hang", "x", Duration::from_secs(30)),
                &c,
            )
            .await;
        assert_eq!(e, Err(ExecError::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_runs_times_out_and_scrubs() {
        let x = LocalExecutor::new(Scrubber::new(vec!["topsecret99".into()]));
        let c = Cancel::new();
        let o = x
            .run(
                ExecRequest::command(
                    "l",
                    "t",
                    "echo hi topsecret99; echo err >&2; exit 3",
                    Duration::from_secs(5),
                ),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(o.exit_code, Some(3));
        assert_eq!(o.stdout.trim(), "hi ***");
        assert_eq!(o.stderr.trim(), "err");
        let o = x
            .run(
                ExecRequest::command("l", "t", "sleep 5", Duration::from_millis(100)),
                &c,
            )
            .await
            .unwrap();
        assert!(o.timed_out);
        assert!(o.duration_ms < 3000);
    }
}
