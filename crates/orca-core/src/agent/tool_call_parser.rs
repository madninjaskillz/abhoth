//! Placeholder for the `ToolCallParser` port (replaced by the full port on merge).

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

pub fn parse_tool_calls_from_text(text: &str) -> Vec<ParsedToolCall> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let Some(end) = after.find("</tool_call>") else {
            break;
        };
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(after[..end].trim())
            && let Some(name) = o.get("name").and_then(Value::as_str)
        {
            out.push(ParsedToolCall {
                id: format!("parsed_{:032x}", out.len()),
                name: name.to_lowercase(),
                arguments: o
                    .get("arguments")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        rest = &after[end..];
    }
    out
}

pub fn should_nudge_for_tool_calls(_text: &str) -> bool {
    false
}
