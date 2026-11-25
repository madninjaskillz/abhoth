//! `multi_edit`: several exact replacements across files, validated first and written
//! all-or-nothing.

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{append_line, decode_text, get_bool_lenient, io_message, retry_io};
use serde_json::Value;

use crate::util::{display, schema};

pub struct MultiEditTool;

struct EditEntry {
    path: String,
    old: String,
    new: String,
    replace_all: bool,
}

fn apply_edit(content: &str, edit: &EditEntry) -> String {
    if edit.replace_all {
        content.replace(&edit.old, &edit.new)
    } else {
        content.replacen(&edit.old, &edit.new, 1)
    }
}

fn count_occurrences(text: &str, value: &str) -> usize {
    text.matches(value).count()
}

/// `text[..maxLen] + "..."` with the length in UTF-16 units.
fn truncate(text: &str, max_len: usize) -> String {
    if orca_tools_common::utf16_len(text) <= max_len {
        return text.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > max_len {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out + "..."
}

#[async_trait::async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multi_edit"
    }

    fn description(&self) -> &str {
        "Apply multiple edits across one or more files atomically. If any edit fails, all changes are rolled back. Each edit specifies a file path, old_string to find, and new_string to replace it with."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES
            | ToolCapabilities::MODIFIES_FILE_CONTENT
            | ToolCapabilities::DIRECTORY_RESTRICTED
            | ToolCapabilities::HAS_DIFF_OUTPUT
    }

    fn affected_file_paths(&self, args: &Value) -> Vec<String> {
        args.get("edits")
            .and_then(Value::as_array)
            .map(|edits| {
                edits
                    .iter()
                    .filter_map(|e| e.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4dd}",
            short_description: "Edit multiple files",
            summary_arg_keys: &[],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "description": "Array of edits to apply. All edits are validated first, then applied atomically.",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The path to the file to edit"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The exact text to find and replace"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The text to replace it with"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace all occurrences instead of requiring uniqueness. Defaults to false."
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        "required": ["edits"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(items) = args.get("edits").and_then(Value::as_array) else {
            return ToolResult::error("'edits' must be a non-empty array.");
        };
        let mut edits = Vec::new();
        for item in items {
            let get = |k: &str| item.get(k).and_then(Value::as_str);
            let (Some(path), Some(old), Some(new)) =
                (get("path"), get("old_string"), get("new_string"))
            else {
                return ToolResult::error(
                    "Each edit must have 'path', 'old_string', and 'new_string'.",
                );
            };
            if path.is_empty() {
                return ToolResult::error(
                    "Each edit must have 'path', 'old_string', and 'new_string'.",
                );
            }
            edits.push(EditEntry {
                path: display(&ctx.full_path(path)),
                old: old.to_string(),
                new: new.to_string(),
                replace_all: get_bool_lenient(item.get("replace_all"), false),
            });
        }
        if edits.is_empty() {
            return ToolResult::error("'edits' array must not be empty.");
        }

        // Phase 1: validate. Each edit sees the content left by the earlier edits to its file.
        // (path, original content) in first-seen order, like the C# Dictionary.
        let mut snapshots: Vec<(String, String)> = Vec::new();
        let mut current: Vec<(String, String)> = Vec::new();
        for edit in &edits {
            if !std::path::Path::new(&edit.path).is_file() {
                return ToolResult::error(format!("File not found: {}", edit.path));
            }
            if !snapshots.iter().any(|(p, _)| *p == edit.path) {
                let text = match tokio::fs::read(&edit.path).await {
                    Ok(b) => decode_text(&b),
                    Err(e) => return ToolResult::error(io_message(&e)),
                };
                snapshots.push((edit.path.clone(), text.clone()));
                current.push((edit.path.clone(), text));
            }
            if edit.old.is_empty() {
                // C# loops forever (counting) or throws (Replace) on an empty old_string.
                return ToolResult::error(
                    "String cannot be of zero length. (Parameter 'oldValue')",
                );
            }
            let slot = current
                .iter_mut()
                .find(|(p, _)| *p == edit.path)
                .expect("snapshot taken above");
            if !slot.1.contains(&edit.old) {
                return ToolResult::error(format!(
                    "old_string not found in {}: \"{}\"",
                    edit.path,
                    truncate(&edit.old, 80)
                ));
            }
            if !edit.replace_all {
                let count = count_occurrences(&slot.1, &edit.old);
                if count > 1 {
                    return ToolResult::error(format!(
                        "old_string appears {count} times in {}. Provide more context or set replace_all to true.",
                        edit.path
                    ));
                }
            }
            slot.1 = apply_edit(&slot.1, edit);
        }

        // Phase 2 and 3: write every file; on a failure restore the ones already written.
        let mut written: Vec<&str> = Vec::new();
        for (path, content) in &current {
            if let Err(e) = retry_io(|| tokio::fs::write(path, content.as_bytes())).await {
                for p in &written {
                    if let Some((_, original)) = snapshots.iter().find(|(s, _)| s == p) {
                        let _ = tokio::fs::write(p, original.as_bytes()).await;
                    }
                }
                return ToolResult::error(format!(
                    "Failed to write {} file(s), rolled back all changes: {}",
                    written.len(),
                    io_message(&e)
                ));
            }
            written.push(path);
        }

        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!(
                "Applied {} edit(s) across {} file(s):",
                edits.len(),
                current.len()
            ),
        );
        for edit in &edits {
            // replace_all counts in the ORIGINAL file (as C# does).
            let count = if edit.replace_all {
                let original = snapshots
                    .iter()
                    .find(|(p, _)| *p == edit.path)
                    .map_or("", |(_, c)| c.as_str());
                count_occurrences(original, &edit.old)
            } else {
                1
            };
            append_line(
                &mut sb,
                &format!("  - {}: {count} replacement(s)", edit.path),
            );
        }
        ToolResult::success(sb.trim_end().to_string())
    }
}
