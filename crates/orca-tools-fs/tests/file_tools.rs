//! Ported from `OpenOrca.Tools.Tests/FileSystemToolTests.cs` (apply_patch tests wait for the
//! apply_patch port in phase 2) plus exact-output checks against the C# format.

use std::fs;
use std::path::Path;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_common::NEWLINE;
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
    fn root(&self) -> String {
        self.dir.path().display().to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.dir.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }
    async fn run(&self, tool: &dyn Tool, args: Value) -> ToolResult {
        tool.execute(&args, &ToolContext::new(self.dir.path()))
            .await
    }
}

fn ok(r: &ToolResult) {
    assert!(!r.is_error, "unexpected error: {}", r.content);
}

// ── read_file ──

#[tokio::test]
async fn read_file_reads_existing_file_with_csharp_layout() {
    let e = Env::new();
    e.write("test.txt", "line1\nline2\nline3");
    let r = e.run(&ReadFileTool, json!({"path": e.p("test.txt")})).await;
    ok(&r);
    let n = NEWLINE;
    assert_eq!(
        r.content,
        format!(
            "File: {} (3 lines, showing 1-3){n}{n}1\tline1{n}2\tline2{n}3\tline3{n}",
            e.p("test.txt")
        )
    );
}

#[tokio::test]
async fn read_file_relative_path_offset_limit_and_padding() {
    let e = Env::new();
    let body: String = (1..=12).map(|i| format!("l{i}\n")).collect();
    e.write("sub/f.txt", &body);
    let r = e
        .run(
            &ReadFileTool,
            json!({"path": "sub/f.txt", "offset": "9", "limit": 3}),
        )
        .await;
    ok(&r);
    assert!(r.content.contains("(12 lines, showing 9-11)"));
    assert!(
        r.content.contains(&format!(" 9\tl9{NEWLINE}")),
        "{}",
        r.content
    );
    assert!(r.content.contains(&format!("11\tl11{NEWLINE}")));
}

#[tokio::test]
async fn read_file_errors() {
    let e = Env::new();
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("nonexistent/file.txt")}))
        .await;
    assert!(r.is_error && r.content.contains("not found"));

    fs::write(
        e.dir.path().join("binary.bin"),
        [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0],
    )
    .unwrap();
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("binary.bin")}))
        .await;
    assert!(r.is_error && r.content.contains("Binary file"));

    fs::write(e.dir.path().join("data.txt"), [b'a', 0, b'b']).unwrap();
    let r = e.run(&ReadFileTool, json!({"path": e.p("data.txt")})).await;
    assert!(
        r.is_error && r.content.contains("Binary file"),
        "null bytes mean binary"
    );

    e.write("image.png", "not actually binary but has binary extension");
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("image.png")}))
        .await;
    assert!(r.is_error && r.content.contains("Binary file"));
}

#[tokio::test]
async fn read_file_header_and_byte_cap() {
    let e = Env::new();
    e.write("header_test.txt", "a\nb\nc\nd\ne");
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("header_test.txt")}))
        .await;
    assert!(r.content.contains("5 lines") && r.content.contains("showing"));

    let big: String = (0..2000)
        .map(|i| format!("Line {i}: {}{NEWLINE}", "x".repeat(50)))
        .collect();
    e.write("large.txt", &big);
    assert!(fs::metadata(e.dir.path().join("large.txt")).unwrap().len() > 50 * 1024);
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("large.txt")}))
        .await;
    ok(&r);
    assert!(r.content.contains("Truncated") && r.content.contains("50KB"));
    assert!(r.content.contains("+ lines, showing 1-"));

    e.write("small.txt", "just a small file\nwith a few lines\n");
    let r = e
        .run(&ReadFileTool, json!({"path": e.p("small.txt")}))
        .await;
    assert!(!r.content.contains("Truncated"));
}

// ── write_file ──

#[tokio::test]
async fn write_file_creates_overwrites_and_appends() {
    let e = Env::new();
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("new_file.txt"), "content": "Hello World"}),
        )
        .await;
    ok(&r);
    assert_eq!(
        r.content,
        format!("Created: {} (11 chars)", e.p("new_file.txt"))
    );
    assert_eq!(e.read("new_file.txt"), "Hello World");

    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("sub/dir/file.txt"), "content": "nested"}),
        )
        .await;
    ok(&r);
    assert!(e.dir.path().join("sub/dir/file.txt").exists());

    e.write("existing.txt", "line1\nline2\n");
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("existing.txt"), "content": "line1\nline2\nline3\nline4\n"}),
        )
        .await;
    assert_eq!(
        r.content,
        format!(
            "Overwritten: {} (24 chars)\nLines: 3 \u{2192} 5 (+2)",
            e.p("existing.txt")
        )
    );

    e.write("append.txt", "Line1\n");
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("append.txt"), "content": "Line2\n", "append": true}),
        )
        .await;
    assert!(r.content.contains("Appended"));
    assert_eq!(e.read("append.txt"), "Line1\nLine2\n");
}

#[tokio::test]
async fn write_file_rejects_empty_and_role_tags_and_unescapes() {
    let e = Env::new();
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("a.txt"), "content": "  "}),
        )
        .await;
    assert!(r.is_error && r.content.contains("is empty"));
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("a.txt"), "content": "</assistant>"}),
        )
        .await;
    assert!(r.is_error && r.content.contains("role tag"));
    let r = e
        .run(
            &WriteFileTool,
            json!({"path": e.p("b.txt"), "content": "a\\nb"}),
        )
        .await;
    ok(&r);
    assert_eq!(e.read("b.txt"), "a\nb");
}

/// Writing over a folder fails with .NET's `UnauthorizedAccessException` text (cross-check
/// 232).
#[cfg(windows)]
#[tokio::test]
async fn write_file_over_a_folder_names_the_path() {
    let e = Env::new();
    e.write("dir/inner.txt", "i");
    let r = e
        .run(&WriteFileTool, json!({"path": e.p("dir"), "content": "x"}))
        .await;
    assert!(r.is_error);
    assert_eq!(
        r.content,
        format!(
            "Error writing file: Access to the path '{}' is denied.",
            e.p("dir")
        )
    );
}

// ── edit_file ──

#[tokio::test]
async fn edit_file_basic_replacements() {
    let e = Env::new();
    e.write("edit.txt", "Hello World, Hello!");
    ok(&e
        .run(
            &EditFileTool,
            json!({"path": e.p("edit.txt"), "old_string": "World", "new_string": "OpenOrca"}),
        )
        .await);
    assert_eq!(e.read("edit.txt"), "Hello OpenOrca, Hello!");

    e.write("edit2.txt", "Hello Hello");
    let r = e
        .run(
            &EditFileTool,
            json!({"path": e.p("edit2.txt"), "old_string": "Hello", "new_string": "Hi"}),
        )
        .await;
    assert!(r.is_error && r.content.contains("appears 2 times"));

    e.write("edit3.txt", "Hello Hello Hello");
    let r = e
        .run(&EditFileTool, json!({"path": e.p("edit3.txt"), "old_string": "Hello", "new_string": "Hi", "replace_all": true}))
        .await;
    ok(&r);
    assert!(r.content.starts_with("Replaced 3 occurrence(s)"));
    assert_eq!(e.read("edit3.txt"), "Hi Hi Hi");
}

#[tokio::test]
async fn edit_file_create_if_missing() {
    let e = Env::new();
    let r = e
        .run(
            &EditFileTool,
            json!({"path": e.p("brand_new.txt"), "old_string": "", "new_string": "new content", "create_if_missing": true}),
        )
        .await;
    ok(&r);
    assert!(r.content.contains("Created"));
    assert_eq!(e.read("brand_new.txt"), "new content");
    let r = e
        .run(
            &EditFileTool,
            json!({"path": e.p("nope.txt"), "old_string": "something", "new_string": "new", "create_if_missing": true}),
        )
        .await;
    assert!(r.is_error && r.content.to_lowercase().contains("not found"));
    assert!(!Path::new(&e.p("nope.txt")).exists());
}

#[tokio::test]
async fn edit_file_diff_output_matches_csharp() {
    let e = Env::new();
    e.write("diff.txt", "before1\nbefore2\ntarget\nafter1\nafter2");
    let r = e
        .run(
            &EditFileTool,
            json!({"path": e.p("diff.txt"), "old_string": "target", "new_string": "replaced"}),
        )
        .await;
    ok(&r);
    let n = NEWLINE;
    let expected = format!(
        "Replaced 1 occurrence(s) in {}\n\n--- diff ---{n}     1   before1{n}     2   before2{n}     3 - target{n}     3 + replaced{n}     4   after1{n}     5   after2{n}",
        e.p("diff.txt")
    );
    assert_eq!(r.content, expected);
}

#[tokio::test]
async fn edit_file_double_escaped_newlines() {
    let e = Env::new();
    e.write("esc.txt", "line1\nline2\nline3");
    ok(&e.run(&EditFileTool, json!({"path": e.p("esc.txt"), "old_string": "line1\\nline2", "new_string": "replaced"})).await);
    assert_eq!(e.read("esc.txt"), "replaced\nline3");
    e.write("esc2.txt", "aaa\nbbb\nccc");
    ok(&e
        .run(
            &EditFileTool,
            json!({"path": e.p("esc2.txt"), "old_string": "aaa\\nbbb", "new_string": "xxx\\nyyy"}),
        )
        .await);
    assert_eq!(e.read("esc2.txt"), "xxx\nyyy\nccc");
}

#[tokio::test]
async fn edit_file_whitespace_normalized_matches() {
    let e = Env::new();
    e.write("ws.js", "class Foo {\n    update() {\n        this.platforms.create(x + i * 32, pos.y, 'brick');\n    }\n}");
    ok(&e
        .run(
            &EditFileTool,
            json!({"path": e.p("ws.js"), "old_string": "this.platforms.create(x + i * 32, pos.y, 'brick');",
                "new_string": "this.blocks.create(x + i * 32, pos.y, 'brick');"}),
        )
        .await);
    assert!(
        e.read("ws.js")
            .contains("        this.blocks.create(x + i * 32, pos.y, 'brick');")
    );

    e.write(
        "multi.js",
        "function test() {\n    if (true) {\n        doStuff();\n    }\n}",
    );
    ok(&e
        .run(
            &EditFileTool,
            json!({"path": e.p("multi.js"), "old_string": "if (true) {\n    doStuff();\n}",
                "new_string": "if (false) {\n    doOther();\n    doMore();\n}"}),
        )
        .await);
    let c = e.read("multi.js");
    for want in [
        "    if (false) {",
        "        doOther();",
        "        doMore();",
        "    }",
    ] {
        assert!(c.contains(want), "{want:?} in {c}");
    }

    e.write("ambig.txt", "    doStuff();\n    doStuff();\n");
    let r = e.run(&EditFileTool, json!({"path": e.p("ambig.txt"), "old_string": "doStuff();", "new_string": "doOther();"})).await;
    assert!(r.is_error);

    e.write(
        "fuzzy.txt",
        "    public void Hello() {\n        Console.Write(\"hi\");\n    }\n",
    );
    ok(&e
        .run(
            &EditFileTool,
            json!({"path": e.p("fuzzy.txt"), "old_string": "public void Hello() {\n    Console.Write(\"hi\");\n}",
                "new_string": "public void Hello() {\n    Console.Write(\"hello world\");\n}"}),
        )
        .await);
    assert!(e.read("fuzzy.txt").contains("hello world"));
}

#[tokio::test]
async fn edit_file_not_found_shows_current_content() {
    let e = Env::new();
    e.write("x.txt", "alpha\nbeta");
    let r = e
        .run(
            &EditFileTool,
            json!({"path": e.p("x.txt"), "old_string": "gamma", "new_string": "delta"}),
        )
        .await;
    assert!(r.is_error);
    assert_eq!(
        r.content,
        format!(
            "old_string not found in {}. Current file content:\n\nalpha\nbeta",
            e.p("x.txt")
        )
    );
}

mod matching {
    use orca_tools_fs::edit_file_matching::*;

    #[test]
    fn line_trimmed_finds_trailing_whitespace_difference() {
        let m =
            try_line_trimmed_match("line1  \nline2\nline3  \n", "line1\nline2\nline3\n").unwrap();
        assert!(m.contains("line1"));
    }

    #[test]
    fn indentation_flexible_matches_different_indent_level() {
        let content = "class Foo {\n    public void Bar() {\n        DoStuff();\n    }\n}\n";
        let m =
            try_indentation_flexible_match(content, "  public void Bar() {\n      DoStuff();\n  }")
                .unwrap();
        assert!(m.contains("Bar()"));
    }

    #[test]
    fn block_anchor_finds_by_first_and_last_line() {
        let content = [
            "// header",
            "function foo() {",
            "  const a = 1;",
            "  const b = 2;",
            "  const c = 3;",
            "  return a + b;",
            "}",
            "// footer",
        ]
        .join("\n")
            + "\n";
        let old = [
            "function foo() {",
            "  const a = 1;",
            "  const b = 99;",
            "  const c = 3;",
            "  return a + b;",
            "}",
        ]
        .join("\n");
        assert!(
            try_block_anchor_match(&content, &old)
                .unwrap()
                .contains("function foo()")
        );
    }

    #[test]
    fn multi_stage_falls_through_to_whitespace_stage() {
        let m = try_multi_stage_match(
            "  alpha\n  beta\n  gamma\n",
            "alpha\nbeta\ngamma",
            "REPLACED",
        )
        .unwrap();
        assert!(m.matched_old.contains("alpha"));
        assert_eq!(m.adjusted_new, "  REPLACED");
    }

    // WhitespaceNormalizedMatchTests (StringEscapeHelperTests.cs)
    #[test]
    fn whitespace_normalized_cases() {
        assert_eq!(
            try_whitespace_normalized_match("    foo();\n", "foo();").as_deref(),
            Some("    foo();")
        );
        assert_eq!(
            try_whitespace_normalized_match("foo();\n", "foo();"),
            None,
            "identical text is not a fuzzy match"
        );
        assert_eq!(
            try_whitespace_normalized_match("  a\n  a\n", "a"),
            None,
            "ambiguous"
        );
        assert_eq!(try_whitespace_normalized_match("x\n", ""), None);
        assert_eq!(
            try_whitespace_normalized_match("\tfoo\r\n\tbar\r\n", "foo\nbar").as_deref(),
            Some("\tfoo\r\n\tbar\r")
        );
    }

    #[test]
    fn adjust_indentation_adds_only_the_missing_indent() {
        assert_eq!(adjust_indentation("a\n\nb", "    x", "x"), "    a\n\n    b");
        assert_eq!(adjust_indentation("a", "  x", "    x"), "a");
    }
}

// ── list_directory ──

#[tokio::test]
async fn list_directory_lists_dirs_first_then_files() {
    let e = Env::new();
    e.write("b.txt", "");
    e.write("a.txt", "");
    fs::create_dir(e.dir.path().join("subdir")).unwrap();
    e.write(".hidden", "");
    let r = e.run(&ListDirectoryTool, json!({"path": e.root()})).await;
    ok(&r);
    let n = NEWLINE;
    assert_eq!(r.content, format!("3 entries\nsubdir/{n}a.txt{n}b.txt{n}"));
    let r = e
        .run(
            &ListDirectoryTool,
            json!({"path": e.root(), "show_hidden": true, "show_size": true}),
        )
        .await;
    assert!(r.content.contains(".hidden  (0 B)"));
}

#[tokio::test]
async fn list_directory_recursive_skips_node_modules_and_gitignored() {
    let e = Env::new();
    e.write("a/nested.txt", "");
    e.write("top.txt", "");
    e.write("src/app.ts", "code");
    e.write("node_modules/pkg.js", "module");
    e.write(".gitignore", "*.log\n# comment\n/dist/\n");
    e.write("debug.log", "x");
    e.write("dist/out.js", "x");
    let r = e
        .run(
            &ListDirectoryTool,
            json!({"path": e.root(), "recursive": true}),
        )
        .await;
    ok(&r);
    for want in ["nested.txt", "top.txt", "src"] {
        assert!(r.content.contains(want), "{want}: {}", r.content);
    }
    for not in ["node_modules", "debug.log"] {
        assert!(!r.content.contains(not), "{not}: {}", r.content);
    }
    let r = e
        .run(&ListDirectoryTool, json!({"path": e.p("missing")}))
        .await;
    assert!(r.is_error && r.content.starts_with("Directory not found"));
}

// ── glob ──

#[tokio::test]
async fn glob_finds_matching_files_with_size_and_age() {
    let e = Env::new();
    e.write("test.cs", "hello world");
    e.write("test.txt", "");
    let r = e
        .run(&GlobTool, json!({"pattern": "*.cs", "path": e.root()}))
        .await;
    ok(&r);
    let n = NEWLINE;
    assert_eq!(
        r.content,
        format!("1 matches:{n}{n}test.cs  (11B, just now){n}")
    );
}

#[tokio::test]
async fn glob_exclude_and_nested_paths_use_forward_slashes() {
    let e = Env::new();
    e.write("src/app.cs", "");
    e.write("bin/app.cs", "");
    let r = e
        .run(
            &GlobTool,
            json!({"pattern": "**/*.cs", "path": e.root(), "exclude": "bin/**"}),
        )
        .await;
    ok(&r);
    assert!(r.content.contains("src/app.cs"));
    assert!(!r.content.contains("bin"));
    let r = e
        .run(&GlobTool, json!({"pattern": "*.none", "path": e.root()}))
        .await;
    assert_eq!(r.content, "No files matched the pattern.");
}

// ── grep ──

#[tokio::test]
async fn grep_content_mode_matches_csharp_layout() {
    let e = Env::new();
    e.write("search.cs", "public class Foo\n{\n    void Bar() { }\n}\n");
    let r = e
        .run(
            &GrepTool,
            json!({"pattern": "class Foo", "path": e.root(), "glob": "*.cs", "context": 1}),
        )
        .await;
    ok(&r);
    let n = NEWLINE;
    assert_eq!(
        r.content,
        format!("Found 1 match(es):\n\nsearch.cs:1:{n}  > 1\tpublic class Foo{n}    2\t{{{n}{n}")
    );
}

#[tokio::test]
async fn grep_files_only_and_count_modes() {
    let e = Env::new();
    e.write("match1.cs", "class Foo {}");
    e.write("match2.cs", "class Foo {}");
    let r = e.run(&GrepTool, json!({"pattern": "class Foo", "path": e.root(), "glob": "*.cs", "output_mode": "files_only"})).await;
    ok(&r);
    assert!(
        r.content.contains("match1.cs")
            && r.content.contains("match2.cs")
            && r.content.contains("2 files")
    );

    let e = Env::new();
    e.write("counts.cs", "foo\nfoo\nbar\nfoo\n");
    let r = e
        .run(
            &GrepTool,
            json!({"pattern": "foo", "path": e.root(), "glob": "*.cs", "output_mode": "count"}),
        )
        .await;
    let n = NEWLINE;
    assert_eq!(
        r.content,
        format!("1 files with 3 total matches:{n}{n}  counts.cs: 3{n}")
    );
}

#[tokio::test]
async fn grep_default_extensions_case_insensitive_and_errors() {
    let e = Env::new();
    e.write("notes.md", "Hello there");
    e.write("blob.xyz", "hello there");
    let r = e
        .run(
            &GrepTool,
            json!({"pattern": "HELLO", "path": e.root(), "case_insensitive": true}),
        )
        .await;
    assert!(
        r.content.contains("notes.md") && !r.content.contains("blob.xyz"),
        "{}",
        r.content
    );
    let r = e
        .run(&GrepTool, json!({"pattern": "(", "path": e.root()}))
        .await;
    assert!(r.is_error && r.content.starts_with("Invalid regex"));
    let r = e
        .run(&GrepTool, json!({"pattern": "zzz", "path": e.root()}))
        .await;
    assert_eq!(r.content, "No matches found.");
}
