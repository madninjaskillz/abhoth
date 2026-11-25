//! `HookRunner`: shell commands run before and after tool calls (`hooks.preToolHooks` /
//! `hooks.postToolHooks`, keyed by tool name or `*`).
//!
//! The command is written to the shell's stdin (`cmd.exe` with `@echo off` on Windows,
//! `/bin/bash` elsewhere) so it needs no argument quoting. The hook sees `ORCA_TOOL_NAME`,
//! `ORCA_TOOL_ARGS`, `ORCA_TOOL_ERROR` (`True`/`False`) and, after the call,
//! `ORCA_TOOL_RESULT` (at most 10,000 UTF-16 units). A pre-hook that exits non-zero blocks
//! the call. Hooks time out after 30 seconds (the process tree is killed; exit code -1).

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::config::HooksConfig;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_MAX_UTF16: usize = 10_000;

pub struct HookRunner {
    config: HooksConfig,
    pending: Mutex<JoinSet<()>>,
}

impl HookRunner {
    pub fn new(config: HooksConfig) -> Self {
        Self {
            config,
            pending: Mutex::new(JoinSet::new()),
        }
    }

    /// True when no hook at all is configured (the pipeline can skip the calls).
    pub fn is_empty(&self) -> bool {
        self.config.pre_tool_hooks.is_empty() && self.config.post_tool_hooks.is_empty()
    }

    /// `RunPreHookAsync`: true when the tool may run, false when the hook blocked it.
    pub async fn run_pre_hook(&self, tool_name: &str, args_json: &str) -> bool {
        let Some(command) = find_hook(&self.config.pre_tool_hooks, tool_name) else {
            return true;
        };
        tracing::debug!("Running pre-hook for {tool_name}: {command}");
        let code = run_hook_process(command, tool_name, args_json, None, false).await;
        if code != 0 {
            tracing::warn!("Pre-hook for {tool_name} returned exit code {code} - blocking tool");
            return false;
        }
        true
    }

    /// `RunPostHookAsync`: the outcome is only logged.
    pub async fn run_post_hook(
        &self,
        tool_name: &str,
        args_json: &str,
        result: &str,
        is_error: bool,
    ) {
        let Some(command) = find_hook(&self.config.post_tool_hooks, tool_name) else {
            return;
        };
        tracing::debug!("Running post-hook for {tool_name}: {command}");
        run_hook_process(command, tool_name, args_json, Some(result), is_error).await;
    }

    /// `RunPostHookInBackground`: run the post-hook without waiting for it; tracked so
    /// [`Self::wait_for_pending_post_hooks`] can wait for it before the process exits.
    pub fn run_post_hook_in_background(
        self: &Arc<Self>,
        tool_name: &str,
        args_json: &str,
        result: &str,
        is_error: bool,
    ) {
        if find_hook(&self.config.post_tool_hooks, tool_name).is_none() {
            return;
        }
        let (this, tool, args, result) = (
            Arc::clone(self),
            tool_name.to_string(),
            args_json.to_string(),
            result.to_string(),
        );
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // Reap finished hooks so the set does not grow over a long session.
        while pending.try_join_next().is_some() {}
        pending.spawn(async move {
            this.run_post_hook(&tool, &args, &result, is_error).await;
        });
    }

    /// `WaitForPendingPostHooksAsync`: wait (at most `timeout`) for running post-hooks.
    pub async fn wait_for_pending_post_hooks(&self, timeout: Duration) {
        let mut set = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if set.is_empty() {
            return;
        }
        let _ =
            tokio::time::timeout(timeout, async { while set.join_next().await.is_some() {} }).await;
        // Hooks still running after the timeout keep running detached (their processes are
        // not killed), as in C#.
        set.detach_all();
    }
}

/// `FindHook`: the tool's own hook, else the `*` hook.
fn find_hook<'a>(hooks: &'a crate::config::Map<String>, tool_name: &str) -> Option<&'a str> {
    hooks
        .get(tool_name)
        .or_else(|| hooks.get("*"))
        .map(String::as_str)
}

fn truncate_utf16(s: &str, max: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= max {
        s.to_string()
    } else {
        String::from_utf16_lossy(&units[..max])
    }
}

/// `RunHookProcessAsync`: the exit code, or -1 when the shell could not start or timed out.
async fn run_hook_process(
    command: &str,
    tool_name: &str,
    args_json: &str,
    result: Option<&str>,
    is_error: bool,
) -> i32 {
    let shell = if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/bash"
    };
    let mut cmd = super::hidden_command(shell);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("ORCA_TOOL_NAME", tool_name)
        .env("ORCA_TOOL_ARGS", args_json)
        .env("ORCA_TOOL_ERROR", if is_error { "True" } else { "False" });
    if let Some(r) = result {
        cmd.env("ORCA_TOOL_RESULT", truncate_utf16(r, RESULT_MAX_UTF16));
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to start hook process ({command}): {e}");
            return -1;
        }
    };
    let pid = child.id();
    let nl = if cfg!(windows) { "\r\n" } else { "\n" };
    if let Some(mut stdin) = child.stdin.take() {
        let mut script = String::new();
        if cfg!(windows) {
            script.push_str("@echo off");
            script.push_str(nl);
        }
        script.push_str(command);
        script.push_str(nl);
        let _ = stdin.write_all(script.as_bytes()).await;
        // Dropping stdin closes it.
    }
    // Drain both pipes concurrently: a chatty hook would otherwise fill a pipe and hang.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    match tokio::time::timeout(HOOK_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            let stderr = err_task.await.unwrap_or_default();
            let _ = out_task.await;
            let code = status.code().unwrap_or(-1);
            if code != 0 && !stderr.trim().is_empty() {
                let shown = if stderr.chars().count() > 2000 {
                    format!("{}...", stderr.chars().take(2000).collect::<String>())
                } else {
                    stderr
                };
                tracing::warn!("Hook stderr ({command}): {shown}");
            }
            code
        }
        Ok(Err(e)) => {
            tracing::warn!("Hook process failed ({command}): {e}");
            -1
        }
        Err(_) => {
            tracing::warn!("Hook timed out (30s): {command}");
            kill_tree(pid, &mut child).await;
            -1
        }
    }
}

async fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = super::hidden_command("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(windows))]
    let _ = pid;
    let _ = child.kill().await;
}
