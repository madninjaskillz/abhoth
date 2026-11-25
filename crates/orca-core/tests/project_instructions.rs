//! Ported from `ProjectInstructionsLoaderTests.cs`.

use std::path::{Path, PathBuf};

use orca_core::config::project_instructions::{
    MAX_CHARS_PER_FILE, MAX_TOTAL_CHARS, find_project_root, format_for_prompt,
    get_instructions_path, load, load_all,
};

struct Temp {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Temp {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        Self { _tmp: tmp, dir }
    }
    fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

#[test]
fn find_project_root_finds_git_directory() {
    let t = Temp::new();
    let sub = t.mkdir("src/deep");
    t.mkdir(".git");
    assert_eq!(find_project_root(&sub), Some(t.dir.clone()));
}

#[test]
fn find_project_root_finds_sln_file() {
    let t = Temp::new();
    let sub = t.mkdir("src");
    t.write("Test.sln", "");
    assert_eq!(find_project_root(&sub), Some(t.dir.clone()));
}

#[test]
fn find_project_root_returns_null_when_no_marker_found() {
    let t = Temp::new();
    let isolated = t.mkdir("no-markers/deep");
    // Markers may exist in parent dirs on the real filesystem; it must just not fail.
    let root = find_project_root(&isolated);
    assert!(root.is_none_or(|r| r.is_dir()));
}

#[test]
fn load_async_returns_content_when_orca_md_exists() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("ORCA.md", "# Test Instructions\nDo stuff.");
    let content = load(&t.dir).unwrap();
    assert!(content.contains("Test Instructions"));
}

#[test]
fn load_async_prefers_orca_dir_file() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("ORCA.md", "root level");
    t.write(".orca/ORCA.md", "orca dir level");
    assert_eq!(load(&t.dir).as_deref(), Some("orca dir level"));
}

#[test]
fn load_async_returns_null_when_no_file_exists() {
    let t = Temp::new();
    t.mkdir(".git");
    assert_eq!(load(&t.dir), None);
}

#[test]
fn get_instructions_path_returns_path_when_file_exists() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("ORCA.md", "content");
    let path = get_instructions_path(&t.dir).unwrap();
    assert!(path.to_string_lossy().ends_with("ORCA.md"));
}

#[test]
fn get_instructions_path_returns_default_path_when_no_file() {
    let t = Temp::new();
    t.mkdir(".git");
    let path = get_instructions_path(&t.dir).unwrap();
    assert!(path.to_string_lossy().ends_with("ORCA.md"));
}

// --- LoadAllAsync: ORCA.md + AGENTS.md + CLAUDE.md ---

#[test]
fn load_all_async_reads_all_three_in_precedence_order() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("CLAUDE.md", "claude rules");
    t.write("AGENTS.md", "agents rules");
    t.write(".orca/ORCA.md", "orca rules");

    let files = load_all(&t.dir);

    let rel: Vec<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();
    assert_eq!(rel, [".orca/ORCA.md", "AGENTS.md", "CLAUDE.md"]);
    let content: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
    assert_eq!(content, ["orca rules", "agents rules", "claude rules"]);
}

#[test]
fn load_all_async_works_without_orca_md() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("AGENTS.md", "agents only");
    let files = load_all(&t.dir);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative_path, "AGENTS.md");
}

#[test]
fn load_all_async_skips_duplicate_content_and_import_only_stubs() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("ORCA.md", "same rules\r\n");
    t.write("AGENTS.md", "same rules");
    t.write("CLAUDE.md", "@AGENTS.md\n");
    let files = load_all(&t.dir);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative_path, "ORCA.md");
}

#[test]
fn load_all_async_caps_size() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("AGENTS.md", &"a".repeat(MAX_CHARS_PER_FILE + 500));
    t.write("CLAUDE.md", &"b".repeat(MAX_CHARS_PER_FILE + 500));

    let files = load_all(&t.dir);

    assert!(files.iter().all(|f| f.truncated));
    let total: usize = files.iter().map(|f| f.content.len()).sum();
    assert!(total < MAX_TOTAL_CHARS + 500);
    assert!(files[0].content.contains("truncated"));
}

#[test]
fn format_for_prompt_labels_each_file_by_path() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("ORCA.md", "orca rules");
    t.write("CLAUDE.md", "claude rules");

    let block = format_for_prompt(&load_all(&t.dir)).unwrap();

    assert!(block.starts_with("PROJECT INSTRUCTIONS"));
    assert!(block.contains("--- ORCA.md ---\norca rules"));
    assert!(block.contains("--- CLAUDE.md ---\nclaude rules"));
    assert!(block.find("ORCA.md").unwrap() < block.find("CLAUDE.md").unwrap());
}

#[test]
fn format_for_prompt_no_files_returns_null() {
    assert_eq!(format_for_prompt(&[]), None);
}

// --- Rust-only ---

#[test]
fn truncation_message_names_the_file_and_cap() {
    let t = Temp::new();
    t.mkdir(".git");
    t.write("AGENTS.md", &"a".repeat(MAX_CHARS_PER_FILE + 1));
    let files = load_all(&t.dir);
    assert_eq!(
        files[0].content,
        format!(
            "{}\n[... truncated: AGENTS.md exceeds {MAX_CHARS_PER_FILE} characters]",
            "a".repeat(MAX_CHARS_PER_FILE)
        )
    );
    assert_eq!(
        format_for_prompt(&files).unwrap(),
        format!(
            "PROJECT INSTRUCTIONS:\n\n--- AGENTS.md ---\n{}",
            files[0].content
        )
    );
}

#[test]
fn git_file_is_not_a_root_marker() {
    // A worktree's `.git` is a file; C# only accepts a `.git` directory.
    let t = Temp::new();
    t.mkdir("proj/.git");
    t.write("proj/wt/.git", "gitdir: elsewhere");
    let wt: &Path = &t.dir.join("proj/wt");
    assert_eq!(find_project_root(wt), Some(t.dir.join("proj")));
}
