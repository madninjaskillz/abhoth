//! The archive tool (C# has no tests for it): create / list / extract round trip, the error
//! paths, and the zip-slip guard.

use std::fs;
use std::io::Write;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_archive::ArchiveTool;
use serde_json::{Value, json};

async fn run(dir: &std::path::Path, args: Value) -> ToolResult {
    ArchiveTool.execute(&args, &ToolContext::new(dir)).await
}

#[tokio::test]
async fn create_list_extract_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    fs::create_dir_all(d.join("data/sub")).unwrap();
    fs::create_dir_all(d.join("data/empty")).unwrap();
    fs::write(d.join("data/a.txt"), "hello").unwrap();
    fs::write(d.join("data/sub/b.txt"), "x".repeat(5000)).unwrap();

    let r = run(
        d,
        json!({"action": "create", "archive_path": "out/data.zip", "source_path": "data"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.starts_with(&format!(
        "Created: {} (",
        d.join("out").join("data.zip").display()
    )));
    assert!(r.content.ends_with(" bytes)"));

    let r = run(d, json!({"action": "list", "archive_path": "out/data.zip"})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("Entries: 3"), "{}", r.content);
    assert!(r.content.contains("  data/a.txt"));
    assert!(r.content.contains("data/sub/b.txt"));
    assert!(r.content.contains("data/empty/"));
    assert!(r.content.contains("       5,000 bytes  "), "{}", r.content);
    assert!(r.content.contains("Total uncompressed: 5,005 bytes"));

    let r = run(
        d,
        json!({"action": "extract", "archive_path": "out/data.zip", "output_path": "unz"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(
        r.content,
        format!("Extracted 3 entries to: {}", d.join("unz").display())
    );
    assert_eq!(
        fs::read_to_string(d.join("unz/data/a.txt")).unwrap(),
        "hello"
    );
    assert!(d.join("unz/data/empty").is_dir());

    // A single file is stored under its name; extract defaults to the archive's folder.
    let r = run(
        d,
        json!({"action": "create", "archive_path": "one.zip", "source_path": "data/a.txt"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    fs::remove_file(d.join("data/a.txt")).unwrap();
    let r = run(d, json!({"action": "extract", "archive_path": "one.zip"})).await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "hello");
}

#[tokio::test]
async fn errors_match_csharp_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let zip = d.join("missing.zip");
    let r = run(d, json!({"action": "list", "archive_path": "missing.zip"})).await;
    assert_eq!(r.content, format!("Archive not found: {}", zip.display()));
    let r = run(
        d,
        json!({"action": "create", "archive_path": "missing.zip"}),
    )
    .await;
    assert_eq!(r.content, "'source_path' is required for action 'create'.");
    let r = run(
        d,
        json!({"action": "create", "archive_path": "x.zip", "source_path": "nope"}),
    )
    .await;
    assert_eq!(
        r.content,
        format!("Source not found: {}", d.join("nope").display())
    );
    let r = run(d, json!({"action": "frob", "archive_path": "x.zip"})).await;
    assert_eq!(
        r.content,
        "Unknown action: frob. Use 'create', 'extract', or 'list'."
    );
}

#[tokio::test]
async fn extract_refuses_path_traversal_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    {
        let f = fs::File::create(d.join("evil.zip")).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file("../escape.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(b"pwned").unwrap();
        z.finish().unwrap();
    }
    let r = run(
        d,
        json!({"action": "extract", "archive_path": "evil.zip", "output_path": "out"}),
    )
    .await;
    assert!(r.is_error);
    assert_eq!(
        r.content,
        "Archive contains path traversal entry: ../escape.txt"
    );
    assert!(!d.join("escape.txt").exists());
}
