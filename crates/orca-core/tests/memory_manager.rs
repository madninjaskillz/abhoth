//! Ported from `MemoryManagerTests.cs`.

use std::path::{Path, PathBuf};

use orca_core::config::OrcaConfig;
use orca_core::config::memory_manager::MemoryManager;

struct Fixture {
    tmp: tempfile::TempDir,
    global_dir: PathBuf,
    config: OrcaConfig,
    manager: MemoryManager,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let global_dir = tmp.path().join("global_memory");
        let config = OrcaConfig::default();
        // skip_project_dir = true keeps it away from the real project's .git root
        let manager = MemoryManager::with_global_dir(&config, &global_dir, true);
        Self {
            tmp,
            global_dir,
            config,
            manager,
        }
    }
}

fn md_files(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn save_learnings_creates_file() {
    let f = Fixture::new();
    f.manager
        .save_learnings("- Learned pattern A\n- Learned pattern B")
        .unwrap();

    assert!(f.global_dir.is_dir());
    let files = md_files(&f.global_dir);
    assert_eq!(files.len(), 1);
    assert!(
        std::fs::read_to_string(&files[0])
            .unwrap()
            .contains("Learned pattern A")
    );
}

#[test]
fn load_all_memory_returns_content() {
    let f = Fixture::new();
    f.manager.save_learnings("- Test learning 1").unwrap();
    assert!(f.manager.load_all_memory().contains("Test learning 1"));
}

#[test]
fn load_all_memory_empty_when_no_files() {
    let f = Fixture::new();
    let mgr = MemoryManager::with_global_dir(&f.config, f.tmp.path().join("empty_memory"), true);
    assert!(mgr.load_all_memory().is_empty());
}

#[test]
fn list_async_returns_files_with_previews() {
    let f = Fixture::new();
    f.manager
        .save_learnings("# First Line Preview\nMore content here")
        .unwrap();
    let list = f.manager.list();
    assert_eq!(list.len(), 1);
    assert!(list[0].1.contains("First Line Preview"));
}

#[test]
fn clear_async_deletes_all_files() {
    let f = Fixture::new();
    f.manager.save_learnings("- Learning 1").unwrap();
    // Different content gives a different hash.
    f.manager
        .save_learnings("- Learning 2 is different")
        .unwrap();

    f.manager.clear();

    assert!(md_files(&f.global_dir).is_empty());
}

#[test]
fn prune_deletes_oldest_when_over_max() {
    let f = Fixture::new();
    let mut config = OrcaConfig::default();
    config.memory.max_memory_files = 2;
    let mgr = MemoryManager::with_global_dir(&config, &f.global_dir, true);

    mgr.save_learnings("Learning A unique content").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    mgr.save_learnings("Learning B different content").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    mgr.save_learnings("Learning C yet another content")
        .unwrap();

    let n = md_files(&f.global_dir).len();
    assert!(n <= 2, "Expected at most 2 files, got {n}");
}

#[test]
fn save_learnings_empty_content_does_nothing() {
    let f = Fixture::new();
    f.manager.save_learnings("").unwrap();
    f.manager.save_learnings("  ").unwrap();
    assert!(md_files(&f.global_dir).is_empty());
}

// --- Rust-only ---

#[test]
fn file_name_is_utc_date_and_hash_prefix() {
    let f = Fixture::new();
    f.manager.save_learnings("abc").unwrap();
    let files = md_files(&f.global_dir);
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    // SHA-256("abc") starts with ba7816.
    assert!(name.ends_with("-ba7816.md"), "{name}");
    assert_eq!(name.len(), "yyyyMMdd-ba7816.md".len());
}

#[test]
fn sources_join_files_with_platform_blank_line() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.global_dir).unwrap();
    let a = f.global_dir.join("a.md");
    let b = f.global_dir.join("b.md");
    std::fs::write(&a, " older ").unwrap();
    std::fs::write(&b, "newer\n").unwrap();
    let now = std::time::SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_modified(now - std::time::Duration::from_secs(60))
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&b)
        .unwrap()
        .set_modified(now)
        .unwrap();
    let nl = orca_tools_common::NEWLINE;
    assert_eq!(f.manager.load_all_memory(), format!("newer{nl}{nl}older"));
}
