//! `InputPreprocessor`: `@path` references in a prompt are replaced with the file's contents
//! (`[File: path]` ... `[/File]`, cut at 50,000 characters). Paths that are not files stay as
//! typed.

use std::path::Path;

const MAX_FILE_CHARS: usize = 50_000;

/// The expanded prompt and the files that were inlined (display paths, in order).
pub fn expand_file_references(input: &str, base_directory: &Path) -> (String, Vec<String>) {
    if input.trim().is_empty() || !input.contains('@') {
        return (input.to_string(), Vec::new());
    }
    let matches = find_references(input);
    if matches.is_empty() {
        return (input.to_string(), Vec::new());
    }
    let cwd = base_directory.display().to_string();
    let mut result = input.to_string();
    let mut expanded = Vec::new();
    // In reverse so the byte offsets stay valid.
    for (start, end) in matches.into_iter().rev() {
        let rel = &input[start + 1..end];
        let full = orca_abstractions::full_path(base_directory, rel);
        if !full.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let mut content = orca_tools_common::decode_text(&bytes);
        if orca_tools_common::utf16_len(&content) > MAX_FILE_CHARS {
            content = format!("{}\n... (truncated)", take_utf16(&content, MAX_FILE_CHARS));
        }
        let display = orca_core::engines::paths::get_relative_path(
            &cwd,
            &full.display().to_string(),
        );
        let replacement = format!("[File: {display}]\n{content}\n[/File]");
        result.replace_range(start..end, &replacement);
        expanded.push(display);
    }
    expanded.reverse();
    (result, expanded)
}

/// The first `units` UTF-16 code units of `s` (never splitting a character).
fn take_utf16(s: &str, units: usize) -> &str {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        n += c.len_utf16();
        if n > units {
            return &s[..i];
        }
    }
    s
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn in_path_class(c: char) -> bool {
    is_word(c) || matches!(c, '.' | '/' | '\\' | '-')
}

/// The .NET pattern `(?<=^|\s)@([\w./\\-]+[\w.])` by hand (the regex crate has no lookbehind):
/// byte ranges of the whole `@path` tokens.
fn find_references(input: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let preceded_ok = i == 0 || chars[i - 1].1.is_whitespace();
        if c != '@' || !preceded_ok {
            i += 1;
            continue;
        }
        // The longest run of path characters, backed off to end in a word char or '.'.
        let mut j = i + 1;
        while j < chars.len() && in_path_class(chars[j].1) {
            j += 1;
        }
        let mut end = j;
        while end > i + 1 && !(is_word(chars[end - 1].1) || chars[end - 1].1 == '.') {
            end -= 1;
        }
        // At least two characters: one from the run, then the final [\w.].
        if end >= i + 3 {
            let end_byte = chars.get(end).map_or(input.len(), |(b, _)| *b);
            out.push((at, end_byte));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Ported from InputPreprocessorTests.cs.
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn expand_file_references_valid_file_expands_content() {
        let d = dir();
        std::fs::write(d.path().join("test.txt"), "hello world").unwrap();
        let (r, files) = expand_file_references("explain @test.txt", d.path());
        assert!(r.contains("[File: test.txt]"));
        assert!(r.contains("hello world"));
        assert!(r.contains("[/File]"));
        assert!(!r.contains("@test.txt"));
        assert_eq!(files, ["test.txt"]);
    }

    #[test]
    fn expand_file_references_non_existent_file_left_as_is() {
        let d = dir();
        let (r, _) = expand_file_references("explain @nonexistent.txt", d.path());
        assert!(r.contains("@nonexistent.txt"));
        assert!(!r.contains("[File:"));
    }

    #[test]
    fn expand_file_references_multiple_files_all_expanded() {
        let d = dir();
        std::fs::write(d.path().join("a.txt"), "content a").unwrap();
        std::fs::write(d.path().join("b.txt"), "content b").unwrap();
        let (r, files) = expand_file_references("compare @a.txt and @b.txt", d.path());
        assert!(r.contains("content a") && r.contains("content b"));
        assert!(!r.contains("@a.txt") && !r.contains("@b.txt"));
        assert_eq!(files, ["a.txt", "b.txt"]);
    }

    #[test]
    fn expand_file_references_email_like_pattern_left_as_is() {
        let d = dir();
        let (r, _) = expand_file_references("contact user@email.com for help", d.path());
        assert!(!r.contains("[File:"));
    }

    #[test]
    fn expand_file_references_large_file_truncated() {
        let d = dir();
        std::fs::write(d.path().join("large.txt"), "x".repeat(60_000)).unwrap();
        let (r, _) = expand_file_references("read @large.txt", d.path());
        assert!(r.contains("[File: large.txt]"));
        assert!(r.contains("... (truncated)"));
        assert!(r.len() < 55_000);
    }

    #[test]
    fn expand_file_references_no_at_sign_returns_unchanged() {
        let d = dir();
        let input = "just a normal message";
        assert_eq!(expand_file_references(input, d.path()).0, input);
    }

    #[test]
    fn expand_file_references_empty_input_returns_unchanged() {
        let d = dir();
        assert_eq!(expand_file_references("", d.path()).0, "");
    }

    #[test]
    fn expand_file_references_sub_directory_expands_correctly() {
        let d = dir();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src").join("main.cs"), "class Main {}").unwrap();
        let (r, _) = expand_file_references("review @src/main.cs", d.path());
        assert!(r.contains("class Main {}"));
        assert!(!r.contains("@src/main.cs"));
    }

    #[test]
    fn reference_ends_on_word_char_or_dot() {
        let refs = find_references("see @a/b- and @x, @.");
        let input = "see @a/b- and @x, @.";
        let got: Vec<&str> = refs.iter().map(|(s, e)| &input[*s..*e]).collect();
        assert_eq!(got, ["@a/b"]);
    }
}
