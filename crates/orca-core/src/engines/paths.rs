//! The `System.IO.Path` pieces the bridges use, with .NET's rules on each platform.

const WINDOWS: bool = cfg!(windows);

fn is_sep(c: char) -> bool {
    c == '/' || (WINDOWS && c == '\\')
}

/// `Path.IsPathFullyQualified`.
pub fn is_path_fully_qualified(path: &str) -> bool {
    if !WINDOWS {
        return path.starts_with('/');
    }
    let c: Vec<char> = path.chars().take(3).collect();
    if c.len() < 2 {
        return false;
    }
    if is_sep(c[0]) {
        // "\\server\share" and "\\?\" are fully qualified; "\dir" is not.
        return c[1] == '?' || is_sep(c[1]);
    }
    c.len() >= 3 && c[0].is_ascii_alphabetic() && c[1] == ':' && is_sep(c[2])
}

/// `Path.IsPathRooted`.
pub fn is_path_rooted(path: &str) -> bool {
    if !WINDOWS {
        return path.starts_with('/');
    }
    let mut it = path.chars();
    match (it.next(), it.next()) {
        (Some(a), _) if is_sep(a) => true,
        (Some(a), Some(':')) if a.is_ascii_alphabetic() => true,
        _ => false,
    }
}

/// `Path.Combine(a, b)`.
pub fn combine(a: &str, b: &str) -> String {
    if a.is_empty() || is_path_rooted(b) {
        return b.to_string();
    }
    if a.ends_with(is_sep) || (WINDOWS && a.ends_with(':')) {
        format!("{a}{b}")
    } else {
        format!("{a}{}{b}", std::path::MAIN_SEPARATOR)
    }
}

/// `Path.GetFullPath(path)` against the process's current directory (lexical, as .NET).
pub fn get_full_path(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    orca_abstractions::full_path(&cwd, path)
        .to_string_lossy()
        .into_owned()
}

/// `Path.TrimEndingDirectorySeparator`: drops one trailing separator unless it is the root.
pub fn trim_ending_separator(path: &str) -> &str {
    if path.len() > 1 && path.ends_with(is_sep) {
        let trimmed = &path[..path.len() - 1];
        // "C:\" and "/" stay as they are.
        if (WINDOWS && trimmed.len() == 2 && trimmed.ends_with(':')) || trimmed.is_empty() {
            return path;
        }
        return trimmed;
    }
    path
}

/// Compares paths the way the file system does (case-insensitive on Windows).
pub fn path_eq(a: &str, b: &str) -> bool {
    if WINDOWS {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// `Path.GetFileName`.
pub fn file_name(path: &str) -> &str {
    match path.rfind(is_sep) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// `Path.GetFileNameWithoutExtension`.
pub fn file_name_without_extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// `Path.GetRelativePath(relative_to, path)` for the cases the bridges need: `path` inside
/// `relative_to` gives the rest, the same folder gives ".", anything else comes back whole
/// (callers then keep the original path).
pub fn get_relative_path(relative_to: &str, path: &str) -> String {
    let base = get_full_path(relative_to);
    let full = get_full_path(path);
    let base = trim_ending_separator(&base);
    let full_t = trim_ending_separator(&full);
    if path_eq(base, full_t) {
        return ".".into();
    }
    if full_t.len() > base.len() + 1
        && full_t.is_char_boundary(base.len())
        && path_eq(&full_t[..base.len()], base)
        && full_t[base.len()..].starts_with(is_sep)
    {
        return full_t[base.len() + 1..].to_string();
    }
    // Outside: .NET returns a "..\.." path or the full path; both make callers keep `path`.
    if is_path_fully_qualified(&full) && !path_eq(root_of(&full), root_of(base)) {
        return full;
    }
    format!("..{}{}", std::path::MAIN_SEPARATOR, full)
}

fn root_of(p: &str) -> &str {
    if WINDOWS && p.len() >= 2 && p.as_bytes()[1] == b':' {
        &p[..2]
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_and_rooted() {
        if WINDOWS {
            assert!(is_path_fully_qualified(r"C:\x"));
            assert!(is_path_fully_qualified(r"\\server\share"));
            assert!(!is_path_fully_qualified(r"\x"));
            assert!(!is_path_fully_qualified("C:x"));
            assert!(is_path_rooted(r"\x"));
            assert!(is_path_rooted("C:x"));
            assert_eq!(combine(r"C:\a", "b"), r"C:\a\b");
            assert_eq!(trim_ending_separator(r"C:\a\"), r"C:\a");
            assert_eq!(trim_ending_separator(r"C:\"), r"C:\");
        } else {
            assert!(is_path_fully_qualified("/x"));
            assert!(!is_path_fully_qualified(r"C:\x"));
            assert_eq!(combine("/a", "b"), "/a/b");
        }
        assert_eq!(file_name_without_extension("dir/claude.cmd"), "claude");
    }
}
