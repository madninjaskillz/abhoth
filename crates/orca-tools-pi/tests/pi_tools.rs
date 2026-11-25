//! Port of tests/OpenOrca.Tools.Tests/PiToolsTests.cs: the Pi-profile tools. Cases are ported
//! from PiG's Go tests (internal/codingagent/tools: edit_fuzzy_test.go,
//! tools_edit_port_test.go, truncate tests), which in turn port Pi 0.87.1's
//! packages/coding-agent/test/tools.test.ts.

use std::path::{Path, PathBuf};

use orca_abstractions::{Tool, ToolCapabilities, ToolContext, ToolResult};
use orca_tools_pi::edit_diff::{self, PiEdit};
use orca_tools_pi::{
    PiBashTool, PiEditTool, PiFindTool, PiGrepTool, PiLsTool, PiReadTool, PiShellConfig,
    PiToolsetOptions, PiWriteTool, arguments, bash, create_toolset, paths, read, shell, truncate,
    write,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Validate (like the host pipeline) then execute.
async fn run(tool: &dyn Tool, args_json: &str, cwd: &Path) -> ToolResult {
    let prepared = match tool.prepare_arguments(args_json) {
        Some(Ok(json)) => json,
        Some(Err(e)) => return ToolResult::error(e),
        None => args_json.to_string(),
    };
    let args: Value = serde_json::from_str(&prepared).unwrap();
    tool.execute(&args, &ToolContext::new(cwd)).await
}

async fn run_edit(dir: &Path, content: &str, edits: &[(&str, &str)]) -> (ToolResult, String) {
    let file = dir.join("f.txt");
    std::fs::write(&file, content.as_bytes()).unwrap();
    let edits: Vec<Value> = edits
        .iter()
        .map(|(o, n)| json!({ "oldText": o, "newText": n }))
        .collect();
    let args = json!({ "path": s(&file), "edits": edits }).to_string();
    let result = run(&PiEditTool, &args, dir).await;
    let after = String::from_utf8(std::fs::read(&file).unwrap()).unwrap();
    (result, after)
}

// ── edit: fuzzy matching (tools.test.ts:1072-1250) ──────────────────────────────────

const FUZZY_CASES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "trailing whitespace stripped",
        "line one   \nline two  \nline three\n",
        "line one\nline two\n",
        "replaced\n",
        "replaced\nline three\n",
    ),
    (
        "fullwidth punctuation",
        "\u{4F60}\u{597D}\u{FF0C}\u{4E16}\u{754C}\n\u{4F60}\u{597D}\u{FF08}\u{4E16}\u{754C}\u{FF09}\n",
        "\u{4F60}\u{597D},\u{4E16}\u{754C}\n\u{4F60}\u{597D}(\u{4E16}\u{754C})\n",
        "\u{4F60}\u{597D}\u{FF0C}pi\n\u{4F60}\u{597D}(pi)\n",
        "\u{4F60}\u{597D}\u{FF0C}pi\n\u{4F60}\u{597D}(pi)\n",
    ),
    (
        "compatibility-equivalent forms",
        "\u{FF21}\u{FF22}\u{FF23}\u{FF11}\u{FF12}\u{FF13}\ncaf\u{00E9}\n",
        "ABC123\ncaf\u{00E9}\n",
        "XYZ789\ncoffee\n",
        "XYZ789\ncoffee\n",
    ),
    (
        "smart single quotes",
        "console.log(\u{2018}hello\u{2019});\n",
        "console.log('hello');",
        "console.log('world');",
        "console.log('world');\n",
    ),
    (
        "smart double quotes",
        "const msg = \u{201C}Hello World\u{201D};\n",
        "const msg = \"Hello World\";",
        "const msg = \"Goodbye\";",
        "const msg = \"Goodbye\";\n",
    ),
    (
        "unicode dashes",
        "range: 1\u{2013}5\nbreak\u{2014}here\n",
        "range: 1-5\nbreak-here",
        "range: 10-50\nbreak--here",
        "range: 10-50\nbreak--here\n",
    ),
    (
        "non-breaking space",
        "hello\u{00A0}world\n",
        "hello world",
        "hello universe",
        "hello universe\n",
    ),
    (
        "exact preferred over fuzzy",
        "const x = 'exact';\nconst y = 'other';\n",
        "const x = 'exact';",
        "const x = 'changed';",
        "const x = 'changed';\nconst y = 'other';\n",
    ),
    (
        "fuzzy replacement equal to nearby line",
        "replace me   \nafter   \n",
        "replace me\n",
        "after\n",
        "after\nafter   \n",
    ),
];

#[tokio::test]
async fn edit_fuzzy_matching() {
    for &(name, content, old_text, new_text, want) in FUZZY_CASES {
        let dir = tmp();
        let (result, after) = run_edit(dir.path(), content, &[(old_text, new_text)]).await;
        assert!(!result.is_error, "{name}: {}", result.content);
        assert_eq!(
            result.content,
            format!(
                "Successfully replaced 1 block(s) in {}.",
                s(&dir.path().join("f.txt"))
            ),
            "{name}"
        );
        assert_eq!(after, want, "{name}");
    }
}

#[tokio::test]
async fn edit_fuzzy_multi_edit_preserves_untouched_lines() {
    let dir = tmp();
    let (result, after) = run_edit(
        dir.path(),
        "keep before  \nfirst target  \nfirst after\nkeep middle   \nsecond target  \nsecond after\nkeep after  \n",
        &[
            ("first target\nfirst after", "FIRST\nFIRST2"),
            ("second target\nsecond after", "SECOND\nSECOND2"),
        ],
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Successfully replaced 2 block(s)"),
        "{}",
        result.content
    );
    assert_eq!(
        after,
        "keep before  \nFIRST\nFIRST2\nkeep middle   \nSECOND\nSECOND2\nkeep after  \n"
    );
}

#[tokio::test]
async fn edit_fuzzy_match_rewrites_only_touched_lines() {
    let dir = tmp();
    let (result, after) = run_edit(
        dir.path(),
        "Title \u{2014} with \u{201C}quotes\u{201D}  \nhard break  \nchange \u{2018}me\u{2019}\nx\u{00B2}\n",
        &[("change 'me'", "changed")],
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        after,
        "Title \u{2014} with \u{201C}quotes\u{201D}  \nhard break  \nchanged\nx\u{00B2}\n"
    );
}

#[tokio::test]
async fn edit_fuzzy_multi_edit_mode() {
    let dir = tmp();
    let (result, after) = run_edit(
        dir.path(),
        "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
        &[
            ("console.log('hello');\n", "console.log('world');\n"),
            ("hello world\n", "hello universe\n"),
        ],
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(after, "console.log('world');\nhello universe\n");
}

#[test]
fn edit_keeps_trailing_nul_bytes_and_splits_like_pi() {
    assert_eq!(
        edit_diff::apply_edits(
            "target x\nkeep\0\0",
            &[PiEdit::new("target\u{00A0}x", "done")],
            "file"
        ),
        Ok("done\nkeep\0\0".to_string())
    );
    assert!(edit_diff::split_lines_with_endings("").is_empty());
    assert_eq!(edit_diff::split_lines_with_endings("a"), ["a"]);
    assert_eq!(edit_diff::split_lines_with_endings("a\n"), ["a\n"]);
    assert_eq!(
        edit_diff::split_lines_with_endings("a\n\nb\0"),
        ["a\n", "\n", "b\0"]
    );
}

// ── edit: errors (tools.test.ts:297-417) ────────────────────────────────────────────

#[tokio::test]
async fn edit_errors_use_pi_text_and_leave_file_untouched() {
    let dir = tmp();
    let d = dir.path();
    let file = s(&d.join("f.txt"));

    let (r1, a1) = run_edit(d, "Hello, world!", &[("nonexistent", "testing")]).await;
    assert!(r1.is_error);
    assert_eq!(
        r1.content,
        format!(
            "Could not find the exact text in {file}. The old text must match exactly including all whitespace and newlines."
        )
    );
    assert_eq!(a1, "Hello, world!");

    let (r2, _) = run_edit(d, "foo foo foo", &[("foo", "bar")]).await;
    assert_eq!(
        r2.content,
        format!(
            "Found 3 occurrences of the text in {file}. The text must be unique. Please provide more context to make it unique."
        )
    );

    let (r3, _) = run_edit(d, "hello\nworld\n", &[]).await;
    assert_eq!(
        r3.content,
        "Edit tool input is invalid. edits must contain at least one replacement."
    );

    let (r4, a4) = run_edit(
        d,
        "one\ntwo\nthree\n",
        &[
            ("one\ntwo\n", "ONE\nTWO\n"),
            ("two\nthree\n", "TWO\nTHREE\n"),
        ],
    )
    .await;
    assert_eq!(
        r4.content,
        format!(
            "edits[0] and edits[1] overlap in {file}. Merge them into one edit or target disjoint regions."
        )
    );
    assert_eq!(a4, "one\ntwo\nthree\n");

    let (r5, a5) = run_edit(
        d,
        "alpha\nbeta\ngamma\n",
        &[("alpha\n", "ALPHA\n"), ("missing\n", "MISSING\n")],
    )
    .await;
    assert_eq!(
        r5.content,
        format!(
            "Could not find edits[1] in {file}. The oldText must match exactly including all whitespace and newlines."
        )
    );
    assert_eq!(a5, "alpha\nbeta\ngamma\n");

    let (r6, _) = run_edit(
        d,
        "hello world   \nhello world\n",
        &[("hello world", "replaced")],
    )
    .await;
    assert!(r6.content.contains("Found 2 occurrences"), "{}", r6.content);

    let (r7, _) = run_edit(d, "same\n", &[("same", "same")]).await;
    assert_eq!(
        r7.content,
        format!(
            "No changes made to {file}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    );

    let (r8, _) = run_edit(d, "x\n", &[("", "y")]).await;
    assert_eq!(r8.content, format!("oldText must not be empty in {file}."));
}

#[tokio::test]
async fn edit_missing_file_reports_enoent() {
    let dir = tmp();
    let path = s(&dir.path().join("missing.txt"));
    let args = json!({ "path": path, "edits": [{ "oldText": "hello", "newText": "world" }] });
    let result = run(&PiEditTool, &args.to_string(), dir.path()).await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!("Could not edit file: {path}. Error code: ENOENT.")
    );
}

#[tokio::test]
async fn edit_multiple_disjoint_regions() {
    let dir = tmp();
    let (result, after) = run_edit(
        dir.path(),
        "alpha\nbeta\ngamma\ndelta\n",
        &[("alpha", "ALPHA"), ("gamma", "GAMMA")],
    )
    .await;
    assert!(
        result.content.contains("Successfully replaced 2 block(s)"),
        "{}",
        result.content
    );
    assert_eq!(after, "ALPHA\nbeta\nGAMMA\ndelta\n");
}

// ── edit: CRLF and BOM (tools.test.ts:1299-1364) ────────────────────────────────────

const CRLF_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "line one\r\nline two\r\nline three\r\n",
        "line two\n",
        "replaced line\n",
        "line one\r\nreplaced line\r\nline three\r\n",
    ),
    (
        "first\r\nsecond\r\nthird\r\n",
        "second\n",
        "REPLACED\n",
        "first\r\nREPLACED\r\nthird\r\n",
    ),
    (
        "first\nsecond\nthird\n",
        "second\n",
        "REPLACED\n",
        "first\nREPLACED\nthird\n",
    ),
    (
        "\u{FEFF}first\r\nsecond\r\nthird\r\n",
        "second\n",
        "REPLACED\n",
        "\u{FEFF}first\r\nREPLACED\r\nthird\r\n",
    ),
];

#[tokio::test]
async fn edit_preserves_line_endings_and_bom() {
    for &(content, old_text, new_text, want) in CRLF_CASES {
        let dir = tmp();
        let (result, after) = run_edit(dir.path(), content, &[(old_text, new_text)]).await;
        assert!(!result.is_error, "{content:?}: {}", result.content);
        assert_eq!(after, want, "{content:?}");
    }
}

#[tokio::test]
async fn edit_crlf_bom_multi_edit_and_cross_variant_duplicates() {
    let dir = tmp();
    let (r1, a1) = run_edit(
        dir.path(),
        "\u{FEFF}first\r\nsecond\r\nthird\r\nfourth\r\n",
        &[("second\n", "SECOND\n"), ("fourth\n", "FOURTH\n")],
    )
    .await;
    assert!(!r1.is_error, "{}", r1.content);
    assert_eq!(a1, "\u{FEFF}first\r\nSECOND\r\nthird\r\nFOURTH\r\n");

    let (r2, _) = run_edit(
        dir.path(),
        "hello\r\nworld\r\n---\r\nhello\nworld\n",
        &[("hello\nworld\n", "replaced\n")],
    )
    .await;
    assert!(r2.is_error);
    assert!(r2.content.contains("Found 2 occurrences"), "{}", r2.content);
}

// ── edit: argument repair (edit.ts prepareEditArguments) ────────────────────────────

#[test]
fn prepare_edit_arguments_repairs_common_shapes() {
    let as_string = arguments::prepare_edit_arguments(
        r#"{"path":"a","edits":"[{\"oldText\":\"x\",\"newText\":\"y\"}]"}"#,
    );
    assert_eq!(
        as_string,
        r#"{"path":"a","edits":[{"oldText":"x","newText":"y"}]}"#
    );

    let single =
        arguments::prepare_edit_arguments(r#"{"path":"a","edits":{"oldText":"x","newText":"y"}}"#);
    assert_eq!(
        single,
        r#"{"path":"a","edits":[{"oldText":"x","newText":"y"}]}"#
    );

    let single_string = arguments::prepare_edit_arguments(
        r#"{"path":"a","edits":"{\"oldText\":\"x\",\"newText\":\"y\"}"}"#,
    );
    assert_eq!(
        single_string,
        r#"{"path":"a","edits":[{"oldText":"x","newText":"y"}]}"#
    );

    let legacy = arguments::prepare_edit_arguments(r#"{"path":"a","oldText":"x","newText":"y"}"#);
    assert_eq!(
        legacy,
        r#"{"path":"a","edits":[{"oldText":"x","newText":"y"}]}"#
    );

    // C# asserts reference identity (Assert.Same); Rust returns an owned String, so the
    // "passes through unchanged" contract is checked by value.
    let untouched = r#"{"path":"a","edits":[{"oldText":"x","newText":"y"}]}"#;
    assert_eq!(arguments::prepare_edit_arguments(untouched), untouched);
}

#[tokio::test]
async fn edit_accepts_legacy_top_level_old_new_text() {
    let dir = tmp();
    let file = dir.path().join("legacy.txt");
    std::fs::write(&file, "abc\n").unwrap();
    let args = json!({ "path": s(&file), "oldText": "b", "newText": "B" });
    let result = run(&PiEditTool, &args.to_string(), dir.path()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "aBc\n");
}

// ── validation and coercion (pi-ai utils/validation.ts) ─────────────────────────────

#[test]
fn validation_coerces_numeric_strings_and_drops_optional_nulls() {
    let prep = PiReadTool
        .prepare_arguments(r#"{"path":"a.txt","offset":"5","limit":null}"#)
        .unwrap();
    assert_eq!(prep, Ok(r#"{"path":"a.txt","offset":5}"#.to_string()));
}

#[test]
fn validation_wraps_scalar_into_array() {
    let prep = PiEditTool
        .prepare_arguments(r#"{"path":"a.txt","edits":"not json"}"#)
        .unwrap();
    assert_eq!(
        prep,
        Err("Validation failed for tool \"edit\":\n  - edits.0: must be object\n\nReceived arguments:\n{\n  \"path\": \"a.txt\",\n  \"edits\": \"not json\"\n}".to_string())
    );
}

#[test]
fn validation_reports_missing_required_in_pi_format() {
    let prep = PiWriteTool
        .prepare_arguments(r#"{"path":"a.txt"}"#)
        .unwrap();
    assert_eq!(
        prep,
        Err("Validation failed for tool \"write\":\n  - content: must have required properties content\n\nReceived arguments:\n{\n  \"path\": \"a.txt\"\n}".to_string())
    );
}

#[test]
fn validation_reports_type_mismatch_and_nested_required() {
    let bash = PiBashTool::default()
        .prepare_arguments(r#"{"command":{"x":1}}"#)
        .unwrap()
        .unwrap_err();
    assert!(
        bash.starts_with(
            "Validation failed for tool \"bash\":\n  - command: must be string\n\nReceived arguments:\n"
        ),
        "{bash}"
    );

    let edit = PiEditTool
        .prepare_arguments(r#"{"path":"a","edits":[{"oldText":"x"}]}"#)
        .unwrap()
        .unwrap_err();
    assert!(
        edit.contains("  - edits.0.newText: must have required properties newText"),
        "{edit}"
    );
}

#[test]
fn validation_stringifies_scalars_for_string_fields() {
    let prep = PiWriteTool
        .prepare_arguments(r#"{"path":"a.txt","content":42}"#)
        .unwrap();
    assert_eq!(prep, Ok(r#"{"path":"a.txt","content":"42"}"#.to_string()));
}

// ── truncation (truncate.ts) ────────────────────────────────────────────────────────

#[test]
fn truncate_head_and_tail_follow_pi() {
    let content = (1..=3000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let head = truncate::head_default(&content);
    assert!(head.truncated);
    assert_eq!(head.truncated_by, Some("lines"));
    assert_eq!(head.total_lines, 3000);
    assert_eq!(head.output_lines, 2000);
    assert!(head.content.ends_with("\n2000"));

    let tail = truncate::tail_default(&content);
    assert_eq!(tail.output_lines, 2000);
    assert!(tail.content.starts_with("1001\n"));

    let big = "x".repeat(60 * 1024);
    let partial = truncate::tail_default(&format!("a\n{big}"));
    assert!(partial.last_line_partial);
    assert_eq!(partial.output_bytes, truncate::DEFAULT_MAX_BYTES);

    let first = truncate::head_default(&format!("{big}\nb"));
    assert!(first.first_line_exceeds_limit);
    assert_eq!(first.content, "");

    assert_eq!(truncate::format_size(500), "500B");
    assert_eq!(truncate::format_size(50 * 1024), "50.0KB");
    assert_eq!(truncate::format_size(1536 * 1024), "1.5MB");
    assert_eq!(
        truncate::truncate_line("abcdef", 3),
        ("abc... [truncated]".to_string(), true)
    );
}

// ── read (read.ts) ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn read_returns_raw_content_with_pi_continuation_notices() {
    let dir = tmp();
    let d = dir.path();
    let text = (1..=2500)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(d.join("big.txt"), text).unwrap();

    let all = run(&PiReadTool, r#"{"path":"big.txt"}"#, d).await;
    assert!(!all.is_error, "{}", all.content);
    assert!(all.content.starts_with("line 1\nline 2\n"));
    assert!(
        all.content
            .ends_with("line 2000\n\n[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"),
        "{}",
        &all.content[all.content.len() - 200..]
    );

    let limited = run(
        &PiReadTool,
        r#"{"path":"big.txt","offset":10,"limit":2}"#,
        d,
    )
    .await;
    assert_eq!(
        limited.content,
        "line 10\nline 11\n\n[2489 more lines in file. Use offset=12 to continue.]"
    );

    let beyond = run(&PiReadTool, r#"{"path":"big.txt","offset":9999}"#, d).await;
    assert!(beyond.is_error);
    assert_eq!(
        beyond.content,
        "Offset 9999 is beyond end of file (2500 lines total)"
    );

    let missing = run(&PiReadTool, r#"{"path":"nope.txt"}"#, d).await;
    assert!(missing.is_error);
    assert_eq!(
        missing.content,
        format!(
            "ENOENT: no such file or directory, access '{}'",
            s(&d.join("nope.txt"))
        )
    );
}

#[test]
fn read_byte_limit_and_long_first_line() {
    let lines = vec!["a".repeat(100); 1000].join("\n");
    let by_bytes = read::read_text(&lines, "f", None, None);
    assert!(
        by_bytes.content.contains("(50.0KB limit). Use offset="),
        "{}",
        by_bytes.content
    );

    let long_line = read::read_text(
        &format!("{}\nshort", "z".repeat(60 * 1024)),
        "f.txt",
        None,
        None,
    );
    assert_eq!(
        long_line.content,
        "[Line 1 is 60.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' f.txt | head -c 51200]"
    );
}

// ── write (write.ts) ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_creates_parents_and_reports_path() {
    let dir = tmp();
    let path = s(&dir.path().join("nested").join("dir").join("test.txt"));
    let args = json!({ "path": path, "content": "Nested content" });
    let result = run(&PiWriteTool, &args.to_string(), dir.path()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, format!("Successfully wrote to {path}"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Nested content");
}

#[tokio::test]
async fn write_small_change_to_long_file_hints_edit() {
    let dir = tmp();
    let path = s(&dir.path().join("long.py"));
    let mut lines: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).unwrap();
    lines[10] = "changed".into();

    let args = json!({ "path": path, "content": lines.join("\n") });
    let result = run(&PiWriteTool, &args.to_string(), dir.path()).await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        format!(
            "Successfully wrote to {path} (rewrote 40 lines to change 1; for a change like this use edit, which outputs only the changed text)"
        )
    );
}

#[test]
fn write_rewrite_hint_only_for_small_changes_to_long_files() {
    for (old_lines, new_lines, changed) in [
        (None, 40, 0),      // new file
        (Some(10), 10, 1),  // short file
        (Some(40), 40, 30), // most of the file changed
    ] {
        let previous = old_lines.map(|n: usize| {
            (1..=n)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let content = (1..=new_lines)
            .map(|i| {
                if i <= changed {
                    format!("x{i}")
                } else {
                    format!("l{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            write::rewrite_hint(previous.as_deref(), &content),
            "",
            "{old_lines:?} {new_lines} {changed}"
        );
    }
}

// ── bash (bash.ts) ──────────────────────────────────────────────────────────────────

/// A bash, or `None` (the bash tests then return early: C# skips them with "no bash
/// available").
fn bash_shell() -> Option<PiShellConfig> {
    shell::resolve(None).ok()
}

#[tokio::test]
async fn bash_runs_in_bash_and_reports_exit_codes() {
    let Some(sh) = bash_shell() else {
        return; // no bash available
    };
    let dir = tmp();
    let d = dir.path();
    let tool = PiBashTool::with_shell(Some(sh));

    let ok = run(&tool, r#"{"command":"echo hello; echo err 1>&2"}"#, d).await;
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(ok.content.replace('\r', ""), "hello\nerr\n");

    let none = run(&tool, r#"{"command":"true"}"#, d).await;
    assert_eq!(none.content, "(no output)");

    let fail = run(&tool, r#"{"command":"echo boom; exit 3"}"#, d).await;
    assert!(fail.is_error);
    assert_eq!(
        fail.content.replace('\r', ""),
        "boom\n\n\nCommand exited with code 3"
    );

    let quiet = run(&tool, r#"{"command":"exit 2"}"#, d).await;
    assert_eq!(quiet.content, "(no output)\n\nCommand exited with code 2");
}

#[tokio::test]
async fn bash_passes_quotes_backslashes_and_unicode_through() {
    let Some(sh) = bash_shell() else {
        return; // no bash available
    };
    let dir = tmp();
    let tool = PiBashTool::with_shell(Some(sh));
    let command = "X='a b'; printf '%s|' \"$X\" \"q\\\"uote\" 'back\\slash' \"*.none\" \"h\u{00E9}llo\"; echo";
    let result = run(
        &tool,
        &json!({ "command": command }).to_string(),
        dir.path(),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content.replace('\r', ""),
        "a b|q\"uote|back\\slash|*.none|h\u{00E9}llo|\n"
    );
}

#[tokio::test]
async fn bash_a_truncated_failing_test_run_ends_with_a_test_summary() {
    let Some(sh) = bash_shell() else {
        return; // no bash available
    };
    let dir = tmp();
    let d = dir.path();
    let tool = PiBashTool::with_shell(Some(sh));
    let command = "echo 'FAIL: test_total (tests.test_sales.T)'; seq 1 3000; echo 'Ran 40 tests in 0.31s'; echo; echo 'FAILED (failures=1)'; exit 1";
    let result = run(&tool, &json!({ "command": command }).to_string(), d).await;
    assert!(result.is_error);
    let text = result.content.replace('\r', "");
    let at = text.find("[Showing").expect("a truncation notice");
    assert!(!text[..at].contains("FAIL: test_total"));
    assert!(
        text.contains("[Test summary (whole output): FAILED (failures=1) after Ran 40 tests in 0.31s. Failing: test_total (tests.test_sales.T)]"),
        "{}",
        &text[at..]
    );
    assert!(
        text.ends_with("Command exited with code 1"),
        "{}",
        &text[at..]
    );
    delete_spill_file(&text);

    // Short runs show everything already, and other long output gets no summary.
    let short_run = run(
        &tool,
        &json!({ "command": "echo 'FAIL: test_a (t.T)'; echo 'Ran 1 test in 0.1s'; echo 'FAILED (failures=1)'" }).to_string(),
        d,
    )
    .await;
    assert!(!short_run.content.contains("Test summary"));
    let plain = run(&tool, r#"{"command":"seq 1 3000"}"#, d).await;
    assert!(!plain.content.contains("Test summary"));
    delete_spill_file(&plain.content);
}

/// Removes the "Full output: <path>]" temp file a truncated bash result names (C# leaves
/// some of them behind; harmless either way).
fn delete_spill_file(text: &str) {
    if let Some(at) = text.find("Full output: ") {
        let rest = &text[at + "Full output: ".len()..];
        if let Some(end) = rest.find(']') {
            let _ = std::fs::remove_file(PathBuf::from(&rest[..end]));
        }
    }
}

#[tokio::test]
async fn bash_tail_truncates_and_saves_full_output() {
    let Some(sh) = bash_shell() else {
        return; // no bash available
    };
    let dir = tmp();
    let tool = PiBashTool::with_shell(Some(sh));
    let result = run(&tool, r#"{"command":"seq 1 3000"}"#, dir.path()).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.starts_with("1001\n"));
    let marker = "\n\n[Showing lines 1001-3000 of 3000. Full output: ";
    let at = result.content.find(marker);
    assert!(
        matches!(at, Some(a) if a > 0),
        "{}",
        &result.content[result.content.len() - 200..]
    );
    let at = at.unwrap();
    let path = &result.content[at + marker.len()..result.content.len() - 1];
    assert!(
        regex::Regex::new(r"pi-bash-[0-9a-f]{16}\.log$")
            .unwrap()
            .is_match(path),
        "{path}"
    );
    let saved = std::fs::read_to_string(path).unwrap();
    assert_eq!(saved.split('\n').filter(|l| !l.is_empty()).count(), 3000);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn bash_timeout_and_invalid_timeout() {
    let Some(sh) = bash_shell() else {
        return; // no bash available
    };
    let dir = tmp();
    let d = dir.path();
    let tool = PiBashTool::with_shell(Some(sh));

    let timed_out = run(
        &tool,
        r#"{"command":"echo start; sleep 20","timeout":1}"#,
        d,
    )
    .await;
    assert!(timed_out.is_error);
    assert_eq!(
        timed_out.content.replace('\r', ""),
        format!("start\n\n\n{}", bash::timeout_message("1"))
    );

    let invalid = run(&tool, r#"{"command":"echo x","timeout":0}"#, d).await;
    assert_eq!(
        invalid.content,
        "Invalid timeout: must be a finite number of seconds"
    );
}

#[test]
fn shell_quotes_windows_arguments_by_crt_rules() {
    assert_eq!(bash::quote_windows_argument("a b"), "\"a b\"");
    assert_eq!(
        bash::quote_windows_argument("say \"hi\""),
        "\"say \\\"hi\\\"\""
    );
    assert_eq!(bash::quote_windows_argument("C:\\dir\\"), "\"C:\\dir\\\\\"");
    assert!(shell::is_legacy_wsl_bash(r"C:\Windows\System32\bash.exe"));
    assert!(!shell::is_legacy_wsl_bash(
        r"C:\Program Files\Git\bin\bash.exe"
    ));
}

// ── paths (path-utils.ts) ───────────────────────────────────────────────────────────

#[test]
fn paths_strip_at_prefix_normalise_spaces_and_shell_drives() {
    let dir = tmp();
    let d = dir.path();
    assert_eq!(
        paths::resolve_to_cwd("@a\u{00A0}b.txt", d),
        orca_abstractions::full_path(d, "a b.txt")
    );
    if cfg!(windows) {
        assert_eq!(
            paths::normalize_windows_shell_path("/c/Users/x"),
            r"C:\Users\x"
        );
        assert_eq!(paths::normalize_windows_shell_path("/mnt/d"), r"D:\");
        assert_eq!(paths::normalize_windows_shell_path("/tmp/x"), "/tmp/x");
    }
}

// ── ls / find / grep (optional extras) ──────────────────────────────────────────────

#[tokio::test]
async fn extras_ls_find_grep_use_pi_formats() {
    let dir = tmp();
    let d = dir.path();
    std::fs::create_dir_all(d.join("src").join("sub")).unwrap();
    std::fs::write(d.join("src").join("a.ts"), "const a = 1;\nTODO fix\n").unwrap();
    std::fs::write(d.join("src").join("sub").join("b.ts"), "TODO later\n").unwrap();
    std::fs::write(d.join("README.md"), "hi\n").unwrap();

    let ls = run(&PiLsTool, "{}", d).await;
    assert_eq!(ls.content, "README.md\nsrc/");

    let find = run(&PiFindTool, r#"{"pattern":"*.ts"}"#, d).await;
    assert_eq!(find.content, "src/a.ts\nsrc/sub/b.ts");
    let none = run(&PiFindTool, r#"{"pattern":"*.py"}"#, d).await;
    assert_eq!(none.content, "No files found matching pattern");

    let grep = run(&PiGrepTool, r#"{"pattern":"TODO"}"#, d).await;
    assert_eq!(
        grep.content,
        "src/a.ts:2: TODO fix\nsrc/sub/b.ts:1: TODO later"
    );
    let ctx = run(
        &PiGrepTool,
        r#"{"pattern":"fix","context":1,"glob":"*.ts"}"#,
        d,
    )
    .await;
    // Pi splits the file on "\n", so the empty line after the final newline is context too.
    assert_eq!(
        ctx.content,
        "src/a.ts-1- const a = 1;\nsrc/a.ts:2: TODO fix\nsrc/a.ts-3- "
    );
    let limited = run(&PiGrepTool, r#"{"pattern":"TODO","limit":1}"#, d).await;
    assert_eq!(
        limited.content,
        "src/a.ts:2: TODO fix\n\n[1 matches limit reached. Use limit=2 for more, or refine pattern]"
    );
}

// ── tool metadata ───────────────────────────────────────────────────────────────────

#[test]
fn toolset_exposes_pi_names_schemas_and_capabilities() {
    let tools = create_toolset(&PiToolsetOptions {
        extra_tools: true,
        ..Default::default()
    });
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        ["read", "bash", "edit", "write", "grep", "find", "ls", "job"]
    );
    let core = create_toolset(&PiToolsetOptions {
        jobs: false,
        ..Default::default()
    });
    let core_names: Vec<&str> = core.iter().map(|t| t.name()).collect();
    assert_eq!(core_names, ["read", "bash", "edit", "write"]);
    for t in &tools {
        assert!(
            t.capabilities().contains(ToolCapabilities::VERBATIM_OUTPUT),
            "{}",
            t.name()
        );
    }
    let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();
    assert!(
        by_name("edit")
            .capabilities()
            .contains(ToolCapabilities::MODIFIES_FILE_CONTENT | ToolCapabilities::MODIFIES_FILES)
    );
    assert!(
        by_name("read")
            .capabilities()
            .contains(ToolCapabilities::READS_FILE_CONTENT)
    );
    assert_eq!(
        serde_json::to_string(&by_name("write").parameter_schema()).unwrap(),
        r#"{"type":"object","required":["path","content"],"properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}}}"#
    );
}
