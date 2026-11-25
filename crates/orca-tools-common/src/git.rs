//! `GitHelper`: runs `git` for the git tools.

use std::path::Path;
use std::time::Duration;

use orca_abstractions::{ToolContext, ToolResult};

use crate::process::{RunError, command, run_captured};

const GIT_TIMEOUT_SECONDS: u64 = 60;

/// `GitHelper.EscapeArg`: wraps in double quotes, backslashes and quotes backslash-escaped.
pub fn escape_arg(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// `GitHelper.RunGitAsync`: `git <arguments>` in `working_directory` (resolved against the
/// tool context like `Path.GetFullPath`). Exit code 0 gives stdout (or `(no output)`), any
/// other gives stderr (or stdout when stderr is empty) as an error.
pub async fn run_git(arguments: &str, working_directory: &str, ctx: &ToolContext) -> ToolResult {
    let dir = ctx.full_path(working_directory);
    if !dir.is_dir() {
        return ToolResult::error(format!("Directory not found: {}", dir.display()));
    }
    if !is_git_repository(&dir) {
        return ToolResult::error(format!(
            "Not a git repository: {}. No .git directory was found here or in any parent directory. To fix this, use the bash tool to run 'git init' in the target directory, or change the 'path' parameter to point to an existing git repository.",
            dir.display()
        ));
    }
    let cmd = command("git", arguments, &dir);
    match run_captured(cmd, Duration::from_secs(GIT_TIMEOUT_SECONDS)).await {
        Ok(c) if c.exit_code != 0 => ToolResult::error(if c.stderr.is_empty() {
            c.stdout
        } else {
            c.stderr
        }),
        Ok(c) => ToolResult::success(if c.stdout.is_empty() {
            "(no output)".to_string()
        } else {
            c.stdout
        }),
        Err(RunError::TimedOutReading | RunError::TimedOutWaiting) => ToolResult::error(format!(
            "Git command timed out after {GIT_TIMEOUT_SECONDS}s: git {arguments}"
        )),
        Err(RunError::Spawn(e) | RunError::Io(e)) => ToolResult::error(format!(
            "Git error: {}",
            spawn_error_message("git", &dir, &e)
        )),
    }
}

/// The .NET `Win32Exception` text for a program that could not be started (e.g. not on the
/// PATH); other I/O errors as Rust prints them.
pub fn spawn_error_message(program: &str, dir: &Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        let native = if cfg!(windows) {
            "The system cannot find the file specified."
        } else {
            "No such file or directory"
        };
        format!(
            "An error occurred trying to start process '{program}' with working directory '{}'. {native}",
            dir.display()
        )
    } else {
        e.to_string()
    }
}

/// `GitHelper.IsGitRepository`: a `.git` directory or file here or in any parent.
pub fn is_git_repository(directory: &Path) -> bool {
    let mut dir = Some(directory);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_arg_quotes_and_escapes() {
        assert_eq!(escape_arg("a b"), "\"a b\"");
        assert_eq!(escape_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(escape_arg(r"C:\x"), r#""C:\\x""#);
    }

    #[test]
    fn git_repository_found_in_parent() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(!is_git_repository(&sub) || is_git_repository(dir.path().parent().unwrap()));
        std::fs::write(dir.path().join(".git"), "gitdir: elsewhere").unwrap();
        assert!(is_git_repository(&sub));
    }
}
