//! `CheckpointManager`: a copy of each file before the first modification in a session, so it
//! can be diffed or restored.
//!
//! Layout (same as C#): `<config dir>/checkpoints/<session id>/manifest.json` (a JSON array of
//! [`CheckpointEntry`], `JsonDefaults` format) and one `yyyyMMdd_HHmmss_<hash>.bak` per file,
//! `<hash>` being the first 16 hex digits (lower case) of the SHA-256 of the full path.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::datetime::DotNetDateTime;
use crate::config::{default_config_dir, dotnet_json};

/// `CheckpointEntry`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CheckpointEntry {
    pub file_path: String,
    pub backup_file: String,
    pub timestamp: DotNetDateTime,
    pub size_bytes: i64,
}

pub struct CheckpointManager {
    base_dir: PathBuf,
    /// Parallel tool calls race on the same manifest.json.
    manifest_lock: Mutex<()>,
}

impl Default for CheckpointManager {
    /// `<config dir>/checkpoints`.
    fn default() -> Self {
        Self::new(default_config_dir().join("checkpoints"))
    }
}

impl CheckpointManager {
    /// Checkpoints under `base_dir` (C#'s test constructor; the app uses [`Default`]).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            manifest_lock: Mutex::new(()),
        }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    fn manifest_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("manifest.json")
    }

    /// `SnapshotAsync`: back up `file_path` unless it does not exist or this session already
    /// has a checkpoint of it.
    pub fn snapshot(&self, file_path: &Path, session_id: &str) -> io::Result<()> {
        let file_path = full_path(file_path);
        if !file_path.is_file() {
            return Ok(());
        }
        let _guard = self.manifest_lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;
        let mut manifest = self.load_manifest(session_id);
        let key = file_path.display().to_string();
        if manifest.iter().any(|e| same_path(&e.file_path, &key)) {
            return Ok(());
        }
        let timestamp = DotNetDateTime::now();
        let backup_name = format!("{}_{}.bak", timestamp.compact_stamp(), path_hash(&key));
        copy_with_retry(&file_path, &dir.join(&backup_name))?;
        let size = std::fs::metadata(&file_path)?.len() as i64;
        manifest.push(CheckpointEntry {
            file_path: key,
            backup_file: backup_name,
            timestamp,
            size_bytes: size,
        });
        self.save_manifest(session_id, &manifest)
    }

    /// `ListAsync`: the session's checkpoints, oldest first.
    pub fn list(&self, session_id: &str) -> Vec<CheckpointEntry> {
        self.load_manifest(session_id)
    }

    fn find(&self, file_path: &Path, session_id: &str) -> (String, Option<CheckpointEntry>) {
        let key = full_path(file_path).display().to_string();
        let entry = self
            .load_manifest(session_id)
            .into_iter()
            .find(|e| same_path(&e.file_path, &key));
        (key, entry)
    }

    /// `DiffAsync`: a simple line diff between the checkpoint and the file now.
    pub fn diff(&self, file_path: &Path, session_id: &str) -> io::Result<String> {
        let (key, entry) = self.find(file_path, session_id);
        let Some(entry) = entry else {
            return Ok(format!("No checkpoint found for: {key}"));
        };
        let backup = self.session_dir(session_id).join(&entry.backup_file);
        if !backup.is_file() {
            return Ok("Checkpoint backup file is missing.".into());
        }
        let original = read_text(&backup)?;
        let current = if Path::new(&key).is_file() {
            Some(read_text(Path::new(&key))?)
        } else {
            None
        };
        let original_lines = split_all(&original);
        let current_lines = current.as_deref().map(split_all).unwrap_or_default();
        Ok(simple_diff(&original_lines, &current_lines, &key))
    }

    /// `RestoreAsync`: copy the checkpoint back. False when there is none (or its backup is gone).
    pub fn restore(&self, file_path: &Path, session_id: &str) -> io::Result<bool> {
        let (key, entry) = self.find(file_path, session_id);
        let Some(entry) = entry else {
            return Ok(false);
        };
        let backup = self.session_dir(session_id).join(&entry.backup_file);
        if !backup.is_file() {
            return Ok(false);
        }
        let target = PathBuf::from(&key);
        if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        copy_with_retry(&backup, &target)?;
        Ok(true)
    }

    /// `Cleanup`: delete all checkpoints of a session.
    pub fn cleanup(&self, session_id: &str) -> io::Result<()> {
        let dir = self.session_dir(session_id);
        if dir.is_dir() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// `CleanupOlderThan`: delete session directories not written to within `max_age`.
    /// Returns how many were removed.
    pub fn cleanup_older_than(&self, max_age: Duration) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.base_dir) else {
            return 0;
        };
        let cutoff = SystemTime::now().checked_sub(max_age);
        let mut removed = 0;
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let old = match (e.metadata().and_then(|m| m.modified()), cutoff) {
                (Ok(m), Some(c)) => m < c,
                _ => false,
            };
            if old && std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    fn load_manifest(&self, session_id: &str) -> Vec<CheckpointEntry> {
        let path = self.manifest_path(session_id);
        let Ok(bytes) = std::fs::read(&path) else {
            return Vec::new();
        };
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        match serde_json::from_slice::<Option<Vec<CheckpointEntry>>>(bytes) {
            Ok(m) => m.unwrap_or_default(),
            Err(_) => {
                // Corrupted: delete and start fresh rather than silently losing history.
                let _ = std::fs::remove_file(&path);
                Vec::new()
            }
        }
    }

    fn save_manifest(&self, session_id: &str, manifest: &[CheckpointEntry]) -> io::Result<()> {
        let path = self.manifest_path(session_id);
        let json = dotnet_json::to_string_dotnet(manifest).map_err(io::Error::other)?;
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}

/// `Path.GetFullPath` against the process directory.
fn full_path(p: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    orca_abstractions::full_path(&cwd, &p.to_string_lossy())
}

fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// `ComputeHash`: SHA-256 of the UTF-8 path, first 16 hex digits, lower case.
pub fn path_hash(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// A byte copy that gives the target a fresh write time (a FileStream copy in C#), retried on
/// I/O errors (locked files) up to three times with 50/100/200 ms pauses.
fn copy_with_retry(source: &Path, dest: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        let result = (|| {
            let mut from = std::fs::File::open(source)?;
            let mut to = std::fs::File::create(dest)?;
            io::copy(&mut from, &mut to).map(|_| ())
        })();
        match result {
            Err(_) if attempt < 3 => {
                std::thread::sleep(Duration::from_millis(50 << attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn read_text(p: &Path) -> io::Result<String> {
    Ok(orca_tools_common::decode_text(&std::fs::read(p)?))
}

/// `Split(["\r\n", "\r", "\n"], None)`: a trailing line break gives a trailing empty line.
fn split_all(text: &str) -> Vec<String> {
    let mut lines = orca_tools_common::split_lines(text)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if text.is_empty() || text.ends_with(['\n', '\r']) {
        lines.push(String::new());
    }
    lines
}

/// `GenerateSimpleDiff`: line-by-line comparison, a hunk header where a run of changes starts.
fn simple_diff(original: &[String], current: &[String], file_path: &str) -> String {
    use orca_tools_common::append_line;
    let mut sb = String::new();
    append_line(&mut sb, &format!("--- {file_path} (checkpoint)"));
    append_line(&mut sb, &format!("+++ {file_path} (current)"));
    let mut in_hunk = false;
    for i in 0..original.len().max(current.len()) {
        let (o, c) = (original.get(i), current.get(i));
        if o != c {
            if !in_hunk {
                append_line(&mut sb, &format!("@@ line {} @@", i + 1));
                in_hunk = true;
            }
            if let Some(o) = o {
                append_line(&mut sb, &format!("- {o}"));
            }
            if let Some(c) = c {
                append_line(&mut sb, &format!("+ {c}"));
            }
        } else {
            in_hunk = false;
        }
    }
    sb.trim_end().to_string()
}
