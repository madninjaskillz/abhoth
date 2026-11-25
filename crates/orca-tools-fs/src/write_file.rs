use std::sync::LazyLock;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{
    decode_text, get_bool_lenient, io_message_for, required_str, retry_io,
    unescape_literal_sequences, utf16_len,
};
use regex::Regex;
use serde_json::Value;

use crate::util::{display, schema};

pub struct WriteFileTool;

static ROLE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^</?(assistant|user|system|tool)>$").unwrap());

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it and parent directories if needed. By default overwrites existing content. Set append=true to append instead."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES
            | ToolCapabilities::MODIFIES_FILE_CONTENT
            | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{270f}\u{fe0f}",
            short_description: "Write file",
            summary_arg_keys: &["path"],
            aliases: &["touch"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The path to the file to write"
            },
            "content": {
                "type": "string",
                "description": "The content to write to the file"
            },
            "append": {
                "type": "boolean",
                "description": "Append to the file instead of overwriting. Defaults to false."
            }
        },
        "required": ["path", "content"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let (path, content) = match (required_str(args, "path"), required_str(args, "content")) {
            (Ok(p), Ok(c)) => (p, c),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(e),
        };
        let append = get_bool_lenient(args.get("append"), false);
        // Local models sometimes double-escape newlines (a literal \n instead of a line break).
        let content = unescape_literal_sequences(content);
        let full = ctx.full_path(path);
        let shown = display(&full);
        let file_name = full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if content.trim().is_empty() {
            return ToolResult::error(format!(
                "Error: The content for '{file_name}' is empty. Please include the FULL file content in the 'content' argument and try again."
            ));
        }
        let trimmed = content.trim();
        if utf16_len(trimmed) < 50 && ROLE_TAG.is_match(trimmed) {
            return ToolResult::error(format!(
                "Error: The content for '{file_name}' appears to be a role tag rather than file content. Please include the FULL file content in the 'content' argument and try again."
            ));
        }

        if let Some(dir) = full.parent()
            && let Err(e) = tokio::fs::create_dir_all(dir).await
        {
            let dir = display(dir);
            return ToolResult::error(format!("Error writing file: {}", io_message_for(&e, &dir)));
        }
        let len = utf16_len(&content);
        if append {
            let r = retry_io(|| async {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&full)
                    .await?;
                f.write_all(content.as_bytes()).await?;
                f.flush().await
            })
            .await;
            return match r {
                Ok(()) => ToolResult::success(format!("Appended: {shown} ({len} chars added)")),
                Err(e) => ToolResult::error(format!(
                    "Error writing file: {}",
                    io_message_for(&e, &shown)
                )),
            };
        }

        let previous = match tokio::fs::read(&full).await {
            Ok(b) => Some(decode_text(&b)),
            Err(_) => None,
        };
        if let Err(e) = retry_io(|| tokio::fs::write(&full, content.as_bytes())).await {
            return ToolResult::error(format!(
                "Error writing file: {}",
                io_message_for(&e, &shown)
            ));
        }
        let mut result = match &previous {
            None => format!("Created: {shown} ({len} chars)"),
            Some(_) => format!("Overwritten: {shown} ({len} chars)"),
        };
        if let Some(prev) = previous.filter(|p| *p != content) {
            let old_lines = prev.split('\n').count() as i64;
            let new_lines = content.split('\n').count() as i64;
            let delta = new_lines - old_lines;
            let delta = if delta > 0 {
                format!("+{delta}")
            } else {
                delta.to_string()
            };
            result.push_str(&format!(
                "\nLines: {old_lines} \u{2192} {new_lines} ({delta})"
            ));
        }
        ToolResult::success(result)
    }
}
