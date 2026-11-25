//! Shell tools (port of `OpenOrca.Tools.Shell`): `bash`, `get_process_output`,
//! `stop_process`. `start_background_process` and `list_processes` are phase 2.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{append_line, get_i32_lenient, keep_head_and_tail, required_str};
use serde_json::Value;

pub mod process;

/// Every tool in this crate.
pub fn all_tools(idle_timeout_seconds: i32) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(BashTool {
            idle_timeout_seconds,
        }),
        Arc::new(GetProcessOutputTool),
        Arc::new(StopProcessTool),
    ]
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

/// Head + tail cap on command output, under the pipeline's 30k cap.
const MAX_OUTPUT_CHARS: usize = 28_000;

pub struct BashTool {
    /// `shell.idleTimeoutSeconds`: how long to wait before handing back a process ID.
    pub idle_timeout_seconds: i32,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: 120,
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
        let managed = match process::start(command, &work_dir) {
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

pub struct GetProcessOutputTool;

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
            match process::list_all().into_iter().max_by_key(|p| p.started) {
                Some(p) => id = p.id.clone(),
                None => return ToolResult::error("No background processes found."),
            }
        }
        let Some(p) = process::get(&id) else {
            return ToolResult::error(format!(
                "No process found with ID \"{id}\". Use start_background_process first."
            ));
        };
        let lines = p.tail_lines(tail);
        let status = match p.exit_code() {
            Some(code) => format!("Exited with code {code}"),
            None => "Running".into(),
        };
        let secs = p.started.elapsed().as_secs();
        let mut sb = String::new();
        append_line(&mut sb, &format!("Process {}: {}", p.id, p.command));
        append_line(&mut sb, &format!("Status: {status}"));
        append_line(
            &mut sb,
            &format!(
                "Uptime: {:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            ),
        );
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

pub struct StopProcessTool;

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
        let Some(p) = process::get(id) else {
            return ToolResult::error(format!("No process found with ID \"{id}\"."));
        };
        let was_running = !p.has_exited();
        process::stop(id);
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
