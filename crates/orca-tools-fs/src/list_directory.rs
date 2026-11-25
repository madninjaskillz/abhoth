use std::path::Path;

use globset::GlobMatcher;
use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{append_line, decode_text, get_bool_lenient, split_lines};
use serde_json::Value;

use crate::util::{display, glob_matcher, name_key, rel_native, schema};

pub struct ListDirectoryTool;

const MAX_ENTRIES: usize = 1000;

fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", b / 1024.0),
        n if n < 1024 * 1024 * 1024 => format!("{:.1} MB", b / (1024.0 * 1024.0)),
        _ => format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0)),
    }
}

/// Built-in ignores plus the nearest `.gitignore` (up to 10 levels up), each pattern matched
/// against every path segment, as the C# tool does.
fn load_ignore_patterns(directory: &Path) -> Vec<GlobMatcher> {
    let mut patterns: Vec<String> = [
        ".git",
        "node_modules",
        "bin",
        "obj",
        ".vs",
        "__pycache__",
        ".mypy_cache",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut dir = Some(directory);
    for _ in 0..10 {
        let Some(d) = dir else { break };
        let gitignore = d.join(".gitignore");
        if gitignore.is_file() {
            if let Ok(bytes) = std::fs::read(&gitignore) {
                for line in split_lines(&decode_text(&bytes)) {
                    let t = line.trim();
                    if !t.is_empty() && !t.starts_with('#') {
                        patterns.push(t.trim_start_matches('/').to_string());
                    }
                }
            }
            break;
        }
        dir = d.parent();
    }
    patterns
        .iter()
        .filter_map(|p| {
            glob_matcher(&if p.ends_with('/') {
                format!("{p}**")
            } else {
                p.clone()
            })
            .ok()
        })
        .collect()
}

fn is_ignored(relative: &str, ignore: &[GlobMatcher]) -> bool {
    relative
        .replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty())
        .any(|seg| ignore.iter().any(|m| m.is_match(seg)))
}

#[async_trait::async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn description(&self) -> &str {
        "List files and directories in a given path. Returns names with type indicators (/ for directories). Use recursive to see nested structure, show_hidden to include dotfiles, and show_size to see file sizes."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    // The C# tool declares no capabilities (so it is not directory-restricted either).
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::NONE
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4c2}",
            short_description: "List directory contents",
            summary_arg_keys: &["path"],
            aliases: &["ls"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The directory path to list. Defaults to current directory."
            },
            "recursive": {
                "type": "boolean",
                "description": "List entries recursively (capped at 1000 entries). Defaults to false."
            },
            "show_hidden": {
                "type": "boolean",
                "description": "Include hidden files/directories (starting with '.'). Defaults to false."
            },
            "show_size": {
                "type": "boolean",
                "description": "Show file sizes. Defaults to false."
            }
        }
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let recursive = get_bool_lenient(args.get("recursive"), false);
        let show_hidden = get_bool_lenient(args.get("show_hidden"), false);
        let show_size = get_bool_lenient(args.get("show_size"), false);
        let full = ctx.full_path(path);
        if !full.is_dir() {
            return ToolResult::error(format!("Directory not found: {}", display(&full)));
        }
        let ignore = load_ignore_patterns(&full);

        // (is_file, sort name, display name, size)
        let mut entries: Vec<(bool, String, Option<u64>)> = Vec::new();
        let walker = walkdir::WalkDir::new(&full)
            .min_depth(1)
            .max_depth(if recursive { usize::MAX } else { 1 });
        for entry in walker.into_iter().filter_map(Result::ok) {
            let is_dir = entry.file_type().is_dir();
            let name = if recursive {
                rel_native(&full, entry.path())
            } else {
                entry.file_name().to_string_lossy().into_owned()
            };
            let size = if is_dir {
                None
            } else {
                entry.metadata().ok().map(|m| m.len())
            };
            entries.push((!is_dir, name, size));
        }
        entries.sort_by_key(|e| (e.0, name_key(&e.1)));

        let mut sb = String::new();
        let mut count = 0;
        for (is_file, name, size) in &entries {
            let file_name = Path::new(name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !show_hidden && file_name.starts_with('.') {
                continue;
            }
            if is_ignored(name, &ignore) {
                continue;
            }
            if !is_file {
                append_line(&mut sb, &format!("{name}/"));
            } else if show_size {
                append_line(
                    &mut sb,
                    &format!("{name}  ({})", format_size(size.unwrap_or(0))),
                );
            } else {
                append_line(&mut sb, name);
            }
            count += 1;
            if count >= MAX_ENTRIES {
                append_line(
                    &mut sb,
                    &format!("... (truncated at {MAX_ENTRIES} entries)"),
                );
                break;
            }
        }
        if count == 0 {
            return ToolResult::success("(empty directory)");
        }
        ToolResult::success(format!("{count} entries\n{sb}"))
    }
}
