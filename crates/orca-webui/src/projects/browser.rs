//! `FolderBrowser`: the add-project wizard's folder picker. The folders are on the machine
//! openorca runs on: the browser asks, this lists sub-folders (no files, no contents).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::paths;
use crate::projects::manager::expand_env;

pub const MAX_FOLDERS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub name: String,
    pub path: String,
    pub is_repo: bool,
    pub repo_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderPlace {
    pub label: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderListing {
    pub path: String,
    pub parent: Option<String>,
    pub folders: Vec<FolderEntry>,
    pub places: Vec<FolderPlace>,
    pub truncated: bool,
}

/// Why browsing or making a folder failed: 400, 404, 409 (exists) or 403.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowseError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Exists(String),
    #[error("Access denied.")]
    Denied,
    #[error("{0}")]
    Io(String),
}

pub fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
    "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
    "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Why `name` can't be a new folder's name, or `None` when it can (valid on Windows and Linux).
pub fn folder_name_error(name: Option<&str>) -> Option<String> {
    let name = name.unwrap_or("");
    if name.trim().is_empty() {
        return Some("Type a name for the new folder.".into());
    }
    if name == "." || name == ".." {
        return Some("\".\" and \"..\" aren't folder names.".into());
    }
    if name.contains(['/', '\\']) {
        return Some("A folder name can't contain / or \\.".into());
    }
    for c in name.chars() {
        let code = c as u32;
        if code < 32 || code == 127 {
            return Some("A folder name can't contain control characters.".into());
        }
        if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Some(format!("A folder name can't contain {c}."));
        }
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("A folder name can't end with a dot or a space.".into());
    }
    if name.encode_utf16().count() > 255 {
        return Some("That name is too long.".into());
    }
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    if RESERVED
        .iter()
        .any(|r| r.to_lowercase() == stem.to_lowercase())
    {
        return Some(format!(
            "\"{}\" is a reserved name on Windows.",
            stem.to_uppercase()
        ));
    }
    None
}

/// The picker's "New folder": makes `name` inside `parent`. Returns the new folder's path.
pub fn create_folder(parent: Option<&str>, name: Option<&str>) -> Result<String, BrowseError> {
    let parent = parent.unwrap_or("");
    if parent.trim().is_empty() {
        return Err(BrowseError::BadRequest(
            "Choose the folder to make the new folder in.".into(),
        ));
    }
    if let Some(e) = folder_name_error(name) {
        return Err(BrowseError::BadRequest(e));
    }
    let name = name.unwrap_or("");
    let dir = paths::normalize(&expand_env(parent.trim().trim_matches('"')));
    if !dir.is_dir() {
        return Err(BrowseError::NotFound(format!(
            "No such folder on this machine: {}",
            dir.display()
        )));
    }
    let target = dir.join(name);
    let norm = paths::normalize_path(&target);
    if !norm
        .parent()
        .is_some_and(|up| paths::same_path(&up.to_string_lossy(), &dir.to_string_lossy()))
    {
        return Err(BrowseError::BadRequest(
            "A folder name can't contain a path.".into(),
        ));
    }
    if target.is_dir() {
        return Err(BrowseError::Exists(format!(
            "A folder named \"{name}\" already exists here."
        )));
    }
    if target.exists() {
        return Err(BrowseError::Exists(format!(
            "A file named \"{name}\" already exists here."
        )));
    }
    match std::fs::create_dir_all(&target) {
        Ok(()) => Ok(target.to_string_lossy().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(BrowseError::Denied),
        Err(e) => Err(BrowseError::Io(format!("Couldn't make the folder: {e}"))),
    }
}

/// The projects locations, main one first: OPENORCA_PROJECT_ROOTS ("label=path;label=path"),
/// else OPENORCA_PROJECTS_ROOT, else (Linux/macOS) /projects. Only folders that exist.
pub fn project_roots() -> Vec<FolderPlace> {
    let mut list = Vec::new();
    if let Ok(spec) = std::env::var("OPENORCA_PROJECT_ROOTS")
        && !spec.is_empty()
    {
        for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (label, p) = match item.find('=') {
                Some(eq) if eq > 0 => (
                    item[..eq].trim().to_string(),
                    item[eq + 1..].trim().to_string(),
                ),
                _ => (
                    Path::new(item.trim_end_matches('/'))
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    item.to_string(),
                ),
            };
            if !p.is_empty() && Path::new(&p).is_dir() {
                list.push(FolderPlace { label, path: p });
            }
        }
        return list;
    }
    let root = match std::env::var("OPENORCA_PROJECTS_ROOT") {
        Ok(r) if !r.is_empty() => Some(r),
        _ if cfg!(windows) => None,
        _ => Some("/projects".to_string()),
    };
    if let Some(r) = root.filter(|r| Path::new(r).is_dir()) {
        list.push(FolderPlace {
            label: "Projects".into(),
            path: r,
        });
    }
    list
}

pub async fn list(path: Option<&str>) -> Result<FolderListing, BrowseError> {
    let home = home_dir();
    let explicit = path.filter(|p| !p.trim().is_empty());
    let mut dir = match explicit {
        None => home.clone(),
        Some(p) => paths::normalize(&expand_env(p.trim().trim_matches('"'))),
    };
    let roots = project_roots();
    if explicit.is_none()
        && let Some(first) = roots.first()
    {
        dir = PathBuf::from(&first.path); // the main projects location is where the wizard starts
    }
    if !dir.is_dir() {
        return Err(BrowseError::NotFound(format!(
            "No such folder on this machine: {}",
            dir.display()
        )));
    }
    let mut folders = Vec::new();
    let mut truncated = false;
    let it = std::fs::read_dir(&dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => BrowseError::Denied,
        _ => BrowseError::NotFound(format!("No such folder on this machine: {}", dir.display())),
    })?;
    for e in it.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) && !e.path().is_dir() {
            continue;
        }
        if folders.len() >= MAX_FOLDERS {
            truncated = true;
            break;
        }
        let full = e.path();
        let dot_git = full.join(".git").exists();
        folders.push(FolderEntry {
            name: e.file_name().to_string_lossy().to_string(),
            path: full.to_string_lossy().to_string(),
            is_repo: dot_git,
            repo_error: None,
        });
    }
    // A .git is only a hint: ask git, the same check the git panel makes, so the two agree.
    let candidates: Vec<usize> = (0..folders.len()).filter(|&i| folders[i].is_repo).collect();
    for chunk in candidates.chunks(8) {
        let probes = futures_util::future::join_all(chunk.iter().map(|&i| {
            let p = PathBuf::from(&folders[i].path);
            async move { crate::git::service::probe(&p).await }
        }))
        .await;
        for (&i, probe) in chunk.iter().zip(probes) {
            let f = &mut folders[i];
            f.is_repo = probe
                .root
                .as_deref()
                .is_some_and(|r| paths::same_path(r, &f.path));
            f.repo_error = if probe.root.is_none() {
                probe.error
            } else {
                None
            };
        }
    }
    folders.sort_by(|a, b| {
        // Hidden (dot) folders last, then by name.
        let (ha, hb) = (a.name.starts_with('.'), b.name.starts_with('.'));
        ha.cmp(&hb)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let mut places = vec![FolderPlace {
        label: "Home".into(),
        path: home.to_string_lossy().to_string(),
    }];
    for sub in ["Documents", "Desktop"] {
        let p = home.join(sub);
        if p.is_dir() {
            places.push(FolderPlace {
                label: sub.into(),
                path: p.to_string_lossy().to_string(),
            });
        }
    }
    let github = home.join("Documents").join("GitHub");
    if github.is_dir() {
        places.push(FolderPlace {
            label: "GitHub".into(),
            path: github.to_string_lossy().to_string(),
        });
    }
    let mut all = roots;
    all.extend(places);
    let mut places = all;
    if cfg!(windows) {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).is_dir() {
                places.push(FolderPlace {
                    label: root.clone(),
                    path: root,
                });
            }
        }
    } else {
        places.push(FolderPlace {
            label: "/".into(),
            path: "/".into(),
        });
    }
    Ok(FolderListing {
        path: dir.to_string_lossy().to_string(),
        parent: dir.parent().map(|p| p.to_string_lossy().to_string()),
        folders,
        places,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_must_work_on_windows_and_linux() {
        assert!(folder_name_error(Some("ok-name")).is_none());
        assert_eq!(
            folder_name_error(Some("a/b")).as_deref(),
            Some("A folder name can't contain / or \\.")
        );
        assert_eq!(
            folder_name_error(Some("a?")).as_deref(),
            Some("A folder name can't contain ?.")
        );
        assert_eq!(
            folder_name_error(Some("con.txt")).as_deref(),
            Some("\"CON\" is a reserved name on Windows.")
        );
        assert_eq!(
            folder_name_error(Some("x.")).as_deref(),
            Some("A folder name can't end with a dot or a space.")
        );
        assert_eq!(
            folder_name_error(Some(" ")).as_deref(),
            Some("Type a name for the new folder.")
        );
    }

    #[tokio::test]
    async fn create_and_list_folders() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().to_string();
        let made = create_folder(Some(&parent), Some("new one")).unwrap();
        assert!(Path::new(&made).is_dir());
        assert!(matches!(
            create_folder(Some(&parent), Some("new one")),
            Err(BrowseError::Exists(_))
        ));
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        let l = list(Some(&parent)).await.unwrap();
        let names: Vec<_> = l.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["new one", ".hidden"]);
        assert!(l.places.iter().any(|p| p.label == "Home"));
    }
}
