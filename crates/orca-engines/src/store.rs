//! Package stores (ROUTER-MEDIA-PLAN.md 10 and 13.3): metadata only.
//!
//! A store is a folder or URL with an `index.json` listing its packages; each package is a
//! folder with `abhoth-engine.json` and small text templates. The default store is the repo's
//! `stores/default/`, embedded at build time so it works offline. Store URLs the admin adds are
//! untrusted unless marked trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::exec::Fetch;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::pins;
use crate::validate::{self, Files, Mode, Report};

pub const INDEX_FILE: &str = "index.json";
pub const DEFAULT_STORE: &str = "default";

/// A package: its manifest and every text file in its folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub manifest: Manifest,
    /// Relative path → text (includes `abhoth-engine.json`).
    pub files: Files,
    /// The store id (`default`, `my`, or a store's id).
    pub store: String,
    pub trusted: bool,
}

impl Package {
    pub fn key(&self) -> String {
        self.manifest.key()
    }
    /// Re-validate (store packages strictly, My packages as private).
    pub fn validate(&self) -> Report {
        let mode = if self.store == "my" {
            Mode::Private
        } else {
            Mode::Store
        };
        let text = self
            .files
            .get(MANIFEST_FILE)
            .cloned()
            .unwrap_or_else(|| self.manifest.to_json_pretty());
        validate::validate_text(&text, &self.files, mode)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub packages: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub version: String,
    /// The package folder, relative to the index.
    pub path: String,
    /// sha256 of the manifest file.
    pub sha256: String,
    pub name: String,
    pub kinds: Vec<String>,
    /// Method types in order (`["docker", "pip"]`).
    pub methods: Vec<String>,
    #[serde(default)]
    pub summary: String,
    /// The package's other files (relative to its folder) with their sha256.
    #[serde(default)]
    pub files: Vec<IndexFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexFile {
    pub path: String,
    pub sha256: String,
}

/// Build a store's index from its folder: every subfolder with a manifest, validated as a store
/// package. Returns the index and each package's report (an invalid package is left out).
pub fn build_index(dir: &Path, name: &str) -> (Index, Vec<(String, Report)>) {
    let mut entries = vec![];
    let mut reports = vec![];
    let mut subdirs: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    subdirs.sort();
    for d in subdirs {
        if !d.join(MANIFEST_FILE).is_file() {
            continue;
        }
        let folder = d
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let r = validate::validate_path(&d, Mode::Store);
        if let (true, Some(m)) = (r.ok, &r.manifest) {
            let text = std::fs::read(d.join(MANIFEST_FILE)).unwrap_or_default();
            let mut files = vec![];
            for f in &r.files {
                if f.path == MANIFEST_FILE {
                    continue;
                }
                let b = std::fs::read(d.join(&f.path)).unwrap_or_default();
                files.push(IndexFile {
                    path: f.path.clone(),
                    sha256: pins::text_sha256(&b),
                });
            }
            entries.push(IndexEntry {
                id: m.id.clone(),
                version: m.version.clone(),
                path: folder.clone(),
                sha256: pins::text_sha256(&text),
                name: m.name.clone(),
                kinds: m.kinds.clone(),
                methods: m
                    .methods
                    .iter()
                    .map(|x| x.r#type.as_str().to_string())
                    .collect(),
                summary: m.description.lines().next().unwrap_or("").to_string(),
                files,
            });
        }
        reports.push((folder, r));
    }
    (
        Index {
            schema: 1,
            name: name.into(),
            description: String::new(),
            packages: entries,
        },
        reports,
    )
}

/// Read a package folder from disk (My packages, a local store).
pub fn load_dir(dir: &Path, store: &str, trusted: bool) -> Result<Package, Box<Report>> {
    let mode = if store == "my" {
        Mode::Private
    } else {
        Mode::Store
    };
    let r = validate::validate_path(dir, mode);
    let Some(m) = r.manifest.clone() else {
        return Err(Box::new(r));
    };
    let mut files = Files::new();
    for f in &r.files {
        if let Ok(t) = std::fs::read_to_string(dir.join(&f.path)) {
            files.insert(f.path.clone(), t);
        }
    }
    if !r.ok {
        return Err(Box::new(r));
    }
    Ok(Package {
        manifest: m,
        files,
        store: store.into(),
        trusted,
    })
}

/// Fetch one package from a store URL (`<base>/index.json`), checking every file's sha256
/// against the index.
pub async fn fetch_package(
    fetch: &dyn Fetch,
    base: &str,
    e: &IndexEntry,
    store: &str,
    trusted: bool,
) -> Result<Package, String> {
    if e.path.contains("..") || e.path.starts_with('/') {
        return Err(format!("{}: a bad package path", e.path));
    }
    let base = base.trim_end_matches('/');
    let mut files = Files::new();
    let mtext = fetch
        .text(&format!("{base}/{}/{MANIFEST_FILE}", e.path))
        .await?;
    if pins::text_sha256(mtext.as_bytes()) != e.sha256 {
        return Err(format!(
            "{}: the manifest does not match the index's sha256",
            e.id
        ));
    }
    files.insert(MANIFEST_FILE.into(), mtext.clone());
    for f in &e.files {
        if f.path.contains("..") || f.path.starts_with('/') {
            return Err(format!("{}: a bad file path", f.path));
        }
        let t = fetch.text(&format!("{base}/{}/{}", e.path, f.path)).await?;
        if pins::text_sha256(t.as_bytes()) != f.sha256 {
            return Err(format!(
                "{}/{}: does not match the index's sha256",
                e.id, f.path
            ));
        }
        files.insert(f.path.clone(), t);
    }
    let r = validate::validate_text(&mtext, &files, Mode::Store);
    let Some(m) = r.manifest.clone().filter(|_| r.ok) else {
        return Err(format!(
            "{} does not validate: {}",
            e.id,
            r.errors()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    };
    Ok(Package {
        manifest: m,
        files,
        store: store.into(),
        trusted,
    })
}

/// The URL a store's `index.json` is read from. A GitHub repo URL
/// (`https://github.com/<owner>/<repo>[/tree/<ref>/<dir>]`) reads the raw file.
pub fn index_base(url: &str) -> Result<String, String> {
    let u = url.trim().trim_end_matches('/');
    if !u.starts_with("https://") {
        return Err("a store URL must be https".into());
    }
    let u = u.strip_suffix("/index.json").unwrap_or(u);
    if let Some(rest) = u.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 {
            return Err("a GitHub store URL is https://github.com/<owner>/<repo>".into());
        }
        let (owner, repo) = (parts[0], parts[1].trim_end_matches(".git"));
        let (gref, sub) = if parts.len() >= 4 && parts[2] == "tree" {
            (parts[3].to_string(), parts[4..].join("/"))
        } else {
            ("HEAD".to_string(), String::new())
        };
        let mut b = format!("https://raw.githubusercontent.com/{owner}/{repo}/{gref}");
        if !sub.is_empty() {
            b.push('/');
            b.push_str(&sub);
        }
        return Ok(b);
    }
    Ok(u.to_string())
}

/// The default store, embedded at build time (`stores/default/`).
pub mod embedded {
    use super::*;

    include!(concat!(env!("OUT_DIR"), "/default_store.rs"));

    /// `(relative path, text)` of every file in `stores/default/`.
    pub fn files() -> &'static [(&'static str, &'static str)] {
        DEFAULT_STORE_FILES
    }

    /// The JSON Schema of the manifest (`stores/schema/abhoth-engine.schema.json`).
    pub fn schema() -> &'static str {
        MANIFEST_SCHEMA
    }

    /// Every package in the default store (trusted).
    pub fn packages() -> Vec<Package> {
        let mut by_dir: std::collections::BTreeMap<&str, Files> = Default::default();
        for (p, t) in files() {
            if let Some((dir, rest)) = p.split_once('/') {
                by_dir
                    .entry(dir)
                    .or_default()
                    .insert(rest.to_string(), t.to_string());
            }
        }
        let mut out = vec![];
        for (_dir, files) in by_dir {
            let Some(text) = files.get(MANIFEST_FILE) else {
                continue;
            };
            let r = validate::validate_text(text, &files, Mode::Store);
            if let (true, Some(m)) = (r.ok, r.manifest) {
                out.push(Package {
                    manifest: m,
                    files,
                    store: DEFAULT_STORE.into(),
                    trusted: true,
                });
            }
        }
        out
    }
}
