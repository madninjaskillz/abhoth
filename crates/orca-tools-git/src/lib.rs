//! Git tools (port of `OpenOrca.Tools.Git`) and the `github` tool (port of
//! `OpenOrca.Tools.GitHub`). Both shell out, to `git` and `gh`, with the same argument strings
//! as C#.

use std::sync::Arc;

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::git::{escape_arg, run_git};
use orca_tools_common::{append_line, get_bool_lenient, get_i32_lenient, required_str};
use serde_json::Value;

pub mod github;

pub use github::GitHubTool;

/// Every tool in this crate: the nine git tools and `github`.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(GitStatusTool),
        Arc::new(GitDiffTool),
        Arc::new(GitLogTool),
        Arc::new(GitCommitTool),
        Arc::new(GitBranchTool),
        Arc::new(GitCheckoutTool),
        Arc::new(GitPullTool),
        Arc::new(GitPushTool),
        Arc::new(GitStashTool),
        Arc::new(GitHubTool::default()),
    ]
}

pub(crate) fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

/// `args.TryGetProperty(name, out var p) ? p.GetString() ?? default : default`.
pub(crate) fn str_or<'a>(args: &'a Value, name: &str, default: &'a str) -> &'a str {
    args.get(name).and_then(Value::as_str).unwrap_or(default)
}

/// `args.TryGetProperty(name, out var p) ? p.GetString() : null`, treating `""` as absent
/// (every caller checks `string.IsNullOrEmpty`).
pub(crate) fn non_empty<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

const BRANCH_EMOJI: &str = "\u{1f33f}";

fn hints(short: &'static str, keys: &'static [&'static str]) -> Option<ToolRenderHints> {
    Some(ToolRenderHints {
        emoji: BRANCH_EMOJI,
        short_description: short,
        summary_arg_keys: keys,
        aliases: &[],
    })
}

pub struct GitStatusTool;

#[async_trait::async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }
    fn description(&self) -> &str {
        "Show the working tree status (short format with branch info). Returns modified, staged, and untracked files. Always run this before git_commit to see what will be committed."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git status", &[])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        run_git("status --short --branch", str_or(args, "path", "."), ctx).await
    }
}

pub struct GitDiffTool;

#[async_trait::async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Show changes between commits, working tree, and staging area. Supports staged diffs (--cached), file-specific diffs, and branch comparisons via the base parameter."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git diff", &["path"])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            },
            "staged": {
                "type": "boolean",
                "description": "Show staged changes (--cached). Defaults to false."
            },
            "file": {
                "type": "string",
                "description": "Optional specific file to diff."
            },
            "base": {
                "type": "string",
                "description": "Diff against a branch or commit (e.g. 'main', 'HEAD~3'). Uses three-dot diff: base...HEAD."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = str_or(args, "path", ".");
        let staged = get_bool_lenient(args.get("staged"), false);
        let mut git_args = match non_empty(args, "base") {
            Some(base) => format!("diff {}", escape_arg(&format!("{base}...HEAD"))),
            None if staged => "diff --cached".to_string(),
            None => "diff".to_string(),
        };
        if let Some(file) = non_empty(args, "file") {
            git_args.push_str(&format!(" -- {}", escape_arg(file)));
        }
        run_git(&git_args, path, ctx).await
    }
}

pub struct GitLogTool;

#[async_trait::async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }
    fn description(&self) -> &str {
        "Show recent commit history with one-line summaries. Filter by file path or author name."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git log", &[])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            },
            "count": {
                "type": "integer",
                "description": "Number of commits to show. Defaults to 20."
            },
            "oneline": {
                "type": "boolean",
                "description": "Use one-line format. Defaults to true."
            },
            "file": {
                "type": "string",
                "description": "Filter commits to those affecting this file path."
            },
            "author": {
                "type": "string",
                "description": "Filter commits by author name or email."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = str_or(args, "path", ".");
        let count = get_i32_lenient(args.get("count"), 20);
        let oneline = get_bool_lenient(args.get("oneline"), true);
        let format = if oneline {
            "--oneline"
        } else {
            "--format=medium"
        };
        let mut git_args = format!("log {format} -n {count}");
        if let Some(author) = non_empty(args, "author") {
            git_args.push_str(&format!(" --author={}", escape_arg(author)));
        }
        if let Some(file) = non_empty(args, "file") {
            git_args.push_str(&format!(" -- {}", escape_arg(file)));
        }
        run_git(&git_args, path, ctx).await
    }
}

pub struct GitCommitTool;

#[async_trait::async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }
    fn description(&self) -> &str {
        "Stage files and create a git commit. Can stage specific files or all changes. Returns the commit hash and a diff stat summary after committing."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git commit", &["message"])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "The commit message"
            },
            "files": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Specific files to stage. If empty, stages all changes."
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        },
        "required": ["message"]
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let message = match required_str(args, "message") {
            Ok(m) => m,
            Err(e) => return ToolResult::error(e),
        };
        let path = str_or(args, "path", ".");
        let files: Vec<&str> = args
            .get("files")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let stage_args = if files.is_empty() {
            "add -A".to_string()
        } else {
            let quoted: Vec<String> = files.iter().map(|f| escape_arg(f)).collect();
            format!("add {}", quoted.join(" "))
        };
        let stage = run_git(&stage_args, path, ctx).await;
        if stage.is_error {
            return ToolResult::error(format!("Failed to stage: {}", stage.content));
        }

        let commit = run_git(&format!("commit -m {}", escape_arg(message)), path, ctx).await;
        if commit.is_error {
            return commit;
        }

        let mut sb = String::new();
        append_line(&mut sb, &commit.content);
        let log = run_git("log --oneline -1", path, ctx).await;
        if !log.is_error {
            append_line(&mut sb, "");
            append_line(&mut sb, &format!("Commit: {}", log.content.trim()));
        }
        let stat = run_git("diff --stat HEAD~1", path, ctx).await;
        if !stat.is_error {
            append_line(&mut sb, "");
            append_line(&mut sb, &stat.content);
        }
        ToolResult::success(sb)
    }
}

pub struct GitBranchTool;

#[async_trait::async_trait]
impl Tool for GitBranchTool {
    fn name(&self) -> &str {
        "git_branch"
    }
    fn description(&self) -> &str {
        "List, create, or delete git branches. Lists both local and remote branches. Use git_checkout to switch branches after creating."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git branch", &["name"])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "create", "delete"],
                "description": "Action to perform. Defaults to 'list'."
            },
            "name": {
                "type": "string",
                "description": "Branch name (required for create/delete)"
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let action = str_or(args, "action", "list");
        let name = non_empty(args, "name");
        let path = str_or(args, "path", ".");
        match (action, name) {
            ("create", Some(name)) => {
                run_git(&format!("branch {}", escape_arg(name)), path, ctx).await
            }
            ("delete", Some(name)) => {
                run_git(&format!("branch -d {}", escape_arg(name)), path, ctx).await
            }
            ("list", _) => run_git("branch -a", path, ctx).await,
            _ => ToolResult::error("Invalid action or missing branch name."),
        }
    }
}

pub struct GitCheckoutTool;

#[async_trait::async_trait]
impl Tool for GitCheckoutTool {
    fn name(&self) -> &str {
        "git_checkout"
    }
    fn description(&self) -> &str {
        "Switch branches or restore working tree files. Set create=true to create and switch to a new branch in one step (-b flag)."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git checkout", &["branch"])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "description": "Branch name or commit to checkout"
            },
            "create": {
                "type": "boolean",
                "description": "Create a new branch (-b flag). Defaults to false."
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        },
        "required": ["target"]
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let target = match required_str(args, "target") {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e),
        };
        let create = get_bool_lenient(args.get("create"), false);
        let path = str_or(args, "path", ".");
        let flag = if create { "-b " } else { "" };
        run_git(&format!("checkout {flag}{}", escape_arg(target)), path, ctx).await
    }
}

pub struct GitPullTool;

#[async_trait::async_trait]
impl Tool for GitPullTool {
    fn name(&self) -> &str {
        "git_pull"
    }
    fn description(&self) -> &str {
        "Pull changes from a remote repository. Fetches and merges (or rebases if configured) the specified branch."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git pull", &[])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "remote": {
                "type": "string",
                "description": "Remote name. Defaults to 'origin'."
            },
            "branch": {
                "type": "string",
                "description": "Branch to pull. Defaults to the tracking branch."
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let remote = str_or(args, "remote", "origin");
        let path = str_or(args, "path", ".");
        let mut git_args = format!("pull {}", escape_arg(remote));
        if let Some(branch) = non_empty(args, "branch") {
            git_args.push_str(&format!(" {}", escape_arg(branch)));
        }
        run_git(&git_args, path, ctx).await
    }
}

pub struct GitPushTool;

#[async_trait::async_trait]
impl Tool for GitPushTool {
    fn name(&self) -> &str {
        "git_push"
    }
    fn description(&self) -> &str {
        "Push commits to a remote repository. Uses --force-with-lease for safe force pushes. Always confirm with the user before force pushing."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Dangerous
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git push", &[])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "remote": {
                "type": "string",
                "description": "Remote name. Defaults to 'origin'."
            },
            "branch": {
                "type": "string",
                "description": "Branch to push. Defaults to the current branch."
            },
            "force": {
                "type": "boolean",
                "description": "Force push using --force-with-lease (safer than --force). Defaults to false."
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let remote = str_or(args, "remote", "origin");
        let force = get_bool_lenient(args.get("force"), false);
        let path = str_or(args, "path", ".");
        let mut git_args = format!("push {}", escape_arg(remote));
        if let Some(branch) = non_empty(args, "branch") {
            git_args.push_str(&format!(" {}", escape_arg(branch)));
        }
        if force {
            git_args.push_str(" --force-with-lease");
        }
        run_git(&git_args, path, ctx).await
    }
}

pub struct GitStashTool;

#[async_trait::async_trait]
impl Tool for GitStashTool {
    fn name(&self) -> &str {
        "git_stash"
    }
    fn description(&self) -> &str {
        "Stash or restore uncommitted changes. Actions: push (save changes), pop (restore and remove), list (show stashes), apply (restore and keep), drop (discard a stash)."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Git stash", &["action"])
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["push", "pop", "list", "apply", "drop"],
                "description": "Stash action to perform. Defaults to 'push'."
            },
            "message": {
                "type": "string",
                "description": "Message for 'push' action to label the stash."
            },
            "index": {
                "type": "integer",
                "description": "Stash index for pop/apply/drop (e.g. 0 for stash@{0}). Defaults to 0."
            },
            "path": {
                "type": "string",
                "description": "The repository path. Defaults to current directory."
            }
        }
    }"#,
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let action = str_or(args, "action", "push");
        let index = get_i32_lenient(args.get("index"), 0);
        let path = str_or(args, "path", ".");
        let git_args = match (action, non_empty(args, "message")) {
            ("push", Some(message)) => format!("stash push -m {}", escape_arg(message)),
            ("push", None) => "stash push".to_string(),
            ("pop", _) => format!("stash pop stash@{{{index}}}"),
            ("list", _) => "stash list".to_string(),
            ("apply", _) => format!("stash apply stash@{{{index}}}"),
            ("drop", _) => format!("stash drop stash@{{{index}}}"),
            (other, _) => format!("stash {other}"),
        };
        run_git(&git_args, path, ctx).await
    }
}
