//! Pi's optional extras (`pi.extraTools`): ls (ls.ts), find (find.ts) and grep (grep.ts), with
//! Pi's schemas, output and notices (`PiSearchTools`). Divergences, as in C#: a built-in walker
//! replaces fd/ripgrep and only skips .git and node_modules instead of honouring .gitignore.

use std::path::Path;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolResult, ToolRiskLevel,
};
use regex::{Regex, RegexBuilder};
use serde_json::Value;

use crate::truncate::{
    self, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, NO_LINE_LIMIT, format_size, js_number,
};
use crate::{PiToolInfo, fail, fail_with, fs_errors, get_bool, get_number, get_string, paths};

fn search_root(args: &Value, ctx: &ToolContext) -> std::path::PathBuf {
    let p = get_string(args, "path")
        .filter(|p| !p.is_empty())
        .unwrap_or(".");
    paths::resolve_to_cwd(p, &ctx.cwd)
}

fn with_notices(content: String, notices: &[String]) -> String {
    if notices.is_empty() {
        content
    } else {
        format!("{content}\n\n[{}]", notices.join(". "))
    }
}

// ---- ls -------------------------------------------------------------------------------------

pub struct PiLsTool;

const LS_DEFAULT_LIMIT: f64 = 500.0;
const LS_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: current directory)"},"limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}}"#;

impl PiToolInfo for PiLsTool {
    fn prompt_snippet(&self) -> String {
        "List directory contents".into()
    }
}

#[async_trait::async_trait]
impl Tool for PiLsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        crate::schema(LS_SCHEMA)
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::DIRECTORY_RESTRICTED
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), LS_SCHEMA, arguments_json))
    }
    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        crate::affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let dir = search_root(args, ctx);
        let limit = get_number(args, "limit").unwrap_or(LS_DEFAULT_LIMIT);
        let shown = dir.to_string_lossy().into_owned();
        if dir.is_file() {
            return fail(format!("Not a directory: {shown}"));
        }
        if !dir.is_dir() {
            return fail_with(format!("Path not found: {shown}"), ToolErrorCode::NotFound);
        }
        let mut entries: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(e) => {
                return fail(format!(
                    "Cannot read directory: {}",
                    fs_errors::format_io(&e, "scandir", Some(&shown))
                ));
            }
        };
        entries.sort_by(|a, b| ordinal_utf16(&a.to_lowercase(), &b.to_lowercase()));
        let mut results = Vec::new();
        let mut entry_limit_reached = false;
        for entry in entries {
            if results.len() as f64 >= limit {
                entry_limit_reached = true;
                break;
            }
            let full = dir.join(&entry);
            if full.is_dir() {
                results.push(format!("{entry}/"));
            } else if full.is_file() {
                results.push(entry);
            }
        }
        if results.is_empty() {
            return ToolResult::success("(empty directory)");
        }
        let tr = truncate::head(&results.join("\n"), DEFAULT_MAX_BYTES, NO_LINE_LIMIT);
        let mut notices = Vec::new();
        if entry_limit_reached {
            notices.push(format!(
                "{} entries limit reached. Use limit={} for more",
                js_number(limit),
                js_number(limit * 2.0)
            ));
        }
        if tr.truncated {
            notices.push(format!(
                "{} limit reached",
                format_size(DEFAULT_MAX_BYTES as u64)
            ));
        }
        ToolResult::success(with_notices(tr.content, &notices))
    }
}

/// `string.CompareOrdinal`: UTF-16 code unit order.
fn ordinal_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

// ---- find -----------------------------------------------------------------------------------

pub struct PiFindTool;

const FIND_DEFAULT_LIMIT: f64 = 1000.0;
const FIND_SCHEMA: &str = r#"{"type":"object","required":["pattern"],"properties":{"pattern":{"type":"string","description":"Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},"path":{"type":"string","description":"Directory to search in (default: current directory)"},"limit":{"type":"number","description":"Maximum number of results (default: 1000)"}}}"#;

impl PiToolInfo for PiFindTool {
    fn prompt_snippet(&self) -> String {
        "Find files by glob pattern (respects .gitignore)".into()
    }
}

#[async_trait::async_trait]
impl Tool for PiFindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        crate::schema(FIND_SCHEMA)
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::DIRECTORY_RESTRICTED
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), FIND_SCHEMA, arguments_json))
    }
    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        crate::affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let pattern = get_string(args, "pattern").unwrap_or("");
        let root = search_root(args, ctx);
        let limit = get_number(args, "limit").unwrap_or(FIND_DEFAULT_LIMIT);
        if !root.is_dir() {
            return fail_with(
                format!("Path not found: {}", root.to_string_lossy()),
                ToolErrorCode::NotFound,
            );
        }
        let glob = if pattern.contains('/') && !pattern.starts_with("**/") && pattern != "**" {
            format!("**/{}", pattern.trim_start_matches('/'))
        } else {
            pattern.to_string()
        };
        let Some(matcher) = glob_to_regex(&glob) else {
            return fail(format!("Invalid glob pattern: {pattern}"));
        };
        let match_full_path = pattern.contains('/');

        let mut results = Vec::new();
        for rel in walk(&root) {
            let candidate = if match_full_path {
                rel.as_str()
            } else {
                rel.rsplit('/').next().unwrap_or(&rel)
            };
            if matcher.is_match(candidate) {
                results.push(rel.clone());
                if results.len() as f64 >= limit {
                    break;
                }
            }
        }
        if results.is_empty() {
            return ToolResult::success("No files found matching pattern");
        }
        let limit_reached = results.len() as f64 >= limit;
        let tr = truncate::head(&results.join("\n"), DEFAULT_MAX_BYTES, NO_LINE_LIMIT);
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{} results limit reached. Use limit={} for more, or refine pattern",
                js_number(limit),
                js_number(limit * 2.0)
            ));
        }
        if tr.truncated {
            notices.push(format!(
                "{} limit reached",
                format_size(DEFAULT_MAX_BYTES as u64)
            ));
        }
        ToolResult::success(with_notices(tr.content, &notices))
    }
}

// ---- grep -----------------------------------------------------------------------------------

pub struct PiGrepTool;

const GREP_DEFAULT_LIMIT: f64 = 100.0;
const GREP_SCHEMA: &str = r#"{"type":"object","required":["pattern"],"properties":{"pattern":{"type":"string","description":"Search pattern (regex or literal string)"},"path":{"type":"string","description":"Directory or file to search (default: current directory)"},"glob":{"type":"string","description":"Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},"literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},"context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},"limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}}}"#;

impl PiToolInfo for PiGrepTool {
    fn prompt_snippet(&self) -> String {
        "Search file contents for patterns (respects .gitignore)".into()
    }
}

#[async_trait::async_trait]
impl Tool for PiGrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        crate::schema(GREP_SCHEMA)
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::DIRECTORY_RESTRICTED
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), GREP_SCHEMA, arguments_json))
    }
    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        crate::affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let pattern = get_string(args, "pattern").unwrap_or("");
        let root_path = search_root(args, ctx);
        let glob = get_string(args, "glob").filter(|g| !g.is_empty());
        let ignore_case = get_bool(args, "ignoreCase");
        let literal = get_bool(args, "literal");
        let context = get_number(args, "context").unwrap_or(0.0).max(0.0);
        let limit = get_number(args, "limit")
            .unwrap_or(GREP_DEFAULT_LIMIT)
            .max(1.0);

        let is_directory = root_path.is_dir();
        if !is_directory && !root_path.is_file() {
            return fail_with(
                format!("Path not found: {}", root_path.to_string_lossy()),
                ToolErrorCode::NotFound,
            );
        }
        let source = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = match RegexBuilder::new(&source)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(r) => r,
            Err(e) => return fail(format!("regex parse error: {e}")),
        };
        let glob_re = match glob {
            Some(g) => match glob_to_regex(g) {
                Some(r) => Some(r),
                None => return fail(format!("Invalid glob pattern: {g}")),
            },
            None => None,
        };

        let (root, files): (std::path::PathBuf, Vec<String>) = if is_directory {
            let files = walk(&root_path)
                .into_iter()
                .filter(|rel| match (&glob_re, glob) {
                    (Some(g), Some(text)) => {
                        let candidate = if text.contains('/') {
                            rel.as_str()
                        } else {
                            rel.rsplit('/').next().unwrap_or(rel)
                        };
                        g.is_match(candidate)
                    }
                    _ => true,
                })
                .collect();
            (root_path.clone(), files)
        } else {
            let name = root_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (
                root_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                vec![name],
            )
        };

        let mut output_lines = Vec::new();
        let mut match_count = 0u64;
        let mut lines_truncated = false;
        let mut limit_reached = false;
        for rel in &files {
            let Ok(bytes) = std::fs::read(root.join(rel)) else {
                continue;
            };
            if bytes[..bytes.len().min(8000)].contains(&0) {
                continue; // binary
            }
            let text = crate::edit_diff::normalize_to_lf(&String::from_utf8_lossy(&bytes));
            let lines: Vec<&str> = text.split('\n').collect();
            for (i, line) in lines.iter().enumerate() {
                if i == lines.len() - 1 && line.is_empty() {
                    break;
                }
                if !re.is_match(line) {
                    continue;
                }
                match_count += 1;
                let line_number = i as i64 + 1;
                let start = (line_number as f64 - context).max(1.0) as i64;
                let end = (lines.len() as f64).min(line_number as f64 + context) as i64;
                for current in start..=end {
                    let (text, was) = truncate::truncate_line(
                        lines[(current - 1) as usize],
                        GREP_MAX_LINE_LENGTH,
                    );
                    lines_truncated |= was;
                    output_lines.push(if current == line_number {
                        format!("{rel}:{current}: {text}")
                    } else {
                        format!("{rel}-{current}- {text}")
                    });
                }
                if match_count as f64 >= limit {
                    limit_reached = true;
                    break;
                }
            }
            if limit_reached {
                break;
            }
        }

        if match_count == 0 {
            return ToolResult::success("No matches found");
        }
        let tr = truncate::head(&output_lines.join("\n"), DEFAULT_MAX_BYTES, NO_LINE_LIMIT);
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{} matches limit reached. Use limit={} for more, or refine pattern",
                js_number(limit),
                js_number(limit * 2.0)
            ));
        }
        if tr.truncated {
            notices.push(format!(
                "{} limit reached",
                format_size(DEFAULT_MAX_BYTES as u64)
            ));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
        }
        ToolResult::success(with_notices(tr.content, &notices))
    }
}

// ---- glob and walk (PiGlob) -----------------------------------------------------------------

/// `PiGlob.ToRegex` as regex source (without anchors).
fn glob_source(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut sb = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 1;
                if i + 1 < chars.len() && chars[i + 1] == '/' {
                    i += 1;
                    sb.push_str("(?:.*/)?");
                } else {
                    sb.push_str(".*");
                }
            }
            '*' => sb.push_str("[^/]*"),
            '?' => sb.push_str("[^/]"),
            '{' => match chars[i..].iter().position(|&ch| ch == '}') {
                None => sb.push_str("\\{"),
                Some(rel) => {
                    let close = i + rel;
                    let inner: String = chars[i + 1..close].iter().collect();
                    let alternatives: Vec<String> = inner.split(',').map(glob_source).collect();
                    sb.push_str("(?:");
                    sb.push_str(&alternatives.join("|"));
                    sb.push(')');
                    i = close;
                }
            },
            '[' => match chars[i + 1..].iter().position(|&ch| ch == ']') {
                None => sb.push_str("\\["),
                Some(rel) => {
                    let close = i + 1 + rel;
                    let mut cls: String = chars[i + 1..close].iter().collect();
                    if let Some(rest) = cls.strip_prefix('!') {
                        cls = format!("^{rest}");
                    }
                    sb.push('[');
                    sb.push_str(&cls.replace('\\', "\\\\"));
                    sb.push(']');
                    i = close;
                }
            },
            c => sb.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    sb
}

/// `PiGlob.ToRegex` (case-insensitive on Windows). `None` for a pattern the regex engine
/// rejects (C# would throw).
pub fn glob_to_regex(glob: &str) -> Option<Regex> {
    RegexBuilder::new(&format!("^{}$", glob_source(glob)))
        .case_insensitive(cfg!(windows))
        .build()
        .ok()
}

/// Files under `root` as '/'-separated relative paths, sorted per directory (files first, then
/// subdirectories depth-first), skipping .git and node_modules.
pub fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel)
        };
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            // Directory.GetDirectories follows links to directories; GetFiles lists the rest.
            let is_dir = ft.is_dir() || (ft.is_symlink() && e.path().is_dir());
            if is_dir {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        files.sort_by(|a, b| ordinal_utf16(a, b));
        for f in files {
            out.push(join(&rel, &f));
        }
        dirs.sort_by(|a, b| ordinal_utf16(a, b));
        for d in dirs.iter().rev() {
            if !d.eq_ignore_ascii_case(".git") && !d.eq_ignore_ascii_case("node_modules") {
                stack.push(join(&rel, d));
            }
        }
    }
    out
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}
