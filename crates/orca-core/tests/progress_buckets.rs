//! Ported from `tests/OpenOrca.Core.Tests/Engines/ProgressBucketTests.cs`: progress.js state
//! files (fixtures in its exact format) read as OpenOrca bars.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_core::progress::{self, ProgressBucketWatcher};

struct Roots {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    fallback: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("orca-progress");
        let fallback = tmp.path().join("orca-progress-fb");
        Self {
            _tmp: tmp,
            root,
            fallback,
        }
    }

    fn all(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.fallback.clone()]
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Write a key file the way progress.js does (JSON.stringify, no spaces).
#[allow(clippy::too_many_arguments)]
fn set(
    root: &Path,
    session: &str,
    key: &str,
    label: &str,
    pct: i32,
    colour: &str,
    started: i64,
    updated: Option<i64>,
) {
    let dir = root.join(session);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{key}.json")),
        format!(
            "{{\"label\":\"{label}\",\"pct\":{pct},\"colour\":\"{colour}\",\"started\":{started},\"updated\":{},\"source\":\"env\"}}",
            updated.unwrap_or_else(now)
        ),
    )
    .unwrap();
}

#[test]
fn read_orders_by_start_maps_colour_to_letter_skips_stale() {
    let r = Roots::new();
    let s = progress::bridge_session("abc123");
    assert_eq!("openorca-abc123", s);
    set(
        &r.root,
        &s,
        "tests",
        "Running API tests",
        37,
        "green",
        2000,
        None,
    );
    set(
        &r.root,
        &s,
        "build",
        "Build the app",
        80,
        "blue",
        1000,
        None,
    );
    set(
        &r.root,
        &s,
        "old",
        "Abandoned",
        10,
        "yellow",
        500,
        Some(now() - 31 * 60 * 1000),
    );
    std::fs::write(r.root.join(&s).join("broken.json"), "{not json").unwrap();

    let bars = progress::read(&s, Some(&r.all()), None);
    assert_eq!(
        vec!["build", "tests"],
        bars.iter().map(|b| b.key.as_str()).collect::<Vec<_>>()
    );
    assert_eq!('A', bars[0].letter());
    assert_eq!('B', bars[1].letter());
    assert_eq!(37.0, bars[1].pct);
    assert_eq!("Running API tests", bars[1].label);
    assert_eq!(bars[1].source.as_deref(), Some("env"));
}

#[test]
fn read_merges_fallback_root_newer_wins() {
    let r = Roots::new();
    let s = "openorca-x";
    set(
        &r.root,
        s,
        "a",
        "Old label",
        10,
        "purple",
        1,
        Some(now() - 5000),
    );
    set(&r.fallback, s, "a", "New label", 60, "purple", 1, None);
    set(&r.fallback, s, "b", "Sandboxed", 5, "orange", 2, None);
    let bars = progress::read(s, Some(&r.all()), None);
    assert_eq!(2, bars.len());
    assert_eq!("New label", bars[0].label);
    assert_eq!('D', bars[0].letter());
    assert_eq!('E', bars[1].letter());
}

#[test]
fn safe_session_matches_progress_js() {
    assert_eq!("openorca-a_b", progress::safe_session("openorca-a/b"));
    assert_eq!("_hidden", progress::safe_session("..hidden"));
    assert_eq!(80, progress::safe_session(&"x".repeat(100)).len());
}

#[test]
fn try_parse_defaults_and_clamps() {
    // Not in the C# tests: missing fields, clamped pct, non-integer times.
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("k.json");
    std::fs::write(&file, r#"{"pct":150,"started":1.5}"#).unwrap();
    let bar = progress::try_parse(&file).unwrap();
    assert_eq!(bar.label, "k");
    assert_eq!(bar.colour, "blue");
    assert_eq!(bar.pct, 100.0);
    assert_eq!(bar.started, 0);
    assert_eq!(bar.source, None);
    std::fs::write(&file, "[1]").unwrap();
    assert_eq!(progress::try_parse(&file), None);
}

fn wait_until(condition: impl Fn() -> bool) {
    for _ in 0..300 {
        if condition() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(condition());
}

#[test]
fn watcher_sees_set_update_done_and_clear() {
    let r = Roots::new();
    let s = "openorca-w";
    // C# passes useFileWatcher: false; the Rust watcher always polls.
    let watcher = ProgressBucketWatcher::new(s, Some(r.all()), Some(Duration::from_millis(50)));
    let changes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&changes);
    watcher.on_changed(Box::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    set(&r.root, s, "demo", "Demo bar", 50, "blue", 1, None);
    wait_until(|| {
        let b = watcher.bars();
        b.len() == 1 && b[0].pct == 50.0
    });

    set(&r.root, s, "demo", "Demo bar", 90, "blue", 1, None);
    wait_until(|| {
        let b = watcher.bars();
        b.len() == 1 && b[0].pct == 90.0
    });

    std::fs::remove_file(r.root.join(s).join("demo.json")).unwrap(); // progress.js done
    wait_until(|| watcher.bars().is_empty());

    set(&r.root, s, "x", "Again", 5, "green", 1, None);
    wait_until(|| watcher.bars().len() == 1);
    watcher.clear_bucket(); // the session ended
    assert!(watcher.bars().is_empty());
    assert!(!r.root.join(s).exists());
    assert!(changes.load(Ordering::SeqCst) >= 4);
    assert_eq!(watcher.session(), s);
}
