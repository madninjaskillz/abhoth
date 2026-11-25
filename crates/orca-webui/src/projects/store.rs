//! `ProjectStore` (the project list in `projects.json`, re-read before each change and written
//! through a temp file) and `EngineChoiceStore` (each project's engine, profile or mode).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;

/// A project: a folder the web UI works in (one tab).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub added_at: String,
    /// The user it belongs to (D140); none: the server's own (the primary project, projects
    /// from before accounts), which admins see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreFile {
    version: i32,
    projects: Option<Vec<ProjectInfo>>,
}

/// Next to projects.json: the folder of the last opened project.
const LAST_OPENED_FILE: &str = "last-project";

pub struct ProjectStore {
    file: PathBuf,
    lock: Mutex<()>,
}

/// The state home: `ABHOTH_HOME` / `OPENORCA_HOME`, else `~/.abhoth`.
pub fn openorca_home() -> PathBuf {
    orca_core::config::default_config_dir()
}

impl ProjectStore {
    pub fn new(file: impl Into<PathBuf>) -> Self {
        Self {
            file: file.into(),
            lock: Mutex::new(()),
        }
    }

    /// The default location: next to config.json.
    pub fn default_path() -> PathBuf {
        openorca_home().join("projects.json")
    }

    pub fn file_path(&self) -> &Path {
        &self.file
    }

    /// The folder the store and its companion files live in.
    pub fn state_dir(&self) -> PathBuf {
        let full = paths::normalize_path(&self.file);
        full.parent().map(Path::to_path_buf).unwrap_or(full)
    }

    /// Stable id for a folder (the same folder gets the same tab id after a restart). A user's
    /// project hashes the owner in too, so two users opening one folder get two projects;
    /// owner-less ones keep the id they had before accounts.
    pub fn id_for(path: &str, owner: Option<i64>) -> String {
        let mut norm = paths::normalize(path).to_string_lossy().to_string();
        if paths::case_insensitive() {
            norm = norm.to_uppercase();
        }
        if let Some(o) = owner {
            norm = format!(
                "user:{o}
{norm}"
            );
        }
        let hash = Sha256::digest(norm.as_bytes());
        crate::util::hex_lower(&hash)[..12].to_string()
    }

    pub fn load(&self) -> Vec<ProjectInfo> {
        let _g = self.lock.lock().unwrap();
        self.read()
    }

    /// Add a folder for `owner` (or return the existing entry for it). Either way an owner-less
    /// one (the desktop app's, or a token-mode web UI's) becomes the last opened project
    /// ([`Self::last_opened`]).
    pub fn add(&self, path: &str, name: Option<&str>, owner: Option<i64>) -> ProjectInfo {
        let full = paths::normalize(path).to_string_lossy().to_string();
        let _g = self.lock.lock().unwrap();
        if owner.is_none() {
            let _ = std::fs::write(self.state_dir().join(LAST_OPENED_FILE), &full);
        }
        let mut list = self.read();
        let id = Self::id_for(&full, owner);
        if let Some(existing) = list.iter().find(|p| p.id == id) {
            return existing.clone();
        }
        let info = ProjectInfo {
            id,
            name: match name.map(str::trim) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => default_name(&full),
            },
            path: full,
            added_at: crate::util::utc_now_iso(),
            owner,
        };
        list.push(info.clone());
        self.write(&list);
        info
    }

    /// The folder of the owner-less project opened last (started in, or added from the UI), while it is
    /// still in the list and exists. The desktop app opens it when launched from its install
    /// folder (D137).
    pub fn last_opened(&self) -> Option<PathBuf> {
        let _g = self.lock.lock().unwrap();
        let text = std::fs::read_to_string(self.state_dir().join(LAST_OPENED_FILE)).ok()?;
        let last = text.trim();
        let listed = self
            .read()
            .into_iter()
            .any(|p| paths::same_path(&p.path, last));
        let dir = PathBuf::from(last);
        (listed && dir.is_dir()).then_some(dir)
    }

    /// Forget a project (never touches its folder).
    pub fn remove(&self, id: &str) -> bool {
        let _g = self.lock.lock().unwrap();
        let mut list = self.read();
        let before = list.len();
        list.retain(|p| p.id != id);
        if list.len() == before {
            return false;
        }
        self.write(&list);
        true
    }

    fn read(&self) -> Vec<ProjectInfo> {
        let Ok(text) = std::fs::read_to_string(&self.file) else {
            return vec![];
        };
        let text = text.trim_start_matches('\u{feff}');
        let Ok(doc) = serde_json::from_str::<StoreFile>(text) else {
            return vec![];
        };
        doc.projects
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !p.path.is_empty())
            .map(|mut p| {
                p.id = Self::id_for(&p.path, p.owner);
                p
            })
            .collect()
    }

    fn write(&self, list: &[ProjectInfo]) {
        if let Some(dir) = self.file.parent()
            && !dir.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        let doc = StoreFile {
            version: 1,
            projects: Some(list.to_vec()),
        };
        let Ok(text) = orca_core::config::dotnet_json::to_string_dotnet(&doc) else {
            return;
        };
        let tmp = PathBuf::from(format!(
            "{}.{}.tmp",
            self.file.display(),
            crate::util::short_id(8)
        ));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &self.file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

pub fn default_name(path: &str) -> String {
    let full = paths::normalize(path);
    match full.file_name() {
        Some(n) if !n.is_empty() => n.to_string_lossy().to_string(),
        _ => path.to_string(),
    }
}

/// Each project's engine ("claude", "codex"; native is the default and isn't stored) in
/// `project-engines.json`, or another per-project choice with its own file and default.
pub struct EngineChoiceStore {
    file: PathBuf,
    default_value: String,
    lock: Mutex<()>,
}

impl EngineChoiceStore {
    pub fn new(dir: &Path, file_name: &str, default_value: &str) -> Self {
        Self {
            file: dir.join(file_name),
            default_value: default_value.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn engines(dir: &Path) -> Self {
        Self::new(dir, "project-engines.json", "native")
    }

    pub fn get(&self, project_id: &str) -> Option<String> {
        let _g = self.lock.lock().unwrap();
        self.load().get(project_id).cloned()
    }

    pub fn set(&self, project_id: &str, value: &str) {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        if value.eq_ignore_ascii_case(&self.default_value) {
            all.remove(project_id);
        } else {
            all.insert(project_id.into(), value.into());
        }
        self.save(&all);
    }

    pub fn remove(&self, project_id: &str) {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        if all.remove(project_id).is_some() {
            self.save(&all);
        }
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    fn save(&self, all: &BTreeMap<String, String>) {
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = PathBuf::from(format!("{}.tmp", self.file.display()));
        if std::fs::write(&tmp, serde_json::to_string(all).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }
}

#[cfg(test)]
mod last_opened_tests {
    use super::*;

    #[test]
    fn the_last_added_or_reopened_folder_is_the_last_opened_while_listed() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store = ProjectStore::new(home.path().join("projects.json"));
        assert_eq!(store.last_opened(), None);
        store.add(&a.path().to_string_lossy(), None, None);
        let pb = store.add(&b.path().to_string_lossy(), None, None);
        assert!(paths::same_path(
            &store.last_opened().unwrap().to_string_lossy(),
            &pb.path
        ));
        // Opening a listed project again makes it the last one.
        let pa = store.add(&a.path().to_string_lossy(), None, None);
        assert!(paths::same_path(
            &store.last_opened().unwrap().to_string_lossy(),
            &pa.path
        ));
        // A user's own project (accounts mode) does not count.
        store.add(&b.path().to_string_lossy(), None, Some(7));
        assert!(paths::same_path(
            &store.last_opened().unwrap().to_string_lossy(),
            &pa.path
        ));
        // A project removed from the list is not offered.
        store.remove(&pa.id);
        assert_eq!(store.last_opened(), None);
    }
}
