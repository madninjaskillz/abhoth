//! The `github` tool (port of `OpenOrca.Tools.GitHub`): runs the `gh` CLI.

use std::time::Duration;

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::get_i32_lenient;
use orca_tools_common::process::{RunError, command, run_captured};
use serde_json::Value;

use crate::{non_empty, schema, str_or};

const GH_TIMEOUT: Duration = Duration::from_secs(30);

const NOT_INSTALLED: &str = "The 'gh' CLI is not installed or not in PATH. Install from https://cli.github.com/ and run 'gh auth login' to authenticate.";

pub struct GitHubTool {
    /// The program to run; `gh` (tests point it at a missing program so they never reach
    /// GitHub).
    pub program: String,
}

impl Default for GitHubTool {
    fn default() -> Self {
        Self {
            program: "gh".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for GitHubTool {
    fn name(&self) -> &str {
        "github"
    }

    fn description(&self) -> &str {
        "Interact with GitHub using the gh CLI. Actions: pr_list (list pull requests), pr_view (view PR details), pr_create (create PR), pr_diff (view PR diff), pr_merge (merge PR), pr_checks (view PR check status), pr_comment (comment on PR), issue_list (list issues), issue_view (view issue), issue_create (create issue), repo_view (view repo info). Requires the 'gh' CLI to be installed and authenticated."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f33f}",
            short_description: "GitHub operation",
            summary_arg_keys: &["command"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "pr_list", "pr_view", "pr_create", "pr_diff", "pr_merge", "pr_checks", "pr_comment",
                    "issue_list", "issue_view", "issue_create",
                    "repo_view"
                ],
                "description": "The GitHub action to perform"
            },
            "number": {
                "type": "integer",
                "description": "PR or issue number (required for pr_view, pr_diff, pr_merge, pr_checks, pr_comment, issue_view)"
            },
            "title": {
                "type": "string",
                "description": "Title for pr_create or issue_create"
            },
            "body": {
                "type": "string",
                "description": "Body/description for pr_create, issue_create, or pr_comment"
            },
            "base": {
                "type": "string",
                "description": "Base branch for pr_create (e.g. 'main'). Defaults to repo default branch."
            },
            "head": {
                "type": "string",
                "description": "Head branch for pr_create. Defaults to current branch."
            },
            "state": {
                "type": "string",
                "enum": ["open", "closed", "merged", "all"],
                "description": "Filter by state for pr_list or issue_list. Defaults to 'open'."
            },
            "labels": {
                "type": "string",
                "description": "Comma-separated labels for issue_create (e.g. 'bug,urgent')"
            },
            "merge_method": {
                "type": "string",
                "enum": ["merge", "squash", "rebase"],
                "description": "Merge method for pr_merge. Defaults to 'merge'."
            },
            "limit": {
                "type": "integer",
                "description": "Max results for pr_list and issue_list. Defaults to 20."
            },
            "repo": {
                "type": "string",
                "description": "Repository in 'owner/repo' format. Defaults to the repo in the current directory."
            },
            "path": {
                "type": "string",
                "description": "Working directory for the command. Defaults to current directory."
            }
        },
        "required": ["action"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let path = str_or(args, "path", ".");
        let Some(gh_args) = build_gh_args(action, args) else {
            return ToolResult::error(format!(
                "Invalid action or missing required parameters for '{action}'."
            ));
        };
        run_gh(&self.program, &gh_args, path, ctx).await
    }
}

/// `GitHubTool.BuildGhArgs`: the `gh` argument string for an action, or `None` when the action
/// is unknown or misses a required parameter.
pub fn build_gh_args(action: &str, args: &Value) -> Option<String> {
    let number = get_i32_lenient(args.get("number"), 0);
    let title = non_empty(args, "title");
    let body = non_empty(args, "body");
    let state = str_or(args, "state", "open");
    let merge_method = str_or(args, "merge_method", "merge");
    let limit = get_i32_lenient(args.get("limit"), 20);
    let repo_flag = non_empty(args, "repo")
        .map(|r| format!(" --repo {r}"))
        .unwrap_or_default();

    Some(match action {
        "pr_list" => format!("pr list --state {state} --limit {limit}{repo_flag}"),
        "pr_view" if number > 0 => format!("pr view {number}{repo_flag}"),
        "pr_create" if title.is_some() => {
            let mut cmd = format!("pr create --title \"{}\"", escape(title?));
            if let Some(body) = body {
                cmd.push_str(&format!(" --body \"{}\"", escape(body)));
            }
            if let Some(base) = non_empty(args, "base") {
                cmd.push_str(&format!(" --base {base}"));
            }
            if let Some(head) = non_empty(args, "head") {
                cmd.push_str(&format!(" --head {head}"));
            }
            cmd + &repo_flag
        }
        "pr_diff" if number > 0 => format!("pr diff {number}{repo_flag}"),
        "pr_merge" if number > 0 => format!("pr merge {number} --{merge_method}{repo_flag}"),
        "pr_checks" if number > 0 => format!("pr checks {number}{repo_flag}"),
        "pr_comment" if number > 0 && body.is_some() => format!(
            "pr comment {number} --body \"{}\"{repo_flag}",
            escape(body?)
        ),
        "issue_list" => format!("issue list --state {state} --limit {limit}{repo_flag}"),
        "issue_view" if number > 0 => format!("issue view {number}{repo_flag}"),
        "issue_create" if title.is_some() => {
            let mut cmd = format!("issue create --title \"{}\"", escape(title?));
            if let Some(body) = body {
                cmd.push_str(&format!(" --body \"{}\"", escape(body)));
            }
            if let Some(labels) = non_empty(args, "labels") {
                cmd.push_str(&format!(" --label \"{}\"", escape(labels)));
            }
            cmd + &repo_flag
        }
        "repo_view" => format!("repo view{repo_flag}"),
        _ => return None,
    })
}

/// `GitHubTool.EscapeArg`: backslashes and quotes escaped, no surrounding quotes.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `GhHelper.RunGhAsync`. Unlike git, no repository is required (`repo view --repo` works
/// anywhere).
async fn run_gh(program: &str, arguments: &str, path: &str, ctx: &ToolContext) -> ToolResult {
    let dir = ctx.full_path(path);
    if !dir.is_dir() {
        return ToolResult::error(format!("Directory not found: {}", dir.display()));
    }
    let output = match run_captured(command(program, arguments, &dir), GH_TIMEOUT).await {
        Ok(o) => o,
        // Process.Start throws Win32Exception for any start failure.
        Err(RunError::Spawn(_)) => return ToolResult::error(NOT_INSTALLED),
        // C# times out on the wait with this message; a timeout while the output is still open
        // escapes as a cancelled read.
        Err(RunError::TimedOutWaiting) => {
            return ToolResult::error("GitHub CLI command timed out after 30 seconds.");
        }
        Err(RunError::TimedOutReading) => {
            return ToolResult::error("GitHub CLI error: The operation was canceled.");
        }
        Err(RunError::Io(e)) => return ToolResult::error(format!("GitHub CLI error: {e}")),
    };
    if output.exit_code != 0 {
        let err = if output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        if err.contains("gh auth login") || err.contains("not logged") {
            return ToolResult::error(format!(
                "GitHub CLI not authenticated. Run 'gh auth login' first.\n\n{err}"
            ));
        }
        if err.contains("Could not resolve") || err.contains("not a git repository") {
            return ToolResult::error(format!(
                "Not in a GitHub repository. Use --repo owner/name or cd into a git repo.\n\n{err}"
            ));
        }
        return ToolResult::error(err);
    }
    ToolResult::success(if output.stdout.is_empty() {
        "(no output)".to_string()
    } else {
        output.stdout
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_gh_arguments_like_csharp() {
        let b = |a: Value| build_gh_args(a["action"].as_str().unwrap(), &a);
        assert_eq!(
            b(json!({"action": "pr_list", "state": "closed", "limit": 5})).unwrap(),
            "pr list --state closed --limit 5"
        );
        assert_eq!(
            b(json!({"action": "issue_list", "repo": "cli/cli", "limit": 3})).unwrap(),
            "issue list --state open --limit 3 --repo cli/cli"
        );
        assert_eq!(
            b(json!({"action": "pr_create", "title": "Fix \"x\"", "body": "a\\b", "base": "main", "head": "f"})).unwrap(),
            r#"pr create --title "Fix \"x\"" --body "a\\b" --base main --head f"#
        );
        assert_eq!(
            b(json!({"action": "pr_merge", "number": "7", "merge_method": "squash"})).unwrap(),
            "pr merge 7 --squash"
        );
        assert_eq!(
            b(json!({"action": "issue_create", "title": "T", "labels": "bug,urgent"})).unwrap(),
            r#"issue create --title "T" --label "bug,urgent""#
        );
        assert_eq!(
            b(json!({"action": "pr_comment", "number": 2, "body": "hi"})).unwrap(),
            r#"pr comment 2 --body "hi""#
        );
        assert_eq!(b(json!({"action": "repo_view"})).unwrap(), "repo view");
        assert!(b(json!({"action": "pr_view", "number": 0})).is_none());
        assert!(b(json!({"action": "pr_create", "title": ""})).is_none());
    }
}
