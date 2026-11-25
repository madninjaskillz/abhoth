//! Ported from `OpenOrca.Tools.Tests`: `DeleteMoveToolTests.cs`, `MultiEditToolTests.cs`, the
//! apply_patch / UnifiedDiffParser tests of `FileSystemToolTests.cs`, the cd tests of
//! `ShellToolTests.cs` and the file-tool cases of `SecurityTests.cs`, plus exact-output checks.

use std::fs;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_fs::unified_diff::{self, DiffLine, DiffLineType, Hunk, PatchFile};
use orca_tools_fs::*;
use serde_json::{Value, json};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn p(&self, rel: &str) -> String {
        self.dir.path().join(rel).display().to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.dir.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }
    fn exists(&self, rel: &str) -> bool {
        self.dir.path().join(rel).exists()
    }
    async fn run(&self, tool: &dyn Tool, args: Value) -> ToolResult {
        tool.execute(&args, &ToolContext::new(self.dir.path()))
            .await
    }
}

fn windows_dir() -> Option<String> {
    if cfg!(windows) {
        std::env::var("SystemRoot").ok()
    } else {
        Some("/etc".into())
    }
}

// ── DeleteFileTool ──

#[tokio::test]
async fn delete_file_tool_deletes_file() {
    let env = Env::new();
    env.write("to_delete.txt", "content");
    let r = env
        .run(&DeleteFileTool, json!({"path": env.p("to_delete.txt")}))
        .await;
    assert!(!r.is_error);
    assert!(!env.exists("to_delete.txt"));
    assert_eq!(
        r.content,
        format!("Deleted file: {}", env.p("to_delete.txt"))
    );
}

#[tokio::test]
async fn delete_file_tool_deletes_empty_directory() {
    let env = Env::new();
    fs::create_dir(env.p("empty_dir")).unwrap();
    let r = env
        .run(&DeleteFileTool, json!({"path": env.p("empty_dir")}))
        .await;
    assert!(!r.is_error);
    assert!(!env.exists("empty_dir"));
}

#[tokio::test]
async fn delete_file_tool_requires_recursive_for_non_empty_dir() {
    let env = Env::new();
    env.write("nonempty_dir/file.txt", "data");
    let r = env
        .run(&DeleteFileTool, json!({"path": env.p("nonempty_dir")}))
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("recursive"));
}

#[tokio::test]
async fn delete_file_tool_recursive_deletes_non_empty_dir() {
    let env = Env::new();
    env.write("nonempty_dir2/file.txt", "data");
    let r = env
        .run(
            &DeleteFileTool,
            json!({"path": env.p("nonempty_dir2"), "recursive": true}),
        )
        .await;
    assert!(!r.is_error);
    assert!(!env.exists("nonempty_dir2"));
}

#[tokio::test]
async fn delete_file_tool_rejects_not_found() {
    let env = Env::new();
    let r = env
        .run(&DeleteFileTool, json!({"path": "/nonexistent/path/xyz"}))
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("not found"));
}

#[tokio::test]
async fn delete_file_tool_rejects_dangerous_path() {
    let env = Env::new();
    let Some(dangerous) = windows_dir() else {
        return;
    };
    let r = env.run(&DeleteFileTool, json!({"path": dangerous})).await;
    assert!(r.is_error);
    assert!(r.content.to_lowercase().contains("dangerous"));
}

// ── SecurityTests (file tools) ──

#[tokio::test]
async fn delete_file_tool_rejects_user_home_directory() {
    let env = Env::new();
    let Some(home) = orca_tools_common::path_safety::user_profile() else {
        return;
    };
    let r = env.run(&DeleteFileTool, json!({"path": home})).await;
    assert!(r.is_error);
    assert!(r.content.to_lowercase().contains("dangerous"));
}

#[tokio::test]
async fn delete_file_tool_rejects_path_traversal() {
    let env = Env::new();
    let path = if cfg!(windows) {
        r"C:\Users\test\..\.."
    } else {
        "/home/user/../../etc"
    };
    let r = env.run(&DeleteFileTool, json!({"path": path})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn move_file_tool_rejects_dangerous_source() {
    let env = Env::new();
    let Some(dangerous) = windows_dir() else {
        return;
    };
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": dangerous, "destination": env.p("x")}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.to_lowercase().contains("dangerous"));
}

#[tokio::test]
async fn copy_file_tool_rejects_nonexistent_source() {
    let env = Env::new();
    let r = env
        .run(
            &CopyFileTool,
            json!({"source": "/nonexistent/path", "destination": env.p("dst")}),
        )
        .await;
    assert!(r.is_error);
}

// ── MoveFileTool / RenameFileTool ──

#[tokio::test]
async fn move_file_tool_moves_file() {
    let env = Env::new();
    env.write("src.txt", "content");
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": env.p("src.txt"), "destination": env.p("dst.txt")}),
        )
        .await;
    assert!(!r.is_error);
    assert!(!env.exists("src.txt"));
    assert_eq!(env.read("dst.txt"), "content");
    assert_eq!(
        r.content,
        format!(
            "Moved file: {} \u{2192} {}",
            env.p("src.txt"),
            env.p("dst.txt")
        )
    );
}

#[tokio::test]
async fn move_file_tool_creates_parent_dirs() {
    let env = Env::new();
    env.write("move_src.txt", "data");
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": "move_src.txt", "destination": "sub/dir/move_dst.txt"}),
        )
        .await;
    assert!(!r.is_error);
    assert!(env.exists("sub/dir/move_dst.txt"));
}

#[tokio::test]
async fn move_file_tool_refuses_existing_destination_without_overwrite() {
    let env = Env::new();
    env.write("a.txt", "a");
    env.write("b.txt", "b");
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": "a.txt", "destination": "b.txt"}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.starts_with("Error moving: "));
    assert_eq!(env.read("b.txt"), "b");
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": "a.txt", "destination": "b.txt", "overwrite": true}),
        )
        .await;
    assert!(!r.is_error);
    assert_eq!(env.read("b.txt"), "a");
}

#[tokio::test]
async fn move_file_tool_overwrites_directory_with_backup() {
    let env = Env::new();
    env.write("from/new.txt", "new");
    env.write("to/old.txt", "old");
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": "from", "destination": "to"}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("Destination directory already exists"));
    let r = env
        .run(
            &MoveFileTool,
            json!({"source": "from", "destination": "to", "overwrite": true}),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(env.exists("to/new.txt") && !env.exists("to/old.txt"));
    let leftovers = fs::read_dir(env.dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("orca-backup")
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn move_file_affected_paths_are_source_and_path() {
    let args = json!({"source": "a.txt", "destination": "b.txt"});
    assert_eq!(
        MoveFileTool.affected_file_paths(&args, &orca_abstractions::ToolContext::new(".")),
        vec!["a.txt"]
    );
}

#[tokio::test]
async fn rename_file_tool_renames_file() {
    let env = Env::new();
    env.write("old.txt", "x");
    let r = env
        .run(
            &RenameFileTool,
            json!({"source": "old.txt", "destination": "new.txt"}),
        )
        .await;
    assert_eq!(
        r.content,
        format!(
            "Renamed: {} \u{2192} {}",
            env.p("old.txt"),
            env.p("new.txt")
        )
    );
    assert!(env.exists("new.txt"));
}

// ── CopyFileTool ──

#[tokio::test]
async fn copy_file_tool_copies_file() {
    let env = Env::new();
    env.write("copy_src.txt", "copy me");
    let r = env
        .run(
            &CopyFileTool,
            json!({"source": env.p("copy_src.txt"), "destination": env.p("copy_dst.txt")}),
        )
        .await;
    assert!(!r.is_error);
    assert!(env.exists("copy_src.txt"));
    assert_eq!(env.read("copy_dst.txt"), "copy me");
}

#[tokio::test]
async fn copy_file_tool_refuses_existing_file_without_overwrite() {
    let env = Env::new();
    env.write("a.txt", "a");
    env.write("b.txt", "b");
    let r = env
        .run(
            &CopyFileTool,
            json!({"source": "a.txt", "destination": "b.txt"}),
        )
        .await;
    assert_eq!(
        r.content,
        format!(
            "Error copying: The file '{}' already exists.",
            env.p("b.txt")
        )
    );
}

#[tokio::test]
async fn copy_file_tool_requires_recursive_for_dirs() {
    let env = Env::new();
    env.write("copy_dir_src/file.txt", "data");
    let r = env
        .run(
            &CopyFileTool,
            json!({"source": env.p("copy_dir_src"), "destination": env.p("copy_dir_dst")}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("recursive"));
}

#[tokio::test]
async fn copy_file_tool_recursive_copies_dir() {
    let env = Env::new();
    env.write("copy_dir_src2/file.txt", "nested data");
    env.write("copy_dir_src2/sub/deep.txt", "deep");
    let r = env
        .run(
            &CopyFileTool,
            json!({"source": env.p("copy_dir_src2"), "destination": env.p("copy_dir_dst2"), "recursive": true}),
        )
        .await;
    assert!(!r.is_error);
    assert!(env.exists("copy_dir_dst2/file.txt"));
    assert!(env.exists("copy_dir_dst2/sub/deep.txt"));
}

// ── mkdir / create_directory / cd ──

#[tokio::test]
async fn mkdir_creates_and_reports_existing() {
    let env = Env::new();
    let r = env.run(&MkdirTool, json!({"path": "a/b"})).await;
    assert_eq!(
        r.content,
        format!(
            "Directory created: {}",
            env.p("a\\b".replace('\\', std::path::MAIN_SEPARATOR_STR).as_str())
        )
    );
    let r = env.run(&CreateDirectoryTool, json!({"path": "a/b"})).await;
    assert!(r.content.starts_with("Directory already exists: "));
}

#[tokio::test]
async fn cd_tool_valid_directory_changes_directory() {
    // C# checks the process directory; here the result asks the host to switch.
    let env = Env::new();
    fs::create_dir(env.p("sub")).unwrap();
    let r = env.run(&CdTool, json!({"path": "sub"})).await;
    assert!(!r.is_error);
    assert_eq!(r.content, format!("Changed directory to: {}", env.p("sub")));
    assert_eq!(r.cwd_change(), Some(env.dir.path().join("sub")));
}

#[tokio::test]
async fn cd_tool_invalid_directory_returns_error() {
    let env = Env::new();
    let r = env
        .run(
            &CdTool,
            json!({"path": "/nonexistent/path/that/should/not/exist"}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.cwd_change().is_none());
}

// ── MultiEditTool ──

#[tokio::test]
async fn multi_edit_applies_edits_to_multiple_files() {
    let env = Env::new();
    env.write("file1.txt", "Hello World");
    env.write("file2.txt", "Foo Bar Baz");
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [
                {"path": env.p("file1.txt"), "old_string": "World", "new_string": "OpenOrca"},
                {"path": env.p("file2.txt"), "old_string": "Bar", "new_string": "Qux"}
            ]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(env.read("file1.txt"), "Hello OpenOrca");
    assert_eq!(env.read("file2.txt"), "Foo Qux Baz");
    assert!(r.content.contains("2 edit(s)"));
}

#[tokio::test]
async fn multi_edit_rolls_back_on_failure() {
    let env = Env::new();
    env.write("rollback1.txt", "Original1");
    env.write("rollback2.txt", "Original2");
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [
                {"path": env.p("rollback1.txt"), "old_string": "Original1", "new_string": "Modified1"},
                {"path": env.p("rollback2.txt"), "old_string": "NONEXISTENT", "new_string": "Modified2"}
            ]}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("not found"));
    assert_eq!(env.read("rollback1.txt"), "Original1");
    assert_eq!(env.read("rollback2.txt"), "Original2");
}

#[tokio::test]
async fn multi_edit_replace_all_replaces_all_occurrences() {
    let env = Env::new();
    env.write("replaceall.txt", "aaa bbb aaa ccc aaa");
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [{"path": env.p("replaceall.txt"), "old_string": "aaa", "new_string": "zzz", "replace_all": true}]}),
        )
        .await;
    assert!(!r.is_error);
    assert_eq!(env.read("replaceall.txt"), "zzz bbb zzz ccc zzz");
    assert!(r.content.ends_with(": 3 replacement(s)"));
}

#[tokio::test]
async fn multi_edit_empty_edits_array_returns_error() {
    let env = Env::new();
    let r = env.run(&MultiEditTool, json!({"edits": []})).await;
    assert!(r.is_error);
    assert!(r.content.contains("empty"));
}

#[tokio::test]
async fn multi_edit_multiple_edits_to_same_file() {
    let env = Env::new();
    env.write("multi.txt", "alpha beta gamma");
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [
                {"path": env.p("multi.txt"), "old_string": "alpha", "new_string": "ALPHA"},
                {"path": env.p("multi.txt"), "old_string": "gamma", "new_string": "GAMMA"}
            ]}),
        )
        .await;
    assert!(!r.is_error);
    assert_eq!(env.read("multi.txt"), "ALPHA beta GAMMA");
}

#[tokio::test]
async fn multi_edit_errors_on_non_unique_string() {
    let env = Env::new();
    env.write("nonunique.txt", "hello hello hello");
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [{"path": env.p("nonunique.txt"), "old_string": "hello", "new_string": "world"}]}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("3 times"));
}

#[tokio::test]
async fn multi_edit_errors_on_missing_file() {
    let env = Env::new();
    let r = env
        .run(
            &MultiEditTool,
            json!({"edits": [{"path": "/nonexistent/file.txt", "old_string": "a", "new_string": "b"}]}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.to_lowercase().contains("not found"));
}

#[tokio::test]
async fn multi_edit_affected_paths_list_every_edit() {
    let args = json!({"edits": [{"path": "a"}, {"path": "b"}, {}]});
    assert_eq!(
        MultiEditTool.affected_file_paths(&args, &orca_abstractions::ToolContext::new(".")),
        vec!["a", "b"]
    );
}

// ── ApplyPatchTool ──

#[tokio::test]
async fn apply_patch_tool_applies_single_hunk_patch() {
    let env = Env::new();
    env.write("patch_test.txt", "line1\nline2\nline3\nline4\nline5\n");
    let f = env.p("patch_test.txt");
    let patch =
        format!("--- a/{f}\n+++ b/{f}\n@@ -2,3 +2,3 @@\n line2\n-line3\n+line3_modified\n line4");
    let r = env
        .run(&ApplyPatchTool, json!({"patch": patch, "path": f}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("1 hunk(s) applied"));
    let content = env.read("patch_test.txt");
    assert!(content.contains("line3_modified"));
    assert!(!content.contains("\nline3\n"));
    assert_eq!(content, "line1\nline2\nline3_modified\nline4\nline5\n");
}

#[tokio::test]
async fn apply_patch_tool_applies_multi_hunk_patch() {
    let env = Env::new();
    let lines: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
    env.write("multi_hunk.txt", &(lines.join("\n") + "\n"));
    let patch = "--- a/file\n+++ b/file\n@@ -2,3 +2,3 @@\n line2\n-line3\n+line3_changed\n line4\n@@ -18,3 +18,3 @@\n line18\n-line19\n+line19_changed\n line20";
    let r = env
        .run(
            &ApplyPatchTool,
            json!({"patch": patch, "path": env.p("multi_hunk.txt")}),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("2 hunk(s) applied"));
    let content = env.read("multi_hunk.txt");
    assert!(content.contains("line3_changed") && content.contains("line19_changed"));
}

#[tokio::test]
async fn apply_patch_tool_creates_new_file() {
    let env = Env::new();
    let patch = "--- /dev/null\n+++ b/new_file.txt\n@@ -0,0 +1,3 @@\n+hello\n+world\n+!";
    let r = env
        .run(
            &ApplyPatchTool,
            json!({"patch": patch, "path": env.p("new_file.txt")}),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("Created"));
    let nl = orca_tools_common::NEWLINE;
    assert_eq!(env.read("new_file.txt"), format!("hello{nl}world{nl}!{nl}"));
}

#[tokio::test]
async fn apply_patch_tool_rejects_invalid_patch() {
    let env = Env::new();
    let r = env
        .run(
            &ApplyPatchTool,
            json!({"patch": "this is not a valid patch"}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("Could not parse"));
}

#[tokio::test]
async fn apply_patch_tool_fuzzy_match_with_offset() {
    let env = Env::new();
    env.write("fuzz.txt", "extra\nline1\nline2\nline3\nline4\n");
    let patch = "--- a/file\n+++ b/file\n@@ -2,3 +2,3 @@\n line2\n-line3\n+line3_fuzzed\n line4";
    let r = env
        .run(
            &ApplyPatchTool,
            json!({"patch": patch, "path": env.p("fuzz.txt")}),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(env.read("fuzz.txt").contains("line3_fuzzed"));
}

#[tokio::test]
async fn apply_patch_tool_fails_on_mismatched_context() {
    let env = Env::new();
    env.write("mismatch.txt", "alpha\nbeta\ngamma\n");
    let patch =
        "--- a/file\n+++ b/file\n@@ -1,3 +1,3 @@\n wrong_context\n-beta\n+beta_changed\n gamma";
    let r = env
        .run(
            &ApplyPatchTool,
            json!({"patch": patch, "path": env.p("mismatch.txt")}),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("context does not match"));
}

#[tokio::test]
async fn apply_patch_tool_result_format() {
    let env = Env::new();
    env.write("one.txt", "one\n");
    env.write("gone.txt", "bye\n");
    let patch = "--- a/one.txt\n+++ b/one.txt\n@@ -1,1 +1,1 @@\n-one\n+ONE\n--- a/gone.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye";
    let r = env.run(&ApplyPatchTool, json!({"patch": patch})).await;
    let nl = orca_tools_common::NEWLINE;
    assert_eq!(
        r.content,
        format!(
            "Applied 2/2 hunk(s) across 2 file(s)\n\nPatched: {} (1 hunk(s) applied){nl}Deleted: {}{nl}",
            env.p("one.txt"),
            env.p("gone.txt")
        )
    );
    assert_eq!(env.read("one.txt"), "ONE\n");
    assert!(!env.exists("gone.txt"));
}

// ── UnifiedDiffParser ──

#[test]
fn unified_diff_parser_parses_single_file_patch() {
    let patch =
        "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,3 +1,4 @@\n line1\n+inserted\n line2\n line3";
    let files = unified_diff::parse(patch);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].source_path, "hello.txt");
    assert_eq!(files[0].hunks.len(), 1);
    assert_eq!(files[0].hunks[0].lines.len(), 4);
}

#[test]
fn unified_diff_parser_parses_new_file_patch() {
    let patch = "--- /dev/null\n+++ b/newfile.txt\n@@ -0,0 +1,2 @@\n+first\n+second";
    let files = unified_diff::parse(patch);
    assert_eq!(files.len(), 1);
    assert!(files[0].is_new_file());
    assert_eq!(files[0].target_path, "newfile.txt");
}

#[test]
fn unified_diff_parser_applies_simple_patch() {
    let content = "aaa\nbbb\nccc\n";
    let patch = PatchFile {
        source_path: "test.txt".into(),
        target_path: "test.txt".into(),
        hunks: vec![Hunk {
            source_start: 1,
            source_count: 3,
            target_start: 1,
            target_count: 3,
            lines: vec![
                DiffLine::new(DiffLineType::Context, "aaa"),
                DiffLine::new(DiffLineType::Remove, "bbb"),
                DiffLine::new(DiffLineType::Add, "BBB"),
                DiffLine::new(DiffLineType::Context, "ccc"),
            ],
        }],
    };
    let result = unified_diff::apply(content, &patch);
    assert!(result.success);
    assert_eq!(result.hunks_applied, 1);
    let out = result.result_content.unwrap();
    assert!(out.contains("BBB"));
    assert!(!out.contains("\nbbb\n"));
}

#[test]
fn unified_diff_parser_keeps_crlf_and_reads_header_counts() {
    let files = unified_diff::parse("--- a/x\r\n+++ b/x\r\n@@ -2 +2 @@\r\n-b\r\n+B");
    let h = &files[0].hunks[0];
    assert_eq!(
        (
            h.source_start,
            h.source_count,
            h.target_start,
            h.target_count
        ),
        (2, 1, 2, 1)
    );
    let r = unified_diff::apply("a\r\nb\r\nc\r\n", &files[0]);
    assert_eq!(r.result_content.unwrap(), "a\r\nB\r\nc\r\n");
}
