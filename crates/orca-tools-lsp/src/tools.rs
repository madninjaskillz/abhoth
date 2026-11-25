//! The `lsp_*` tools (`DiagnosticsTool`, `HoverTool`, `GoToDefinitionTool`,
//! `FindReferencesTool`, `DocumentSymbolsTool`, `WorkspaceSymbolsTool`, `CallHierarchyTool`).

// The helpers return the tool's finished error result; it is built once per call.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::{append_line, decode_text, get_i32_lenient, required_str};
use serde_json::{Value, json};

use crate::client::{LspClientManager, LspConnection, detect_language, path_to_uri, uri_to_path};
use crate::protocol::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, DocumentSymbol,
    SymbolInformation, parse_locations, severity_name, symbol_kind_name,
};

const NOT_CONFIGURED: &str = "LSP is not configured.";
const NOT_CONFIGURED_HINT: &str =
    "LSP is not configured. Add language server configurations to config.json under 'lsp.servers'.";
const BAD_POSITION: &str = "line must be an integer >= 1 (1-based) and character an integer >= 0.";

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("valid schema")
}

const POSITION_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "path": { "type": "string", "description": "Path to the file" },
        "line": { "type": "integer", "description": "Line number (1-based)" },
        "character": { "type": "integer", "description": "Column number (0-based)" }
    },
    "required": ["path", "line", "character"]
}"#;

/// `Path.GetExtension`: ".rs", or "" without one.
fn extension(path: &Path) -> String {
    match path.extension().map(|e| e.to_string_lossy().to_string()) {
        Some(e) if !e.is_empty() => format!(".{e}"),
        _ => String::new(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The required `path`, made absolute against the working folder.
fn full_path(args: &Value, ctx: &ToolContext) -> Result<PathBuf, ToolResult> {
    required_str(args, "path")
        .map(|p| ctx.full_path(p))
        .map_err(ToolResult::error)
}

/// `line` (1-based, returned 0-based) and `character`, lenient about strings.
fn position(args: &Value) -> Result<(i64, i64), ToolResult> {
    for name in ["line", "character"] {
        if args.get(name).is_none() {
            return Err(ToolResult::error(format!(
                "Missing required argument: {name}"
            )));
        }
    }
    let line = i64::from(get_i32_lenient(args.get("line"), 0)) - 1;
    let character = i64::from(get_i32_lenient(args.get("character"), 0));
    if line < 0 || character < 0 {
        return Err(ToolResult::error(BAD_POSITION));
    }
    Ok((line, character))
}

/// The language's connection, after opening the file (`didOpen`) when it exists.
async fn open_document(
    manager: &LspClientManager,
    path: &Path,
    not_configured_suffix: impl FnOnce(&str) -> String,
) -> Result<Arc<LspConnection>, ToolResult> {
    let Some(lang) = detect_language(path) else {
        return Err(ToolResult::error(format!(
            "No language detected for {} files.",
            extension(path)
        )));
    };
    let Some(conn) = manager.get_client(lang).await else {
        return Err(ToolResult::error(format!(
            "No LSP server configured for language '{lang}'.{}",
            not_configured_suffix(lang)
        )));
    };
    if path.is_file()
        && let Ok(bytes) = tokio::fs::read(path).await
    {
        manager.notify_did_open(path, &decode_text(&bytes)).await;
    }
    Ok(conn)
}

fn no_suffix(_: &str) -> String {
    String::new()
}

fn text_document_position(path: &Path, line: i64, character: i64) -> Value {
    json!({"textDocument": {"uri": path_to_uri(path)}, "position": {"line": line, "character": character}})
}

fn hints(
    emoji: &'static str,
    short: &'static str,
    keys: &'static [&'static str],
) -> Option<ToolRenderHints> {
    Some(ToolRenderHints {
        emoji,
        short_description: short,
        summary_arg_keys: keys,
        aliases: &[],
    })
}

macro_rules! lsp_tool {
    ($ty:ident) => {
        /// See the module docs.
        #[derive(Default)]
        pub struct $ty {
            pub manager: Option<Arc<LspClientManager>>,
        }

        impl $ty {
            pub fn new(manager: Option<Arc<LspClientManager>>) -> Self {
                Self { manager }
            }
        }
    };
}

lsp_tool!(DiagnosticsTool);
lsp_tool!(HoverTool);
lsp_tool!(GoToDefinitionTool);
lsp_tool!(FindReferencesTool);
lsp_tool!(DocumentSymbolsTool);
lsp_tool!(WorkspaceSymbolsTool);
lsp_tool!(CallHierarchyTool);

// ── lsp_diagnostics ──

#[async_trait::async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }
    fn description(&self) -> &str {
        "Get current errors and warnings for a file from the language server. Shows diagnostic messages with severity, line numbers, and codes. Requires an LSP server."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to check for diagnostics" },
            "severity": { "type": "string", "description": "Filter by severity: 'error', 'warning', 'info', 'hint', or 'all' (default: 'all')" }
        },
        "required": ["path"]
    }"#,
        )
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{26a0}\u{fe0f}", "Get diagnostics", &["path"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED);
        };
        let path = match full_path(args, ctx) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // A JSON null severity filters nothing (and names nothing), as in C#.
        let severity: Option<String> = match args.get("severity") {
            None => Some("all".into()),
            Some(v) => v.as_str().map(str::to_lowercase),
        };
        if let Err(e) = open_document(manager, &path, no_suffix).await {
            return e;
        }
        // Give the server a moment to publish diagnostics.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let diagnostics = manager.get_diagnostics(&path);
        let name = file_name(&path);
        if diagnostics.is_empty() {
            return ToolResult::success(format!("No diagnostics for {name}. File appears clean."));
        }
        let wanted = match severity.as_deref() {
            Some("error") => Some(1),
            Some("warning") => Some(2),
            Some("info" | "information") => Some(3),
            Some("hint") => Some(4),
            _ => None,
        };
        let filtered: Vec<_> = diagnostics
            .iter()
            .filter(|d| wanted.is_none() || d.severity == wanted)
            .collect();
        if filtered.is_empty() {
            return ToolResult::success(format!(
                "No {} diagnostics for {name}.",
                severity.unwrap_or_default()
            ));
        }
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!("Diagnostics for {name} ({} issue(s)):", filtered.len()),
        );
        for d in filtered.iter().take(20) {
            let code = d.code_text().map(|c| format!(" [{c}]")).unwrap_or_default();
            let source = d
                .source
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            append_line(
                &mut sb,
                &format!(
                    "  Line {}:{} [{}]{code}{source}: {}",
                    d.range.start.line + 1,
                    d.range.start.character,
                    severity_name(d.severity),
                    d.message
                ),
            );
        }
        if filtered.len() > 20 {
            append_line(
                &mut sb,
                &format!("  ... and {} more diagnostics.", filtered.len() - 20),
            );
        }
        ToolResult::success(sb)
    }
}

// ── lsp_hover ──

/// `ExtractHoverContent`.
fn extract_hover_content(hover: &Value) -> String {
    let Some(contents) = hover.get("contents") else {
        return "No hover content.".into();
    };
    let value_of = |v: &Value| v.get("value").and_then(Value::as_str).map(str::to_string);
    match contents {
        Value::String(s) => s.clone(),
        Value::Object(_) if contents.get("value").is_some() => {
            value_of(contents).unwrap_or_default()
        }
        Value::Array(items) => items
            .iter()
            .map(|c| match c {
                Value::String(s) => s.clone(),
                _ if c.get("value").is_some() => value_of(c).unwrap_or_default(),
                _ => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => other.to_string(),
    }
}

#[async_trait::async_trait]
impl Tool for HoverTool {
    fn name(&self) -> &str {
        "lsp_hover"
    }
    fn description(&self) -> &str {
        "Get type information and documentation for the symbol at the given file position. Returns hover content from the language server. Requires an LSP server configured for the file's language."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(POSITION_SCHEMA)
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f4ac}", "Hover info", &["path", "line"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED_HINT);
        };
        let (path, (line, character)) =
            match full_path(args, ctx).and_then(|p| Ok((p, position(args)?))) {
                Ok(v) => v,
                Err(e) => return e,
            };
        let conn = match open_document(manager, &path, no_suffix).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        match conn
            .request(
                "textDocument/hover",
                text_document_position(&path, line, character),
            )
            .await
        {
            Ok(Value::Null) => {
                ToolResult::success("No hover information available at this position.")
            }
            Ok(result) => ToolResult::success(extract_hover_content(&result)),
            Err(e) => ToolResult::error(format!("LSP error: {e}")),
        }
    }
}

// ── lsp_go_to_definition ──

#[async_trait::async_trait]
impl Tool for GoToDefinitionTool {
    fn name(&self) -> &str {
        "lsp_go_to_definition"
    }
    fn description(&self) -> &str {
        "Jump to the definition of a symbol at the given file position. Returns the file path and line where the symbol is defined. Requires an LSP server configured for the file's language."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(POSITION_SCHEMA)
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f3af}", "Go to definition", &["path", "line"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED_HINT);
        };
        let (path, (line, character)) =
            match full_path(args, ctx).and_then(|p| Ok((p, position(args)?))) {
                Ok(v) => v,
                Err(e) => return e,
            };
        let conn = match open_document(manager, &path, |lang| {
            format!(" Add it to config.json under 'lsp.servers.{lang}'.")
        })
        .await
        {
            Ok(c) => c,
            Err(e) => return e,
        };
        match conn
            .request(
                "textDocument/definition",
                text_document_position(&path, line, character),
            )
            .await
        {
            Ok(result) => {
                let locations = parse_locations(&result);
                if locations.is_empty() {
                    return ToolResult::success("No definition found.");
                }
                let mut sb = String::new();
                append_line(
                    &mut sb,
                    &format!("Found {} definition(s):", locations.len()),
                );
                for loc in &locations {
                    append_line(
                        &mut sb,
                        &format!(
                            "  {}:{}:{}",
                            uri_to_path(&loc.uri),
                            loc.range.start.line + 1,
                            loc.range.start.character
                        ),
                    );
                }
                ToolResult::success(sb)
            }
            Err(e) => ToolResult::error(format!("LSP error: {e}")),
        }
    }
}

// ── lsp_find_references ──

#[async_trait::async_trait]
impl Tool for FindReferencesTool {
    fn name(&self) -> &str {
        "lsp_find_references"
    }
    fn description(&self) -> &str {
        "Find all references to the symbol at the given file position. Returns all locations where the symbol is used. Requires an LSP server configured for the file's language."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(POSITION_SCHEMA)
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f50d}", "Find references", &["path", "line"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED_HINT);
        };
        let (path, (line, character)) =
            match full_path(args, ctx).and_then(|p| Ok((p, position(args)?))) {
                Ok(v) => v,
                Err(e) => return e,
            };
        let conn = match open_document(manager, &path, no_suffix).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        let mut params = text_document_position(&path, line, character);
        params["context"] = json!({"includeDeclaration": true});
        match conn.request("textDocument/references", params).await {
            Ok(result) => {
                let locations = parse_locations(&result);
                if locations.is_empty() {
                    return ToolResult::success("No references found.");
                }
                let mut sb = String::new();
                append_line(&mut sb, &format!("Found {} reference(s):", locations.len()));
                // Grouped by file in first-seen order, each group by line (a stable sort).
                let mut groups: Vec<(&str, Vec<_>)> = Vec::new();
                for loc in &locations {
                    match groups.iter_mut().find(|(u, _)| *u == loc.uri) {
                        Some((_, g)) => g.push(loc),
                        None => groups.push((&loc.uri, vec![loc])),
                    }
                }
                for (uri, mut group) in groups {
                    append_line(&mut sb, &format!("  {}:", uri_to_path(uri)));
                    group.sort_by_key(|l| l.range.start.line);
                    for loc in group {
                        append_line(
                            &mut sb,
                            &format!(
                                "    Line {}:{}",
                                loc.range.start.line + 1,
                                loc.range.start.character
                            ),
                        );
                    }
                }
                ToolResult::success(sb)
            }
            Err(e) => ToolResult::error(format!("LSP error: {e}")),
        }
    }
}

// ── lsp_document_symbols ──

fn render_document_symbol(sb: &mut String, sym: &DocumentSymbol, depth: usize) {
    let indent = " ".repeat((depth + 1) * 2);
    let detail = sym
        .detail
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| format!(" {d}"))
        .unwrap_or_default();
    append_line(
        sb,
        &format!(
            "{indent}[{}] {}{detail} \u{2014} line {}",
            symbol_kind_name(sym.kind),
            sym.name,
            sym.range.start.line + 1
        ),
    );
    for child in sym.children.iter().flatten() {
        render_document_symbol(sb, child, depth + 1);
    }
}

#[async_trait::async_trait]
impl Tool for DocumentSymbolsTool {
    fn name(&self) -> &str {
        "lsp_document_symbols"
    }
    fn description(&self) -> &str {
        "Get the symbol outline (classes, methods, properties, etc.) for a file. Returns a hierarchical list of all symbols defined in the file. Requires an LSP server."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" }
        },
        "required": ["path"]
    }"#,
        )
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f4c4}", "Document outline", &["path"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED);
        };
        let path = match full_path(args, ctx) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let conn = match open_document(manager, &path, no_suffix).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        let params = json!({"textDocument": {"uri": path_to_uri(&path)}});
        let result = match conn.request("textDocument/documentSymbol", params).await {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("LSP error: {e}")),
        };
        if result.is_null() {
            return ToolResult::success("No symbols found in this file.");
        }
        let mut sb = String::new();
        append_line(&mut sb, &format!("Symbols in {}:", file_name(&path)));
        for item in result.as_array().into_iter().flatten() {
            // DocumentSymbol (hierarchical) or SymbolInformation (flat).
            if item.get("children").is_some() || item.get("selectionRange").is_some() {
                if let Ok(sym) = serde_json::from_value::<DocumentSymbol>(item.clone()) {
                    render_document_symbol(&mut sb, &sym, 0);
                }
            } else if let Ok(sym) = serde_json::from_value::<SymbolInformation>(item.clone()) {
                append_line(
                    &mut sb,
                    &format!(
                        "  [{}] {} \u{2014} line {}",
                        symbol_kind_name(sym.kind),
                        sym.name,
                        sym.location.range.start.line + 1
                    ),
                );
            }
        }
        ToolResult::success(sb)
    }
}

// ── lsp_workspace_symbols ──

#[async_trait::async_trait]
impl Tool for WorkspaceSymbolsTool {
    fn name(&self) -> &str {
        "lsp_workspace_symbols"
    }
    fn description(&self) -> &str {
        "Search for symbols (classes, methods, functions, variables) across the entire workspace by name. Returns matching symbols with their locations. Requires an LSP server."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Symbol name or partial name to search for" },
            "language": { "type": "string", "description": "Language ID to use (e.g., 'csharp', 'typescript', 'python'). If omitted, tries all configured servers." }
        },
        "required": ["query"]
    }"#,
        )
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f50e}", "Search symbols", &["query"])
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED);
        };
        let query = match required_str(args, "query") {
            Ok(q) => q,
            Err(e) => return ToolResult::error(e),
        };
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if language.is_empty() {
            return ToolResult::error(
                "Please specify a 'language' parameter (e.g., 'csharp', 'typescript', 'python').",
            );
        }
        let Some(conn) = manager.get_client(language).await else {
            return ToolResult::error(format!(
                "No LSP server configured for language '{language}'."
            ));
        };
        let result = match conn
            .request("workspace/symbol", json!({"query": query}))
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("LSP error: {e}")),
        };
        let symbols: Vec<SymbolInformation> = serde_json::from_value(result).unwrap_or_default();
        if symbols.is_empty() {
            return ToolResult::success(format!("No symbols found matching '{query}'."));
        }
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!("Found {} symbol(s) matching '{query}':", symbols.len()),
        );
        for sym in symbols.iter().take(50) {
            let container = sym
                .container_name
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(|c| format!(" in {c}"))
                .unwrap_or_default();
            append_line(
                &mut sb,
                &format!(
                    "  [{}] {}{container} \u{2014} {}:{}",
                    symbol_kind_name(sym.kind),
                    sym.name,
                    uri_to_path(&sym.location.uri),
                    sym.location.range.start.line + 1
                ),
            );
        }
        if symbols.len() > 50 {
            append_line(
                &mut sb,
                &format!(
                    "  ... and {} more. Refine your query for fewer results.",
                    symbols.len() - 50
                ),
            );
        }
        ToolResult::success(sb)
    }
}

// ── lsp_call_hierarchy ──

#[async_trait::async_trait]
impl Tool for CallHierarchyTool {
    fn name(&self) -> &str {
        "lsp_call_hierarchy"
    }
    fn description(&self) -> &str {
        "Trace the call hierarchy for a function/method at the given position. Shows incoming callers and outgoing callees. Requires an LSP server with call hierarchy support."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" },
            "line": { "type": "integer", "description": "Line number (1-based)" },
            "character": { "type": "integer", "description": "Column number (0-based)" },
            "direction": { "type": "string", "description": "'incoming' (who calls this?), 'outgoing' (what does this call?), or 'both' (default)" }
        },
        "required": ["path", "line", "character"]
    }"#,
        )
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("\u{1f4de}", "Call hierarchy", &["path", "line"])
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = &self.manager else {
            return ToolResult::error(NOT_CONFIGURED);
        };
        let (path, (line, character)) =
            match full_path(args, ctx).and_then(|p| Ok((p, position(args)?))) {
                Ok(v) => v,
                Err(e) => return e,
            };
        let direction = args
            .get("direction")
            .and_then(Value::as_str)
            .map(str::to_lowercase)
            .unwrap_or_else(|| "both".into());
        let conn = match open_document(manager, &path, no_suffix).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        match call_hierarchy(&conn, &path, line, character, &direction).await {
            Ok(text) => ToolResult::success(text),
            Err(e) => ToolResult::error(format!("LSP error: {e}")),
        }
    }
}

async fn call_hierarchy(
    conn: &LspConnection,
    path: &Path,
    line: i64,
    character: i64,
    direction: &str,
) -> Result<String, crate::client::LspError> {
    let items = conn
        .request(
            "textDocument/prepareCallHierarchy",
            text_document_position(path, line, character),
        )
        .await?;
    // The item goes back to the server exactly as it came (servers keep state in `data`).
    let Some(raw_item) = items.as_array().and_then(|a| a.first()).cloned() else {
        return Ok("No call hierarchy item found at this position. Ensure the cursor is on a function/method name.".into());
    };
    let item: CallHierarchyItem = serde_json::from_value(raw_item.clone()).unwrap_or_default();
    let mut sb = String::new();
    append_line(
        &mut sb,
        &format!(
            "Call hierarchy for: {} ({})",
            item.name,
            symbol_kind_name(item.kind)
        ),
    );
    if direction == "incoming" || direction == "both" {
        let result = conn
            .request("callHierarchy/incomingCalls", json!({"item": raw_item}))
            .await?;
        let calls: Option<Vec<CallHierarchyIncomingCall>> = serde_json::from_value(result).ok();
        append_line(
            &mut sb,
            &format!("\nIncoming calls ({}):", calls.as_ref().map_or(0, Vec::len)),
        );
        for call in calls.iter().flatten().take(20) {
            append_line(
                &mut sb,
                &format!(
                    "  {} \u{2014} {}:{}",
                    call.from.name,
                    uri_to_path(&call.from.uri),
                    call.from.range.start.line + 1
                ),
            );
        }
    }
    if direction == "outgoing" || direction == "both" {
        let result = conn
            .request("callHierarchy/outgoingCalls", json!({"item": raw_item}))
            .await?;
        let calls: Option<Vec<CallHierarchyOutgoingCall>> = serde_json::from_value(result).ok();
        append_line(
            &mut sb,
            &format!("\nOutgoing calls ({}):", calls.as_ref().map_or(0, Vec::len)),
        );
        for call in calls.iter().flatten().take(20) {
            append_line(
                &mut sb,
                &format!(
                    "  {} \u{2014} {}:{}",
                    call.to.name,
                    uri_to_path(&call.to.uri),
                    call.to.range.start.line + 1
                ),
            );
        }
    }
    Ok(sb)
}
