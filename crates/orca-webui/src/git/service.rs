//! `GitService`: the git panel's operations through the `git` CLI (porcelain output, argument
//! lists, never a shell), and `GitAuth`, which hands a token to one git command through a
//! private file and a command-line credential helper.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::paths;

pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFile {
    pub path: String,
    pub orig_path: Option<String>,
    pub index: char,
    pub work_tree: char,
}

impl GitFile {
    pub fn untracked(&self) -> bool {
        self.index == '?' && self.work_tree == '?'
    }
    pub fn staged(&self) -> bool {
        !self.untracked() && self.index != ' ' && self.index != '?' && self.index != '!'
    }
    pub fn unstaged(&self) -> bool {
        self.untracked() || (self.work_tree != ' ' && self.work_tree != '!')
    }
    pub fn conflicted(&self) -> bool {
        self.index == 'U'
            || self.work_tree == 'U'
            || (self.index == 'A' && self.work_tree == 'A')
            || (self.index == 'D' && self.work_tree == 'D')
    }
}

/// A folder's repository root, or why there is none (`None` error: simply not a repository).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoProbe {
    pub root: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    pub is_repo: bool,
    pub root: Option<String>,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i32,
    pub behind: i32,
    pub files: Vec<GitFile>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiff {
    pub path: String,
    pub original: String,
    pub modified: String,
    pub binary: bool,
    pub too_large: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitResult {
    pub exit_code: i32,
    pub output: String,
    pub error: String,
    /// Why it failed, when the browser can offer a next step: "dirty", "unmerged", "conflicts".
    pub code: Option<String>,
}

impl GitResult {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
    pub fn failed(code: i32, error: &str) -> Self {
        Self {
            exit_code: code,
            error: error.into(),
            ..Self::default()
        }
    }
}

/// An error the endpoints map to a status code (C#'s exception types).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    /// `ArgumentException`: 400.
    #[error("{0}")]
    BadRequest(String),
    /// `InvalidOperationException`: not a repository, git can't run.
    #[error("{0}")]
    Invalid(String),
}

pub struct GitService {
    working_directory: PathBuf,
}

impl GitService {
    pub fn new(working_directory: impl Into<PathBuf>) -> Self {
        Self {
            working_directory: working_directory.into(),
        }
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub async fn root(&self) -> Option<String> {
        probe(&self.working_directory).await.root
    }

    async fn no_repo(&self) -> GitError {
        GitError::Invalid(
            probe(&self.working_directory)
                .await
                .error
                .unwrap_or_else(|| "Not a git repository.".into()),
        )
    }

    pub async fn status(&self) -> GitStatus {
        let p = probe(&self.working_directory).await;
        let Some(root) = p.root else {
            return GitStatus {
                is_repo: false,
                root: None,
                branch: None,
                upstream: None,
                ahead: 0,
                behind: 0,
                files: vec![],
                error: Some(p.error.unwrap_or_else(|| {
                    format!("Not a git repository: {}", self.working_directory.display())
                })),
            };
        };
        let r = run(
            Path::new(&root),
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--branch",
                "--untracked-files=all",
            ],
            None,
            None,
            None,
        )
        .await;
        if !r.ok() {
            return GitStatus {
                is_repo: true,
                root: Some(root),
                branch: None,
                upstream: None,
                ahead: 0,
                behind: 0,
                files: vec![],
                error: Some(r.error.trim().to_string()),
            };
        }
        parse_status(&root, &r.output)
    }

    /// Both sides of one file's change. staged: HEAD vs the index. Otherwise: the index (or
    /// HEAD, for a file not in the index) vs the working tree.
    pub async fn diff(
        &self,
        path: &str,
        staged: bool,
        orig_path: Option<&str>,
    ) -> Result<Option<GitDiff>, GitError> {
        let Some(root) = self.root().await else {
            return Ok(None);
        };
        let rel = normalize_relative(Some(path));
        let rel_orig = match orig_path {
            None => rel.clone(),
            Some(o) => normalize_relative(Some(o)),
        };
        let (Some(rel), Some(rel_orig)) = (rel, rel_orig) else {
            return Err(GitError::BadRequest(
                "Path must stay inside the repository.".into(),
            ));
        };
        let root_p = Path::new(&root);
        let (original, modified) = if staged {
            (
                show(root_p, &format!("HEAD:{rel_orig}"))
                    .await
                    .unwrap_or_default(),
                show(root_p, &format!(":{rel}")).await.unwrap_or_default(),
            )
        } else {
            let original = match show(root_p, &format!(":{rel}")).await {
                Some(b) => b,
                None => show(root_p, &format!("HEAD:{rel}"))
                    .await
                    .unwrap_or_default(),
            };
            let full = root_p.join(&rel);
            let modified = if full.is_file() {
                read_capped(&full).await
            } else {
                vec![]
            };
            (original, modified)
        };
        let too_large = original.len() > MAX_DIFF_BYTES || modified.len() > MAX_DIFF_BYTES;
        let binary = looks_binary(&original) || looks_binary(&modified);
        let hide = binary || too_large;
        Ok(Some(GitDiff {
            path: rel,
            original: if hide {
                String::new()
            } else {
                decode(&original)
            },
            modified: if hide {
                String::new()
            } else {
                decode(&modified)
            },
            binary,
            too_large,
        }))
    }

    /// A file (relative to the working folder) as committed at HEAD: (exists, text, binary).
    pub async fn head_content(&self, path: &str) -> Result<(bool, String, bool), GitError> {
        let rel = normalize_relative(Some(path))
            .ok_or_else(|| GitError::BadRequest("Path must stay inside the project.".into()))?;
        let Some(root) = self.root().await else {
            return Ok((false, String::new(), false));
        };
        let full = paths::normalize_path(&self.working_directory.join(&rel));
        let repo_rel = paths::to_relative(Path::new(&root), &full);
        if repo_rel.starts_with("..") {
            return Err(GitError::BadRequest(
                "Path must stay inside the repository.".into(),
            ));
        }
        let Some(bytes) = show(Path::new(&root), &format!("HEAD:{repo_rel}")).await else {
            return Ok((false, String::new(), false));
        };
        if bytes.len() > MAX_DIFF_BYTES || looks_binary(&bytes) {
            return Ok((true, String::new(), true));
        }
        Ok((true, decode(&bytes), false))
    }

    pub async fn stage(&self, paths: &[String]) -> Result<GitResult, GitError> {
        let root = match self.root().await {
            Some(r) => r,
            None => return Err(self.no_repo().await),
        };
        let rels = checked_paths(paths)?;
        let mut args = vec!["add", "-A", "--"];
        args.extend(rels.iter().map(String::as_str));
        Ok(run(Path::new(&root), &args, None, None, None).await)
    }

    pub async fn unstage(&self, paths: &[String]) -> Result<GitResult, GitError> {
        let root = match self.root().await {
            Some(r) => r,
            None => return Err(self.no_repo().await),
        };
        let rels = checked_paths(paths)?;
        let root = Path::new(&root);
        let has_head = run(
            root,
            &["rev-parse", "--verify", "-q", "HEAD"],
            None,
            None,
            None,
        )
        .await
        .ok();
        let mut args = if has_head {
            vec!["restore", "--staged", "--"]
        } else {
            vec!["rm", "--cached", "-q", "-r", "--"]
        };
        args.extend(rels.iter().map(String::as_str));
        Ok(run(root, &args, None, None, None).await)
    }

    pub async fn commit(&self, message: &str) -> Result<GitResult, GitError> {
        if message.trim().is_empty() {
            return Ok(GitResult::failed(2, "A commit message is required."));
        }
        let root = match self.root().await {
            Some(r) => r,
            None => return Err(self.no_repo().await),
        };
        // The message goes through stdin: no quoting, any length, any characters.
        Ok(run(
            Path::new(&root),
            &["commit", "-F", "-"],
            Some(message),
            None,
            None,
        )
        .await)
    }

    /// fetch, pull (fast-forward only) or push, with the project's token when it has one. A
    /// branch with no upstream is pushed to origin and tracked.
    pub async fn remote(&self, op: &str, token: Option<&str>) -> Result<GitResult, GitError> {
        let root = match self.root().await {
            Some(r) => r,
            None => return Err(self.no_repo().await),
        };
        let args: &[&str] = match op {
            "fetch" => &["fetch", "--prune", "--progress"],
            "pull" => &["pull", "--ff-only", "--progress"],
            "push" => &["push", "--progress"],
            _ => return Err(GitError::BadRequest(format!("Unknown operation: {op}"))),
        };
        let five_min = Some(Duration::from_secs(300));
        let root = Path::new(&root);
        let mut r = run(root, args, None, token, five_min).await;
        if op == "push" && !r.ok() && r.error.contains("has no upstream branch") {
            r = run(
                root,
                &["push", "--progress", "-u", "origin", "HEAD"],
                None,
                token,
                five_min,
            )
            .await;
        }
        Ok(r)
    }

    /// The URL of origin, or `None`.
    pub async fn origin(&self) -> Option<String> {
        let r = run(
            &self.working_directory,
            &["remote", "get-url", "origin"],
            None,
            None,
            None,
        )
        .await;
        r.ok().then(|| r.output.trim().to_string())
    }
}

/// `git rev-parse --show-toplevel` in `dir`: the root, or why not.
pub async fn probe(dir: &Path) -> RepoProbe {
    let r = match run_checked(
        dir,
        &["rev-parse", "--show-toplevel"],
        None,
        None,
        Some(Duration::from_secs(20)),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return RepoProbe {
                root: None,
                error: Some(format!("Can't run git: {e}")),
            };
        }
    };
    if r.ok() {
        RepoProbe {
            root: Some(
                paths::normalize(r.output.trim())
                    .to_string_lossy()
                    .to_string(),
            ),
            error: None,
        }
    } else {
        RepoProbe {
            root: None,
            error: failure_reason(&r.error),
        }
    }
}

/// `None` when git's stderr says "not a git repository"; else its message.
pub fn failure_reason(stderr: &str) -> Option<String> {
    let text = stderr.trim();
    if text.is_empty() {
        return Some("git rev-parse failed.".into());
    }
    if text.to_lowercase().contains("not a git repository") {
        return None;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    Some(if units.len() > 1000 {
        format!("{}…", String::from_utf16_lossy(&units[..1000]))
    } else {
        text.to_string()
    })
}

pub fn parse_status(root: &str, porcelain: &str) -> GitStatus {
    let entries: Vec<&str> = porcelain.split('\0').collect();
    let (mut branch, mut upstream, mut ahead, mut behind) = (None, None, 0, 0);
    let mut files = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let e = entries[i];
        i += 1;
        if e.is_empty() {
            continue;
        }
        if let Some(h) = e.strip_prefix("## ") {
            (branch, upstream, ahead, behind) = parse_branch(h);
            continue;
        }
        let chars: Vec<char> = e.chars().collect();
        if chars.len() < 4 {
            continue;
        }
        let (x, y) = (chars[0], chars[1]);
        let path: String = chars[3..].iter().collect();
        let mut orig = None;
        // Renames and copies carry the old path as the next NUL-separated field.
        if (x == 'R' || x == 'C') && i < entries.len() {
            orig = Some(entries[i].to_string());
            i += 1;
        }
        files.push(GitFile {
            path,
            orig_path: orig,
            index: x,
            work_tree: y,
        });
    }
    GitStatus {
        is_repo: true,
        root: Some(root.to_string()),
        branch,
        upstream,
        ahead,
        behind,
        files,
        error: None,
    }
}

fn parse_branch(header: &str) -> (Option<String>, Option<String>, i32, i32) {
    // "main...origin/main [ahead 1, behind 2]" · "No commits yet on main" · "HEAD (no branch)"
    let (mut ahead, mut behind) = (0, 0);
    let mut header = header.to_string();
    if let Some(bracket) = header.find(" [") {
        let tail = header[bracket + 2..].trim_end_matches(']').to_string();
        for part in tail.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.parse().unwrap_or(0);
            }
            if let Some(n) = part.strip_prefix("behind ") {
                behind = n.parse().unwrap_or(0);
            }
        }
        header.truncate(bracket);
    }
    if let Some(b) = header.strip_prefix("No commits yet on ") {
        return (Some(b.into()), None, 0, 0);
    }
    if let Some(b) = header.strip_prefix("Initial commit on ") {
        return (Some(b.into()), None, 0, 0);
    }
    match header.find("...") {
        Some(dots) => (
            Some(header[..dots].into()),
            Some(header[dots + 3..].into()),
            ahead,
            behind,
        ),
        None => (Some(header), None, ahead, behind),
    }
}

fn checked_paths(paths: &[String]) -> Result<Vec<String>, GitError> {
    if paths.is_empty() {
        return Err(GitError::BadRequest("No paths given.".into()));
    }
    paths
        .iter()
        .map(|p| {
            normalize_relative(Some(p)).ok_or_else(|| {
                GitError::BadRequest(format!("Path must stay inside the repository: {p}"))
            })
        })
        .collect()
}

/// A repository-relative path with forward slashes, or `None` if it escapes the repository.
pub fn normalize_relative(path: Option<&str>) -> Option<String> {
    let path = path?;
    if path.trim().is_empty() || path.contains('\0') {
        return None;
    }
    let p = path.replace('\\', "/");
    let p = p.trim();
    if p.starts_with('/') || Path::new(p).is_absolute() || p.contains(':') {
        return None;
    }
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.contains(&"..") || parts.is_empty() {
        return None;
    }
    Some(
        parts
            .into_iter()
            .filter(|s| *s != ".")
            .collect::<Vec<_>>()
            .join("/"),
    )
}

pub fn looks_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

/// UTF-8 without a BOM (invalid bytes replaced, as .NET does).
pub fn decode(data: &[u8]) -> String {
    let start = if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    String::from_utf8_lossy(&data[start..]).into_owned()
}

async fn read_capped(path: &Path) -> Vec<u8> {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.len() as usize > MAX_DIFF_BYTES => vec![0u8; MAX_DIFF_BYTES + 1],
        _ => tokio::fs::read(path).await.unwrap_or_default(),
    }
}

async fn show(root: &Path, spec: &str) -> Option<Vec<u8>> {
    let (code, bytes, _) = run_raw(root, &["show", spec], None, None, None)
        .await
        .ok()?;
    (code == 0).then_some(bytes)
}

/// Run git; a failure to start becomes exit code -1 with the reason as stderr.
pub async fn run(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    token: Option<&str>,
    timeout: Option<Duration>,
) -> GitResult {
    match run_checked(cwd, args, stdin, token, timeout).await {
        Ok(r) => r,
        Err(e) => GitResult::failed(-1, &e),
    }
}

/// `GitService.RunAsync`: retries a few times when another git holds `index.lock`; the token
/// is scrubbed from the output.
pub async fn run_checked(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    token: Option<&str>,
    timeout: Option<Duration>,
) -> Result<GitResult, String> {
    let (mut code, mut bytes, mut err) = run_raw(cwd, args, stdin, token, timeout).await?;
    let mut attempt = 0;
    while code != 0 && attempt < 3 && err.contains("index.lock") {
        tokio::time::sleep(Duration::from_millis(300 * (attempt + 1))).await;
        (code, bytes, err) = run_raw(cwd, args, stdin, token, timeout).await?;
        attempt += 1;
    }
    let output = String::from_utf8_lossy(&bytes).into_owned();
    Ok(match token {
        None => GitResult {
            exit_code: code,
            output,
            error: err,
            code: None,
        },
        Some(t) => GitResult {
            exit_code: code,
            output: scrub_text(&output, Some(t)),
            error: scrub_text(&err, Some(t)),
            code: None,
        },
    })
}

pub const TOKEN_FILE_VARIABLE: &str = "OPENORCA_GIT_TOKEN_FILE";

/// The -c options that install the credential helper (no secret in them).
pub fn helper_args() -> Vec<String> {
    let helper = format!(
        "!f() {{ test \"$1\" = get || exit 0; echo username=x-access-token; printf 'password='; cat \"${TOKEN_FILE_VARIABLE}\"; echo; }}; f"
    );
    vec![
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        format!("credential.helper={helper}"),
        "-c".into(),
        "credential.useHttpPath=false".into(),
    ]
}

/// Removes the token (and an URL-embedded credential) from text git printed.
pub fn scrub_text(text: &str, token: Option<&str>) -> String {
    static URL_CRED: OnceLock<Regex> = OnceLock::new();
    let mut t = text.to_string();
    if let Some(tok) = token.filter(|t| !t.is_empty()) {
        t = t.replace(tok, "***");
    }
    URL_CRED
        .get_or_init(|| Regex::new(r"(https?://)[^/\s@]+@").unwrap())
        .replace_all(&t, "${1}***@")
        .into_owned()
}

/// Sets the no-prompt environment on a git command (`GitAuth.Apply` without a token).
pub fn no_prompt_env(cmd: &mut tokio::process::Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
}

/// A private temp folder holding the token for one command; removed on drop.
pub struct TokenFile {
    dir: Option<tempfile::TempDir>,
}

impl TokenFile {
    /// Writes the token (when given) and adds the helper arguments and variable to `cmd`.
    pub fn apply(cmd: &mut tokio::process::Command, token: Option<&str>) -> std::io::Result<Self> {
        no_prompt_env(cmd);
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Ok(Self { dir: None });
        };
        let dir = tempfile::Builder::new().prefix("oo-git-").tempdir()?;
        let file = dir.path().join("t");
        std::fs::write(&file, token)?;
        cmd.env(TOKEN_FILE_VARIABLE, &file);
        cmd.args(helper_args());
        Ok(Self { dir: Some(dir) })
    }

    pub fn is_active(&self) -> bool {
        self.dir.is_some()
    }
}

async fn run_raw(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    token: Option<&str>,
    timeout: Option<Duration>,
) -> Result<(i32, Vec<u8>, String), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(cwd);
    // Paths as stored (no octal quoting), no pager, no prompts, no colour.
    cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"]);
    let _auth = TokenFile::apply(&mut cmd, token).map_err(|e| e.to_string())?;
    cmd.args(args);
    // The panel polls git status: it mustn't take index.lock.
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let Some(mut si) = child.stdin.take() {
        if let Some(text) = stdin {
            let _ = si.write_all(text.as_bytes()).await;
        }
        drop(si);
    }
    let fut = child.wait_with_output();
    let out = match tokio::time::timeout(timeout.unwrap_or(Duration::from_secs(60)), fut).await {
        Ok(r) => r.map_err(|e| e.to_string())?,
        Err(_) => return Err("git timed out.".into()),
    };
    Ok((
        out.status.code().unwrap_or(-1),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_branch_renames_and_untracked() {
        let s = parse_status(
            "/r",
            "## main...origin/main [ahead 2, behind 1]\0 M a.txt\0R  new.txt\0old.txt\0?? sub/x y.md\0",
        );
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.files.len(), 3);
        assert!(s.files[0].unstaged() && !s.files[0].staged());
        assert_eq!(s.files[1].orig_path.as_deref(), Some("old.txt"));
        assert!(s.files[1].staged());
        assert!(s.files[2].untracked());
        let empty = parse_status("/r", "## No commits yet on main\0");
        assert_eq!(empty.branch.as_deref(), Some("main"));
    }

    #[test]
    fn relative_paths_stay_inside() {
        assert_eq!(
            normalize_relative(Some("a\\b/./c")).as_deref(),
            Some("a/b/c")
        );
        assert_eq!(normalize_relative(Some("../x")), None);
        assert_eq!(normalize_relative(Some("/x")), None);
        assert_eq!(normalize_relative(Some("c:x")), None);
        assert_eq!(normalize_relative(Some(" ")), None);
    }

    #[test]
    fn scrub_hides_tokens_and_url_credentials() {
        assert_eq!(
            scrub_text("push https://u:secret@github.com/x tok123", Some("tok123")),
            "push https://***@github.com/x ***"
        );
    }

    #[test]
    fn failure_reason_is_none_for_plain_not_a_repo() {
        assert_eq!(
            failure_reason("fatal: not a git repository (or any parent)"),
            None
        );
        assert_eq!(
            failure_reason("fatal: detected dubious ownership").as_deref(),
            Some("fatal: detected dubious ownership")
        );
    }
}
