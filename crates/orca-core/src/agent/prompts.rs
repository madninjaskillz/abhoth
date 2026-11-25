//! Prompt helpers used by the agent loop: the tool signature formatter
//! (`ToolSignatureFormatter`) and the reply tag helpers (`<think>`, `<next-prompt>`).
//!
//! The prompt texts live in [`super::prompt_constants`] (re-exported here so existing callers
//! keep compiling), the system prompt in [`super::system_prompt`] and the template files in
//! [`crate::config::prompt_manager`].

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

pub use super::prompt_constants::{
    MAX_SUMMARY_CONTINUATIONS, SUMMARY_CONTINUE_MESSAGE, SUMMARY_REQUEST_MESSAGE,
    VERIFICATION_MESSAGE, VERIFICATION_RETRY_MESSAGE, platform_lines, summary_request_message,
};

/// `ToolSignatureFormatter.Format`: `name(params) — first sentence of the description`.
pub fn format_signature(name: &str, description: Option<&str>, schema: &Value) -> String {
    let sig = format!("{name}({})", format_parameters(schema));
    match description.filter(|d| !d.trim().is_empty()) {
        None => sig,
        Some(d) => format!("{sig} \u{2014} {}", first_sentence(d)),
    }
}

/// `ToolSignatureFormatter.FormatParameters`: `path: string, offset?: number, ...`.
pub fn format_parameters(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut parts = Vec::new();
    for (name, prop) in props {
        let mut s = name.clone();
        if !required.contains(&name.as_str()) {
            s.push('?');
        }
        s.push_str(": ");
        s.push_str(&format_type(prop));
        if let Some(def) = prop.get("default") {
            match def {
                Value::String(d) => s.push_str(&format!(" = \"{d}\"")),
                Value::Number(_) | Value::Bool(_) => s.push_str(&format!(" = {def}")),
                _ => {}
            }
        }
        parts.push(s);
    }
    parts.join(", ")
}

fn format_type(prop: &Value) -> String {
    if !prop.is_object() {
        return "any".into();
    }
    if let Some(values) = prop.get("enum").and_then(Value::as_array) {
        let vals: Vec<String> = values
            .iter()
            .take(8)
            .map(|v| match v {
                Value::String(s) => format!("\"{s}\""),
                other => other.to_string(),
            })
            .collect();
        if !vals.is_empty() {
            return vals.join(" | ");
        }
    }
    match prop.get("type").and_then(Value::as_str) {
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("string") => "string".into(),
        Some("array") => match prop.get("items") {
            Some(items) => format!("{}[]", format_type(items)),
            None => "any[]".into(),
        },
        Some("object") => "object".into(),
        _ => "any".into(),
    }
}

fn first_sentence(description: &str) -> &str {
    match description.find(". ") {
        Some(idx) if idx > 0 => &description[..=idx],
        _ => description,
    }
}

static THINK_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<think>.*?</think>").unwrap());
static THINK_UNCLOSED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<think>.*$").unwrap());

/// `ToolCallDispatcher.ExtractFinalProse`: the text with `<think>` blocks removed, trimmed.
pub fn extract_final_prose(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let t = THINK_BLOCK.replace_all(text, "");
    let t = THINK_UNCLOSED.replace_all(&t, "");
    t.trim().to_string()
}

/// `IsSubstantiveFinalMessage`: a real wrap-up (at least 40 chars and 8 words), as opposed to
/// a bare "Done." that needs a summary request.
pub fn is_substantive_final_message(prose: &str) -> bool {
    prose.chars().count() >= 40 && prose.split_whitespace().count() >= 8
}

/// `StripNextPromptTag`: removes `<next-prompt>...</next-prompt>` (an unclosed tag cuts the rest).
pub fn strip_next_prompt_tag(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find("<next-prompt>") else {
        return text.to_string();
    };
    match lower[start..].find("</next-prompt>") {
        None => text[..start].trim_end().to_string(),
        Some(rel) => {
            let end = start + rel + "</next-prompt>".len();
            format!("{}{}", &text[..start], &text[end..])
                .trim()
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Ported from ToolSignatureFormatterTests.cs ---
    // Format_AllRegisteredTools_ProduceNonEmptySingleLineSignatures lives in orca-cli
    // (tests/tool_signatures.rs), which owns the tool registry.

    fn schema(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn format_required_and_optional_params_marks_optional_with_question_mark() {
        let s = schema(
            r#"{"type":"object","properties":{
                "path":{"type":"string"},
                "create_dirs":{"type":"boolean","default":true}
            },"required":["path"]}"#,
        );
        assert_eq!(
            format_signature(
                "write_file",
                Some("Write content to a file. More detail here."),
                &s
            ),
            "write_file(path: string, create_dirs?: boolean = true) \u{2014} Write content to a file."
        );
    }

    #[test]
    fn format_integer_params_render_as_number() {
        let s = schema(
            r#"{"type":"object","properties":{
                "line":{"type":"integer"},
                "ratio":{"type":"number"}
            },"required":["line"]}"#,
        );
        assert_eq!(format_parameters(&s), "line: number, ratio?: number");
    }

    #[test]
    fn format_enum_param_renders_literal_union() {
        let s = schema(
            r#"{"type":"object","properties":{
                "mode":{"type":"string","enum":["fast","full"]}
            },"required":["mode"]}"#,
        );
        assert_eq!(format_parameters(&s), "mode: \"fast\" | \"full\"");
    }

    #[test]
    fn format_array_param_renders_element_type() {
        let s = schema(
            r#"{"type":"object","properties":{
                "files":{"type":"array","items":{"type":"string"}}
            },"required":["files"]}"#,
        );
        assert_eq!(format_parameters(&s), "files: string[]");
    }

    #[test]
    fn format_empty_schema_produces_empty_params() {
        assert_eq!(format_parameters(&schema("{}")), "");
        assert_eq!(
            format_signature("noop", Some("Does nothing"), &schema("{}")),
            "noop() \u{2014} Does nothing"
        );
    }

    // --- Rust-only ---

    #[test]
    fn signature_lists_required_and_optional_params() {
        let schema = json!({"type":"object","properties":{
            "path":{"type":"string"},"offset":{"type":"integer"},"mode":{"type":"string","enum":["a","b"]},
            "tags":{"type":"array","items":{"type":"string"}},"n":{"type":"integer","default":5}},
            "required":["path"]});
        assert_eq!(
            format_signature("read_file", Some("Read a file. More text."), &schema),
            "read_file(path: string, offset?: number, mode?: \"a\" | \"b\", tags?: string[], n?: number = 5) \u{2014} Read a file."
        );
    }

    #[test]
    fn final_prose_and_substance() {
        assert_eq!(extract_final_prose("<think>x</think> Done."), "Done.");
        assert_eq!(extract_final_prose("Hi <THINK>never closed"), "Hi");
        assert!(!is_substantive_final_message("Done."));
        assert!(is_substantive_final_message(
            "I created hello.txt containing the greeting you asked for."
        ));
    }

    #[test]
    fn next_prompt_tag_is_stripped() {
        assert_eq!(
            strip_next_prompt_tag("Summary.\n<next-prompt>run tests</next-prompt>"),
            "Summary."
        );
        assert_eq!(
            strip_next_prompt_tag("Summary.\n<next-prompt>run te"),
            "Summary."
        );
        assert_eq!(strip_next_prompt_tag("plain"), "plain");
    }
}
