//! Folder browsing on a server (D152 follow-up): the `list-dir`, `path-stat` and `mkdir`
//! builtins, run where the files are (locally, or on the host through hostctl). Read only apart
//! from `mkdir`, which creates one folder in an existing one. Paths are plain arguments (no shell
//! ever sees them), normalised, absolute, with no `..`; the real path (symlinks resolved) must stay
//! inside the allowed roots, and the hidden system trees (`/proc`, `/sys`, `/dev`, …) never show.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

/// The most entries read from one directory (the rest is reported as truncated).
pub const MAX_ENTRIES: usize = 5000;
/// The most entries in one page.
pub const MAX_PAGE: usize = 500;
/// Hidden on every server, whatever the roots say.
pub const HIDDEN: &[&str] = &["/proc", "/sys", "/dev", "/run", "/boot"];

/// The roots a server allows (from `servers.x.browse_roots`; default the whole tree less [`HIDDEN`]).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Roots {
    #[serde(default)]
    pub roots: Vec<String>,
}

impl Roots {
    pub fn list(&self) -> Vec<String> {
        if self.roots.is_empty() {
            vec!["/".to_string()]
        } else {
            self.roots.iter().filter_map(|r| clean(r).ok()).collect()
        }
    }

    /// Inside a root and not under a hidden tree.
    pub fn allows(&self, p: &str) -> bool {
        let p = unify(p);
        if HIDDEN.iter().any(|h| within(&p, h)) {
            return false;
        }
        self.list().iter().any(|r| within(&p, &unify(r)))
    }
}

/// Windows paths as `C:/x` (a local server on a Windows router; tests).
fn unify(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p).replace('\\', "/");
    if cfg!(windows) && p.as_bytes().get(1) == Some(&b':') {
        // drive letters compare case-insensitively; a bare drive is its root
        let mut s = p[..1].to_ascii_uppercase() + &p[1..];
        if s.len() == 2 {
            s.push('/');
        }
        return s;
    }
    p
}

/// `p` is `root` or below it (by whole components).
pub fn within(p: &str, root: &str) -> bool {
    if root == "/" || (root.len() == 3 && root.ends_with(":/")) {
        return p.starts_with(root.trim_end_matches('/')) || p == root;
    }
    let r = root.trim_end_matches('/');
    p == r || p.starts_with(&format!("{r}/"))
}

/// A normalised absolute path: no `..`, no `.`, no doubled or trailing slashes, no control
/// characters or backslashes. `C:/…` is allowed only on a Windows host.
pub fn clean(p: &str) -> Result<String, String> {
    let p = p.trim();
    if p.is_empty() {
        return Err("a path is needed".into());
    }
    if p.len() > 4096 || p.chars().any(|c| c.is_control()) {
        return Err("not a plain path".into());
    }
    let (prefix, rest) = if let Some(r) = p.strip_prefix('/') {
        ("", r)
    } else if cfg!(windows)
        && p.as_bytes().get(1) == Some(&b':')
        && matches!(p.as_bytes().get(2), Some(b'/') | Some(b'\\'))
    {
        (&p[..2], &p[3..])
    } else {
        return Err(format!("{p:?} is not an absolute path (start with /)"));
    };
    let rest = if cfg!(windows) {
        rest.replace('\\', "/")
    } else {
        rest.to_string()
    };
    if !cfg!(windows) && rest.contains('\\') {
        return Err("no backslashes in a path".into());
    }
    let mut parts = Vec::new();
    for c in rest.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err("no .. in a path".into()),
            c => parts.push(c),
        }
    }
    Ok(format!("{prefix}/{}", parts.join("/")))
}

/// The parent of a clean path (`None` at the top).
pub fn parent(p: &str) -> Option<String> {
    let p = p.trim_end_matches('/');
    let i = p.rfind('/')?;
    if i == 0 {
        return (!p.is_empty()).then(|| "/".to_string());
    }
    if cfg!(windows) && i == 2 && p.as_bytes().get(1) == Some(&b':') {
        return Some(format!("{}/", &p[..2]));
    }
    Some(p[..i].to_string())
}

const MODEL_EXT: &[&str] = &[
    "gguf",
    "safetensors",
    "ninfer",
    "onnx",
    "bin",
    "pt",
    "pth",
    "ckpt",
    "exl2",
    "ggml",
    "nemo",
];

pub fn is_model_file(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, e)| MODEL_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// The real path (symlinks resolved), as a clean string.
fn real(p: &str) -> std::io::Result<String> {
    let r = std::fs::canonicalize(p)?;
    Ok(unify(&r.to_string_lossy()))
}

/// Free bytes for unprivileged users on the volume holding `p`.
#[cfg(unix)]
pub fn free_bytes(p: &str) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(Path::new(p).as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid NUL-terminated path and s a writable statvfs.
    let r = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    (r == 0).then(|| (s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(not(unix))]
pub fn free_bytes(_p: &str) -> Option<u64> {
    None
}

/// Whether this process (the engine user on the host) may create files in `p`.
#[cfg(unix)]
pub fn writable(p: &str) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(Path::new(p).as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

#[cfg(not(unix))]
pub fn writable(p: &str) -> bool {
    std::fs::metadata(p).is_ok_and(|m| !m.permissions().readonly())
}

/// Clean, resolve and check a path against the roots; the real path.
fn checked(p: &str, roots: &Roots) -> Result<(String, String), String> {
    let p = clean(p)?;
    if !roots.allows(&p) {
        return Err(format!("{p} is outside the folders this server allows"));
    }
    let r = real(&p).map_err(|e| format!("{p}: {e}"))?;
    if !roots.allows(&r) {
        return Err(format!(
            "{p} leads outside the allowed folders (a link to {r})"
        ));
    }
    Ok((p, r))
}

/// One page of a folder: folders first, then files, by name.
pub fn list(path: &str, roots: &Roots, offset: usize, limit: usize) -> Result<Value, String> {
    let (p, r) = checked(path, roots)?;
    let rd = std::fs::read_dir(&r).map_err(|e| format!("{p}: {e}"))?;
    let mut items: Vec<(bool, String, Value)> = Vec::new();
    let mut truncated = false;
    for e in rd {
        if items.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        let full = format!("{}/{}", p.trim_end_matches('/'), name);
        if HIDDEN.iter().any(|h| within(&unify(&full), h)) {
            continue;
        }
        let Ok(md) = e.path().symlink_metadata() else {
            continue;
        };
        let link = md.file_type().is_symlink();
        // a link counts as what it points to, and only when that stays inside the roots
        let (md, outside) = if link {
            match (
                std::fs::metadata(e.path()),
                real(&e.path().to_string_lossy()),
            ) {
                (Ok(m), Ok(t)) => (m, !roots.allows(&t)),
                _ => (md, true),
            }
        } else {
            (md, false)
        };
        let dir = md.is_dir();
        items.push((
            !dir,
            name.to_lowercase(),
            json!({
                "name": name,
                "path": full,
                "dir": dir,
                "link": link,
                "outside": outside,
                "size": (!dir).then_some(md.len()),
                "model": !dir && is_model_file(&name),
            }),
        ));
    }
    items.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = items.len();
    let limit = limit.clamp(1, MAX_PAGE);
    let page: Vec<Value> = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|x| x.2)
        .collect();
    Ok(json!({
        "path": p,
        "real": r,
        "parent": parent(&p).filter(|x| roots.allows(x)),
        "entries": page,
        "offset": offset,
        "total": total,
        "truncated": truncated,
        "more": offset + limit < total,
        "free": free_bytes(&r),
        "writable": writable(&r),
        "roots": roots.list(),
    }))
}

/// What a typed path is: exists, a folder, writable, free space, inside the roots.
pub fn stat(path: &str, roots: &Roots) -> Result<Value, String> {
    let p = clean(path)?;
    if !roots.allows(&p) {
        return Ok(json!({"path": p, "allowed": false, "exists": false}));
    }
    match real(&p) {
        Err(_) => {
            // a folder that does not exist yet: report its nearest existing parent
            let mut up = parent(&p);
            while let Some(u) = up.clone() {
                if Path::new(&u).exists() {
                    break;
                }
                up = parent(&u);
            }
            let w = up.as_deref().is_some_and(writable);
            Ok(
                json!({"path": p, "allowed": true, "exists": false, "parentExists": up, "parentWritable": w}),
            )
        }
        Ok(r) => {
            let md = std::fs::metadata(&r).map_err(|e| e.to_string())?;
            Ok(json!({
                "path": p, "real": r, "allowed": roots.allows(&r), "exists": true,
                "dir": md.is_dir(), "size": (!md.is_dir()).then_some(md.len()),
                "writable": writable(&r), "free": free_bytes(&r),
            }))
        }
    }
}

/// Create one folder in an existing, allowed, writable folder.
pub fn mkdir(path: &str, roots: &Roots) -> Result<Value, String> {
    let p = clean(path)?;
    let par = parent(&p).ok_or("not at the top")?;
    let name = &p[par.trim_end_matches('/').len() + 1..];
    if name.is_empty() || name.starts_with('.') || name.len() > 255 {
        return Err("a folder name is needed (not starting with a dot)".into());
    }
    let (_, rp) = checked(&par, roots)?;
    if Path::new(&p).exists() {
        return Err(format!("{p} already exists"));
    }
    std::fs::create_dir(format!("{}/{name}", rp.trim_end_matches('/')))
        .map_err(|e| format!("{p}: {e}"))?;
    stat(&p, roots)
}

#[cfg(test)]
mod tests;
