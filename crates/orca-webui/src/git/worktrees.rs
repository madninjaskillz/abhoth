//! `Worktrees`: `git worktree list --porcelain` for a project; the project's own folder first.

use std::path::Path;

use serde::Serialize;

use super::service::run;
use crate::paths;

/// One worktree as the file tree shows it. `path` is the folder the tree lists: for a project
/// that is a sub-folder of its repository, the same sub-folder in each worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: String,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub main: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

impl WorktreeInfo {
    pub fn own(root: &str, top: &str) -> Self {
        Self {
            path: root.into(),
            worktree_path: top.into(),
            branch: None,
            head: None,
            main: true,
            detached: false,
            locked: false,
            prunable: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub async fn list(project_root: &Path) -> Vec<WorktreeInfo> {
    let root = paths::normalize_path(project_root);
    let root_s = root.to_string_lossy().to_string();
    let top = run(&root, &["rev-parse", "--show-toplevel"], None, None, None).await;
    if !top.ok() {
        return vec![WorktreeInfo::own(&root_s, &root_s)];
    }
    let toplevel = paths::normalize(top.output.trim());
    let top_s = toplevel.to_string_lossy().to_string();
    // The project may be a sub-folder of its repository: show that sub-folder in every worktree.
    let sub = if paths::is_under(&root, &toplevel) {
        paths::to_relative(&toplevel, &root)
    } else {
        String::new()
    };
    let r = run(
        &root,
        &["worktree", "list", "--porcelain"],
        None,
        None,
        None,
    )
    .await;
    let parsed = if r.ok() { parse(&r.output) } else { vec![] };
    let mut list = Vec::new();
    for w in parsed {
        let wt_path = paths::normalize(&w.path);
        let wt_s = wt_path.to_string_lossy().to_string();
        let shown = if sub.is_empty() {
            wt_path.clone()
        } else {
            paths::normalize_path(&wt_path.join(&sub))
        };
        let is_own = paths::eq(&wt_s, &top_s);
        if !shown.is_dir() && !is_own {
            continue; // a bare repo, a pruned worktree, or one without this sub-folder
        }
        list.push(WorktreeInfo {
            path: if is_own {
                root_s.clone()
            } else {
                shown.to_string_lossy().to_string()
            },
            worktree_path: wt_s,
            branch: w.branch,
            head: w.head,
            main: is_own,
            detached: w.detached,
            locked: w.locked,
            prunable: w.prunable,
        });
    }
    if !list.iter().any(|w| w.main) {
        list.insert(0, WorktreeInfo::own(&root_s, &top_s));
    }
    // The project's own worktree first, then git's order.
    let (mut main, rest): (Vec<_>, Vec<_>) = list.into_iter().partition(|w| w.main);
    main.extend(rest);
    main
}

pub fn parse(porcelain: &str) -> Vec<Raw> {
    let text = porcelain.replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n").filter(|b| !b.is_empty()) {
        let mut r = Raw {
            path: String::new(),
            branch: None,
            head: None,
            detached: false,
            bare: false,
            locked: false,
            prunable: false,
        };
        let mut has_path = false;
        for line in block.split('\n').filter(|l| !l.is_empty()) {
            if let Some(p) = line.strip_prefix("worktree ") {
                r.path = p.into();
                has_path = true;
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                r.head = Some(h.into());
            } else if let Some(b) = line.strip_prefix("branch ") {
                r.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).into());
            } else if line == "detached" {
                r.detached = true;
            } else if line == "bare" {
                r.bare = true;
            } else if line.starts_with("locked") {
                r.locked = true;
            } else if line.starts_with("prunable") {
                r.prunable = true;
            }
        }
        if has_path && !r.bare {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_blocks() {
        let raw = parse(
            "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r-b\nHEAD def\ndetached\nlocked reason\n\nworktree /bare\nbare\n",
        );
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].branch.as_deref(), Some("main"));
        assert!(raw[1].detached && raw[1].locked);
    }
}
