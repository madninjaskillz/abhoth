//! Prompt text and tag helpers used by the agent loop.
//!
//! Phase 1 uses a compact built-in system prompt. The C# prompt system (`PromptManager`,
//! `SystemPromptBuilder`, `prompts/default.md` with its version stamp, project instructions,
//! model notes) is ported in phase 2; see `PORTING.md`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::registry::ToolRegistry;

/// `PromptConstants.SummaryRequestMessage`: asked after a turn that ran tools ends without a
/// substantive wrap-up.
pub const SUMMARY_REQUEST_MESSAGE: &str = "All tool calls are complete. Provide a final summary of what you did.
Use markdown formatting (headers, bold, bullets, inline code) directly in your response \u{2014} do NOT wrap it in a code fence.
Structure:
- **Summary**: 1\u{2013}3 sentences describing what you did overall.
- **Changes made**: A bulleted list of files created, modified, or deleted, with brief descriptions.
- **Key findings**: Any notable discoveries, warnings, or issues encountered (omit if none).
- **Suggested next steps**: Actionable follow-ups the user might want to do (omit if none).
Keep it concise. Use `code` formatting for file paths and commands. Do NOT call any tools.

IMPORTANT: At the very end of your response, on its own line, add a <next-prompt> tag containing
a short follow-up prompt the user might type next (under 80 chars).
Example: <next-prompt>run the tests</next-prompt>
If there is no natural follow-up, omit the tag.";

/// Asked when a summary was cut off at the output limit.
pub const SUMMARY_CONTINUE_MESSAGE: &str = "Your response was cut off. Continue exactly where you stopped \u{2014} do NOT repeat anything you already wrote.";

/// `CliConstants.MaxSummaryContinuations`.
pub const MAX_SUMMARY_CONTINUATIONS: u32 = 3;

const BASE_PROMPT: &str = "You are OpenOrca, an autonomous AI coding agent running in a CLI terminal.
You DO NOT just explain how to do things \u{2014} you DO them directly using your tools.
You are resourceful, persistent, and solve problems independently.

RULES:
- Use the tools to read, create and change files and to run commands; never claim you did something you did not do.
- Read a file before you edit it, and keep edits minimal and exact.
- After changing code, run the relevant build or tests when there are any.
- When the task is done, reply with a short summary of what you did.

ENVIRONMENT:
- Working directory: {{CWD}}
- Platform: {{PLATFORM}}

AVAILABLE TOOLS:
{{TOOL_LIST}}

You have up to {{MAX_ITERATIONS}} iterations per task.";

static EXCESS_BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// The system prompt for this session.
pub fn system_prompt(cwd: &str, registry: &ToolRegistry, max_iterations: i32) -> String {
    let tool_list: Vec<String> = registry
        .all()
        .iter()
        .map(|t| {
            format!(
                "- {}",
                format_signature(t.name(), Some(t.description()), &t.parameter_schema())
            )
        })
        .collect();
    let text = BASE_PROMPT
        .replace("{{CWD}}", cwd)
        .replace("{{PLATFORM}}", &platform())
        .replace("{{TOOL_LIST}}", &tool_list.join("\n"))
        .replace("{{MAX_ITERATIONS}}", &max_iterations.max(1).to_string());
    EXCESS_BLANK_LINES
        .replace_all(&text, "\n\n")
        .trim()
        .to_string()
}

fn platform() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

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
