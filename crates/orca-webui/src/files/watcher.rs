//! `FolderWatcher` / `FolderPoller`: reports which folders of a worktree changed, as a list
//! of relative folders or `None` for "reload everything".
//!
//! The port polls (decision D69): every few seconds `git status` (plus each listed file's size
//! and mtime), the mtimes of `.git/index` and `HEAD`, and the mtimes of the folders near the
//! top of the tree (a created, deleted or renamed entry changes its folder's mtime). That is
//! the C# poller, which C# uses for network shares; it avoids a file-watching dependency.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crate::paths;

pub type ChangedFn = Arc<dyn Fn(Option<Vec<String>>) + Send + Sync>;

const MAX_DIRS: usize = 2000;
const MAX_DEPTH: usize = 4;
const MAX_REPORTED: usize = 200;
const SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "bin",
    "obj",
    ".vs",
    "TestResults",
    "target",
];

/// The polling interval: 4 s on network shares (as C#), 2 s elsewhere.
pub fn default_interval(root: &Path) -> Duration {
    if is_network_path(&root.to_string_lossy()) {
        Duration::from_secs(4)
    } else {
        Duration::from_secs(2)
    }
}

pub struct FolderPoller {
    root: PathBuf,
    changed: ChangedFn,
    last: tokio::sync::Mutex<Option<BTreeMap<String, String>>>,
}

impl FolderPoller {
    pub fn new(root: &Path, changed: ChangedFn) -> Arc<Self> {
        Arc::new(Self {
            root: paths::normalize_path(root),
            changed,
            last: tokio::sync::Mutex::new(None),
        })
    }

    /// One poll: the first records the baseline; later ones report differences.
    pub async fn tick(&self) {
        let Ok(mut last) = self.last.try_lock() else {
            return; // the previous tick is still running
        };
        let now = self.snapshot().await;
        let before = last.replace(now.clone());
        let Some(before) = before else { return };
        let mut dirs = BTreeSet::new();
        for (k, v) in &now {
            if before.get(k) != Some(v) {
                dirs.insert(dir_of(k));
            }
        }
        for k in before.keys() {
            if !now.contains_key(k) {
                dirs.insert(dir_of(k));
            }
        }
        if dirs.is_empty() {
            return;
        }
        let report = (dirs.len() <= MAX_REPORTED).then(|| dirs.into_iter().collect());
        (self.changed)(report);
    }

    /// Keys: "s:<path>" status entries, "d:<dir>" folder mtimes, "g:<file>" git's index/HEAD.
    async fn snapshot(&self) -> BTreeMap<String, String> {
        let mut snap = BTreeMap::new();
        let status = crate::git::service::run(
            &self.root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            None,
            None,
            None,
        )
        .await;
        if status.ok() {
            for entry in status.output.split('\0').filter(|e| !e.is_empty()) {
                if entry.chars().count() < 4 {
                    continue; // the old name of a rename
                }
                let code: String = entry.chars().take(2).collect();
                let rel: String = entry.chars().skip(3).collect();
                let meta = std::fs::metadata(self.root.join(&rel))
                    .ok()
                    .filter(|m| m.is_file());
                let v = match meta {
                    Some(m) => format!("{code}|{}|{}", m.len(), mtime(&m)),
                    None => code,
                };
                snap.insert(format!("s:{}", rel.trim_end_matches('/')), v);
            }
            for g in ["index", "HEAD"] {
                if let Ok(m) = std::fs::metadata(self.root.join(".git").join(g)) {
                    snap.insert(format!("g:.git/{g}"), mtime(&m));
                }
            }
        }
        let root = self.root.clone();
        let dirs = tokio::task::spawn_blocking(move || folder_mtimes(&root))
            .await
            .unwrap_or_default();
        snap.extend(dirs);
        snap
    }
}

fn mtime(m: &std::fs::Metadata) -> String {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default()
}

fn folder_mtimes(root: &Path) -> BTreeMap<String, String> {
    let mut snap = BTreeMap::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), String::new(), 0usize)]);
    let mut seen = 0;
    while let Some((dir, rel, depth)) = queue.pop_front() {
        if seen >= MAX_DIRS {
            break;
        }
        seen += 1;
        let Ok(m) = std::fs::metadata(&dir) else {
            continue;
        };
        snap.insert(format!("d:{rel}"), mtime(&m));
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subs: Vec<(PathBuf, String)> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.path(), e.file_name().to_string_lossy().to_string()))
            .filter(|(_, n)| !SKIP.iter().any(|s| s.eq_ignore_ascii_case(n)))
            .collect();
        subs.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, name) in subs {
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            queue.push_back((path, r, depth + 1));
        }
    }
    snap
}

/// The folder a snapshot key belongs to, in the watcher's terms.
fn dir_of(key: &str) -> String {
    let path = &key[2..];
    match &key[..1] {
        "g" => ".git".into(),
        "d" => path.into(),
        _ => path
            .rfind('/')
            .map_or(String::new(), |i| path[..i].to_string()),
    }
}

/// Polls a folder in the background until dropped.
pub struct FolderWatcher {
    task: tokio::task::JoinHandle<()>,
}

impl FolderWatcher {
    pub fn start(root: &Path, changed: ChangedFn) -> Self {
        let interval = default_interval(root);
        Self::start_with(root, changed, interval)
    }

    pub fn start_with(root: &Path, changed: ChangedFn, interval: Duration) -> Self {
        let poller = FolderPoller::new(root, changed);
        let task = tokio::spawn(async move {
            loop {
                poller.tick().await;
                tokio::time::sleep(interval).await;
            }
        });
        Self { task }
    }
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// True when `path` is on a network file system (NFS, SMB/CIFS, sshfs);
/// OPENORCA_POLL_FOLDERS=1 forces it, =0 turns it off.
pub fn is_network_path(path: &str) -> bool {
    match orca_core::brand::env_var("POLL_FOLDERS")
        .ok_or(std::env::VarError::NotPresent)
        .as_deref()
    {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    if cfg!(windows) {
        return path.starts_with(r"\\");
    }
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => network_fs_type(text.lines(), &paths::normalize(path).to_string_lossy()),
        Err(_) => false,
    }
}

/// The longest mount point containing `path` is a network file system.
pub fn network_fs_type<'a>(mountinfo: impl IntoIterator<Item = &'a str>, path: &str) -> bool {
    let mut best: Option<String> = None;
    let mut kind: Option<String> = None;
    for line in mountinfo {
        // id parent major:minor root mountpoint options [optional...] - fstype source superoptions
        let parts: Vec<&str> = line.split(' ').collect();
        let Some(dash) = parts.iter().position(|p| *p == "-") else {
            continue;
        };
        if parts.len() < 5 || dash + 1 >= parts.len() {
            continue;
        }
        let mount = unescape(parts[4]);
        let inside = mount == "/"
            || path == mount
            || path.starts_with(&format!("{}/", mount.trim_end_matches('/')));
        if inside && best.as_ref().is_none_or(|b| mount.len() >= b.len()) {
            best = Some(mount);
            kind = Some(parts[dash + 1].to_string());
        }
    }
    kind.is_some_and(|t| {
        t.starts_with("nfs")
            || matches!(t.as_str(), "cifs" | "smb3" | "smbfs" | "fuse.sshfs" | "9p")
    })
}

fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 3 < chars.len() && chars[i + 1].is_ascii_digit() {
            let oct: String = chars[i + 1..i + 4].iter().collect();
            if let Ok(n) = u32::from_str_radix(&oct, 8)
                && let Some(c) = char::from_u32(n)
            {
                out.push(c);
            }
            i += 4;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok);
    }

    #[tokio::test]
    async fn reports_new_files_edits_to_changed_files_and_new_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "1").unwrap();
        let seen: Arc<Mutex<Vec<Option<Vec<String>>>>> = Arc::default();
        let s2 = seen.clone();
        let poller = FolderPoller::new(root, Arc::new(move |d| s2.lock().unwrap().push(d)));
        poller.tick().await; // the baseline
        poller.tick().await;
        seen.lock().unwrap().clear();
        let has = |seen: &Arc<Mutex<Vec<Option<Vec<String>>>>>, d: &str| {
            seen.lock()
                .unwrap()
                .iter()
                .any(|x| x.as_ref().is_some_and(|v| v.iter().any(|s| s == d)))
        };
        std::fs::write(root.join("src/b.txt"), "new").unwrap();
        poller.tick().await;
        assert!(has(&seen, "src"));
        seen.lock().unwrap().clear();
        poller.tick().await;
        assert!(seen.lock().unwrap().is_empty(), "nothing changed");
        // An already-untracked file edited again (the status line doesn't change; size does).
        std::fs::write(root.join("src/a.txt"), "1 and more").unwrap();
        poller.tick().await;
        assert!(has(&seen, "src"));
        seen.lock().unwrap().clear();
        std::fs::create_dir_all(root.join("docs/deep")).unwrap();
        poller.tick().await;
        assert!(has(&seen, "docs"));
    }

    #[test]
    fn network_mounts_are_recognised() {
        let mountinfo = [
            "1 0 0:1 / / rw - overlay overlay rw",
            "2 1 0:2 / /projects rw - zfs Storage/ai/orca/projects rw",
            "3 1 0:3 / /projects-share rw,noatime - nfs4 nas-host:/export/projects rw,vers=4.1",
            "4 1 0:4 / /mnt/my\\040share rw - cifs //nas/share rw",
        ];
        assert!(network_fs_type(mountinfo, "/projects-share/demo"));
        assert!(network_fs_type(mountinfo, "/projects-share"));
        assert!(!network_fs_type(mountinfo, "/projects/demo"));
        assert!(!network_fs_type(mountinfo, "/projects-sharex"));
        assert!(network_fs_type(mountinfo, "/mnt/my share/x"));
        assert!(!network_fs_type(mountinfo, "/state"));
    }
}
