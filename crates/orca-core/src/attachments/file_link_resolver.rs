//! `FileLinkResolver`: file paths in output, resolved to files that exist.
//!
//! Finds file paths in output (tool headers, assistant prose, inline code) so a renderer can
//! make them clickable (OSC 8 `file://` links in the TUI, anchors in the web UI). Relative
//! paths resolve against the working directory; a `:12` / `:12:3` / `:10-20` line suffix is
//! allowed but not part of the target.
//!
//! The C# candidate pattern starts with a lookbehind (`(?<![\w/\\.~-])`), which the `regex`
//! crate lacks: the scan below checks the character before each match by hand (as D8/D14 do).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;

use super::{expand_home, full_path};

/// A file path found in some text: where it is (byte offsets), and the file it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReference {
    /// Start of the path in the text (byte offset; C# counts UTF-16 units).
    pub index: usize,
    /// Length of the path in bytes (any `:line` suffix included).
    pub length: usize,
    /// Absolute path of the existing file or directory.
    pub full_path: String,
}

impl FileReference {
    /// The `file:///` URI to link to.
    pub fn uri(&self) -> String {
        to_file_uri(&self.full_path)
    }
}

// Path-ish runs: word chars plus the separators and punctuation paths use (the C# pattern
// without its leading lookbehind).
static CANDIDATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:[A-Za-z]:[\\/]|~[\\/]|\.{1,2}[\\/]|[\\/])?[\w.@+\-]+(?:[\\/][\w.@+\-]+)*[\\/]?(?::\d+(?:[:-]\d+)?)?",
    )
    .expect("static regex")
});

static LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":\d+(?:[:-]\d*)?$").expect("static regex"));

static EXTENSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.[A-Za-z0-9]{1,8}$").expect("static regex"));

/// The lookbehind's class: `[\w/\\.~-]`.
fn blocks_start(c: char) -> bool {
    is_word(c) || matches!(c, '/' | '\\' | '.' | '~' | '-')
}

/// .NET `\w`: letters, marks, digits, connector punctuation.
fn is_word(c: char) -> bool {
    let mut buf = [0u8; 4];
    WORD.is_match(c.encode_utf8(&mut buf))
}

static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\w$").expect("static regex"));

const CACHE_TTL: Duration = Duration::from_secs(3);
static EXISTS_CACHE: LazyLock<Mutex<HashMap<String, (bool, Instant)>>> =
    LazyLock::new(Default::default);

/// The file a path-like token names, when it exists: `token` may be quoted or backticked,
/// carry a line suffix, and be relative to `working_directory`.
pub fn resolve(token: &str, working_directory: &str) -> Option<String> {
    let t = token.trim().trim_matches(['`', '"', '\'']);
    if t.is_empty()
        || t.encode_utf16().count() > 1024
        || t.contains("://")
        || t.contains(['\n', '\r', '*', '?', '<', '>', '|', '\0'])
    {
        return None;
    }
    let t = LINE_SUFFIX.replace(t, "");
    if t.is_empty() {
        return None;
    }
    let t = expand_home(&t);
    let full = full_path(working_directory, &t);
    exists(&full).then_some(full)
}

/// File paths in prose that name existing files. Only tokens that look like paths are
/// checked: they contain a separator, or end in a file extension (`README.md`).
pub fn find_paths(text: &str, working_directory: &str) -> Vec<FileReference> {
    let mut found = Vec::new();
    for m in candidates(text) {
        let value = &text[m.0..m.1];
        // Trailing sentence punctuation is not part of the path ("see src/app.cs.").
        let trimmed = value.trim_end_matches(['.', ':', '-']);
        if trimmed.is_empty() {
            continue;
        }
        let core = LINE_SUFFIX.replace(trimmed, "");
        let has_separator = core.contains('/') || core.contains('\\');
        if !has_separator && !EXTENSION.is_match(&core) {
            continue;
        }
        // Lone numbers / versions ("1.2.3") and single dots aren't paths.
        if !core.chars().any(char::is_alphabetic) {
            continue;
        }
        if let Some(full) = resolve(trimmed, working_directory) {
            found.push(FileReference {
                index: m.0,
                length: trimmed.len(),
                full_path: full,
            });
        }
    }
    found
}

/// The candidate matches (byte ranges) as the C# pattern finds them: a match may not start
/// right after a word character or one of `/ \ . ~ -`.
fn candidates(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= text.len() {
        let Some(m) = CANDIDATE.find_at(text, pos) else {
            break;
        };
        let start = m.start();
        let blocked = text[..start].chars().next_back().is_some_and(blocks_start);
        if blocked {
            // No match may start here; try from the next character.
            pos = start + text[start..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        out.push((start, m.end()));
        pos = m.end().max(start + 1);
    }
    out
}

/// A `file:///` URI for an absolute path: forward slashes, and everything but unreserved
/// characters and `/ :` percent-encoded (so the URI holds no spaces or brackets, which also
/// keeps it safe inside console markup).
pub fn to_file_uri(full_path: &str) -> String {
    let p = full_path.replace('\\', "/");
    let mut sb = String::with_capacity(p.len() + 16);
    sb.push_str("file://");
    if !p.starts_with('/') {
        sb.push('/');
    }
    for b in p.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '/' | ':') {
            sb.push(c);
        } else {
            sb.push_str(&format!("%{b:02X}"));
        }
    }
    sb
}

fn exists(full: &str) -> bool {
    let now = Instant::now();
    let mut cache = EXISTS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((hit, at)) = cache.get(full)
        && now.duration_since(*at) < CACHE_TTL
    {
        return *hit;
    }
    let exists = std::path::Path::new(full).exists();
    if cache.len() > 4096 {
        cache.clear();
    }
    cache.insert(full.to_string(), (exists, now));
    exists
}
