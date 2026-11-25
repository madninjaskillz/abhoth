//! `PathSafetyHelper`: rejects system roots and critical directories for delete / move /
//! rename / archive operations.

use std::path::{Path, PathBuf};

const DANGEROUS_PATHS: &[&str] = &[
    "/", "/usr", "/bin", "/sbin", "/etc", "/var", "/lib", "/boot", "/sys", "/proc", "/dev", "/opt",
];

/// `PathSafetyHelper.IsDangerousPath`. `full_path` must already be absolute (the tools resolve
/// it against their working directory first, as `Path.GetFullPath` does in C#).
pub fn is_dangerous_path(full_path: &Path) -> bool {
    // Check the original path first (before symlink resolution): on macOS /etc is a symlink to
    // /private/etc and the original must still be flagged.
    if is_dangerous_resolved(full_path) {
        return true;
    }
    let resolved = resolve_final_path(full_path);
    resolved != full_path && is_dangerous_resolved(&resolved)
}

/// The final target of a symlink or junction (`ResolveLinkTarget(returnFinalTarget: true)`);
/// the path itself when it is not a link or cannot be resolved.
fn resolve_final_path(full_path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(full_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_link {
        return full_path.to_path_buf();
    }
    match std::fs::canonicalize(full_path) {
        Ok(p) => strip_verbatim(p),
        Err(_) => full_path.to_path_buf(),
    }
}

/// `canonicalize` on Windows returns `\\?\C:\...`; .NET paths have no verbatim prefix.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

fn trim_end_seps(s: &str) -> &str {
    s.trim_end_matches(['\\', '/'])
}

fn is_dangerous_resolved(full_path: &Path) -> bool {
    let full = full_path.to_string_lossy();
    let normalized = full.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    // TrimEnd turns "/" into "", which is not in the set; the root check below catches it.
    if DANGEROUS_PATHS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(normalized))
    {
        return true;
    }

    // Drive roots (C:\, D:\, /).
    if let Some(root) = full_path.ancestors().last() {
        let root = root.to_string_lossy();
        if !root.is_empty() && trim_end_seps(&full).eq_ignore_ascii_case(trim_end_seps(&root)) {
            return true;
        }
    }

    // The user profile directory itself (but not its subdirectories).
    if let Some(home) = user_profile()
        && full.eq_ignore_ascii_case(&home)
    {
        return true;
    }

    // Windows system directories.
    if let Some(sys_root) = special_folder("SystemRoot").or_else(|| special_folder("windir"))
        && starts_with_ignore_case(&full, &sys_root)
    {
        return true;
    }
    if let Some(pf) = special_folder("ProgramFiles")
        && full.eq_ignore_ascii_case(&pf)
    {
        return true;
    }
    false
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// `Environment.GetFolderPath(SpecialFolder.UserProfile)`.
pub fn user_profile() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| full_path_string(&s))
}

/// The Windows-only special folders (`SpecialFolder.Windows`, `ProgramFiles`); empty elsewhere,
/// as in .NET.
fn special_folder(var: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    std::env::var(var)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| full_path_string(&s))
}

fn full_path_string(s: &str) -> String {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    orca_abstractions::full_path(&base, s)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    //! Ported from `OpenOrca.Tools.Tests/PathSafetyHelperTests.cs`.
    use super::*;

    fn dangerous(p: &str) -> bool {
        let base = std::env::temp_dir();
        is_dangerous_path(&orca_abstractions::full_path(&base, p))
    }

    #[test]
    fn is_dangerous_path_drive_root_returns_true() {
        if cfg!(windows) {
            assert!(dangerous(r"C:\"));
        }
    }

    #[test]
    fn is_dangerous_path_drive_root_forward_slash_returns_true() {
        if cfg!(windows) {
            assert!(dangerous("C:/"));
        }
    }

    #[test]
    fn is_dangerous_path_unix_root_returns_true() {
        if !cfg!(windows) {
            assert!(dangerous("/"));
        }
    }

    #[test]
    fn is_dangerous_path_unix_system_dirs_returns_true() {
        if cfg!(windows) {
            return;
        }
        for p in [
            "/usr", "/bin", "/sbin", "/etc", "/var", "/lib", "/boot", "/sys", "/proc", "/dev",
            "/opt",
        ] {
            assert!(dangerous(p), "{p}");
        }
    }

    #[test]
    fn is_dangerous_path_windows_dir_returns_true() {
        if let Some(win) = special_folder("SystemRoot") {
            assert!(dangerous(&win));
        }
    }

    #[test]
    fn is_dangerous_path_windows_system32_returns_true() {
        if let Some(win) = special_folder("SystemRoot") {
            assert!(dangerous(&format!(r"{win}\System32")));
        }
    }

    #[test]
    fn is_dangerous_path_program_files_returns_true() {
        if let Some(pf) = special_folder("ProgramFiles") {
            assert!(dangerous(&pf));
        }
    }

    #[test]
    fn is_dangerous_path_user_home_returns_true() {
        if let Some(home) = user_profile() {
            assert!(dangerous(&home));
        }
    }

    #[test]
    fn is_dangerous_path_user_subdirectory_returns_false() {
        if let Some(home) = user_profile() {
            let p = Path::new(&home).join("Documents");
            assert!(!dangerous(&p.to_string_lossy()));
        }
    }

    #[test]
    fn is_dangerous_path_temp_directory_returns_false() {
        assert!(!dangerous(&std::env::temp_dir().to_string_lossy()));
    }

    #[test]
    fn is_dangerous_path_nested_project_dir_returns_false() {
        if let Some(home) = user_profile() {
            let p = Path::new(&home).join("projects").join("myapp");
            assert!(!dangerous(&p.to_string_lossy()));
        }
    }

    #[test]
    fn is_dangerous_path_traversal_to_root_returns_true() {
        if !cfg!(windows) {
            assert!(dangerous("/home/user/../../"));
        }
    }

    #[test]
    fn is_dangerous_path_traversal_to_etc_returns_true() {
        if !cfg!(windows) {
            assert!(dangerous("/home/user/../../etc"));
        }
    }

    #[test]
    fn is_dangerous_path_windows_traversal_to_drive_root_returns_true() {
        if cfg!(windows) {
            assert!(dangerous(r"C:\Users\test\..\.."));
        }
    }
}
