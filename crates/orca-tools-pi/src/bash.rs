//! Pi's bash tool (bash.ts createShellToolDefinition, `PiBashTool`): one bash process per call,
//! stdout and stderr combined, tail-truncated to 2000 lines / 50KB with the full output spilled
//! to a temp file, "Command exited with code N" as an error result for a non-zero exit.
//! OpenOrca additions: a default and maximum timeout stated in the description,
//! `background: true` (a job for the job tool), and a warning on commands that kill processes
//! by name.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolResult, ToolRiskLevel,
};
use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::jobs::PiJobManager;
use crate::output_accumulator::OutputAccumulator;
use crate::shell::{self, PiShellConfig};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncation, format_size, js_number};
use crate::{PiToolInfo, fail, fail_with, get_bool, get_number, get_string};

/// Longest foreground timeout; longer work runs as a background job.
pub const MAX_TIMEOUT_SECONDS: i32 = 600;

/// The kill rule in `<rules>`.
pub const KILL_GUIDELINE: &str = "Stop only processes you started (job kill, or kill by PID), never by name (taskkill /IM, pkill, killall, Stop-Process -Name): that also closes the user's own copies";

/// Appended to the result of a command that kills processes by name (it still runs).
pub const KILL_BY_NAME_WARNING: &str = "Warning: this command kills processes by name, which also closes copies of that program the user has open. Stop only what you started: job kill for a background job, or kill <pid> / taskkill //PID <pid> //T.";

/// The PI_* variables a command never inherits from OpenOrca's own environment.
const PI_ENVIRONMENT_NAMES: [&str; 5] = [
    "PI_SESSION_ID",
    "PI_SESSION_FILE",
    "PI_PROVIDER",
    "PI_MODEL",
    "PI_REASONING_LEVEL",
];

pub struct PiBashTool {
    /// The bash to run. `None` resolves it per call (from `shell_path`).
    pub shell: Option<PiShellConfig>,
    /// Configured shell override (config pi.shellPath), used when `shell` is `None`.
    pub shell_path: Option<String>,
    /// Timeout in seconds for calls without a timeout argument; stated in the description.
    /// 0 = none.
    pub default_timeout_seconds: i32,
    /// PI_* session variables exposed to commands (Pi's exposeSessionEnvironment).
    pub session_environment: Vec<(String, String)>,
    /// Where `background: true` jobs go (shared with the job tool).
    pub jobs: Arc<PiJobManager>,
    description: String,
    schema_json: String,
}

impl PiBashTool {
    pub fn new(
        shell: Option<PiShellConfig>,
        shell_path: Option<String>,
        default_timeout_seconds: i32,
        session_environment: Vec<(String, String)>,
        jobs: Arc<PiJobManager>,
    ) -> Self {
        let effective = effective_default(default_timeout_seconds);
        let description = format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. {}Run builds and test suites in the foreground with a timeout that covers them; never sleep to wait. background: true starts a server, app or watcher as a job and returns its id at once; wait on it or kill it with the job tool.",
            DEFAULT_MAX_BYTES / 1024,
            match effective {
                Some(d) => format!(
                    "Timeout: {d} seconds unless you pass one (max {MAX_TIMEOUT_SECONDS}). "
                ),
                None =>
                    format!("No timeout unless you pass one (max {MAX_TIMEOUT_SECONDS} seconds). "),
            }
        );
        let schema_json = format!(
            r#"{{"type":"object","required":["command"],"properties":{{"command":{{"type":"string","description":"Shell command to execute"}},"timeout":{{"type":"number","description":"Timeout in seconds ({}max {MAX_TIMEOUT_SECONDS})"}},"background":{{"type":"boolean","description":"Run as a background job and return its id at once (servers, apps, watchers)"}}}}}}"#,
            effective
                .map(|d| format!("default {d}, "))
                .unwrap_or_default()
        );
        Self {
            shell,
            shell_path,
            default_timeout_seconds,
            session_environment,
            jobs,
            description,
            schema_json,
        }
    }

    /// A bash with the given shell and working defaults (tests).
    pub fn with_shell(shell: Option<PiShellConfig>) -> Self {
        Self::new(shell, None, 0, Vec::new(), PiJobManager::shared())
    }
}

impl Default for PiBashTool {
    fn default() -> Self {
        Self::with_shell(None)
    }
}

fn effective_default(default_timeout_seconds: i32) -> Option<i32> {
    (default_timeout_seconds > 0).then(|| default_timeout_seconds.min(MAX_TIMEOUT_SECONDS))
}

impl PiToolInfo for PiBashTool {
    fn prompt_snippet(&self) -> String {
        "Execute bash commands (ls, grep, find, etc.)".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![
            "You can inspect PI_* environment variables for current model and session details."
                .into(),
            KILL_GUIDELINE.into(),
        ]
    }
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// The status line of a foreground call that ran out of time.
pub fn timeout_message(seconds: &str) -> String {
    format!(
        "Command timed out after {seconds} seconds and was killed with its child processes; any output it printed is above. Run it again with a longer timeout (max {MAX_TIMEOUT_SECONDS}), or with background: true and then job wait."
    )
}

#[async_trait::async_trait]
impl Tool for PiBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Dangerous
    }

    fn parameter_schema(&self) -> Value {
        crate::schema(&self.schema_json)
    }

    // LongRunning: the host's fixed per-tool timeout would override the model's own timeout
    // argument; this tool enforces timeout / the default itself.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::LONG_RUNNING
    }

    fn permission_arg_name(&self) -> &str {
        "command"
    }

    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }

    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(
            self.name(),
            &self.schema_json,
            arguments_json,
        ))
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let command = get_string(args, "command").unwrap_or("");
        let timeout = get_number(args, "timeout");
        let background = get_bool(args, "background");

        let mut limit_seconds: Option<f64> = None;
        match timeout {
            Some(t) if !background => {
                if !t.is_finite() || t <= 0.0 {
                    return fail_with(
                        "Invalid timeout: must be a finite number of seconds",
                        ToolErrorCode::InvalidArguments,
                    );
                }
                limit_seconds = Some(t.min(MAX_TIMEOUT_SECONDS as f64)); // a longer ask runs to the cap
            }
            _ => {
                if let Some(d) = effective_default(self.default_timeout_seconds) {
                    limit_seconds = Some(d as f64);
                }
            }
        }
        let timeout_label = limit_seconds.map(js_number).unwrap_or_default();

        let cwd = &ctx.cwd;
        if !cwd.is_dir() {
            return fail(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd.display()
            ));
        }

        let shell = match &self.shell {
            Some(s) => s.clone(),
            None => match shell::resolve(self.shell_path.as_deref()) {
                Ok(s) => s,
                Err(e) => return fail(e),
            },
        };

        let kill_note = if kills_by_name(command) {
            format!("\n\n{KILL_BY_NAME_WARNING}")
        } else {
            String::new()
        };
        if background {
            return self
                .start_background(&shell, command, cwd, &kill_note)
                .await;
        }

        // Merge stderr into stdout inside the shell so interleaving matches what the command
        // actually wrote; two separately pumped pipes race.
        let script = format!("exec 2>&1\n{command}");
        let output = Arc::new(Mutex::new(OutputAccumulator::new("pi-bash")));
        let mut child =
            match create_shell_command(&shell, &script, cwd, &self.session_environment).spawn() {
                Ok(c) => c,
                Err(_) => return fail(format!("spawn {} ENOENT", shell.path)),
            };
        write_command_if_stdin(&mut child, &shell, &script).await;
        let pid = child.id();

        let readers = [
            child.stdout.take().map(|s| pump(s, output.clone())),
            child.stderr.take().map(|s| pump(s, output.clone())),
        ];

        let status = match limit_seconds {
            Some(secs) => tokio::time::timeout(Duration::from_secs_f64(secs), child.wait())
                .await
                .ok(),
            None => Some(child.wait().await),
        };
        let timed_out = status.is_none();
        let status = match status {
            Some(s) => s.ok(),
            None => {
                if let Some(pid) = pid {
                    kill_tree(pid);
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                None
            }
        };

        // Drain the pipes, but don't hang on a background descendant that inherited them.
        let deadline = Instant::now() + Duration::from_secs(1);
        for r in readers.into_iter().flatten() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = tokio::time::timeout(left, r).await;
        }

        let (snapshot, full_path, last_line_bytes) = {
            let mut acc = output.lock().unwrap();
            acc.finish();
            let snap = acc.snapshot();
            (snap, acc.full_output_path().cloned(), acc.last_line_bytes())
        };

        if timed_out {
            return fail_with(
                append_status(
                    &format_output(&snapshot, full_path.as_deref(), last_line_bytes, ""),
                    &timeout_message(&timeout_label),
                ) + &kill_note,
                ToolErrorCode::Timeout,
            );
        }

        let text = with_test_summary(
            format_output(
                &snapshot,
                full_path.as_deref(),
                last_line_bytes,
                "(no output)",
            ),
            &snapshot,
            full_path.as_deref(),
        );
        let Some(code) = status.and_then(exit_code) else {
            return fail(
                append_status(&text, "Command terminated without an exit code") + &kill_note,
            );
        };
        if code != 0 {
            return fail(
                append_status(&text, &format!("Command exited with code {code}")) + &kill_note,
            );
        }
        ToolResult::success(text + &kill_note)
    }
}

impl PiBashTool {
    /// `background: true`: starts the command as a job and returns its id. Waits up to a
    /// second so a command that fails at once (typo, missing file) reports its exit right away.
    async fn start_background(
        &self,
        shell: &PiShellConfig,
        command: &str,
        cwd: &Path,
        kill_note: &str,
    ) -> ToolResult {
        let job = match self
            .jobs
            .start(shell, command, cwd, &self.session_environment)
            .await
        {
            Ok(j) => j,
            Err(e) => return fail(e),
        };
        let grace = Instant::now();
        while !job.has_exited() && grace.elapsed() < Duration::from_millis(1000) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if job.has_exited() {
            let code = job.exit_code();
            let text = crate::job_tool::describe(
                &job,
                &format!(
                    "Job {} (pid {}) already exited with code {}.",
                    job.id,
                    job.pid,
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                ),
                crate::job_tool::EXIT_TAIL_LINES,
            ) + kill_note;
            return if code == Some(0) {
                ToolResult::success(text)
            } else {
                fail(text)
            };
        }
        ToolResult::success(format!(
            "Started job {} (pid {}). Output: {}\nUse job wait (until: a regex for a line such as \"Listening\") instead of sleep; job kill stops it.{kill_note}",
            job.id,
            job.pid,
            job.log_path.display()
        ))
    }
}

/// The exit code as .NET reports it (128 + signal for a signalled Unix process).
fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    if let Some(c) = status.code() {
        return Some(c);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Some(128 + sig);
        }
    }
    None
}

/// A shell process for `script` with Pi's argv rules and PI_* variables; not started.
pub(crate) fn create_shell_command(
    shell: &PiShellConfig,
    script: &str,
    cwd: &Path,
    environment: &[(String, String)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&shell.path);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    if shell.command_from_stdin {
        cmd.arg("-s");
    } else {
        #[cfg(windows)]
        {
            // Always quoted: no MSYS globbing.
            cmd.raw_arg(format!("-c {}", quote_windows_argument(script)));
        }
        #[cfg(not(windows))]
        {
            cmd.arg("-c").arg(script);
        }
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(unix)]
    cmd.process_group(0); // so the whole tree can be killed
    for name in PI_ENVIRONMENT_NAMES {
        cmd.env_remove(name);
    }
    for (name, value) in environment {
        if !value.is_empty() {
            cmd.env(name, value);
        }
    }
    cmd
}

/// Feeds the script on stdin when the shell reads it there, then closes stdin.
pub(crate) async fn write_command_if_stdin(
    child: &mut tokio::process::Child,
    shell: &PiShellConfig,
    script: &str,
) {
    if let Some(mut stdin) = child.stdin.take() {
        if shell.command_from_stdin {
            let _ = stdin.write_all(script.as_bytes()).await;
        }
        drop(stdin);
    }
}

/// Kill a process and everything it started.
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
        // The shell leads its own process group (see `create_shell_command`).
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn pump<R>(mut stream: R, output: Arc<Mutex<OutputAccumulator>>) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 16 * 1024];
        while let Ok(n) = stream.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            output.lock().unwrap().append(&buffer[..n]);
        }
    })
}

/// Quotes one argument by the Microsoft C runtime rules, which MSYS2/Cygwin bash also follows
/// for a double-quoted argument. Always quoted, so the MSYS runtime never globs it.
pub fn quote_windows_argument(arg: &str) -> String {
    let mut sb = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            sb.push_str(&"\\".repeat(backslashes * 2 + 1));
            sb.push('"');
        } else {
            sb.push_str(&"\\".repeat(backslashes));
            sb.push(c);
        }
        backslashes = 0;
    }
    sb.push_str(&"\\".repeat(backslashes * 2));
    sb.push('"');
    sb
}

static KILL_BY_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\btaskkill(\.exe)?\b[^\n;&|]*\s/{1,2}im\b",
        r"|(^|[\s;&|(`])(pkill|killall)(\.exe)?\b",
        r"|\bStop-Process\b[^\n;|]*\s-(Name|ProcessName)\b",
        r"|\bwmic\b[^\n;]*\bname\s*="
    ))
    .unwrap()
});

/// `\b(Get-Process|gps)\s+` (the start of the pipe-to-Stop-Process form).
static GET_PROCESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(Get-Process|gps)\s+").unwrap());

/// The rest of the pipe form, anchored after the whitespace.
static GET_PROCESS_REST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^['"]?[\w.*-]+['"]?[^\n;|]*\|\s*(Stop-Process|spps|kill)\b"#).unwrap()
});

static ID_PARAMETER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^-Id\b").unwrap());

/// True for commands that kill processes by name: taskkill /IM, pkill, killall,
/// Stop-Process -Name, Get-Process name | Stop-Process, wmic ... name=. (The C# pattern's
/// `(?!-Id\b)` lookahead is checked by hand: the `regex` crate has no lookaround.)
pub fn kills_by_name(command: &str) -> bool {
    if KILL_BY_NAME.is_match(command) {
        return true;
    }
    GET_PROCESS.find_iter(command).any(|m| {
        let rest = &command[m.end()..];
        !ID_PARAMETER.is_match(rest) && GET_PROCESS_REST.is_match(rest)
    })
}

/// A truncated unittest/pytest run gets a one-line digest of the whole output.
fn with_test_summary(text: String, snapshot: &Truncation, full_path: Option<&Path>) -> String {
    let Some(path) = full_path.filter(|_| snapshot.truncated) else {
        return text;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return text;
    };
    const CAP: usize = 16 << 20;
    let slice = if bytes.len() > CAP {
        &bytes[bytes.len() - CAP..]
    } else {
        &bytes[..]
    };
    let content = crate::write::decode_text(slice);
    match crate::test_summary::from(&content) {
        Some(summary) => format!("{text}\n\n{summary}"),
        None => text,
    }
}

/// Pi's formatOutput: the tail plus the truncation notice naming the full-output file.
fn format_output(
    snapshot: &Truncation,
    full_path: Option<&Path>,
    last_line_bytes: usize,
    empty_text: &str,
) -> String {
    let text = if snapshot.content.is_empty() {
        empty_text
    } else {
        &snapshot.content
    };
    if !snapshot.truncated {
        return text.to_string();
    }
    let start_line = snapshot.total_lines as i64 - snapshot.output_lines as i64 + 1;
    let end_line = snapshot.total_lines;
    let path = full_path
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if snapshot.last_line_partial {
        return format!(
            "{text}\n\n[Showing last {} of line {end_line} (line is {}). Full output: {path}]",
            format_size(snapshot.output_bytes as u64),
            format_size(last_line_bytes as u64)
        );
    }
    if snapshot.truncated_by == Some("lines") {
        return format!(
            "{text}\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {path}]",
            snapshot.total_lines
        );
    }
    format!(
        "{text}\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path}]",
        snapshot.total_lines,
        format_size(DEFAULT_MAX_BYTES as u64)
    )
}
