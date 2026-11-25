//! A bridge CLI's line-oriented stdio (`IBridgeTransport`, `ProcessBridgeTransport`) and the
//! process spec (`ProcessStartInfo`) engines hand to a transport factory.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

/// What to start (`ProcessStartInfo`): the program, its argument list, folder and environment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcessSpec {
    pub file_name: String,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
    /// The full environment for the process (`psi.Environment`); `None` inherits ours.
    pub environment: Option<HashMap<String, String>>,
}

impl ProcessSpec {
    pub fn new(file_name: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            file_name: file_name.into(),
            arguments,
            ..Default::default()
        }
    }

    /// A tokio command for this spec: no console window, killed on drop; on Unix the child
    /// leads its own process group so the whole tree can be killed.
    pub fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.file_name);
        cmd.args(&self.arguments);
        if let Some(dir) = &self.working_directory {
            cmd.current_dir(dir);
        }
        if let Some(env) = &self.environment {
            cmd.env_clear();
            cmd.envs(env);
        }
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        cmd
    }
}

/// The current process's environment as a map (`psi.Environment` before changes).
pub fn current_environment() -> HashMap<String, String> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Environment helpers with the platform's key comparison (case-insensitive on Windows, as
/// `ProcessStartInfo.Environment` is there).
pub(crate) mod env {
    use std::collections::HashMap;

    fn same(a: &str, b: &str) -> bool {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }

    pub fn remove(env: &mut HashMap<String, String>, name: &str) {
        env.retain(|k, _| !same(k, name));
    }

    pub fn get<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| same(k, name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(env: &HashMap<String, String>, name: &str) -> bool {
        get(env, name).is_some()
    }

    pub fn set(env: &mut HashMap<String, String>, name: &str, value: &str) {
        remove(env, name);
        env.insert(name.to_string(), value.to_string());
    }
}

/// A bridge CLI's stdio: JSON lines in, JSON lines out. Abstracted so tests can replay
/// recorded streams without the CLI installed. `read_line` must be cancel-safe (the engines
/// race it against cancellation and timeouts).
#[async_trait]
pub trait BridgeTransport: Send {
    /// Write one line (a JSON message) to the CLI's stdin.
    async fn write_line(&mut self, line: &str) -> std::io::Result<()>;
    /// The next stdout line, or `None` once the CLI has exited and its output is drained.
    async fn read_line(&mut self) -> Option<String>;
    /// True once the process has exited (or was killed).
    fn has_exited(&mut self) -> bool;
    /// Exit code once exited.
    fn exit_code(&mut self) -> Option<i32>;
    /// The last few KB of stderr (for error messages).
    fn stderr_tail(&self) -> String;
    /// Close stdin (the CLI ends its session at EOF).
    fn close_input(&mut self);
    /// Kill the CLI and every process it started.
    fn kill(&mut self);
    /// Close stdin, give the CLI 3 s to exit, then kill it (`DisposeAsync`).
    async fn dispose(&mut self);
}

/// Starts a transport for a process spec; the default launches a real process.
pub type BridgeTransportFactory =
    Box<dyn FnMut(&ProcessSpec) -> std::io::Result<Box<dyn BridgeTransport>> + Send>;

/// The default factory: [`ProcessBridgeTransport::start`].
pub fn process_factory() -> BridgeTransportFactory {
    Box::new(|spec| {
        ProcessBridgeTransport::start(spec).map(|t| Box::new(t) as Box<dyn BridgeTransport>)
    })
}

const STDERR_KEEP: usize = 4096;

/// A real child process. Killing it takes its whole process tree.
pub struct ProcessBridgeTransport {
    child: Child,
    pid: Option<u32>,
    stdin: Option<ChildStdin>,
    lines: mpsc::UnboundedReceiver<String>,
    stderr: Arc<Mutex<String>>,
    pumps: Vec<tokio::task::JoinHandle<()>>,
    exit: Option<i32>,
    killed: bool,
    disposed: bool,
}

impl ProcessBridgeTransport {
    pub fn start(spec: &ProcessSpec) -> std::io::Result<Self> {
        let mut cmd = spec.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr_pipe = child.stderr.take().expect("stderr is piped");

        let (tx, rx) = mpsc::unbounded_channel();
        let out_pump = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // ReadLine: the terminator (\n or \r\n) is not part of the line.
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                        }
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                        if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let err_buf = stderr.clone();
        let err_pump = tokio::spawn(async move {
            let mut pipe = stderr_pipe;
            let mut chunk = [0u8; 1024];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut s = err_buf.lock().unwrap();
                        s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if s.len() > STDERR_KEEP * 2 {
                            let mut cut = s.len() - STDERR_KEEP;
                            while !s.is_char_boundary(cut) {
                                cut += 1;
                            }
                            s.drain(..cut);
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            pid,
            stdin,
            lines: rx,
            stderr,
            pumps: vec![out_pump, err_pump],
            exit: None,
            killed: false,
            disposed: false,
        })
    }

    /// The process id (diagnostics).
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }

    fn poll_exit(&mut self) -> bool {
        if self.exit.is_some() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit = Some(status.code().unwrap_or(-1));
                true
            }
            Ok(None) => false,
            Err(_) => true,
        }
    }
}

#[async_trait]
impl BridgeTransport for ProcessBridgeTransport {
    async fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Cannot write to a closed TextWriter.",
            ));
        };
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    }

    async fn read_line(&mut self) -> Option<String> {
        self.lines.recv().await
    }

    fn has_exited(&mut self) -> bool {
        self.killed || self.poll_exit()
    }

    fn exit_code(&mut self) -> Option<i32> {
        self.poll_exit();
        self.exit
    }

    fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn close_input(&mut self) {
        self.stdin = None;
    }

    fn kill(&mut self) {
        if !self.poll_exit()
            && let Some(pid) = self.pid
        {
            kill_tree(pid);
        }
        let _ = self.child.start_kill();
        self.killed = true;
    }

    async fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.close_input();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        self.kill();
        for pump in self.pumps.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
        }
    }
}

/// Kill `pid` and every process it started.
pub(crate) fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    #[cfg(unix)]
    {
        // The child leads its own process group (see `ProcessSpec::command`).
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Why [`run_captured`] gave no result.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The process could not start.
    Start(std::io::Error),
    /// The time limit passed; the tree was killed.
    TimedOut,
    /// The caller cancelled; the tree was killed.
    Cancelled,
}

/// Run `spec` to the end with stdout and stderr captured (stdout then stderr, as the C#
/// callers concatenate them); stdin closed at once. Kills the tree on timeout or cancel.
pub(crate) async fn run_captured(
    spec: &ProcessSpec,
    limit: Duration,
    cancel: &super::CancelToken,
) -> Result<(i32, String), RunError> {
    let mut cmd = spec.command();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(RunError::Start)?;
    drop(child.stdin.take());
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let work = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (_, _, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        (
            code,
            String::from_utf8_lossy(&out).into_owned() + &String::from_utf8_lossy(&err),
        )
    };
    let outcome = tokio::select! {
        r = work => Ok(r),
        _ = tokio::time::sleep(limit) => Err(RunError::TimedOut),
        _ = cancel.cancelled() => Err(RunError::Cancelled),
    };
    if outcome.is_err()
        && let Some(pid) = pid
    {
        kill_tree(pid);
    }
    outcome
}
