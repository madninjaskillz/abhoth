//! Ported from tests/OpenOrca.Core.Tests/CheckpointManagerTests.cs, plus the C# manifest
//! format (`fixtures/csharp/checkpoints`, written by the C# `CheckpointManager`; the file path
//! was replaced with `C:\work\project\a.txt`).

use std::path::{Path, PathBuf};

use orca_core::config::dotnet_json::to_string_dotnet_with_newline;
use orca_core::session::CheckpointManager;
use orca_core::session::checkpoint::{CheckpointEntry, path_hash};

struct Fixture {
    temp: tempfile::TempDir,
    manager: CheckpointManager,
}

fn setup() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let manager = CheckpointManager::new(temp.path().join("checkpoints"));
    Fixture { temp, manager }
}

impl Fixture {
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let p = self.temp.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }
}

#[test]
fn snapshot_creates_backup_and_manifest() {
    let f = setup();
    let file = f.file("test.txt", "original content");
    f.manager.snapshot(&file, "session1").unwrap();

    let entries = f.manager.list("session1");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_path, file.display().to_string());
    assert!(entries[0].size_bytes > 0);
    let backup = f
        .temp
        .path()
        .join("checkpoints/session1")
        .join(&entries[0].backup_file);
    assert_eq!(std::fs::read_to_string(backup).unwrap(), "original content");
}

#[test]
fn snapshot_skips_duplicate_for_same_session() {
    let f = setup();
    let file = f.file("dup.txt", "content");
    f.manager.snapshot(&file, "session1").unwrap();
    f.manager.snapshot(&file, "session1").unwrap(); // duplicate
    assert_eq!(f.manager.list("session1").len(), 1);
}

#[test]
fn snapshot_skips_non_existent_file() {
    let f = setup();
    let file = f.temp.path().join("nonexistent.txt");
    f.manager.snapshot(&file, "session1").unwrap();
    assert!(f.manager.list("session1").is_empty());
}

#[test]
fn restore_recover_original_content() {
    let f = setup();
    let file = f.file("restore.txt", "original");
    f.manager.snapshot(&file, "session1").unwrap();
    std::fs::write(&file, "modified").unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "modified");

    assert!(f.manager.restore(&file, "session1").unwrap());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
}

#[test]
fn restore_returns_false_for_uncheckpointed_file() {
    let f = setup();
    let file = f.temp.path().join("nocheckpoint.txt");
    assert!(!f.manager.restore(&file, "session1").unwrap());
}

#[test]
fn diff_shows_differences() {
    let f = setup();
    let file = f.file("diff.txt", "line1\nline2\nline3");
    f.manager.snapshot(&file, "session1").unwrap();
    std::fs::write(&file, "line1\nMODIFIED\nline3").unwrap();

    let diff = f.manager.diff(&file, "session1").unwrap();
    assert!(diff.contains('-'));
    assert!(diff.contains('+'));
    assert!(diff.contains("MODIFIED"));
    // The exact C# layout.
    let nl = orca_tools_common_newline();
    let name = file.display().to_string();
    assert_eq!(
        diff,
        [
            format!("--- {name} (checkpoint)"),
            format!("+++ {name} (current)"),
            "@@ line 2 @@".into(),
            "- line2".into(),
            "+ MODIFIED".into(),
        ]
        .join(nl)
    );
}

fn orca_tools_common_newline() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}

#[test]
fn diff_returns_no_checkpoint_message() {
    let f = setup();
    let file = f.temp.path().join("nodiff.txt");
    let diff = f.manager.diff(&file, "session1").unwrap();
    assert!(diff.contains("No checkpoint"));
}

#[test]
fn cleanup_removes_session_dir() {
    let f = setup();
    let file = f.file("cleanup.txt", "content");
    f.manager.snapshot(&file, "session1").unwrap();
    f.manager.cleanup("session1").unwrap();
    assert!(f.manager.list("session1").is_empty());
}

#[test]
fn list_empty_for_no_checkpoints() {
    let f = setup();
    assert!(f.manager.list("nonexistent_session").is_empty());
}

#[test]
fn multiple_files_independent_checkpoints() {
    let f = setup();
    let file1 = f.file("multi1.txt", "content1");
    let file2 = f.file("multi2.txt", "content2");
    f.manager.snapshot(&file1, "session1").unwrap();
    f.manager.snapshot(&file2, "session1").unwrap();
    assert_eq!(f.manager.list("session1").len(), 2);

    std::fs::write(&file1, "modified1").unwrap();
    std::fs::write(&file2, "modified2").unwrap();
    f.manager.restore(&file1, "session1").unwrap();
    assert_eq!(std::fs::read_to_string(&file1).unwrap(), "content1");
    assert_eq!(std::fs::read_to_string(&file2).unwrap(), "modified2");
}

// ── Beyond the C# tests ──

#[test]
fn backup_name_is_stamp_and_path_hash() {
    // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    assert_eq!(path_hash("abc"), "ba7816bf8f01cfea");
    let f = setup();
    let file = f.file("h.txt", "x");
    f.manager.snapshot(&file, "s").unwrap();
    let e = &f.manager.list("s")[0];
    let hash = path_hash(&file.display().to_string());
    assert!(
        e.backup_file.ends_with(&format!("_{hash}.bak")),
        "{}",
        e.backup_file
    );
    assert_eq!(e.backup_file.len(), "yyyyMMdd_HHmmss_".len() + 16 + 4);
    assert_eq!(&e.backup_file[..15], e.timestamp.compact_stamp());
}

#[test]
fn corrupt_manifest_is_dropped() {
    let f = setup();
    let dir = f.temp.path().join("checkpoints/s");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), "{oops").unwrap();
    assert!(f.manager.list("s").is_empty());
    assert!(!dir.join("manifest.json").exists());
}

#[test]
fn cleanup_older_than_removes_only_old_dirs() {
    let f = setup();
    let file = f.file("a.txt", "x");
    f.manager.snapshot(&file, "fresh").unwrap();
    assert_eq!(
        f.manager
            .cleanup_older_than(std::time::Duration::from_secs(3600)),
        0
    );
    assert_eq!(f.manager.cleanup_older_than(std::time::Duration::ZERO), 1);
    assert!(f.manager.list("fresh").is_empty());
}

#[test]
fn csharp_manifest_reads_and_writes_back_identically() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/csharp/checkpoints");
    let text = std::fs::read_to_string(base.join("csharp01/manifest.json"))
        .unwrap()
        .replace("\r\n", "\n");
    let m = CheckpointManager::new(&base);
    let entries = m.list("csharp01");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_path, r"C:\work\project\a.txt");
    assert_eq!(entries[0].size_bytes, 9);
    assert_eq!(
        entries[0].timestamp.compact_stamp(),
        entries[0].backup_file[..15]
    );
    assert!(
        base.join("csharp01")
            .join(&entries[0].backup_file)
            .is_file()
    );
    let ours: &[CheckpointEntry] = &entries;
    assert_eq!(to_string_dotnet_with_newline(ours, "\n").unwrap(), text);
}
