//! OpenOrca.Commands.FileOps: /add, /checkpoint, /diff, /undo.

use std::path::Path;

use async_trait::async_trait;
use orca_core::engines::paths::get_relative_path;

use super::{Command, CommandContext, CommandResult, diff_markup, n0, run_shell_stdout};
use crate::ui::markup::escape;
use crate::ui::prompt;
use crate::ui::widgets::write_panel;

/// `Directory.GetFiles(dir, pattern)` matching: `*` any run, `?` one character, no case.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The files `/add` reads for one argument (a path, or a wildcard in its file name part).
pub fn add_matches(pattern: &str, cwd: &Path) -> Vec<std::path::PathBuf> {
    if pattern.contains('*') || pattern.contains('?') {
        let norm = pattern.replace('\\', "/");
        let (dir, name) = match norm.rfind('/') {
            Some(i) => (&norm[..i], &norm[i + 1..]),
            None => ("", norm.as_str()),
        };
        let dir = if dir.is_empty() {
            cwd.to_path_buf()
        } else {
            orca_abstractions::full_path(cwd, dir)
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| wildcard_match(name, &e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        out.sort();
        out
    } else {
        let full = orca_abstractions::full_path(cwd, pattern);
        if full.is_file() { vec![full] } else { Vec::new() }
    }
}

pub struct AddCommand;

#[async_trait]
impl Command for AddCommand {
    fn name(&self) -> &'static str {
        "add"
    }
    fn description(&self) -> &'static str {
        "Add file contents to conversation context"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        if args.is_empty() {
            ctx.console
                .markup_line("[yellow]Usage: /add <file1> [[file2]] ...[/]");
            return CommandResult::Ok;
        }
        let cwd = ctx.cwd();
        let cwd_s = cwd.to_string_lossy().to_string();
        let mut added = Vec::new();
        let mut parts = Vec::new();
        for pattern in args {
            let files = add_matches(pattern, &cwd);
            for file in &files {
                match std::fs::read(file) {
                    Ok(bytes) => {
                        let mut content = orca_tools_common::decode_text(&bytes);
                        let units: Vec<u16> = content.encode_utf16().collect();
                        if units.len() > 50_000 {
                            content = format!(
                                "{}\n... (truncated)",
                                String::from_utf16_lossy(&units[..50_000])
                            );
                        }
                        let rel = get_relative_path(&cwd_s, &file.to_string_lossy());
                        parts.push(format!("--- {rel} ---\n{content}"));
                        added.push(rel);
                    }
                    Err(e) => ctx.console.markup_line(&format!(
                        "[red]Failed to read {}: {}[/]",
                        escape(pattern),
                        escape(&e.to_string())
                    )),
                }
            }
            if files.is_empty() {
                ctx.console.markup_line(&format!(
                    "[yellow]No files matched: {}[/]",
                    escape(pattern)
                ));
            }
        }
        if !parts.is_empty() {
            ctx.session
                .messages
                .push(orca_core::client::ChatMessage::user(format!(
                    "Here are the contents of the requested files:\n\n{}",
                    parts.join("\n\n")
                )));
            ctx.console.markup_line(&format!(
                "[green]Added {} file(s) to context: {}[/]",
                added.len(),
                escape(&added.join(", "))
            ));
        }
        CommandResult::Ok
    }
}

pub struct CheckpointCommand;

#[async_trait]
impl Command for CheckpointCommand {
    fn name(&self) -> &'static str {
        "checkpoint"
    }
    fn description(&self) -> &'static str {
        "Manage file checkpoints (auto-saved before edits)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["cp!"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let action = args
            .first()
            .map(|a| a.to_lowercase())
            .unwrap_or_else(|| "list".into());
        let session_id = ctx
            .state
            .current_session_id
            .clone()
            .unwrap_or_else(|| "default".into());
        let cwd = ctx.cwd();
        let full = |a: &String| orca_abstractions::full_path(&cwd, a);
        match action.as_str() {
            "list" => {
                let entries = ctx.checkpoints.list(&session_id);
                if entries.is_empty() {
                    ctx.console
                        .markup_line("[grey]No file checkpoints in this session.[/]");
                    return CommandResult::Ok;
                }
                let mut table = crate::ui::widgets::Table::new()
                    .expand()
                    .column("File")
                    .column("Checkpointed At")
                    .column("Size");
                for e in entries {
                    let rel = get_relative_path(&cwd.to_string_lossy(), &e.file_path);
                    let at = chrono::DateTime::<chrono::Local>::from(e.timestamp.to_system_time())
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string();
                    table.row(&[escape(&rel), at, format!("{} bytes", n0(e.size_bytes))]);
                }
                table.write(ctx.console);
            }
            "diff" => {
                let Some(path) = args.get(1).map(full) else {
                    ctx.console
                        .markup_line("[yellow]Usage: /checkpoint diff <file>[/]");
                    return CommandResult::Ok;
                };
                match ctx.checkpoints.diff(&path, &session_id) {
                    Ok(d) if d.trim().is_empty() => {
                        ctx.console.markup_line("[grey]No differences found.[/]")
                    }
                    Ok(d) => write_panel(
                        ctx.console,
                        &diff_markup(&d),
                        "[bold]Checkpoint Diff[/]",
                        "cyan1",
                    ),
                    Err(e) => ctx
                        .console
                        .markup_line(&format!("[red]{}[/]", escape(&e.to_string()))),
                }
            }
            "restore" => {
                let Some(path) = args.get(1).map(full) else {
                    ctx.console
                        .markup_line("[yellow]Usage: /checkpoint restore <file>[/]");
                    return CommandResult::Ok;
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let choice = prompt::select(
                    ctx.console,
                    &format!("[yellow]Restore {} from checkpoint?[/]", escape(&name)),
                    &["Yes".to_string(), "No".to_string()],
                );
                if choice == Some(0) {
                    let shown = escape(&path.to_string_lossy());
                    if ctx.checkpoints.restore(&path, &session_id).unwrap_or(false) {
                        ctx.console
                            .markup_line(&format!("[green]Restored: {shown}[/]"));
                    } else {
                        ctx.console
                            .markup_line(&format!("[red]No checkpoint found for: {shown}[/]"));
                    }
                } else {
                    ctx.console.markup_line("[grey]Cancelled.[/]");
                }
            }
            "clear" => {
                let _ = ctx.checkpoints.cleanup(&session_id);
                ctx.console
                    .markup_line("[green]All checkpoints cleared for this session.[/]");
            }
            _ => ctx.console.markup_line(
                "[yellow]Usage: /checkpoint list|diff <file>|restore <file>|clear[/]",
            ),
        }
        CommandResult::Ok
    }
}

pub struct DiffCommand;

#[async_trait]
impl Command for DiffCommand {
    fn name(&self) -> &'static str {
        "diff"
    }
    fn description(&self) -> &'static str {
        "Show uncommitted git changes"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let cwd = ctx.cwd();
        let run = |c: &'static str| {
            let cwd = cwd.clone();
            async move { run_shell_stdout(c, &cwd).await }
        };
        let result: std::io::Result<()> = async {
            let staged = run("git diff --cached").await?;
            let staged_stat = run("git diff --cached --stat").await?;
            let unstaged = run("git diff").await?;
            let unstaged_stat = run("git diff --stat").await?;
            if staged.trim().is_empty() && unstaged.trim().is_empty() {
                ctx.console.markup_line("[grey]No uncommitted changes.[/]");
                return Ok(());
            }
            if !staged.trim().is_empty() {
                let d = format!("{staged_stat}\n\n{staged}");
                write_panel(
                    ctx.console,
                    &diff_markup(d.trim_end()),
                    "[bold]Staged Changes[/]",
                    "green",
                );
            }
            if !unstaged.trim().is_empty() {
                let d = format!("{unstaged_stat}\n\n{unstaged}");
                write_panel(
                    ctx.console,
                    &diff_markup(d.trim_end()),
                    "[bold]Unstaged Changes[/]",
                    "yellow",
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            ctx.console.markup_line(&format!(
                "[red]Failed to get diff: {}[/]",
                escape(&e.to_string())
            ));
        }
        CommandResult::Ok
    }
}

pub struct UndoCommand;

#[async_trait]
impl Command for UndoCommand {
    fn name(&self) -> &'static str {
        "undo"
    }
    fn description(&self) -> &'static str {
        "Undo last file edit via checkpoints"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let cwd = ctx.cwd();
        let result: std::io::Result<()> = async {
            let stat = run_shell_stdout("git diff --stat", &cwd).await?;
            let staged = run_shell_stdout("git diff --cached --stat", &cwd).await?;
            let combined = format!("{staged}\n{stat}").trim().to_string();
            if combined.is_empty() {
                ctx.console.markup_line("[grey]No changes to undo.[/]");
                return Ok(());
            }
            write_panel(
                ctx.console,
                &escape(&combined),
                "[yellow]Changes to undo[/]",
                "yellow",
            );
            let choices = [
                "Revert all (git checkout . && git reset HEAD)".to_string(),
                "Stash changes (git stash)".to_string(),
                "Cancel".to_string(),
            ];
            match prompt::select(
                ctx.console,
                "[yellow]How would you like to undo these changes?[/]",
                &choices,
            ) {
                Some(0) => {
                    run_shell_stdout("git reset HEAD", &cwd).await?;
                    run_shell_stdout("git checkout .", &cwd).await?;
                    ctx.console.markup_line("[green]All changes reverted.[/]");
                }
                Some(1) => {
                    let out = run_shell_stdout("git stash", &cwd).await?;
                    ctx.console
                        .markup_line(&format!("[green]{}[/]", escape(out.trim())));
                }
                _ => ctx.console.markup_line("[grey]Cancelled.[/]"),
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            ctx.console.markup_line(&format!(
                "[red]Failed to undo: {}[/]",
                escape(&e.to_string())
            ));
        }
        CommandResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_like_get_files() {
        assert!(wildcard_match("*.rs", "main.RS"));
        assert!(wildcard_match("a?c.txt", "abc.txt"));
        assert!(!wildcard_match("*.rs", "main.rsx"));
    }

    #[test]
    fn add_matches_paths_and_wildcards() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "").unwrap();
        std::fs::write(d.path().join("b.rs"), "").unwrap();
        std::fs::write(d.path().join("c.md"), "").unwrap();
        assert_eq!(add_matches("*.rs", d.path()).len(), 2);
        assert_eq!(add_matches("c.md", d.path()).len(), 1);
        assert!(add_matches("zz.md", d.path()).is_empty());
    }
}
