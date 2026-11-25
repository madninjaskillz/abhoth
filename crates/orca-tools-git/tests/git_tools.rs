//! Ported from `GitToolTests`, `GitRemoteToolTests` and `GitHubToolTests`
//! (tests/OpenOrca.Tools.Tests). Each test gets its own temp repository.

use std::path::Path;

use orca_abstractions::{Tool, ToolContext, ToolResult, ToolRiskLevel};
use orca_tools_git::*;
use serde_json::{Value, json};

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    /// `git init` plus a local identity (GitRemoteToolTests' constructor).
    fn new() -> Self {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init"]);
        repo.git(&["config", "user.email", "test@test.com"]);
        repo.git(&["config", "user.name", "Test"]);
        // Not in C#: a global commit.gpgsign would make every commit here fail.
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    /// GitToolTests' constructor: also an initial commit of initial.txt.
    fn with_initial_commit() -> Self {
        let repo = Self::new();
        repo.write("initial.txt", "hello");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "initial commit"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn path_str(&self) -> String {
        self.path().display().to_string()
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            out.status.success() || args[0] == "commit",
            "git {args:?}: {out:?}"
        );
    }

    async fn run(&self, tool: &dyn Tool, mut args: Value) -> ToolResult {
        args["path"] = json!(self.path_str());
        tool.execute(&args, &ToolContext::new(self.path())).await
    }
}

// ── GitStatusTool ──

#[tokio::test]
async fn git_status_clean_repo_returns_status() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitStatusTool, json!({})).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("## "));
}

#[tokio::test]
async fn git_status_dirty_repo_shows_modified_files() {
    let repo = Repo::with_initial_commit();
    repo.write("new.txt", "new file");
    let result = repo.run(&GitStatusTool, json!({})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("new.txt"));
}

#[tokio::test]
async fn git_status_not_a_git_repo_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let result = GitStatusTool
        .execute(
            &json!({"path": dir.path().display().to_string()}),
            &ToolContext::new(dir.path()),
        )
        .await;
    // The system temp folder is not inside a repository on test machines.
    assert!(result.is_error);
    assert!(result.content.to_lowercase().contains("git repository"));
}

#[tokio::test]
async fn git_status_missing_directory_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let result = GitStatusTool
        .execute(
            &json!({"path": missing.display().to_string()}),
            &ToolContext::new(dir.path()),
        )
        .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!("Directory not found: {}", missing.display())
    );
}

// ── GitLogTool ──

#[tokio::test]
async fn git_log_returns_commit_history() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitLogTool, json!({})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("initial commit"));
}

#[tokio::test]
async fn git_log_with_count_limits_results() {
    let repo = Repo::with_initial_commit();
    repo.write("a.txt", "a");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "second commit"]);
    repo.write("b.txt", "b");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "third commit"]);

    let result = repo.run(&GitLogTool, json!({"count": 1})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("third commit"));
    assert!(!result.content.contains("initial commit"));
}

// ── GitDiffTool ──

#[tokio::test]
async fn git_diff_no_changes_returns_empty() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitDiffTool, json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.content, "(no output)");
}

#[tokio::test]
async fn git_diff_with_changes_shows_diff() {
    let repo = Repo::with_initial_commit();
    repo.write("initial.txt", "modified content");
    let result = repo.run(&GitDiffTool, json!({})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("modified content"));
}

#[tokio::test]
async fn git_diff_staged_shows_staged_changes() {
    let repo = Repo::with_initial_commit();
    repo.write("initial.txt", "staged change");
    repo.git(&["add", "."]);
    let result = repo.run(&GitDiffTool, json!({"staged": true})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("staged change"));
}

// ── GitCommitTool ──

#[tokio::test]
async fn git_commit_with_message_creates_commit() {
    let repo = Repo::with_initial_commit();
    repo.write("commit_test.txt", "test content");
    let result = repo
        .run(
            &GitCommitTool,
            json!({"message": "test commit", "files": ["commit_test.txt"]}),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("test commit"));
    assert!(result.content.contains("Commit: "));
    assert!(result.content.contains("commit_test.txt"));
}

#[tokio::test]
async fn git_commit_stages_all_when_no_files() {
    let repo = Repo::with_initial_commit();
    repo.write("auto_stage.txt", "auto");
    let result = repo
        .run(&GitCommitTool, json!({"message": "auto stage commit"}))
        .await;
    assert!(!result.is_error);
    assert!(result.content.contains("auto stage commit"));
}

#[tokio::test]
async fn git_commit_with_nothing_to_commit_returns_error() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitCommitTool, json!({"message": "empty"})).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("nothing to commit"),
        "{}",
        result.content
    );
}

// ── GitBranchTool ──

#[tokio::test]
async fn git_branch_list_shows_branches() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitBranchTool, json!({"action": "list"})).await;
    assert!(!result.is_error);
    assert!(
        result.content.contains("main") || result.content.contains("master"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn git_branch_create_creates_branch() {
    let repo = Repo::with_initial_commit();
    let result = repo
        .run(
            &GitBranchTool,
            json!({"action": "create", "name": "test-branch"}),
        )
        .await;
    assert!(!result.is_error);
    let list = repo.run(&GitBranchTool, json!({"action": "list"})).await;
    assert!(list.content.contains("test-branch"));
}

#[tokio::test]
async fn git_branch_delete_deletes_branch() {
    let repo = Repo::with_initial_commit();
    repo.git(&["branch", "delete-me"]);
    let result = repo
        .run(
            &GitBranchTool,
            json!({"action": "delete", "name": "delete-me"}),
        )
        .await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn git_branch_create_without_name_returns_error() {
    let repo = Repo::with_initial_commit();
    let result = repo.run(&GitBranchTool, json!({"action": "create"})).await;
    assert!(result.is_error);
    assert_eq!(result.content, "Invalid action or missing branch name.");
}

// ── GitCheckoutTool ──

#[tokio::test]
async fn git_checkout_switch_branch_works() {
    let repo = Repo::with_initial_commit();
    repo.git(&["branch", "checkout-target"]);
    let result = repo
        .run(&GitCheckoutTool, json!({"target": "checkout-target"}))
        .await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn git_checkout_create_and_switch_works() {
    let repo = Repo::with_initial_commit();
    let result = repo
        .run(
            &GitCheckoutTool,
            json!({"target": "new-feature", "create": true}),
        )
        .await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn git_checkout_non_existent_branch_returns_error() {
    let repo = Repo::with_initial_commit();
    let result = repo
        .run(&GitCheckoutTool, json!({"target": "does-not-exist"}))
        .await;
    assert!(result.is_error);
}

// ── GitStashTool (GitRemoteToolTests) ──

fn repo_with_one_commit() -> Repo {
    let repo = Repo::new();
    repo.write("init.txt", "init");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "initial"]);
    repo
}

#[tokio::test]
async fn git_stash_tool_list_on_empty_repo() {
    let repo = repo_with_one_commit();
    let result = repo.run(&GitStashTool, json!({"action": "list"})).await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn git_stash_tool_push_and_pop() {
    let repo = repo_with_one_commit();
    repo.write("changed.txt", "modified");
    repo.git(&["add", "-A"]);

    let push = repo
        .run(
            &GitStashTool,
            json!({"action": "push", "message": "test stash"}),
        )
        .await;
    assert!(!push.is_error, "{}", push.content);

    let list = repo.run(&GitStashTool, json!({"action": "list"})).await;
    assert!(!list.is_error);
    assert!(list.content.contains("test stash"));

    let pop = repo
        .run(&GitStashTool, json!({"action": "pop", "index": 0}))
        .await;
    assert!(!pop.is_error, "{}", pop.content);
}

// ── GitPushTool / GitPullTool ──

#[tokio::test]
async fn git_push_tool_fails_without_remote() {
    let repo = repo_with_one_commit();
    let result = repo.run(&GitPushTool, json!({})).await;
    assert!(result.is_error);
}

#[tokio::test]
async fn git_pull_tool_fails_without_remote() {
    let repo = repo_with_one_commit();
    let result = repo.run(&GitPullTool, json!({})).await;
    assert!(result.is_error);
}

// ── GitHubToolTests ──
//
// The tests that run `gh` (GhNotInstalled, PrList_WithFilters, IssueList_WithRepo) would reach
// GitHub on a machine with an authenticated gh; here they point the tool at a missing program,
// which exercises the same "returns a result, never throws" path. The argument strings they
// build are checked in `github::tests`.

fn gh_missing() -> GitHubTool {
    GitHubTool {
        program: "gh-not-installed-openorca-test".to_string(),
    }
}

async fn gh(tool: &GitHubTool, args: Value) -> ToolResult {
    let dir = tempfile::tempdir().unwrap();
    tool.execute(&args, &ToolContext::new(dir.path())).await
}

#[test]
fn github_tool_has_correct_name() {
    assert_eq!(GitHubTool::default().name(), "github");
}

#[test]
fn github_tool_has_correct_risk_level() {
    assert_eq!(GitHubTool::default().risk_level(), ToolRiskLevel::Moderate);
}

#[tokio::test]
async fn github_tool_invalid_action_returns_error() {
    let result = gh(&GitHubTool::default(), json!({"action": "nonexistent"})).await;
    assert!(result.is_error);
    assert!(result.content.contains("Invalid action"));
}

#[tokio::test]
async fn github_tool_pr_view_missing_number_returns_error() {
    let result = gh(&GitHubTool::default(), json!({"action": "pr_view"})).await;
    assert!(result.is_error);
    assert!(result.content.to_lowercase().contains("missing required"));
}

#[tokio::test]
async fn github_tool_pr_create_missing_title_returns_error() {
    let result = gh(&GitHubTool::default(), json!({"action": "pr_create"})).await;
    assert!(result.is_error);
}

#[tokio::test]
async fn github_tool_issue_create_missing_title_returns_error() {
    let result = gh(&GitHubTool::default(), json!({"action": "issue_create"})).await;
    assert!(result.is_error);
}

#[tokio::test]
async fn github_tool_pr_comment_missing_body_returns_error() {
    let result = gh(
        &GitHubTool::default(),
        json!({"action": "pr_comment", "number": 1}),
    )
    .await;
    assert!(result.is_error);
}

#[tokio::test]
async fn github_tool_gh_not_installed_returns_helpful_error() {
    let result = gh(&gh_missing(), json!({"action": "repo_view"})).await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "The 'gh' CLI is not installed or not in PATH. Install from https://cli.github.com/ and run 'gh auth login' to authenticate."
    );
}

#[tokio::test]
async fn github_tool_pr_list_with_filters() {
    let result = gh(
        &gh_missing(),
        json!({"action": "pr_list", "state": "closed", "limit": 5}),
    )
    .await;
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn github_tool_issue_list_with_repo() {
    let result = gh(
        &gh_missing(),
        json!({"action": "issue_list", "repo": "cli/cli", "limit": 3}),
    )
    .await;
    assert!(!result.content.is_empty());
}

#[test]
fn all_tools_has_the_ten_tools() {
    let names: Vec<String> = all_tools().iter().map(|t| t.name().to_string()).collect();
    assert_eq!(
        names,
        [
            "git_branch",
            "git_checkout",
            "git_commit",
            "git_diff",
            "git_log",
            "git_pull",
            "git_push",
            "git_stash",
            "git_status",
            "github"
        ]
    );
}
