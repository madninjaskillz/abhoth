//! Ported from `McpToolMapperTests` and `McpProxyToolTests`.

use base64::Engine as _;
use orca_abstractions::{Tool, ToolErrorCode, ToolResultPart, ToolRiskLevel};
use orca_tools_mcp::McpProxyTool;
use orca_tools_mcp::mapper::*;
use rmcp::model::{CallToolResult, ToolAnnotations};
use serde_json::{Value, json};

fn tool(v: Value) -> rmcp::model::Tool {
    let mut v = v;
    if v.get("inputSchema").is_none() {
        v["inputSchema"] = json!({"type": "object"});
    }
    serde_json::from_value(v).unwrap()
}

fn result(v: Value) -> CallToolResult {
    serde_json::from_value(v).unwrap()
}

fn annotations(v: Value) -> ToolAnnotations {
    serde_json::from_value(v).unwrap()
}

// ----- Name prefixing / sanitization -----

#[test]
fn build_tool_name_prefixes_and_sanitizes() {
    for (server, tool, expected) in [
        ("github", "create_issue", "mcp_github_create_issue"),
        ("GitHub", "Create-Issue", "mcp_github_create_issue"),
        ("my.server", "tool/name", "mcp_my_server_tool_name"),
        ("srv 1", "a b", "mcp_srv_1_a_b"),
        ("files", "read", "mcp_files_read"),
    ] {
        assert_eq!(build_tool_name(server, tool), expected);
    }
}

#[test]
fn build_tool_name_same_tool_on_different_servers_does_not_collide() {
    assert_ne!(
        build_tool_name("server_a", "search"),
        build_tool_name("server_b", "search")
    );
}

// ----- Risk level -----

#[test]
fn get_risk_level_no_annotations_is_dangerous() {
    assert_eq!(get_risk_level(None), ToolRiskLevel::Dangerous);
}

#[test]
fn get_risk_level_read_only_hint_true_is_read_only() {
    let a = annotations(json!({"readOnlyHint": true}));
    assert_eq!(get_risk_level(Some(&a)), ToolRiskLevel::ReadOnly);
}

#[test]
fn get_risk_level_read_only_hint_false_or_unset_is_dangerous() {
    for a in [json!({"readOnlyHint": false}), json!({})] {
        let a = annotations(a);
        assert_eq!(get_risk_level(Some(&a)), ToolRiskLevel::Dangerous);
    }
}

// ----- Argument conversion -----

#[test]
fn to_argument_dictionary_object_returns_all_properties() {
    let dict = to_argument_map(&json!({"path": "/tmp/x", "count": 3})).unwrap();
    assert_eq!(dict.len(), 2);
    assert_eq!(dict["path"], "/tmp/x");
    assert_eq!(dict["count"], 3);
}

#[test]
fn to_argument_dictionary_non_object_returns_null() {
    assert!(to_argument_map(&json!("just a string")).is_none());
}

#[test]
fn to_argument_dictionary_undefined_element_returns_null() {
    assert!(to_argument_map(&Value::Null).is_none());
}

// ----- Result mapping -----

#[test]
fn to_tool_result_text_content_is_success_with_joined_text() {
    let mapped = to_tool_result(&result(json!({"content": [
        {"type": "text", "text": "line one"}, {"type": "text", "text": "line two"}]})));
    assert!(!mapped.is_error);
    assert_eq!(mapped.error_code, ToolErrorCode::None);
    assert_eq!(mapped.content, "line one\nline two");
    assert!(mapped.parts.is_none());
}

#[test]
fn to_tool_result_is_error_maps_to_execution_failed() {
    let mapped = to_tool_result(&result(json!({"isError": true,
        "content": [{"type": "text", "text": "something broke"}]})));
    assert!(mapped.is_error);
    assert_eq!(mapped.error_code, ToolErrorCode::ExecutionFailed);
    assert_eq!(mapped.content, "something broke");
}

#[test]
fn to_tool_result_is_error_without_content_has_fallback_message() {
    let mapped = to_tool_result(&result(json!({"isError": true, "content": []})));
    assert!(mapped.is_error);
    assert_eq!(mapped.error_code, ToolErrorCode::ExecutionFailed);
    assert_eq!(mapped.content, "MCP tool reported an error.");
}

#[test]
fn to_tool_result_is_error_null_is_success() {
    let mapped = to_tool_result(&result(
        json!({"content": [{"type": "text", "text": "ok"}]}),
    ));
    assert!(!mapped.is_error);
}

#[test]
fn to_tool_result_image_content_maps_to_image_part() {
    // The wire carries base64; the part holds the decoded bytes (see mapper::to_tool_result).
    let pixels = vec![1u8, 2, 3, 4];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&pixels);
    let mapped = to_tool_result(&result(json!({"content": [
        {"type": "image", "data": b64, "mimeType": "image/png"}]})));
    assert!(!mapped.is_error);
    assert_eq!(mapped.content, "(1 image returned)");
    let parts = mapped.parts.unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(
        parts[0],
        ToolResultPart::Image {
            data: pixels,
            mime_type: "image/png".into()
        }
    );
}

#[test]
fn to_tool_result_mixed_text_and_image_keeps_order_in_parts() {
    let mapped = to_tool_result(&result(json!({"content": [
        {"type": "text", "text": "chart below"},
        {"type": "image", "data": "CQk=", "mimeType": "image/jpeg"}]})));
    assert_eq!(mapped.content, "chart below");
    let parts = mapped.parts.unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], ToolResultPart::Text("chart below".into()));
    assert!(
        matches!(&parts[1], ToolResultPart::Image { mime_type, data } if mime_type == "image/jpeg" && data == &vec![9u8, 9])
    );
}

#[test]
fn to_tool_result_empty_content_is_success_with_empty_text() {
    let mapped = to_tool_result(&result(json!({"content": []})));
    assert!(!mapped.is_error);
    assert_eq!(mapped.content, "");
    assert!(mapped.parts.is_none());
}

#[test]
fn to_tool_result_unsupported_block_is_named_in_the_text() {
    let mapped = to_tool_result(&result(json!({"content": [
        {"type": "text", "text": "see"},
        {"type": "resource", "resource": {"uri": "file:///x", "text": "x"}}]})));
    assert_eq!(mapped.content, "see\n[unsupported MCP content: resource]");
    assert!(mapped.parts.is_none());
}

// ----- McpProxyToolTests -----

#[test]
fn name_prefixed_with_server_and_tool() {
    let t = McpProxyTool::detached(
        "demo",
        tool(json!({"name": "read_file", "description": "Reads a file"})),
    );
    assert_eq!(t.name(), "mcp_demo_read_file");
}

#[test]
fn name_sanitizes_non_alphanumeric_and_lowercases() {
    let t = McpProxyTool::detached("My.Server", tool(json!({"name": "Create-Issue"})));
    assert_eq!(t.name(), "mcp_my_server_create_issue");
}

#[test]
fn description_includes_server_tag() {
    let t = McpProxyTool::detached(
        "demo",
        tool(json!({"name": "test_tool", "description": "Does something"})),
    );
    assert!(t.description().starts_with("[MCP:demo]"));
    assert!(t.description().contains("Does something"));
}

#[test]
fn risk_level_defaults_to_dangerous() {
    let t = McpProxyTool::detached("demo", tool(json!({"name": "tool"})));
    assert_eq!(t.risk_level(), ToolRiskLevel::Dangerous);
}

#[test]
fn risk_level_read_only_hint_downgrades_to_read_only() {
    let t = McpProxyTool::detached(
        "demo",
        tool(json!({"name": "tool", "annotations": {"readOnlyHint": true}})),
    );
    assert_eq!(t.risk_level(), ToolRiskLevel::ReadOnly);
}

#[test]
fn parameter_schema_matches_tool_input_schema() {
    let t = McpProxyTool::detached(
        "demo",
        tool(
            json!({"name": "tool", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}}),
        ),
    );
    let schema = t.parameter_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"].get("path").is_some());
}

#[test]
fn implements_i_orca_tool() {
    let t: Box<dyn Tool> = Box::new(McpProxyTool::detached(
        "demo",
        tool(json!({"name": "tool"})),
    ));
    assert_eq!(t.name(), "mcp_demo_tool");
}
