//! The `!command` shortcut (`CommandHandler.ExecuteBashShortcutAsync`) and `TerminalText.Sanitize`.

use std::path::Path;
use std::time::Duration;

use crate::ui::Console;
use crate::ui::markup::escape;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CHARS: usize = 30_000;

/// `TerminalText.Sanitize`: escape sequences out, CRLF to LF, a carriage return keeps the last
/// non-empty segment, tabs become four spaces, backspace erases, other control characters go.
pub fn sanitize(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let re = regex::Regex::new(
        r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1b[@-Z\\-_]|\u{9b}[0-?]*[ -/]*[@-~]",
    )
    .expect("static pattern");
    let text = re.replace_all(text, "").replace("\r\n", "\n");
    let is_bad = |c: char| c != '\n' && (c.is_control() || c == '\u{2028}' || c == '\u{2029}');
    if !text.chars().any(is_bad) {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    for raw in text.split('\n') {
        let line = if raw.contains('\r') {
            raw.split('\r').rev().find(|s| !s.is_empty()).unwrap_or("")
        } else {
            raw
        };
        for c in line.chars() {
            if c == '\t' {
                out.push_str("    ");
            } else if c == '\u{8}' {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.pop();
                }
            } else if !is_bad(c) {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out.pop();
    out
}

fn truncate(text: String) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > MAX_CHARS {
        format!(
            "{}\n... (truncated)",
            String::from_utf16_lossy(&units[..MAX_CHARS])
        )
    } else {
        text
    }
}

/// The lines printed for a finished shortcut.
pub fn result_lines(command: &str, stdout: &str, stderr: &str, exit_code: i32) -> Vec<String> {
    let mut out = vec![format!("  [cyan]●[/] [bold]$[/] {}", escape(command))];
    let mut lines = Vec::new();
    if !stdout.trim().is_empty() {
        lines.extend(
            stdout
                .trim_end()
                .split('\n')
                .map(|l| format!("[dim]{}[/]", escape(l))),
        );
    }
    if !stderr.trim().is_empty() {
        lines.extend(
            stderr
                .trim_end()
                .split('\n')
                .map(|l| format!("[red]{}[/]", escape(l))),
        );
    }
    if lines.is_empty() {
        lines.push("[dim](no output)[/]".into());
    }
    if exit_code != 0 {
        lines.push(format!("[red]✗ exit code {exit_code}[/]"));
    }
    for (i, l) in lines.into_iter().enumerate() {
        out.push(format!(
            "{}{l}",
            if i == 0 { "  [dim]⎿[/]  " } else { "     " }
        ));
    }
    out
}

/// Run `command` (Windows: `cmd /d /s /c "command"`; else bash reading it from stdin).
pub async fn run_shortcut(console: &Console, command: &str, cwd: &Path) {
    use tokio::io::AsyncWriteExt;
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.raw_arg_compat(format!("/d /s /c \"{command}\""));
        c
    } else {
        tokio::process::Command::new("/bin/bash")
    };
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            console.markup_line("[red]Failed to start shell process.[/]");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if !cfg!(windows) {
            let _ = stdin.write_all(format!("{command}\n").as_bytes()).await;
        }
        drop(stdin);
    }
    let output = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            console.markup_line(&format!(
                "[red]Failed to run command: {}[/]",
                escape(&e.to_string())
            ));
            return;
        }
        Err(_) => {
            console.markup_line("[yellow]Command timed out (120s).[/]");
            return;
        }
    };
    let stdout = truncate(sanitize(&orca_tools_common::decode_text(&output.stdout)));
    let stderr = truncate(sanitize(&orca_tools_common::decode_text(&output.stderr)));
    let code = output.status.code().unwrap_or(-1);
    for l in result_lines(command, &stdout, &stderr, code) {
        console.markup_line(&l);
    }
}

/// `raw_arg` exists only on Windows; elsewhere the shortcut does not use it.
trait RawArgCompat {
    fn raw_arg_compat(&mut self, arg: String) -> &mut Self;
}

impl RawArgCompat for tokio::process::Command {
    #[cfg(windows)]
    fn raw_arg_compat(&mut self, arg: String) -> &mut Self {
        self.raw_arg(arg)
    }
    #[cfg(not(windows))]
    fn raw_arg_compat(&mut self, arg: String) -> &mut Self {
        self.arg(arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize("a\r\nb"), "a\nb");
        assert_eq!(sanitize("50%\r100%"), "100%");
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("ab\u{8}c"), "ac");
        assert_eq!(sanitize("x\u{7}y"), "xy");
    }

    #[test]
    fn result_lines_show_output_and_exit_code() {
        let lines: Vec<String> = result_lines("ls", "a\nb\n", "", 0)
            .iter()
            .map(|l| crate::ui::markup::remove(l))
            .collect();
        assert_eq!(lines, ["  ● $ ls", "  ⎿  a", "     b"]);
        let failed: Vec<String> = result_lines("x", "", "", 2)
            .iter()
            .map(|l| crate::ui::markup::remove(l))
            .collect();
        assert_eq!(failed, ["  ● $ x", "  ⎿  (no output)", "     ✗ exit code 2"]);
    }
}
