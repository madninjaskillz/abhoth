//! `FileService`: the file tree and editor's disk access inside one root (a project or one of
//! its worktrees). Every path goes through [`crate::paths::resolve`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::paths;

pub const MAX_ENTRIES: usize = 5000;
pub const MAX_EDIT_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub ignored: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub etag: String,
    pub binary: bool,
    pub too_large: bool,
    pub bom: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteResult {
    pub ok: bool,
    pub etag: Option<String>,
    pub conflict: bool,
    pub error: Option<String>,
}

/// Why a file operation failed (C#'s exception types, mapped to status codes by the endpoints).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    /// `UnauthorizedAccessException`: outside the project (403), or a folder it can't read.
    #[error("{0}")]
    Forbidden(String),
    /// `FileNotFoundException` / `DirectoryNotFoundException` (404).
    #[error("{0}")]
    NotFound(String),
    /// `IOException` (500).
    #[error("{0}")]
    Io(String),
}

pub struct FileService {
    pub root: PathBuf,
}

/// `{length:x}-{LastWriteTimeUtc.Ticks:x}`.
pub fn etag(meta: &std::fs::Metadata) -> String {
    let ticks = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() / 100 + 621_355_968_000_000_000)
        .unwrap_or(0);
    format!("{:x}-{:x}", meta.len(), ticks)
}

impl FileService {
    pub fn new(root: &Path) -> Self {
        Self {
            root: paths::normalize_path(root),
        }
    }

    pub fn resolve(&self, relative: Option<&str>) -> Result<PathBuf, FileError> {
        paths::resolve(&self.root, relative).ok_or_else(|| {
            FileError::Forbidden(format!(
                "Path is outside the project: {}",
                relative.unwrap_or("")
            ))
        })
    }

    /// One folder's children (folders first). Git-ignored entries are left out unless `show_ignored`.
    pub async fn list(
        &self,
        relative_dir: Option<&str>,
        show_ignored: bool,
    ) -> Result<FileListing, FileError> {
        let dir = self.resolve(relative_dir)?;
        if !dir.is_dir() {
            return Err(FileError::NotFound(format!(
                "No such folder: {}",
                relative_dir.unwrap_or("")
            )));
        }
        if let Some(problem) = paths::read_problem(&dir) {
            return Err(FileError::Forbidden(problem));
        }
        let rel_dir = paths::to_relative(&self.root, &dir);
        let mut entries = Vec::new();
        let mut truncated = false;
        if let Ok(it) = std::fs::read_dir(&dir) {
            for e in it.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == ".git" {
                    continue;
                }
                if entries.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                let meta = std::fs::metadata(e.path()).or_else(|_| e.metadata());
                let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
                let rel = if rel_dir.is_empty() {
                    name.clone()
                } else {
                    format!("{rel_dir}/{name}")
                };
                entries.push(FileEntry {
                    name,
                    path: rel,
                    dir: is_dir,
                    ignored: false,
                    size: if is_dir {
                        None
                    } else {
                        meta.ok().map(|m| m.len())
                    },
                });
            }
        }
        if !entries.is_empty() && self.is_git_work_tree() {
            let ignored = self.ignored(&entries).await;
            if !ignored.is_empty() {
                if show_ignored {
                    for e in &mut entries {
                        if ignored.contains(&paths::key(&e.path)) {
                            e.ignored = true;
                        }
                    }
                } else {
                    entries.retain(|e| !ignored.contains(&paths::key(&e.path)));
                }
            }
        }
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(FileListing {
            path: rel_dir,
            entries,
            truncated,
        })
    }

    fn is_git_work_tree(&self) -> bool {
        // The root or any parent holds .git (a folder, or a file for a linked worktree).
        self.root.ancestors().any(|d| d.join(".git").exists())
    }

    /// Which of these entries git ignores (one `git check-ignore` for the whole folder).
    async fn ignored(&self, entries: &[FileEntry]) -> HashSet<String> {
        let mut input = String::new();
        for e in entries {
            input.push_str(&e.path);
            if e.dir {
                input.push('/');
            }
            input.push('\0');
        }
        let r = crate::git::service::run(
            &self.root,
            &["check-ignore", "-z", "--stdin"],
            Some(&input),
            None,
            None,
        )
        .await;
        // Exit 1 = nothing ignored; 128 = not a repository (then nothing is).
        if r.exit_code != 0 {
            return HashSet::new();
        }
        r.output
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| paths::key(p.trim_end_matches('/')))
            .collect()
    }

    pub async fn read(&self, relative: &str) -> Result<FileContent, FileError> {
        let full = self.resolve(Some(relative))?;
        let meta = std::fs::metadata(&full)
            .ok()
            .filter(|m| m.is_file())
            .ok_or_else(|| FileError::NotFound(format!("No such file: {relative}")))?;
        let rel = paths::to_relative(&self.root, &full);
        let tag = etag(&meta);
        if meta.len() > MAX_EDIT_BYTES {
            return Ok(FileContent {
                path: rel,
                content: String::new(),
                etag: tag,
                binary: false,
                too_large: true,
                bom: false,
                size: meta.len(),
            });
        }
        let bytes = tokio::fs::read(&full)
            .await
            .map_err(|e| FileError::Io(e.to_string()))?;
        if crate::git::service::looks_binary(&bytes) {
            return Ok(FileContent {
                path: rel,
                content: String::new(),
                etag: tag,
                binary: true,
                too_large: false,
                bom: false,
                size: meta.len(),
            });
        }
        let bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
        Ok(FileContent {
            path: rel,
            content: crate::git::service::decode(&bytes),
            etag: tag,
            binary: false,
            too_large: false,
            bom,
            size: meta.len(),
        })
    }

    /// Save `content`. With `base_etag`, refuses (conflict) when the file changed on disk since
    /// it was read; a new file's parent folder must exist.
    pub async fn write(
        &self,
        relative: &str,
        content: &str,
        base_etag: Option<&str>,
        bom: bool,
    ) -> Result<FileWriteResult, FileError> {
        let full = self.resolve(Some(relative))?;
        let fail = |e: &str| FileWriteResult {
            ok: false,
            etag: None,
            conflict: false,
            error: Some(e.into()),
        };
        if full == self.root || full.is_dir() {
            return Ok(fail("That is a folder."));
        }
        let Some(parent) = full.parent().filter(|p| p.is_dir()) else {
            return Ok(fail("The folder does not exist."));
        };
        if let Ok(meta) = std::fs::metadata(&full)
            && let Some(base) = base_etag.filter(|b| !b.is_empty())
            && etag(&meta) != base
        {
            return Ok(FileWriteResult {
                ok: false,
                etag: Some(etag(&meta)),
                conflict: true,
                error: Some("The file changed on disk since it was opened.".into()),
            });
        }
        if content.len() as u64 > MAX_EDIT_BYTES {
            return Ok(fail("Too large to save from the editor."));
        }
        let mut bytes = Vec::with_capacity(content.len() + 3);
        if bom {
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bytes.extend_from_slice(content.as_bytes());
        // Write next to it, then swap in: a crash never leaves half a file.
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let temp = parent.join(format!(".{name}.openorca-{}.tmp", crate::util::short_id(8)));
        tokio::fs::write(&temp, &bytes).await.map_err(io_error)?;
        if let Err(e) = std::fs::rename(&temp, &full) {
            let _ = std::fs::remove_file(&temp);
            return Err(io_error(e));
        }
        let meta = std::fs::metadata(&full).map_err(io_error)?;
        Ok(FileWriteResult {
            ok: true,
            etag: Some(etag(&meta)),
            conflict: false,
            error: None,
        })
    }
}

fn io_error(e: std::io::Error) -> FileError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        FileError::Forbidden(e.to_string())
    } else {
        FileError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_write_with_etag_conflicts_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let svc = FileService::new(dir.path());
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "\u{feff}hello").unwrap();
        let c = svc.read("sub/a.txt").await.unwrap();
        assert_eq!((c.content.as_str(), c.bom), ("hello", true));
        let w = svc
            .write("sub/a.txt", "bye", Some(&c.etag), true)
            .await
            .unwrap();
        assert!(w.ok);
        assert_eq!(
            std::fs::read(dir.path().join("sub/a.txt")).unwrap(),
            b"\xEF\xBB\xBFbye"
        );
        let stale = svc
            .write("sub/a.txt", "again", Some("0-0"), false)
            .await
            .unwrap();
        assert!(stale.conflict);
        assert_eq!(
            svc.write("nope/x.txt", "x", None, false)
                .await
                .unwrap()
                .error
                .as_deref(),
            Some("The folder does not exist.")
        );
        assert!(matches!(
            svc.read("../x").await,
            Err(FileError::Forbidden(_))
        ));
        let listing = svc.list(None, false).await.unwrap();
        assert_eq!(listing.entries[0].name, "sub");
        assert!(listing.entries[0].dir);
    }
}
