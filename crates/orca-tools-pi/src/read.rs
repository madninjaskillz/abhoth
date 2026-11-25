//! Pi's read tool (read.ts, `PiReadTool`): no line numbers, head-truncated to 2000 lines / 50KB.

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolResult, ToolResultPart, ToolRiskLevel,
};
use serde_json::Value;

use crate::truncate::{self, DEFAULT_MAX_BYTES, format_size, js_number};
use crate::{PiToolInfo, affected_path, fail, fail_with, fs_errors, get_number, get_string, paths};

pub struct PiReadTool;

const SCHEMA: &str = r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}}}"#;

impl PiToolInfo for PiReadTool {
    fn prompt_snippet(&self) -> String {
        "Read file contents".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use read to examine files instead of cat or sed.".into()]
    }
}

#[async_trait::async_trait]
impl Tool for PiReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn parameter_schema(&self) -> Value {
        crate::schema(SCHEMA)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT
            | ToolCapabilities::READS_FILE_CONTENT
            | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), SCHEMA, arguments_json))
    }

    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let raw_path = get_string(args, "path").unwrap_or("");
        let offset = get_number(args, "offset");
        let limit = get_number(args, "limit");
        let path = paths::resolve_to_cwd(raw_path, &ctx.cwd);
        let shown = path.to_string_lossy().into_owned();

        if !path.exists() {
            return fail_with(
                fs_errors::format("ENOENT", "access", Some(&shown)),
                ToolErrorCode::NotFound,
            );
        }
        if path.is_dir() {
            return fail(fs_errors::format("EISDIR", "read", None));
        }
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => return fail(fs_errors::format_io(&e, "read", None)),
        };
        if let Some(mime) = detect_image_mime(&data) {
            return ToolResult {
                parts: Some(vec![ToolResultPart::Image {
                    data,
                    mime_type: mime.into(),
                }]),
                ..ToolResult::success(format!("Read image file [{mime}]"))
            };
        }
        read_text(&String::from_utf8_lossy(&data), raw_path, offset, limit)
    }
}

/// The text branch of Pi's read.ts execute (`ReadText`).
pub fn read_text(
    text: &str,
    raw_path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
) -> ToolResult {
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total = all_lines.len() as i64;
    let start_line: i64 = match offset {
        Some(o) if o != 0.0 => (o - 1.0).max(0.0) as i64,
        _ => 0,
    };
    let start_display = start_line + 1;
    if start_line >= total {
        return fail(format!(
            "Offset {} is beyond end of file ({total} lines total)",
            js_number(offset.unwrap_or(0.0))
        ));
    }

    let selected: String;
    let mut user_limited_lines: i64 = 0;
    if let Some(lim) = limit {
        let end_line = ((start_line as f64 + lim).min(total as f64)) as i64;
        let slice_end = if end_line < 0 {
            (total + end_line).max(0)
        } else {
            end_line
        };
        let to = slice_end.max(start_line) as usize;
        selected = all_lines[start_line as usize..to].join("\n");
        user_limited_lines = end_line - start_line;
    } else {
        selected = all_lines[start_line as usize..].join("\n");
    }

    let tr = truncate::head_default(&selected);
    let output = if tr.first_line_exceeds_limit {
        let first_line_size = format_size(all_lines[start_line as usize].len() as u64);
        format!(
            "[Line {start_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {raw_path} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES as u64)
        )
    } else if tr.truncated {
        let end_display = start_display + tr.output_lines as i64 - 1;
        let next = end_display + 1;
        if tr.truncated_by == Some("lines") {
            format!(
                "{}\n\n[Showing lines {start_display}-{end_display} of {total}. Use offset={next} to continue.]",
                tr.content
            )
        } else {
            format!(
                "{}\n\n[Showing lines {start_display}-{end_display} of {total} ({} limit). Use offset={next} to continue.]",
                tr.content,
                format_size(DEFAULT_MAX_BYTES as u64)
            )
        }
    } else if limit.is_some() && start_line + user_limited_lines < total {
        let remaining = total - (start_line + user_limited_lines);
        let next = start_line + user_limited_lines + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]",
            tr.content
        )
    } else {
        tr.content
    };
    ToolResult::success(output)
}

fn detect_image_mime(d: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| d.starts_with(sig);
    if starts(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif");
    }
    if d.len() >= 12 && starts(&[0x52, 0x49, 0x46, 0x46]) && &d[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // "BM" + a known DIB header size, so a text file starting with "BM" is not an image.
    if starts(&[0x42, 0x4D])
        && d.len() > 26
        && matches!(d[14], 12 | 40 | 52 | 56 | 108 | 124)
        && d[15] == 0
        && d[16] == 0
        && d[17] == 0
    {
        return Some("image/bmp");
    }
    None
}
