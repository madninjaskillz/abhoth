//! `ProjectPaths` and `FolderAccess`: the one gate between a path from the browser and the
//! disk. A relative path is resolved inside a root and rejected if it could leave it (`..`,
//! rooted or drive paths, NTFS streams, NULs, or a link anywhere on the way whose target is
//! outside the root).

use std::path::{Path, PathBuf};

/// Paths compare case-insensitively on Windows and macOS (`ProjectPaths.Comparison`).
pub fn case_insensitive() -> bool {
    cfg!(windows) || cfg!(target_os = "macos")
}

/// Compare two path strings the platform's way.
pub fn eq(a: &str, b: &str) -> bool {
    if case_insensitive() {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// A key for maps and sets keyed by path (lower-cased where paths ignore case).
pub fn key(p: &str) -> String {
    if case_insensitive() {
        p.to_lowercase()
    } else {
        p.to_string()
    }
}

fn starts_with(s: &str, prefix: &str) -> bool {
    if case_insensitive() {
        s.to_lowercase().starts_with(&prefix.to_lowercase())
    } else {
        s.starts_with(prefix)
    }
}

const SEP: char = std::path::MAIN_SEPARATOR;

fn trim_end_sep(s: &str) -> &str {
    let t = s.trim_end_matches(['/', '\\']);
    // Keep a root ("/", "C:\") whole.
    if t.is_empty() {
        return &s[..1.min(s.len())];
    }
    if cfg!(windows) && t.len() == 2 && t.ends_with(':') && s.len() > 2 {
        return &s[..3];
    }
    t
}

/// A full path without a trailing separator (except a root): `Path.GetFullPath` +
/// `TrimEndingDirectorySeparator`, relative to the current directory.
pub fn normalize(path: &str) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let full = orca_abstractions::full_path(&cwd, path);
    PathBuf::from(trim_end_sep(&full.to_string_lossy()).to_string())
}

pub fn normalize_path(path: &Path) -> PathBuf {
    normalize(&path.to_string_lossy())
}

pub fn same_path(a: &str, b: &str) -> bool {
    eq(
        &normalize(a).to_string_lossy(),
        &normalize(b).to_string_lossy(),
    )
}

/// True when `path` is `root` or inside it (both full paths).
pub fn is_under(path: &Path, root: &Path) -> bool {
    let p = path.to_string_lossy();
    let r = root.to_string_lossy();
    let p = trim_end_sep(&p);
    let r = trim_end_sep(&r);
    if eq(p, r) {
        return true;
    }
    let prefix = if r.ends_with(SEP) {
        r.to_string()
    } else {
        format!("{r}{SEP}")
    };
    starts_with(p, &prefix)
}

/// `relative` (forward or back slashes; empty = the root itself) as a full path inside `root`,
/// or `None` when it is not allowed.
pub fn resolve(root: &Path, relative: Option<&str>) -> Option<PathBuf> {
    let root_full = normalize_path(root);
    let rel = relative.unwrap_or("").replace('\\', "/");
    let rel = rel.trim();
    if rel.contains('\0')
        || rel.contains(':')
        || rel.starts_with('/')
        || Path::new(rel).is_absolute()
    {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts
        .iter()
        .any(|p| *p == ".." || (p.len() > 1 && p.trim_matches('.').is_empty()))
    {
        return None;
    }
    let clean: Vec<&str> = parts.into_iter().filter(|p| *p != ".").collect();
    let full = if clean.is_empty() {
        root_full.clone()
    } else {
        let mut p = root_full.clone();
        for c in &clean {
            p.push(c);
        }
        normalize_path(&p)
    };
    if !is_under(&full, &root_full) {
        return None;
    }
    links_stay_inside(&root_full, &clean).then_some(full)
}

/// A path inside `root` as a forward-slash relative path ("" for the root).
pub fn to_relative(root: &Path, full: &Path) -> String {
    let root = normalize_path(root);
    let full = normalize_path(full);
    let r = root.to_string_lossy().to_string();
    let f = full.to_string_lossy().to_string();
    if eq(&r, &f) {
        return String::new();
    }
    let prefix = if r.ends_with(SEP) {
        r.clone()
    } else {
        format!("{r}{SEP}")
    };
    if starts_with(&f, &prefix) {
        return f[prefix.len()..].replace('\\', "/");
    }
    // Outside the root: a relative path with ".." parts (Path.GetRelativePath).
    let rc: Vec<_> = root.components().collect();
    let fc: Vec<_> = full.components().collect();
    let mut i = 0;
    while i < rc.len()
        && i < fc.len()
        && eq(
            &rc[i].as_os_str().to_string_lossy(),
            &fc[i].as_os_str().to_string_lossy(),
        )
    {
        i += 1;
    }
    let mut parts: Vec<String> = (i..rc.len()).map(|_| "..".to_string()).collect();
    parts.extend(
        fc[i..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().to_string()),
    );
    parts.join("/")
}

/// Strip Windows' verbatim prefix from a canonical path (`\\?\C:\x` -> `C:\x`).
pub fn simplify(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

fn real_path(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir)
        .map(simplify)
        .map(|p| normalize_path(&p))
        .unwrap_or_else(|_| dir.to_path_buf())
}

fn is_link(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(m) => {
            if m.file_type().is_symlink() {
                return true;
            }
            // Junctions and other reparse points on Windows.
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Every existing component that is a link must end up inside the root (or its real location).
fn links_stay_inside(root_full: &Path, parts: &[&str]) -> bool {
    let real_root = real_path(root_full);
    let mut current = root_full.to_path_buf();
    for part in parts {
        current.push(part);
        if std::fs::symlink_metadata(&current).is_err() {
            return true; // nothing further exists, so nothing further can be a link
        }
        if !is_link(&current) {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(&current) else {
            return false;
        };
        let t = normalize_path(&simplify(target));
        if !is_under(&t, root_full) && !is_under(&t, &real_root) {
            return false;
        }
    }
    true
}

/// `FolderAccess.ReadProblem`: `None` when the folder can be listed; else why not.
pub fn read_problem(dir: &Path) -> Option<String> {
    let shown = dir.display().to_string();
    match std::fs::read_dir(dir) {
        Ok(mut it) => match it.next() {
            Some(Err(e)) => Some(io_problem("read", &shown, &e)),
            _ => None,
        },
        Err(e) => Some(io_problem("read", &shown, &e)),
    }
}

fn io_problem(verb: &str, dir: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            format!("Can't {verb} folder {dir}: permission denied.")
        }
        std::io::ErrorKind::NotFound if verb == "read" => format!("No such folder: {dir}."),
        _ => format!("Can't {verb} folder {dir}: {e}"),
    }
}

/// `FolderAccess.ReadWriteProblem`: `None` when the folder can be listed and written.
pub fn read_write_problem(dir: &Path) -> Option<String> {
    if let Some(p) = read_problem(dir) {
        return Some(p);
    }
    let probe = dir.join(format!(
        ".openorca-write-check-{}",
        crate::util::short_id(32)
    ));
    let result = std::fs::File::create(&probe);
    let _ = std::fs::remove_file(&probe);
    match result {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(format!(
            "Can't write to folder {}: permission denied.",
            dir.display()
        )),
        Err(e) => Some(format!("Can't write to folder {}: {e}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(resolve(root, Some("../x")).is_none());
        assert!(resolve(root, Some("a/../../x")).is_none());
        assert!(resolve(root, Some("/etc/passwd")).is_none());
        assert!(resolve(root, Some("C:/x")).is_none());
        assert!(resolve(root, Some("a:stream")).is_none());
        assert!(resolve(root, Some("...")).is_none());
        assert!(resolve(root, Some("a\0b")).is_none());
        let ok = resolve(root, Some("sub/./file.txt")).unwrap();
        assert_eq!(to_relative(root, &ok), "sub/file.txt");
        assert_eq!(resolve(root, Some("")).unwrap(), normalize_path(root));
        assert_eq!(to_relative(root, root), "");
    }

    #[test]
    fn is_under_needs_a_separator_boundary() {
        let root = normalize("/work/proj");
        assert!(is_under(&normalize("/work/proj/a"), &root));
        assert!(is_under(&root, &root));
        assert!(!is_under(&normalize("/work/project"), &root));
    }

    #[cfg(unix)]
    #[test]
    fn links_out_of_the_root_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        assert!(resolve(dir.path(), Some("out/x")).is_none());
    }

    #[test]
    fn folder_access_reports_a_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(
            read_problem(&missing)
                .unwrap()
                .starts_with("No such folder: ")
        );
        assert!(read_write_problem(dir.path()).is_none());
    }
}
