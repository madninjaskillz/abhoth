//! Port of `ProjectInstructionsLoader.cs`: ORCA.md / AGENTS.md / CLAUDE.md from the project
//! root, for the system prompt.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use orca_tools_common::utf16_len;
use regex::Regex;

/// A project instruction file loaded for the system prompt (`ProjectInstructionFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructionFile {
    /// Path on disk (as built from the project root).
    pub full_path: PathBuf,
    /// Path relative to the project root with `/` separators, used as the label.
    pub relative_path: String,
    /// Normalized (and possibly truncated) content.
    pub content: String,
    /// Whether the content was cut to fit the size caps.
    pub truncated: bool,
}

/// `ProjectInstructionsLoader.MaxCharsPerFile` (UTF-16 units, as C# counts).
pub const MAX_CHARS_PER_FILE: usize = 12_000;
/// `ProjectInstructionsLoader.MaxTotalChars`.
pub const MAX_TOTAL_CHARS: usize = 24_000;

static IMPORT_ONLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*@\S+\s*)+$").unwrap());

/// `ProjectInstructionsLoader.FindProjectRoot`: walk up from `start_directory` to the first
/// directory holding a `.git` directory or a `*.sln` file.
pub fn find_project_root(start_directory: &Path) -> Option<PathBuf> {
    if start_directory.as_os_str().is_empty()
        || start_directory.to_string_lossy().trim().is_empty()
        || !start_directory.is_dir()
    {
        return None;
    }
    let start = std::path::absolute(start_directory).ok()?;
    let mut dir = Some(start.as_path());
    while let Some(d) = dir {
        // `.git` must be a directory (as `Directory.Exists`): a worktree's `.git` file is not a marker.
        if d.join(".git").is_dir() || has_sln_file(d) {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// `Directory.EnumerateFiles(dir, "*.sln").Any()` (case-insensitive on Windows).
fn has_sln_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_sln = if cfg!(windows) {
            name.to_ascii_lowercase().ends_with(".sln")
        } else {
            name.ends_with(".sln")
        };
        is_sln && e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink())
    })
}

/// `ProjectInstructionsLoader.LoadAsync`: `.orca/ORCA.md`, else `ORCA.md` at the project root.
pub fn load(start_directory: &Path) -> Option<String> {
    let root = find_project_root(start_directory)?;
    for path in [root.join(".orca").join("ORCA.md"), root.join("ORCA.md")] {
        if path.is_file() {
            return read_text(&path).ok();
        }
    }
    None
}

/// `ProjectInstructionsLoader.GetInstructionsPath`: where ORCA.md exists or should be created.
pub fn get_instructions_path(start_directory: &Path) -> Option<PathBuf> {
    let root = find_project_root(start_directory)?;
    let dot_orca = root.join(".orca").join("ORCA.md");
    if dot_orca.is_file() {
        return Some(dot_orca);
    }
    Some(root.join("ORCA.md"))
}

/// `ProjectInstructionsLoader.LoadAllAsync`: every project instruction file, highest precedence
/// first: ORCA.md (`.orca/ORCA.md` preferred), AGENTS.md, CLAUDE.md. Files resolving to the same
/// path, repeating an earlier file's content, empty, or holding only `@file` import lines are
/// skipped. Content is capped per file and in total.
pub fn load_all(start_directory: &Path) -> Vec<ProjectInstructionFile> {
    let Some(root) = find_project_root(start_directory) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    if let Some(orca) = get_instructions_path(start_directory)
        && orca.is_file()
    {
        candidates.push(orca);
    }
    candidates.push(root.join("AGENTS.md"));
    candidates.push(root.join("CLAUDE.md"));

    let mut result = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut seen_content: HashSet<String> = HashSet::new();
    let mut remaining = MAX_TOTAL_CHARS;

    for path in candidates {
        if remaining == 0 || !path.is_file() {
            continue;
        }
        // Case-insensitive path set, as C# (OrdinalIgnoreCase).
        let full = resolve_final_path(&path).to_string_lossy().to_lowercase();
        if !seen_paths.insert(full) {
            continue;
        }
        let Ok(text) = read_text(&path) else {
            continue;
        };
        let mut content = text.replace("\r\n", "\n").trim().to_string();
        if content.is_empty()
            || IMPORT_ONLY.is_match(&content)
            || !seen_content.insert(content.clone())
        {
            continue;
        }

        let cap = MAX_CHARS_PER_FILE.min(remaining);
        let length = utf16_len(&content);
        let truncated = length > cap;
        if truncated {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            content = format!(
                "{}\n[... truncated: {name} exceeds {cap} characters]",
                take_utf16(&content, cap)
            );
        }
        remaining -= cap.min(utf16_len(&content));

        let relative = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
        result.push(ProjectInstructionFile {
            full_path: path,
            relative_path: relative,
            content,
            truncated,
        });
    }
    result
}

/// `ProjectInstructionsLoader.FormatForPrompt`: the files as one system-prompt block, each
/// labelled by path; `None` when there are none.
pub fn format_for_prompt(files: &[ProjectInstructionFile]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut sb = String::from("PROJECT INSTRUCTIONS");
    sb.push_str(if files.len() > 1 {
        " (listed in order of precedence \u{2014} if they conflict, follow the earlier file):"
    } else {
        ":"
    });
    for file in files {
        sb.push_str("\n\n--- ");
        sb.push_str(&file.relative_path);
        sb.push_str(" ---\n");
        sb.push_str(&file.content);
    }
    Some(sb)
}

/// The first `max` UTF-16 units of `s` (never splitting a character).
fn take_utf16(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

/// `ResolveFinalPath`: the final target of a symlink, as a full path.
fn resolve_final_path(path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_link && let Ok(target) = std::fs::canonicalize(path) {
        return strip_verbatim(target);
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `canonicalize` on Windows returns `\\?\C:\...`; drop the prefix to compare with plain paths.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// `File.ReadAllText`: UTF-8 with the BOM removed.
fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}
