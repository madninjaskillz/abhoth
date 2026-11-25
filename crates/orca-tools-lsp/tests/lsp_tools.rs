//! The LSP tools against `orca-testkit`'s fake language server (there are no C# tests for
//! these tools; see `fake_lsp_server.rs` for the scripted answers).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use orca_abstractions::{Tool, ToolContext};
use orca_tools_common::NEWLINE;
use orca_tools_lsp::client::{detect_language, path_to_uri, uri_to_path};
use orca_tools_lsp::*;
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    manager: Arc<LspClientManager>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {\n    let bad = 1;\n    // todo\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("clean.rs"), "fn ok() {}\n").unwrap();
        let mut configs = HashMap::new();
        configs.insert(
            "rust".to_string(),
            LspServerConfig {
                command: orca_testkit::fake_bin("fake-lsp-server")
                    .display()
                    .to_string(),
                ..LspServerConfig::default()
            },
        );
        configs.insert(
            "python".to_string(),
            LspServerConfig {
                command: "this_lsp_server_does_not_exist_42".into(),
                ..LspServerConfig::default()
            },
        );
        let manager = Arc::new(LspClientManager::new(configs, dir.path()));
        Self { dir, manager }
    }

    fn ctx(&self) -> ToolContext {
        ToolContext::new(self.dir.path())
    }

    fn file(&self, name: &str) -> String {
        uri_to_path(&path_to_uri(&self.dir.path().join(name)))
    }

    async fn run(&self, tool: &dyn Tool, args: Value) -> (bool, String) {
        let r = tool.execute(&args, &self.ctx()).await;
        (r.is_error, r.content)
    }
}

fn lines(parts: &[&str]) -> String {
    parts.iter().map(|l| format!("{l}{NEWLINE}")).collect()
}

#[test]
fn detect_language_maps_extensions_case_insensitively() {
    for (file, lang) in [
        ("a.cs", Some("csharp")),
        ("a.TSX", Some("typescript")),
        ("a.jsx", Some("javascript")),
        ("a.py", Some("python")),
        ("a.rs", Some("rust")),
        ("a.h", Some("cpp")),
        ("a.c", Some("c")),
        ("a.kts", Some("kotlin")),
        ("a.txt", None),
        ("Makefile", None),
    ] {
        assert_eq!(detect_language(Path::new(file)), lang, "{file}");
    }
}

#[test]
fn path_to_uri_and_back() {
    let dir = std::env::temp_dir().join("with space");
    let path = dir.join("x.rs");
    let uri = path_to_uri(&path);
    assert!(uri.starts_with("file:///"), "{uri}");
    assert!(uri.contains("with%20space"), "{uri}");
    assert!(!uri.contains('\\'), "{uri}");
    assert_eq!(uri_to_path(&uri), path.display().to_string());
    assert_eq!(uri_to_path("untitled:1"), "untitled:1");
}

#[tokio::test]
async fn without_a_manager_every_tool_says_lsp_is_not_configured() {
    let f = Fixture::new();
    let pos = json!({"path": "main.rs", "line": 1, "character": 0});
    for tool in all_tools(None) {
        let args = match tool.name() {
            "lsp_workspace_symbols" => json!({"query": "x", "language": "rust"}),
            _ => pos.clone(),
        };
        let (err, text) = f.run(tool.as_ref(), args).await;
        assert!(err);
        let long = matches!(
            tool.name(),
            "lsp_hover" | "lsp_go_to_definition" | "lsp_find_references"
        );
        if long {
            assert_eq!(
                text,
                "LSP is not configured. Add language server configurations to config.json under 'lsp.servers'."
            );
        } else {
            assert_eq!(text, "LSP is not configured.", "{}", tool.name());
        }
    }
}

#[tokio::test]
async fn workspace_symbols_is_never_configured_as_in_csharp() {
    let f = Fixture::new();
    let tools = all_tools(Some(f.manager.clone()));
    let ws = tools
        .iter()
        .find(|t| t.name() == "lsp_workspace_symbols")
        .unwrap();
    let (err, text) = f
        .run(ws.as_ref(), json!({"query": "Foo", "language": "rust"}))
        .await;
    assert!(err);
    assert_eq!(text, "LSP is not configured.");
}

#[tokio::test]
async fn workspace_symbols_with_a_manager_searches() {
    let f = Fixture::new();
    let tool = WorkspaceSymbolsTool::new(Some(f.manager.clone()));
    let (err, text) = f.run(&tool, json!({"query": "Foo"})).await;
    assert!(err);
    assert_eq!(
        text,
        "Please specify a 'language' parameter (e.g., 'csharp', 'typescript', 'python')."
    );
    let (err, text) = f
        .run(&tool, json!({"query": "Foo", "language": "go"}))
        .await;
    assert!(err);
    assert_eq!(text, "No LSP server configured for language 'go'.");
    let (err, text) = f
        .run(&tool, json!({"query": "Foo", "language": "rust"}))
        .await;
    assert!(!err, "{text}");
    let lib = uri_to_path("file:///w/lib.rs");
    assert_eq!(
        text,
        lines(&[
            "Found 1 symbol(s) matching 'Foo':",
            &format!("  [Class] Foo in crate \u{2014} {lib}:5"),
        ])
    );
    f.manager.dispose().await;
}

#[tokio::test]
async fn unknown_extension_and_missing_server_are_errors() {
    let f = Fixture::new();
    let hover = HoverTool::new(Some(f.manager.clone()));
    let (err, text) = f
        .run(&hover, json!({"path": "notes.txt", "line": 1, "character": 0}))
        .await;
    assert!(err);
    assert_eq!(text, "No language detected for .txt files.");
    let (err, text) = f
        .run(&hover, json!({"path": "app.ts", "line": 1, "character": 0}))
        .await;
    assert!(err);
    assert_eq!(text, "No LSP server configured for language 'typescript'.");
    // Configured but the server cannot start.
    let (err, text) = f
        .run(&hover, json!({"path": "app.py", "line": 1, "character": 0}))
        .await;
    assert!(err);
    assert_eq!(text, "No LSP server configured for language 'python'.");
    let def = GoToDefinitionTool::new(Some(f.manager.clone()));
    let (_, text) = f
        .run(&def, json!({"path": "app.ts", "line": 1, "character": 0}))
        .await;
    assert_eq!(
        text,
        "No LSP server configured for language 'typescript'. Add it to config.json under 'lsp.servers.typescript'."
    );
}

#[tokio::test]
async fn bad_positions_are_rejected() {
    let f = Fixture::new();
    let hover = HoverTool::new(Some(f.manager.clone()));
    for args in [
        json!({"path": "main.rs", "line": 0, "character": 0}),
        json!({"path": "main.rs", "line": 1, "character": -1}),
        json!({"path": "main.rs", "line": "x", "character": 0}),
    ] {
        let (err, text) = f.run(&hover, args).await;
        assert!(err);
        assert_eq!(
            text,
            "line must be an integer >= 1 (1-based) and character an integer >= 0."
        );
    }
    let (err, text) = f.run(&hover, json!({"path": "main.rs", "line": 1})).await;
    assert!(err);
    assert_eq!(text, "Missing required argument: character");
}

#[tokio::test]
async fn hover_definition_and_references() {
    let f = Fixture::new();
    let m = Some(f.manager.clone());
    let main = f.file("main.rs");
    let other = f.file("other.rs");

    let hover = HoverTool::new(m.clone());
    assert_eq!(
        f.run(&hover, json!({"path": "main.rs", "line": "2", "character": 4}))
            .await,
        (false, "hover at 2:4".into())
    );
    assert_eq!(
        f.run(&hover, json!({"path": "main.rs", "line": 100, "character": 0}))
            .await,
        (
            false,
            "No hover information available at this position.".into()
        )
    );
    assert_eq!(
        f.run(&hover, json!({"path": "main.rs", "line": 50, "character": 0}))
            .await,
        (true, "LSP error: fake hover failure".into())
    );

    let def = GoToDefinitionTool::new(m.clone());
    assert_eq!(
        f.run(&def, json!({"path": "main.rs", "line": 2, "character": 4}))
            .await,
        (
            false,
            lines(&["Found 1 definition(s):", &format!("  {main}:1:3")])
        )
    );
    assert_eq!(
        f.run(&def, json!({"path": "main.rs", "line": 100, "character": 0}))
            .await,
        (false, "No definition found.".into())
    );

    let refs = FindReferencesTool::new(m.clone());
    assert_eq!(
        f.run(&refs, json!({"path": "main.rs", "line": 2, "character": 4}))
            .await,
        (
            false,
            lines(&[
                "Found 3 reference(s):",
                &format!("  {main}:"),
                "    Line 2:4",
                "    Line 6:2",
                &format!("  {other}:"),
                "    Line 3:0",
            ])
        )
    );
    f.manager.dispose().await;
}

#[tokio::test]
async fn document_symbols_render_both_shapes() {
    let f = Fixture::new();
    let tool = DocumentSymbolsTool::new(Some(f.manager.clone()));
    assert_eq!(
        f.run(&tool, json!({"path": "main.rs"})).await,
        (
            false,
            lines(&[
                "Symbols in main.rs:",
                "  [Struct] Foo struct Foo \u{2014} line 1",
                "    [Method] bar \u{2014} line 3",
                "  [Function] helper \u{2014} line 10",
            ])
        )
    );
    f.manager.dispose().await;
}

#[tokio::test]
async fn call_hierarchy_both_directions() {
    let f = Fixture::new();
    let tool = CallHierarchyTool::new(Some(f.manager.clone()));
    let main = f.file("main.rs");
    let other = f.file("other.rs");
    assert_eq!(
        f.run(&tool, json!({"path": "main.rs", "line": 1, "character": 3}))
            .await,
        (
            false,
            lines(&[
                "Call hierarchy for: main (Function)",
                "\nIncoming calls (1):",
                &format!("  caller \u{2014} {main}:10"),
                "\nOutgoing calls (1):",
                &format!("  helper \u{2014} {other}:20"),
            ])
        )
    );
    let (_, text) = f
        .run(
            &tool,
            json!({"path": "main.rs", "line": 1, "character": 3, "direction": "Incoming"}),
        )
        .await;
    assert!(!text.contains("Outgoing"), "{text}");
    f.manager.dispose().await;
}

#[tokio::test]
async fn diagnostics_are_published_on_open_and_filtered() {
    let f = Fixture::new();
    let tool = DiagnosticsTool::new(Some(f.manager.clone()));
    assert_eq!(
        f.run(&tool, json!({"path": "main.rs"})).await,
        (
            false,
            lines(&[
                "Diagnostics for main.rs (2 issue(s)):",
                "  Line 2:4 [Error] [E1] (fake): bad thing here",
                "  Line 3:0 [Warning] [7]: unfinished work",
            ])
        )
    );
    assert_eq!(
        f.run(&tool, json!({"path": "main.rs", "severity": "WARNING"}))
            .await,
        (
            false,
            lines(&[
                "Diagnostics for main.rs (1 issue(s)):",
                "  Line 3:0 [Warning] [7]: unfinished work",
            ])
        )
    );
    assert_eq!(
        f.run(&tool, json!({"path": "main.rs", "severity": "hint"}))
            .await,
        (false, "No hint diagnostics for main.rs.".into())
    );
    assert_eq!(
        f.run(&tool, json!({"path": "clean.rs"})).await,
        (
            false,
            "No diagnostics for clean.rs. File appears clean.".into()
        )
    );
    f.manager.dispose().await;
}

#[tokio::test]
async fn did_change_bumps_versions_and_a_dead_server_is_restarted() {
    let f = Fixture::new();
    let path = f.dir.path().join("main.rs");
    f.manager.notify_did_open(&path, "fn main() {}\n").await;
    f.manager
        .notify_did_change(&path, "fn main() { bad }\n")
        .await;
    assert!(f.manager.has_server("rust"));
    assert!(!f.manager.has_server("go"));
    let first = f.manager.get_client("rust").await.unwrap();
    f.manager.dispose().await;
    // After shutdown the next use starts a fresh server.
    let second = f.manager.get_client("rust").await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    f.manager.dispose().await;
}
