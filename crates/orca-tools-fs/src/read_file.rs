use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{
    NEWLINE, append_line, decode_text, get_i32_lenient, required_str, split_lines,
};
use serde_json::Value;

use crate::util::{display, schema};

pub struct ReadFileTool;

const DEFAULT_LINE_LIMIT: i32 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTE_SIZE: u64 = 50 * 1024;

const BINARY_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "o", "obj", "a", "lib", "png", "jpg", "jpeg", "gif", "bmp", "ico",
    "webp", "tiff", "svg", "zip", "gz", "tar", "rar", "7z", "bz2", "xz", "zst", "pdf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "wasm", "class", "pyc", "pdb", "mp3", "mp4", "wav",
    "avi", "mkv", "mov", "flac", "ogg", "ttf", "otf", "woff", "woff2", "eot", "bin", "dat", "iso",
    "img",
];

fn binary_error(path: &str) -> ToolResult {
    ToolResult::error(format!(
        "Binary file detected: {path}. Use bash to inspect binary files (e.g., 'xxd' or 'file')."
    ))
}

fn format_file_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b}B"),
        b if b < 1024 * 1024 => format!("{:.1}KB", b as f64 / 1024.0),
        b => format!("{:.1}MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns content with line numbers. Use offset/limit to read specific sections of large files. Detects binary files and returns an error instead of garbled output."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::DIRECTORY_RESTRICTED | ToolCapabilities::READS_FILE_CONTENT
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4d6}",
            short_description: "Read file contents",
            summary_arg_keys: &["path"],
            aliases: &["cat"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The absolute or relative path to the file to read"
            },
            "offset": {
                "type": "integer",
                "description": "Line number to start reading from (1-based). Defaults to 1."
            },
            "limit": {
                "type": "integer",
                "description": "Maximum number of lines to read. Defaults to 2000."
            }
        },
        "required": ["path"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = match required_str(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let offset = get_i32_lenient(args.get("offset"), 1);
        let limit = get_i32_lenient(args.get("limit"), DEFAULT_LINE_LIMIT);
        let full = ctx.full_path(path);
        let shown = display(&full);

        if !full.is_file() {
            return ToolResult::error(format!("File not found: {shown}"));
        }
        let ext = full
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if BINARY_EXTENSIONS.contains(&ext.as_str()) {
            return binary_error(&shown);
        }
        let bytes = match tokio::fs::read(&full).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("Error reading file: {e}")),
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return binary_error(&shown);
        }

        let text = decode_text(&bytes);
        let all = split_lines(&text);
        let file_len = bytes.len() as u64;
        let mut truncated_by_bytes = false;
        let lines: Vec<&str> = if file_len > MAX_BYTE_SIZE {
            // The C# reader stops at the first line read once ~50KB were consumed.
            let mut kept = Vec::new();
            let mut consumed: u64 = 0;
            for line in &all {
                if consumed >= MAX_BYTE_SIZE {
                    truncated_by_bytes = true;
                    break;
                }
                consumed += (line.len() + NEWLINE.len()) as u64;
                kept.push(*line);
            }
            kept
        } else {
            all
        };

        let total = lines.len();
        let start = (offset.max(1) - 1) as usize;
        let start = start.min(usize::MAX);
        let end = total.min(start.saturating_add(limit.max(0) as usize));
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!(
                "File: {shown} ({total}{} lines, showing {}-{end})",
                if truncated_by_bytes { "+" } else { "" },
                start + 1
            ),
        );
        if truncated_by_bytes {
            append_line(
                &mut sb,
                &format!(
                    "[Truncated: file is {}. Showing first ~50KB. Use offset/limit to read other sections.]",
                    format_file_size(file_len)
                ),
            );
        }
        sb.push_str(NEWLINE);
        let width = end.to_string().len();
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            let line = if line.chars().count() > MAX_LINE_LENGTH {
                format!(
                    "{}...",
                    line.chars().take(MAX_LINE_LENGTH).collect::<String>()
                )
            } else {
                (*line).to_string()
            };
            append_line(&mut sb, &format!("{:>width$}\t{line}", i + 1));
        }
        ToolResult::success(sb)
    }
}
