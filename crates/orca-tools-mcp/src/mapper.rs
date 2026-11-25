//! `McpToolMapper`: pure mapping between MCP protocol types and the tool abstractions, kept
//! free of a live client so it is unit-testable.

use base64::Engine as _;
use orca_abstractions::{ToolErrorCode, ToolResult, ToolResultPart, ToolRiskLevel};
use rmcp::model::{CallToolResult, ContentBlock, ToolAnnotations};
use serde_json::{Map, Value};

/// `mcp_{server}_{tool}`, lowercased, every non-ASCII-alphanumeric character replaced by `_`.
/// The server prefix keeps same-named tools on different servers apart and lets permission
/// patterns target one server.
pub fn build_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp_{}_{}", sanitize(server_name), sanitize(tool_name))
}

/// One `_` per UTF-16 unit, as the C# loop over `char`s writes it.
fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            for _ in 0..c.len_utf16() {
                out.push('_');
            }
        }
    }
    out
}

/// MCP servers run arbitrary code: Dangerous, unless the tool is annotated `readOnlyHint`.
pub fn get_risk_level(annotations: Option<&ToolAnnotations>) -> ToolRiskLevel {
    if annotations.and_then(|a| a.read_only_hint) == Some(true) {
        ToolRiskLevel::ReadOnly
    } else {
        ToolRiskLevel::Dangerous
    }
}

/// The tool-call arguments as the request's `arguments` object; anything but an object means
/// "no arguments".
pub fn to_argument_map(args: &Value) -> Option<Map<String, Value>> {
    args.as_object().cloned()
}

/// `ToToolResult`: `isError` becomes an ExecutionFailed error; text blocks are joined with
/// `\n` into the result text; image blocks become image parts; other blocks are named in the
/// text as unsupported.
///
/// Image data is base64-decoded here. The C# mapper copies `ImageContentBlock.Data`, which in
/// the 2.x SDK holds the base64 text's bytes, not the image (see the port's decisions log).
pub fn to_tool_result(result: &CallToolResult) -> ToolResult {
    let mut text_parts: Vec<String> = Vec::new();
    let mut parts: Vec<ToolResultPart> = Vec::new();
    let mut has_non_text = false;

    for block in &result.content {
        match block {
            ContentBlock::Text(t) => {
                text_parts.push(t.text.clone());
                parts.push(ToolResultPart::Text(t.text.clone()));
            }
            ContentBlock::Image(img) => {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(img.data.trim())
                    .unwrap_or_else(|_| img.data.as_bytes().to_vec());
                parts.push(ToolResultPart::Image {
                    data,
                    mime_type: img.mime_type.clone(),
                });
                has_non_text = true;
            }
            other => text_parts.push(format!(
                "[unsupported MCP content: {}]",
                content_type(other)
            )),
        }
    }

    let text = text_parts.join("\n").trim_end().to_string();

    if result.is_error == Some(true) {
        return ToolResult::error_with(
            if text.is_empty() {
                "MCP tool reported an error.".to_string()
            } else {
                text
            },
            ToolErrorCode::ExecutionFailed,
        );
    }

    if has_non_text {
        let image_count = parts
            .iter()
            .filter(|p| matches!(p, ToolResultPart::Image { .. }))
            .count();
        let text = if text.is_empty() {
            format!(
                "({image_count} image{} returned)",
                if image_count == 1 { "" } else { "s" }
            )
        } else {
            text
        };
        let mut r = ToolResult::success(text);
        r.parts = Some(parts);
        return r;
    }

    ToolResult::success(text)
}

/// The block's protocol `type` (`ContentBlock.Type` in C#).
fn content_type(block: &ContentBlock) -> String {
    serde_json::to_value(block)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
