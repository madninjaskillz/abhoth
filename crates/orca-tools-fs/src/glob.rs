use std::time::SystemTime;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{NEWLINE, append_line, get_i32_lenient, required_str};
use serde_json::Value;

use crate::util::{display, glob_matcher, rel_slash, schema, utc_date};

pub struct GlobTool;

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b}B"),
        b if b < 1024 * 1024 => format!("{:.0}KB", b as f64 / 1024.0),
        b => format!("{:.1}MB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn format_age(modified: SystemTime) -> String {
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs_f64();
    let (minutes, hours, days) = (age / 60.0, age / 3600.0, age / 86_400.0);
    if minutes < 1.0 {
        "just now".into()
    } else if hours < 1.0 {
        format!("{}m ago", minutes as i64)
    } else if days < 1.0 {
        format!("{}h ago", hours as i64)
    } else if days < 30.0 {
        format!("{}d ago", days as i64)
    } else {
        utc_date(modified)
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g. '**/*.cs', 'src/**/*.json'). Returns matching file paths sorted by modification time (newest first). Use exclude to skip patterns (e.g. '**/node_modules/**')."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f50d}",
            short_description: "Find files by pattern",
            summary_arg_keys: &["pattern"],
            aliases: &["find_files"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The glob pattern to match files against"
            },
            "path": {
                "type": "string",
                "description": "The directory to search in. Defaults to current directory."
            },
            "exclude": {
                "type": "string",
                "description": "Glob pattern to exclude from results (e.g. '**/node_modules/**', '**/bin/**')"
            },
            "max_results": {
                "type": "integer",
                "description": "Maximum number of results to return. Defaults to 500."
            }
        },
        "required": ["pattern"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let pattern = match required_str(args, "pattern") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let exclude = args
            .get("exclude")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let max_results = get_i32_lenient(args.get("max_results"), 500).max(0) as usize;
        let full = ctx.full_path(path);
        if !full.is_dir() {
            return ToolResult::error(format!("Directory not found: {}", display(&full)));
        }
        let include = match glob_matcher(pattern) {
            Ok(m) => m,
            Err(e) => return ToolResult::error(format!("Error in glob search: {e}")),
        };
        let exclude = match exclude.map(glob_matcher).transpose() {
            Ok(m) => m,
            Err(e) => return ToolResult::error(format!("Error in glob search: {e}")),
        };

        let mut matched: Vec<(String, SystemTime, u64)> = Vec::new();
        for entry in walkdir::WalkDir::new(&full)
            .min_depth(1)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = rel_slash(&full, entry.path());
            if !include.is_match(&rel) || exclude.as_ref().is_some_and(|x| x.is_match(&rel)) {
                continue;
            }
            let meta = entry.metadata().ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            matched.push((rel, modified, meta.map(|m| m.len()).unwrap_or(0)));
        }
        if matched.is_empty() {
            return ToolResult::success("No files matched the pattern.");
        }
        let total = matched.len();
        matched.sort_by_key(|m| std::cmp::Reverse(m.1));
        matched.truncate(max_results);

        let mut sb = String::new();
        append_line(
            &mut sb,
            &if total > max_results {
                format!(
                    "{} matches (showing first {max_results} of {total} total, sorted by modification time):",
                    matched.len()
                )
            } else {
                format!("{} matches:", matched.len())
            },
        );
        sb.push_str(NEWLINE);
        for (rel, modified, size) in &matched {
            append_line(
                &mut sb,
                &format!("{rel}  ({}, {})", format_size(*size), format_age(*modified)),
            );
        }
        ToolResult::success(sb)
    }
}
