//! Shell tools (port of `OpenOrca.Tools.Shell`): `bash`, `get_process_output`,
//! `list_processes`, `start_background_process`, `stop_process`.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{append_line, get_i32_lenient, keep_head_and_tail, required_str};
use serde_json::Value;

pub mod process;

/// Every tool in this crate, in the C# registration order (class names sorted).
pub fn all_tools(idle_timeout_seconds: i32) -> Vec<Arc<dyn Tool>> {
    all_tools_in(idle_timeout_seconds, None)
}

/// [`all_tools`] whose processes belong to `scope` (one per web UI session, D140): they list,
/// read and stop only that scope's processes.
pub fn all_tools_in(idle_timeout_seconds: i32, scope: Option<Arc<str>>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(BashTool {
            idle_timeout_seconds,
            scope: scope.clone(),
        }),
        Arc::new(GetProcessOutputTool {
            scope: scope.clone(),
        }),
        Arc::new(ListProcessesTool {
            scope: scope.clone(),
        }),
        Arc::new(StartBackgroundProcessTool {
            scope: scope.clone(),
        }),
        Arc::new(StopProcessTool { scope }),
    ]
}

/// `TimeSpan` formatted `hh\:mm\:ss` (the hours component wraps at a day, as in .NET).
fn uptime(started: std::time::Instant) -> String {
    let secs = started.elapsed().as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

/// Head + tail cap on command output, under the pipeline's 30k cap.
const MAX_OUTPUT_CHARS: usize = 28_000;

pub struct BashTool {
    /// `shell.idleTimeoutSeconds`: how long to wait before handing back a process ID.
    pub idle_timeout_seconds: i32,
    /// See [`all_tools_in`].
    pub scope: Option<Arc<str>>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: 120,
            scope: None,
        }
    }
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Uses cmd.exe on Windows and /bin/bash on Unix. Output streams to the user in real-time. Waits up to timeout_seconds (default 120s) for completion \u{2014} if the command is still running, returns the process ID so you can check on it with get_process_output or stop it with stop_process. Output is truncated at 30,000 chars."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Dangerous
    }

    // The C# tool declares no capabilities: its time limit is its own (timeout_seconds), and it
    // returns a process ID instead of being cut off by the pipeline timeout.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::LONG_RUNNING
    }

    fn permission_arg_name(&self) -> &str {
        "command"
    }

    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4bb}",
            short_description: "Run shell command",
            summary_arg_keys: &["command"],
            aliases: &["run_command", "execute", "shell"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to execute"
            },
            "working_directory": {
                "type": "string",
                "description": "Working directory for the command. Defaults to current directory."
            },
            "timeout_seconds": {
                "type": "integer",
                "description": "Max seconds to wait for the command to complete (default 120). If exceeded, the process continues in background and its ID is returned."
            },
            "description": {
                "type": "string",
                "description": "Human-readable description of what this command does (for logging/display only)"
            }
        },
        "required": ["command"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        self.execute_streaming(args, ctx, &|_| {}).await
    }

    async fn execute_streaming(
        &self,
        args: &Value,
        ctx: &ToolContext,
        on_output: &(dyn for<'s> Fn(&'s str) + Send + Sync),
    ) -> ToolResult {
        let command = match required_str(args, "command") {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e),
        };
        let work_dir = args
            .get("working_directory")
            .and_then(Value::as_str)
            .unwrap_or(".");
        let timeout =
            get_i32_lenient(args.get("timeout_seconds"), self.idle_timeout_seconds).max(0);
        let work_dir = ctx.full_path(work_dir);
        if !work_dir.is_dir() {
            return ToolResult::error(format!(
                "Working directory not found: {}",
                work_dir.display()
            ));
        }
        let managed = match process::start_in(command, &work_dir, self.scope.clone()) {
            Ok(m) => m,
            Err(e) => return ToolResult::error(format!("Failed to execute command: {e}")),
        };

        let mut cursor = 0usize;
        let flush = |cursor: &mut usize| {
            let (lines, next) = managed.new_lines(*cursor);
            *cursor = next;
            for l in lines {
                on_output(&l);
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
        let mut exited = false;
        loop {
            flush(&mut cursor);
            if managed.has_exited() {
                exited = true;
                break;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            if managed
                .wait_for_exit((deadline - now).min(Duration::from_millis(100)))
                .await
            {
                exited = true;
                break;
            }
        }

        if exited {
            // Let the last lines arrive (C# waits a fixed 150 ms; here: until both pipes close).
            managed.wait_for_output(Duration::from_millis(500)).await;
            flush(&mut cursor);
            let output = keep_head_and_tail(
                &managed.tail_lines(cursor).join("\n"),
                MAX_OUTPUT_CHARS,
                None,
            );
            let code = managed.exit_code().unwrap_or(0);
            let text = format!(
                "Working directory: {}\nExit code: {code}\n\n{output}",
                managed.working_directory.display()
            );
            return if code == 0 {
                ToolResult::success(text)
            } else {
                ToolResult::error(text)
            };
        }

        flush(&mut cursor);
        let output = keep_head_and_tail(
            &managed.tail_lines(cursor).join("\n"),
            MAX_OUTPUT_CHARS,
            None,
        );
        let id = &managed.id;
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!("Command is still running in the background (process ID: \"{id}\")."),
        );
        append_line(
            &mut sb,
            &format!("Working directory: {}", managed.working_directory.display()),
        );
        append_line(&mut sb, &format!("Elapsed: {timeout}s"));
        append_line(&mut sb, "");
        if !output.is_empty() {
            append_line(&mut sb, "--- Output so far ---");
            append_line(&mut sb, &output);
            append_line(&mut sb, "");
        }
        append_line(
            &mut sb,
            &format!(
                "The process is still running. Use get_process_output with process_id \"{id}\" to check for new output."
            ),
        );
        sb.push_str(&format!(
            "Use stop_process with process_id \"{id}\" to terminate it."
        ));
        ToolResult::success(sb)
    }
}

#[derive(Default)]
pub struct GetProcessOutputTool {
    pub scope: Option<Arc<str>>,
}

#[async_trait::async_trait]
impl Tool for GetProcessOutputTool {
    fn name(&self) -> &str {
        "get_process_output"
    }

    fn description(&self) -> &str {
        "Get recent output from a background process started with start_background_process. Returns the last N lines of stdout/stderr, process status, and uptime."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4bb}",
            short_description: "Get process output",
            summary_arg_keys: &["pid"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "process_id": {
                "type": "string",
                "description": "The process ID returned by start_background_process"
            },
            "tail_lines": {
                "type": "integer",
                "description": "Number of recent output lines to return. Defaults to 50."
            }
        },
        "required": ["process_id"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let mut id = match required_str(args, "process_id") {
            Ok(i) => i.to_string(),
            Err(e) => return ToolResult::error(e),
        };
        let tail = get_i32_lenient(args.get("tail_lines"), 50).max(0) as usize;
        if id.eq_ignore_ascii_case("last") || id.eq_ignore_ascii_case("latest") {
            match process::list_in(self.scope.as_deref())
                .into_iter()
                .max_by_key(|p| p.started)
            {
                Some(p) => id = p.id.clone(),
                None => return ToolResult::error("No background processes found."),
            }
        }
        let Some(p) = process::get_in(&id, self.scope.as_deref()) else {
            return ToolResult::error(format!(
                "No process found with ID \"{id}\". Use start_background_process first."
            ));
        };
        let lines = p.tail_lines(tail);
        let status = match p.exit_code() {
            Some(code) => format!("Exited with code {code}"),
            None => "Running".into(),
        };
        let mut sb = String::new();
        append_line(&mut sb, &format!("Process {}: {}", p.id, p.command));
        append_line(&mut sb, &format!("Status: {status}"));
        append_line(&mut sb, &format!("Uptime: {}", uptime(p.started)));
        append_line(
            &mut sb,
            &format!("Total lines captured: {}", p.total_lines_captured()),
        );
        append_line(&mut sb, &format!("--- Last {} lines ---", lines.len()));
        for l in &lines {
            append_line(&mut sb, l);
        }
        ToolResult::success(sb)
    }
}

#[derive(Default)]
pub struct StopProcessTool {
    pub scope: Option<Arc<str>>,
}

#[async_trait::async_trait]
impl Tool for StopProcessTool {
    fn name(&self) -> &str {
        "stop_process"
    }

    fn description(&self) -> &str {
        "Stop a background process by its ID. Kills the entire process tree and returns the final output. Use this to clean up servers and watchers when done."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4bb}",
            short_description: "Stop process",
            summary_arg_keys: &["pid"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "process_id": {
                "type": "string",
                "description": "The process ID returned by start_background_process"
            }
        },
        "required": ["process_id"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let id = match required_str(args, "process_id") {
            Ok(i) => i,
            Err(e) => return ToolResult::error(e),
        };
        let Some(p) = process::get_in(id, self.scope.as_deref()) else {
            return ToolResult::error(format!("No process found with ID \"{id}\"."));
        };
        let was_running = !p.has_exited();
        process::stop_in(id, self.scope.as_deref());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lines = p.tail_lines(20);
        let mut sb = String::new();
        append_line(
            &mut sb,
            &if was_running {
                format!("Stopped process {}: {}", p.id, p.command)
            } else {
                format!(
                    "Process {} had already exited (code {}): {}",
                    p.id,
                    p.exit_code().unwrap_or(0),
                    p.command
                )
            },
        );
        append_line(&mut sb, &format!("--- Last {} lines ---", lines.len()));
        for l in &lines {
            append_line(&mut sb, l);
        }
        ToolResult::success(sb)
    }
}

#[derive(Default)]
pub struct ListProcessesTool {
    pub scope: Option<Arc<str>>,
}

#[async_trait::async_trait]
impl Tool for ListProcessesTool {
    fn name(&self) -> &str {
        "list_processes"
    }

    fn description(&self) -> &str {
        "List all background processes and their status (running/exited), command, uptime, and process ID."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4bb}",
            short_description: "List background processes",
            summary_arg_keys: &[],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {}
    }"#,
        )
    }

    async fn execute(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
        let mut processes = process::list_in(self.scope.as_deref());
        if processes.is_empty() {
            return ToolResult::success("No background processes.");
        }
        // C# enumerates a ConcurrentDictionary (no defined order); oldest first reads best.
        processes.sort_by_key(|p| p.started);
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!("{} background process(es):", processes.len()),
        );
        append_line(&mut sb, "");
        for p in &processes {
            let status = match p.exit_code() {
                Some(code) => format!("Exited (code {code})"),
                None => "Running".into(),
            };
            append_line(&mut sb, &format!("  ID: {}", p.id));
            append_line(&mut sb, &format!("  Command: {}", p.command));
            append_line(&mut sb, &format!("  Status: {status}"));
            append_line(&mut sb, &format!("  Uptime: {}", uptime(p.started)));
            append_line(
                &mut sb,
                &format!("  Lines captured: {}", p.total_lines_captured()),
            );
            append_line(&mut sb, "");
        }
        ToolResult::success(sb)
    }
}

#[derive(Default)]
pub struct StartBackgroundProcessTool {
    pub scope: Option<Arc<str>>,
}

#[async_trait::async_trait]
impl Tool for StartBackgroundProcessTool {
    fn name(&self) -> &str {
        "start_background_process"
    }

    fn description(&self) -> &str {
        "Start a long-running command in the background (servers, watchers, build --watch). Returns a process ID. Use get_process_output to check output and stop_process to terminate. For short commands, prefer bash instead."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Dangerous
    }

    fn permission_arg_name(&self) -> &str {
        "command"
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2699}\u{fe0f}",
            short_description: "Start background process",
            summary_arg_keys: &["command"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to execute in the background"
            },
            "working_directory": {
                "type": "string",
                "description": "Working directory for the command. Defaults to current directory."
            }
        },
        "required": ["command"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let command = match required_str(args, "command") {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e),
        };
        let work_dir = args
            .get("working_directory")
            .and_then(Value::as_str)
            .unwrap_or(".");
        let work_dir = ctx.full_path(work_dir);
        if !work_dir.is_dir() {
            return ToolResult::error(format!(
                "Working directory not found: {}",
                work_dir.display()
            ));
        }

        // Relaunch guard: a model that cannot observe a GUI app will happily start the same
        // command forever, leaking a process (and window) each time.
        if let Some(existing) = process::list_in(self.scope.as_deref())
            .into_iter()
            .find(|p| !p.has_exited() && p.command == command)
        {
            let id = &existing.id;
            return ToolResult::error_with(
                format!(
                    "This exact command is ALREADY RUNNING as process \"{id}\". Do not start it again. Use get_process_output with process_id \"{id}\" to check it, or stop_process first if you changed the code and need a restart. Note: GUI apps (games, windows) produce no output to read \u{2014} verify code changes by compiling/analyzing instead, and ask the user to test the running app."
                ),
                orca_abstractions::ToolErrorCode::Blocked,
            );
        }

        match process::start_in(command, &work_dir, self.scope.clone()) {
            Ok(m) => ToolResult::success(format!(
                "Started background process {id}: {command}\nWorking directory: {}\nUse get_process_output with process_id \"{id}\" to check output.",
                work_dir.display(),
                id = m.id
            )),
            Err(e) => ToolResult::error(format!(
                "Failed to start background process: {}",
                orca_tools_common::io_message(&e)
            )),
        }
    }
}
