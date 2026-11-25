//! Branches, merges, stashes and worktrees (the git panel's branch menu). Every name the
//! browser sends is checked (`git check-ref-format`, no leading "-", commit must resolve)
//! before it reaches a git argument list; failures come back as git's own message, with a
//! `code` where the browser offers a follow-up (stash, force…).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::service::{GitError, GitResult, GitService, run};
use super::worktrees;
use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranch {
    pub name: String,
    pub remote: bool,
    pub current: bool,
    pub upstream: Option<String>,
    pub head: String,
    pub subject: String,
    pub when: String,
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranches {
    pub current: Option<String>,
    pub detached: bool,
    pub local: Vec<GitBranch>,
    pub remote: Vec<GitBranch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStash {
    pub index: i32,
    pub message: String,
    pub when: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitMergeState {
    pub merging: bool,
    pub branch: Option<String>,
    pub conflicts: Vec<String>,
}

fn res(code: i32, output: &str, error: &str) -> GitResult {
    GitResult {
        exit_code: code,
        output: output.into(),
        error: error.into(),
        code: None,
    }
}

fn with_code(mut r: GitResult, code: &str) -> GitResult {
    r.code = Some(code.into());
    r
}

fn bad(msg: String) -> GitError {
    GitError::BadRequest(msg)
}

impl GitService {
    async fn repo_root(&self) -> Result<PathBuf, GitError> {
        match self.root().await {
            Some(r) => Ok(PathBuf::from(r)),
            None => {
                let p = super::service::probe(self.working_directory()).await;
                Err(GitError::Invalid(
                    p.error.unwrap_or_else(|| "Not a git repository.".into()),
                ))
            }
        }
    }

    pub async fn branches(&self) -> Result<GitBranches, GitError> {
        let root = self.repo_root().await?;
        let fmt = "--format=%(refname)%00%(HEAD)%00%(upstream:short)%00%(objectname:short)%00%(committerdate:relative)%00%(worktreepath)%00%(contents:subject)";
        let r = run(
            &root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                fmt,
                "refs/heads",
                "refs/remotes",
            ],
            None,
            None,
            None,
        )
        .await;
        if !r.ok() {
            return Err(GitError::Invalid(r.error.trim().to_string()));
        }
        let (mut local, mut remote, mut current) = (Vec::new(), Vec::new(), None);
        for line in r.output.split('\n').filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\0').collect();
            if f.len() < 7 {
                continue;
            }
            let is_current = f[1] == "*";
            let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
            if let Some(name) = f[0].strip_prefix("refs/heads/") {
                if is_current {
                    current = Some(name.to_string());
                }
                local.push(GitBranch {
                    name: name.into(),
                    remote: false,
                    current: is_current,
                    upstream: opt(f[2]),
                    head: f[3].into(),
                    subject: f[6].into(),
                    when: f[4].into(),
                    worktree: opt(f[5]),
                });
            } else if let Some(name) = f[0].strip_prefix("refs/remotes/")
                && !f[0].ends_with("/HEAD")
            {
                remote.push(GitBranch {
                    name: name.into(),
                    remote: true,
                    current: false,
                    upstream: None,
                    head: f[3].into(),
                    subject: f[6].into(),
                    when: f[4].into(),
                    worktree: None,
                });
            }
        }
        let detached = current.is_none()
            && run(&root, &["symbolic-ref", "-q", "HEAD"], None, None, None)
                .await
                .exit_code
                != 0
            && run(
                &root,
                &["rev-parse", "-q", "--verify", "HEAD"],
                None,
                None,
                None,
            )
            .await
            .ok();
        Ok(GitBranches {
            current,
            detached,
            local,
            remote,
        })
    }

    /// Switch to `branch` (a local branch, or "origin/x": a local x tracking it). With
    /// uncommitted changes and no `stash`: code "dirty".
    pub async fn checkout(&self, branch: &str, stash: bool) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        let b = self.branches().await?;
        let local = b.local.iter().find(|x| x.name == branch);
        let remote = if local.is_none() {
            b.remote.iter().find(|x| x.name == branch)
        } else {
            None
        };
        if local.is_none() && remote.is_none() {
            return Ok(res(2, "", &format!("No such branch: {branch}")));
        }
        if local.is_some_and(|l| l.current) {
            return Ok(res(0, &format!("Already on {branch}"), ""));
        }
        if let Some(elsewhere) = local.and_then(|l| l.worktree.as_deref()) {
            return Ok(res(
                1,
                "",
                &format!(
                    "{branch} is checked out in another worktree ({elsewhere}). Open that worktree instead."
                ),
            ));
        }
        if is_dirty(&root).await {
            if !stash {
                return Ok(with_code(
                    res(1, "", "You have uncommitted changes."),
                    "dirty",
                ));
            }
            let msg = format!("OpenOrca: before switching to {branch}");
            let s = run(
                &root,
                &["stash", "push", "--include-untracked", "-m", &msg],
                None,
                None,
                None,
            )
            .await;
            if !s.ok() {
                return Ok(s);
            }
        }
        if local.is_some() {
            return Ok(run(&root, &["switch", branch], None, None, None).await);
        }
        // origin/feature -> a local "feature" tracking it (or switch to it if it exists already).
        let name = match branch.find('/') {
            Some(i) if i > 0 => &branch[i + 1..],
            _ => branch,
        };
        if b.local.iter().any(|x| x.name == name) {
            return Ok(run(&root, &["switch", name], None, None, None).await);
        }
        Ok(run(
            &root,
            &["switch", "-c", name, "--track", branch],
            None,
            None,
            None,
        )
        .await)
    }

    /// A new branch at `from` (HEAD when empty), optionally switching to it.
    pub async fn create_branch(
        &self,
        name: &str,
        from: Option<&str>,
        checkout: bool,
    ) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        check_branch_name(&root, Some(name)).await?;
        let start = match from.map(str::trim).filter(|f| !f.is_empty()) {
            None => "HEAD".to_string(),
            Some(f) => check_commit(&root, Some(f)).await?,
        };
        if branch_exists(&root, name).await {
            return Ok(res(
                2,
                "",
                &format!("A branch named {name} already exists."),
            ));
        }
        if checkout {
            let r = run(&root, &["switch", "-c", name, &start], None, None, None).await;
            if !r.ok() && start != "HEAD" && is_dirty(&root).await {
                // switch -c carries changes over when it can; say so if it can't.
                return Ok(with_code(r, "dirty"));
            }
            return Ok(r);
        }
        Ok(run(&root, &["branch", name, &start], None, None, None).await)
    }

    pub async fn rename_branch(&self, name: &str, new_name: &str) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        check_local_branch(&root, Some(name)).await?;
        check_branch_name(&root, Some(new_name)).await?;
        if branch_exists(&root, new_name).await {
            return Ok(res(
                2,
                "",
                &format!("A branch named {new_name} already exists."),
            ));
        }
        Ok(run(&root, &["branch", "-m", name, new_name], None, None, None).await)
    }

    /// Delete a local branch. Not merged and no `force`: code "unmerged".
    pub async fn delete_branch(&self, name: &str, force: bool) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        check_local_branch(&root, Some(name)).await?;
        let b = self.branches().await?;
        if b.current.as_deref() == Some(name) {
            return Ok(res(
                2,
                "",
                &format!("{name} is the current branch: switch to another branch first."),
            ));
        }
        let r = run(
            &root,
            &["branch", if force { "-D" } else { "-d" }, name],
            None,
            None,
            None,
        )
        .await;
        if !r.ok() && r.error.to_lowercase().contains("not fully merged") {
            return Ok(with_code(r, "unmerged"));
        }
        Ok(r)
    }

    /// Publish the current branch: `push -u origin HEAD`, with the project's token.
    pub async fn publish(&self, token: Option<&str>) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        if run(&root, &["symbolic-ref", "-q", "HEAD"], None, None, None)
            .await
            .exit_code
            != 0
        {
            return Ok(res(2, "", "HEAD is detached: create a branch to publish."));
        }
        Ok(run(
            &root,
            &["push", "--progress", "-u", "origin", "HEAD"],
            None,
            token,
            Some(Duration::from_secs(300)),
        )
        .await)
    }

    /// Merge `branch` into the current branch. Conflicts: code "conflicts".
    pub async fn merge(&self, branch: &str) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        let commit = check_commit(&root, Some(branch)).await?;
        if self.merge_state().await?.merging {
            return Ok(res(
                2,
                "",
                "A merge is already in progress: finish or abort it first.",
            ));
        }
        let r = run(
            &root,
            &["merge", "--no-edit", "--no-ff", &commit],
            None,
            None,
            Some(Duration::from_secs(120)),
        )
        .await;
        if !r.ok() && !self.merge_state().await?.conflicts.is_empty() {
            return Ok(with_code(r, "conflicts"));
        }
        Ok(r)
    }

    pub async fn merge_state(&self) -> Result<GitMergeState, GitError> {
        let root = self.repo_root().await?;
        let head = run(
            &root,
            &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
            None,
            None,
            None,
        )
        .await;
        let conflicts = run(
            &root,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
            None,
            None,
            None,
        )
        .await;
        let mut files: Vec<String> = Vec::new();
        if conflicts.ok() {
            for f in conflicts.output.split('\0').filter(|f| !f.is_empty()) {
                if !files.iter().any(|x| x == f) {
                    files.push(f.into());
                }
            }
        }
        if !head.ok() {
            return Ok(GitMergeState {
                merging: false,
                branch: None,
                conflicts: files,
            });
        }
        let mut name = None;
        let git_dir = run(&root, &["rev-parse", "--git-dir"], None, None, None).await;
        if git_dir.ok() {
            let dir = orca_abstractions::full_path(&root, git_dir.output.trim());
            if let Ok(text) = std::fs::read_to_string(dir.join("MERGE_MSG"))
                && let Some(first) = text.lines().next()
            {
                // "Merge branch 'feature' into main"
                if let Some(a) = first.find('\'')
                    && let Some(z) = first[a + 1..].find('\'')
                {
                    name = Some(first[a + 1..a + 1 + z].to_string());
                }
            }
        }
        let h = head.output.trim();
        Ok(GitMergeState {
            merging: true,
            branch: Some(name.unwrap_or_else(|| h.chars().take(7).collect())),
            conflicts: files,
        })
    }

    pub async fn abort_merge(&self) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        if !self.merge_state().await?.merging {
            return Ok(res(2, "", "No merge in progress."));
        }
        Ok(run(&root, &["merge", "--abort"], None, None, None).await)
    }

    /// Commit the merge once nothing is in conflict.
    pub async fn continue_merge(&self) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        let state = self.merge_state().await?;
        if !state.merging {
            return Ok(res(2, "", "No merge in progress."));
        }
        if !state.conflicts.is_empty() {
            return Ok(with_code(
                res(
                    1,
                    "",
                    &format!("{} file(s) still in conflict.", state.conflicts.len()),
                ),
                "conflicts",
            ));
        }
        Ok(run(&root, &["commit", "--no-edit"], None, None, None).await)
    }

    pub async fn stashes(&self) -> Result<Vec<GitStash>, GitError> {
        let root = self.repo_root().await?;
        let r = run(
            &root,
            &["stash", "list", "--format=%gd%x00%gs%x00%cr"],
            None,
            None,
            None,
        )
        .await;
        let mut list = Vec::new();
        if !r.ok() {
            return Ok(list);
        }
        for line in r.output.split('\n').filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\0').collect();
            if f.len() >= 3
                && let Some(n) = f[0]
                    .strip_prefix("stash@{")
                    .and_then(|s| s.strip_suffix('}'))
                    .and_then(|s| s.parse().ok())
            {
                list.push(GitStash {
                    index: n,
                    message: f[1].into(),
                    when: f[2].into(),
                });
            }
        }
        Ok(list)
    }

    pub async fn stash(&self, message: Option<&str>) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        if !is_dirty(&root).await {
            return Ok(res(2, "", "Nothing to stash."));
        }
        let mut args = vec!["stash", "push", "--include-untracked"];
        let msg = message.map(str::trim).filter(|m| !m.is_empty());
        if let Some(m) = msg {
            args.extend(["-m", m]);
        }
        Ok(run(&root, &args, None, None, None).await)
    }

    /// apply, pop or drop `stash@{index}`. A conflicting apply/pop: code "conflicts".
    pub async fn stash_op(&self, op: &str, index: i32) -> Result<GitResult, GitError> {
        if !matches!(op, "apply" | "pop" | "drop") {
            return Err(bad(format!("Unknown stash operation: {op}")));
        }
        let root = self.repo_root().await?;
        if index < 0 || !self.stashes().await?.iter().any(|s| s.index == index) {
            return Ok(res(2, "", &format!("No stash number {index}.")));
        }
        let spec = format!("stash@{{{index}}}");
        let r = run(&root, &["stash", op, &spec], None, None, None).await;
        if !r.ok()
            && op != "drop"
            && (r.error.contains("CONFLICT") || r.output.contains("CONFLICT"))
        {
            return Ok(with_code(r, "conflicts"));
        }
        Ok(r)
    }

    /// A worktree for an existing branch, or (with `new_branch`) a new branch at `from`.
    /// Returns the new folder in `output` on success.
    pub async fn add_worktree(
        &self,
        branch: &str,
        new_branch: bool,
        from: Option<&str>,
    ) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        let main = main_worktree(&root).await;
        let folder = worktree_folder(&main, branch);
        let folder_s = folder.to_string_lossy().to_string();
        let args: Vec<String> = if new_branch {
            check_branch_name(&root, Some(branch)).await?;
            if branch_exists(&root, branch).await {
                return Ok(res(
                    2,
                    "",
                    &format!("A branch named {branch} already exists."),
                ));
            }
            let start = match from.map(str::trim).filter(|f| !f.is_empty()) {
                None => "HEAD".to_string(),
                Some(f) => check_commit(&root, Some(f)).await?,
            };
            vec![
                "worktree".into(),
                "add".into(),
                "-b".into(),
                branch.into(),
                folder_s.clone(),
                start,
            ]
        } else {
            check_local_branch(&root, Some(branch)).await?;
            vec![
                "worktree".into(),
                "add".into(),
                folder_s.clone(),
                branch.into(),
            ]
        };
        if folder.exists() {
            return Ok(res(2, "", &format!("{folder_s} already exists.")));
        }
        exclude_worktrees(&root).await;
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run(&root, &argv, None, None, Some(Duration::from_secs(300))).await;
        Ok(if r.ok() {
            res(0, &folder_s, &r.error)
        } else {
            r
        })
    }

    /// Remove a worktree (never the main one). With changes and no `force`: code "dirty".
    pub async fn remove_worktree(&self, path: &str, force: bool) -> Result<GitResult, GitError> {
        let root = self.repo_root().await?;
        let list = worktrees::parse(
            &run(
                &root,
                &["worktree", "list", "--porcelain"],
                None,
                None,
                None,
            )
            .await
            .output,
        );
        if list.is_empty() {
            return Ok(res(2, "", "No worktrees."));
        }
        let Some(target) = list
            .iter()
            .skip(1)
            .find(|w| paths::same_path(&w.path, path))
        else {
            let msg = if paths::same_path(&list[0].path, path) {
                "The main worktree can't be removed.".to_string()
            } else {
                format!("Not one of this repository's worktrees: {path}")
            };
            return Ok(res(2, "", &msg));
        };
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&target.path);
        let r = run(&root, &args, None, None, None).await;
        if !r.ok() && (r.error.contains("modified or untracked") || r.error.contains("use --force"))
        {
            return Ok(with_code(r, "dirty"));
        }
        Ok(r)
    }
}

/// Where a new worktree for `branch` goes: `.worktrees/<branch>` in the main folder.
pub fn worktree_folder(main_root: &Path, branch: &str) -> PathBuf {
    let safe: String = branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = safe.trim_matches(['.', '-']);
    main_root
        .join(".worktrees")
        .join(if safe.is_empty() { "worktree" } else { safe })
}

async fn main_worktree(root: &Path) -> PathBuf {
    let list = worktrees::parse(
        &run(root, &["worktree", "list", "--porcelain"], None, None, None)
            .await
            .output,
    );
    list.first()
        .map(|w| paths::normalize(&w.path))
        .unwrap_or_else(|| root.to_path_buf())
}

async fn exclude_worktrees(root: &Path) {
    let common = run(root, &["rev-parse", "--git-common-dir"], None, None, None).await;
    if !common.ok() {
        return;
    }
    let info = orca_abstractions::full_path(root, common.output.trim()).join("info");
    let exclude = info.join("exclude");
    let _ = std::fs::create_dir_all(&info);
    let text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.split('\n').any(|l| l.trim() == "/.worktrees/") {
        let sep = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = std::fs::write(&exclude, format!("{text}{sep}/.worktrees/\n"));
    }
}

async fn is_dirty(root: &Path) -> bool {
    let r = run(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
        None,
        None,
        None,
    )
    .await;
    r.ok() && !r.output.trim().is_empty()
}

async fn branch_exists(root: &Path, name: &str) -> bool {
    run(
        root,
        &["rev-parse", "-q", "--verify", &format!("refs/heads/{name}")],
        None,
        None,
        None,
    )
    .await
    .ok()
}

/// A name git accepts for a new branch (and that can't be read as an option).
async fn check_branch_name(root: &Path, name: Option<&str>) -> Result<(), GitError> {
    let name = name.unwrap_or("");
    if name.trim().is_empty() {
        return Err(bad("A branch name is required.".into()));
    }
    if name.starts_with('-') || name.chars().any(char::is_whitespace) || name == "HEAD" {
        return Err(bad(format!("Not a valid branch name: {name}")));
    }
    if !run(
        root,
        &["check-ref-format", "--branch", name],
        None,
        None,
        None,
    )
    .await
    .ok()
    {
        return Err(bad(format!("Not a valid branch name: {name}")));
    }
    Ok(())
}

async fn check_local_branch(root: &Path, name: Option<&str>) -> Result<(), GitError> {
    let name = name.unwrap_or("");
    if name.trim().is_empty() || name.starts_with('-') || !branch_exists(root, name).await {
        return Err(bad(format!("No such branch: {name}")));
    }
    Ok(())
}

/// A branch, tag or commit that resolves to a commit; returned as given.
async fn check_commit(root: &Path, rev: Option<&str>) -> Result<String, GitError> {
    let rev = rev.unwrap_or("");
    if rev.trim().is_empty() || rev.starts_with('-') || rev.chars().any(char::is_whitespace) {
        return Err(bad(format!("Not a branch or commit: {rev}")));
    }
    let spec = format!("{rev}^{{commit}}");
    if !run(
        root,
        &["rev-parse", "-q", "--verify", "--end-of-options", &spec],
        None,
        None,
        None,
    )
    .await
    .ok()
    {
        return Err(bad(format!("Not a branch or commit: {rev}")));
    }
    Ok(rev.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_folders_are_safe_names() {
        let main = Path::new("/r");
        assert_eq!(
            worktree_folder(main, "feature/x y"),
            Path::new("/r").join(".worktrees").join("feature-x-y")
        );
        assert_eq!(
            worktree_folder(main, "..."),
            Path::new("/r").join(".worktrees").join("worktree")
        );
    }
}
