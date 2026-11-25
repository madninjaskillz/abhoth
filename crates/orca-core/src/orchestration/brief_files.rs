//! The files a sub-agent's brief names, put into its first prompt (port of `BriefFiles.cs`).
//!
//! On the bench the long-pole agent spent a mean 23 s (up to 52 s, 2-9 requests) on read-only
//! orientation before its first write, re-reading files the Lead had already read. Only
//! existing files inside the working directory, at most [`MAX_FILES`], each cut to
//! [`MAX_CHARS_PER_FILE`] and all of them to [`MAX_TOTAL_CHARS`].
//!
//! Lengths count UTF-16 units, as C# `string.Length` does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::types::ci_key;
use crate::chat::{head16, len16};

/// Most files appended.
pub const MAX_FILES: usize = 6;
/// Longest file text appended (UTF-16 units).
pub const MAX_CHARS_PER_FILE: usize = 12_000;
/// Longest text of all appended files together (UTF-16 units).
pub const MAX_TOTAL_CHARS: usize = 24_000;

/// The brief, followed by the named files' contents; the brief itself when it names none.
pub fn append(brief: &str, working_directory: &Path) -> String {
    let files = find(brief, working_directory);
    if files.is_empty() {
        return brief.to_string();
    }
    let mut sb = String::from(brief.trim_end());
    sb.push_str(
        "\n\nThe files named above, as they are now (no need to read them again unless you change them):",
    );
    let mut total = 0usize;
    for (relative, full) in files {
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let text = String::from_utf8_lossy(bytes);
        if text.contains('\0') {
            continue; // binary
        }
        let room = MAX_CHARS_PER_FILE.min(MAX_TOTAL_CHARS.saturating_sub(total));
        if room < 500 {
            break;
        }
        let len = len16(&text);
        let cut = len > room;
        let shown = if cut { head16(&text, room) } else { &text };
        total += len16(shown);
        if cut {
            sb.push_str(&format!(
                "\n\n<file path=\"{relative}\" shown=\"first {room} of {len} chars: read the rest\">\n"
            ));
        } else {
            sb.push_str(&format!("\n\n<file path=\"{relative}\">\n"));
        }
        sb.push_str(shown.trim_end_matches(['\n', '\r']));
        sb.push_str("\n</file>");
    }
    if total == 0 { brief.to_string() } else { sb }
}

/// Existing files the brief names, in order of first mention: (as written with `/`
/// separators, full path).
pub fn find(brief: &str, working_directory: &Path) -> Vec<(String, PathBuf)> {
    let Ok(root) = std::path::absolute(working_directory) else {
        return Vec::new();
    };
    let root_text = root.to_string_lossy().into_owned();
    let sep = std::path::MAIN_SEPARATOR;
    let root_with_sep = if root_text.ends_with(sep) {
        root_text
    } else {
        format!("{root_text}{sep}")
    };
    let key = |s: &str| {
        if cfg!(windows) {
            ci_key(s)
        } else {
            s.to_string()
        }
    };
    let root_key = key(&root_with_sep);

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for m in path_like_matches(brief) {
        let mut rel = m.trim_end_matches(['.', ',', ':', ';']);
        if let Some(r) = rel.strip_prefix("./") {
            rel = r;
        }
        if rel.is_empty() || Path::new(rel).has_root() || rel.contains("..") {
            continue;
        }
        let Ok(full) = std::path::absolute(root.join(rel)) else {
            continue;
        };
        let full_text = full.to_string_lossy().into_owned();
        if !key(&full_text).starts_with(&root_key)
            || !full.is_file()
            || !seen.insert(key(&full_text))
        {
            continue;
        }
        found.push((rel.replace('\\', "/"), full));
        if found.len() == MAX_FILES {
            break;
        }
    }
    found
}

/// .NET `\w` for one char: letters, non-spacing marks, decimal digits and connector
/// punctuation. A char outside the BMP is a surrogate pair in .NET and never a word char.
fn is_word(c: char) -> bool {
    (c as u32) <= 0xFFFF && (c.is_alphanumeric() || c == '_' || is_connector(c))
}

fn is_connector(c: char) -> bool {
    matches!(
        c,
        '\u{203F}' | '\u{2040}' | '\u{2054}' | '\u{FE33}' | '\u{FE34}' | '\u{FE4D}'
            ..='\u{FE4F}' | '\u{FF3F}'
    )
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// `[\w/\\.-]`: a char a path-like run is made of.
fn in_run(c: char) -> bool {
    is_word(c) || is_sep(c) || c == '.' || c == '-'
}

/// The group-1 texts of the C# `PathLike` regex, hand-written because it uses lookaround:
///
/// `(?<![\w/\\.-])((?:\./)?[\w.-]+(?:[/\\][\w.-]+)*\.[A-Za-z][A-Za-z0-9]{0,7})(?![\w/\\-])`
///
/// The lookbehind means a match can only start where a maximal run of `[\w/\\.-]` chars
/// starts, and only if that run starts with a non-separator. Within the run the regex's
/// backtracking finds the longest prefix that (a) is segments of `[\w.-]+` joined by single
/// separators, (b) ends in a `.` + letter + up to 7 letters/digits within its last segment,
/// with at least one char before that dot, and (c) is followed by the end of the run or a
/// `.` (the lookahead refuses word chars, separators and `-`). A later dot always gives a
/// longer match, so the first success in backtracking order is the longest valid prefix. After
/// a match the rest of its run cannot start another (the lookbehind fails inside a run).
fn path_like_matches(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if !in_run(chars[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && in_run(chars[j]) {
            j += 1;
        }
        if !is_sep(chars[i]) {
            for e in (i + 1..=j).rev() {
                if valid_end(&chars, i, e, j) {
                    out.push(chars[i..e].iter().collect());
                    break;
                }
            }
        }
        i = j;
    }
    out
}

/// Whether `chars[start..end]` (inside the run ending at `run_end`) is a full `PathLike` match.
fn valid_end(chars: &[char], start: usize, end: usize, run_end: usize) -> bool {
    // Lookahead: inside the run only a '.' may follow.
    if end < run_end && chars[end] != '.' {
        return false;
    }
    let s = &chars[start..end];
    if s.is_empty() || is_sep(s[0]) || is_sep(s[s.len() - 1]) {
        return false;
    }
    if s.windows(2).any(|w| is_sep(w[0]) && is_sep(w[1])) {
        return false;
    }
    let last_seg_start = s.iter().rposition(|&c| is_sep(c)).map_or(0, |p| p + 1);
    let seg = &s[last_seg_start..];
    let Some(dot) = seg.iter().rposition(|&c| c == '.') else {
        return false;
    };
    let ext = &seg[dot + 1..];
    dot > 0
        && (1..=8).contains(&ext.len())
        && ext[0].is_ascii_alphabetic()
        && ext.iter().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::path_like_matches;

    #[test]
    fn path_like_follows_the_regex() {
        assert_eq!(
            path_like_matches("Build `src/board.py` per spec.md (section 2). See ./spec.md again."),
            ["src/board.py", "spec.md", "./spec.md"]
        );
        // Version numbers have no letter after the dot; "1.2" is no path.
        assert!(path_like_matches("Version 1.2 is fine.").is_empty());
        // Trailing sentence dot is outside the match; the longest extension wins.
        assert_eq!(path_like_matches("see a.tar.gz."), ["a.tar.gz"]);
        // A leading separator blocks the whole run; '-' after the extension blocks that end.
        assert!(path_like_matches("/etc/hosts.txt").is_empty());
        assert_eq!(path_like_matches("x.md-y"), Vec::<String>::new());
        assert_eq!(path_like_matches("a.b.c-d"), ["a.b"]);
        assert!(path_like_matches("a//b.md").is_empty());
        assert_eq!(
            path_like_matches("dir/file.verylongext1"),
            Vec::<String>::new()
        );
    }
}
