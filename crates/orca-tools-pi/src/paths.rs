//! Port of Pi's path-utils.ts `resolveToCwd` / `expandPath` (`PiPaths`): strips a leading
//! "@", turns unicode spaces into ASCII spaces, expands "~", converts Git Bash / MSYS / WSL
//! drive paths (/c/Users) to native Windows paths, and resolves relative paths against the
//! working directory.

use std::path::{Path, PathBuf};

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn user_profile() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

/// `ExpandPath`.
pub fn expand_path(file_path: &str) -> String {
    let normalized = file_path.strip_prefix('@').unwrap_or(file_path);
    let mut normalized: String = normalized
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();
    let windows = cfg!(windows);
    if windows {
        normalized = normalize_windows_shell_path(&normalized);
    }
    if normalized == "~" {
        return user_profile();
    }
    if normalized.starts_with("~/") || (windows && normalized.starts_with("~\\")) {
        return Path::new(&user_profile())
            .join(&normalized[2..])
            .to_string_lossy()
            .into_owned();
    }
    normalized
}

/// `NormalizeWindowsShellPath`: Git Bash / MSYS / Cygwin / WSL drive path to a native Windows
/// path (`^(?:/mnt|/cygdrive)?/([a-zA-Z])(?:/(.*))?$`).
pub fn normalize_windows_shell_path(file_path: &str) -> String {
    let rest = file_path
        .strip_prefix("/mnt")
        .or_else(|| file_path.strip_prefix("/cygdrive"))
        .filter(|r| r.starts_with('/'))
        .unwrap_or(file_path);
    let try_match = |s: &str| -> Option<String> {
        let s = s.strip_prefix('/')?;
        let mut chars = s.chars();
        let drive = chars.next().filter(char::is_ascii_alphabetic)?;
        let after = chars.as_str();
        let tail = if after.is_empty() {
            String::new()
        } else {
            let t = after.strip_prefix('/')?;
            // `.*` does not cross a line break.
            if t.contains('\n') {
                return None;
            }
            t.replace('/', "\\")
        };
        Some(format!("{}:\\{tail}", drive.to_ascii_uppercase()))
    };
    // The prefix group is optional: with it first, then without it (regex backtracking).
    try_match(rest)
        .or_else(|| try_match(file_path))
        .unwrap_or_else(|| file_path.to_string())
}

/// `ResolveToCwd`: `Path.GetFullPath` of the expanded path against `cwd`.
pub fn resolve_to_cwd(file_path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(file_path);
    orca_abstractions::full_path(cwd, &expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_drive_paths() {
        assert_eq!(normalize_windows_shell_path("/c/Users/x"), "C:\\Users\\x");
        assert_eq!(normalize_windows_shell_path("/mnt/d"), "D:\\");
        assert_eq!(normalize_windows_shell_path("/cygdrive/e/a/b"), "E:\\a\\b");
        assert_eq!(normalize_windows_shell_path("/tmp/x"), "/tmp/x");
        assert_eq!(normalize_windows_shell_path("/c"), "C:\\");
    }
}
