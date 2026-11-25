//! Pi's write tool (write.ts, `PiWriteTool`): creates parent directories, overwrites whole files.

use orca_abstractions::{Tool, ToolCapabilities, ToolContext, ToolResult, ToolRiskLevel};
use serde_json::Value;

use crate::{PiToolInfo, affected_path, fail, fs_errors, get_string, paths, with_file_lock};

pub struct PiWriteTool;

const SCHEMA: &str = r#"{"type":"object","required":["path","content"],"properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}}}"#;

/// Existing files up to this size are compared with the new content for [`rewrite_hint`].
const MAX_COMPARE_BYTES: u64 = 512 * 1024;

/// Rewrites of files at least this long whose changes are small get the hint.
pub const HINT_MIN_LINES: usize = 30;

impl PiToolInfo for PiWriteTool {
    fn prompt_snippet(&self) -> String {
        "Create or overwrite files".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use write only for new files or complete rewrites.".into()]
    }
}

/// .NET `string.ReplaceLineEndings("\n")`: CRLF, CR, LF, NEL, LS, PS and FF become "\n".
pub(crate) fn replace_line_endings_lf(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{0C}' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

/// When a write rewrote an existing file but changed only a small part of it, a short note
/// saying so (an edit would have cost a fraction of the output tokens). "" otherwise.
pub fn rewrite_hint(previous: Option<&str>, content: &str) -> String {
    let Some(previous) = previous else {
        return String::new();
    };
    let old = replace_line_endings_lf(previous);
    let new = replace_line_endings_lf(content);
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    if old_lines.len() < HINT_MIN_LINES || new_lines.len() < HINT_MIN_LINES {
        return String::new();
    }
    let max = old_lines.len().min(new_lines.len());
    let mut prefix = 0;
    while prefix < max && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < max - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let changed = new_lines.len() - prefix - suffix;
    if changed * 2 > new_lines.len() {
        return String::new();
    }
    format!(
        " (rewrote {} lines to change {changed}; for a change like this use edit, which outputs only the changed text)",
        new_lines.len()
    )
}

/// `File.ReadAllText`: UTF-8 with the BOM dropped.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.strip_prefix('\u{FEFF}').unwrap_or(&s).to_string()
}

#[async_trait::async_trait]
impl Tool for PiWriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn parameter_schema(&self) -> Value {
        crate::schema(SCHEMA)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT
            | ToolCapabilities::MODIFIES_FILES
            | ToolCapabilities::MODIFIES_FILE_CONTENT
            | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), SCHEMA, arguments_json))
    }

    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = get_string(args, "path").unwrap_or("");
        let content = get_string(args, "content").unwrap_or("");
        let abs = paths::resolve_to_cwd(path, &ctx.cwd);
        with_file_lock(&abs, async {
            if let Some(dir) = abs.parent()
                && let Err(e) = tokio::fs::create_dir_all(dir).await
            {
                return fail(fs_errors::format_io(
                    &e,
                    "mkdir",
                    Some(&dir.to_string_lossy()),
                ));
            }
            let previous = match tokio::fs::metadata(&abs).await {
                Ok(m) if m.is_file() && m.len() <= MAX_COMPARE_BYTES => {
                    tokio::fs::read(&abs).await.ok().map(|b| decode_text(&b))
                }
                _ => None,
            };
            if let Err(e) = tokio::fs::write(&abs, content.as_bytes()).await {
                return fail(fs_errors::format_io(
                    &e,
                    "open",
                    Some(&abs.to_string_lossy()),
                ));
            }
            ToolResult::success(format!(
                "Successfully wrote to {path}{}",
                rewrite_hint(previous.as_deref(), content)
            ))
        })
        .await
    }
}
