use std::path::{Path, PathBuf};

use fancy_regex::RegexBuilder;
use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{
    NEWLINE, append_line, decode_text, get_bool_lenient, get_i32_lenient, required_str, split_lines,
};
use serde_json::Value;

use crate::util::{glob_matcher, name_key, rel_native, rel_slash, schema};

pub struct GrepTool;

/// Extensions searched when no glob is given.
const TEXT_EXTENSIONS: &[&str] = &[
    "cs",
    "js",
    "ts",
    "tsx",
    "jsx",
    "py",
    "json",
    "xml",
    "yaml",
    "yml",
    "md",
    "txt",
    "html",
    "css",
    "csproj",
    "sln",
    "props",
    "targets",
    "sh",
    "bash",
    "ps1",
    "toml",
    "cfg",
    "ini",
    "go",
    "rs",
    "java",
    "kt",
    "swift",
    "rb",
    "php",
    "sql",
    "dockerfile",
    "gitignore",
    "vue",
    "svelte",
    "proto",
    "graphql",
    "env",
    "gradle",
    "tf",
    "hcl",
];

/// `Path.GetExtension`: ".gitignore" has the extension "gitignore".
fn extension(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn files_to_search(root: &Path, glob: Option<&str>) -> Result<Vec<PathBuf>, String> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let matcher = glob
        .map(glob_matcher)
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .sort_by(|a, b| {
            name_key(&a.file_name().to_string_lossy())
                .cmp(&name_key(&b.file_name().to_string_lossy()))
        })
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| match &matcher {
            Some(m) => m.is_match(rel_slash(root, p)),
            None => TEXT_EXTENSIONS.contains(&extension(p).as_str()),
        })
        .collect();
    files.dedup();
    Ok(files)
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using a regex pattern. Supports glob file filters and context lines. Use output_mode to control output: 'content' (default) shows matching lines, 'files_only' shows file paths, 'count' shows match counts per file."
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
            short_description: "Search file contents",
            summary_arg_keys: &["pattern", "path"],
            aliases: &["search_text"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The regex pattern to search for"
            },
            "path": {
                "type": "string",
                "description": "The directory or file to search in. Defaults to current directory."
            },
            "glob": {
                "type": "string",
                "description": "Optional glob pattern to filter files (e.g. '*.cs', '**/*.json')"
            },
            "context": {
                "type": "integer",
                "description": "Number of context lines before and after each match. Defaults to 0."
            },
            "case_insensitive": {
                "type": "boolean",
                "description": "Whether to search case-insensitively. Defaults to false."
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_only", "count"],
                "description": "Output mode: 'content' shows matching lines (default), 'files_only' returns only file paths, 'count' returns files with match counts."
            },
            "max_results": {
                "type": "integer",
                "description": "Maximum number of matches to return. Defaults to 500."
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
        let glob = args
            .get("glob")
            .and_then(Value::as_str)
            .filter(|g| !g.is_empty());
        let context = get_i32_lenient(args.get("context"), 0).max(0) as usize;
        let case_insensitive = get_bool_lenient(args.get("case_insensitive"), false);
        let output_mode = args
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("content");
        let max_results = get_i32_lenient(args.get("max_results"), 500).max(0) as usize;
        let full = ctx.full_path(path);

        // fancy-regex (D112): lookaround and backreferences as in .NET; plain patterns still run
        // on the regex crate underneath.
        let regex = match RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Invalid regex: {e}")),
        };
        let files = match files_to_search(&full, glob) {
            Ok(f) => f,
            Err(e) => return ToolResult::error(format!("Invalid glob: {e}")),
        };

        let mut sb = String::new();
        let mut match_count = 0usize;
        let mut file_counts: Vec<(String, usize)> = Vec::new();
        for file in files {
            if match_count >= max_results {
                break;
            }
            let Ok(bytes) = tokio::fs::read(&file).await else {
                continue;
            };
            let text = decode_text(&bytes);
            let lines = split_lines(&text);
            let relative = rel_native(&full, &file);
            let mut hits = 0;
            for i in 0..lines.len() {
                if match_count >= max_results {
                    break;
                }
                // A pattern that backtracks too far on a line counts as no match there.
                if !regex.is_match(lines[i]).unwrap_or(false) {
                    continue;
                }
                match_count += 1;
                hits += 1;
                if output_mode == "content" {
                    let start = i.saturating_sub(context);
                    let end = (lines.len() - 1).min(i + context);
                    append_line(&mut sb, &format!("{relative}:{}:", i + 1));
                    for (j, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                        let prefix = if j == i { ">" } else { " " };
                        append_line(&mut sb, &format!("  {prefix} {}\t{line}", j + 1));
                    }
                    sb.push_str(NEWLINE);
                }
            }
            if hits > 0 {
                file_counts.push((relative, hits));
            }
        }
        if match_count == 0 {
            return ToolResult::success("No matches found.");
        }
        let truncated = match_count >= max_results;
        match output_mode {
            "files_only" | "count" => {
                let mut out = String::new();
                append_line(
                    &mut out,
                    &format!(
                        "{} files with {match_count} total matches{}:",
                        file_counts.len(),
                        if truncated { " (truncated)" } else { "" }
                    ),
                );
                out.push_str(NEWLINE);
                if output_mode == "files_only" {
                    file_counts.sort_by_key(|f| name_key(&f.0));
                    for (f, _) in &file_counts {
                        append_line(&mut out, f);
                    }
                } else {
                    // OrderByDescending is stable: ties keep their search order.
                    file_counts.sort_by_key(|f| std::cmp::Reverse(f.1));
                    for (f, n) in &file_counts {
                        append_line(&mut out, &format!("  {f}: {n}"));
                    }
                }
                ToolResult::success(out)
            }
            _ => {
                let header = if truncated {
                    format!("Found {max_results}+ matches (truncated):\n\n")
                } else {
                    format!("Found {match_count} match(es):\n\n")
                };
                ToolResult::success(header + &sb)
            }
        }
    }
}
