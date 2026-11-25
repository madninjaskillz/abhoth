use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{
    append_line, decode_text, get_bool_lenient, required_str, retry_io, unescape_literal_sequences,
    utf16_len,
};
use serde_json::Value;

use crate::util::{display, schema};

pub struct EditFileTool;

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in a file. The old_string must be unique (or use replace_all). Set create_if_missing=true with an empty old_string to create a new file with new_string as content."
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

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4dd}",
            short_description: "Edit file",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
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
            },
            "create_if_missing": {
                "type": "boolean",
                "description": "When true and the file doesn't exist, create it with new_string as content (old_string must be empty). Defaults to false."
            }
        },
        "required": ["path", "old_string", "new_string"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let (path, old, new) = match (
            required_str(args, "path"),
            required_str(args, "old_string"),
            required_str(args, "new_string"),
        ) {
            (Ok(p), Ok(o), Ok(n)) => (p, o.to_string(), n.to_string()),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return ToolResult::error(e),
        };
        let replace_all = get_bool_lenient(args.get("replace_all"), false);
        let create_if_missing = get_bool_lenient(args.get("create_if_missing"), false);
        let full = ctx.full_path(path);
        let shown = display(&full);

        if !full.is_file() {
            if create_if_missing && old.is_empty() {
                if let Some(dir) = full.parent()
                    && let Err(e) = tokio::fs::create_dir_all(dir).await
                {
                    return ToolResult::error(format!("Error editing file: {e}"));
                }
                return match retry_io(|| tokio::fs::write(&full, new.as_bytes())).await {
                    Ok(()) => ToolResult::success(format!(
                        "Created new file: {shown} ({} chars)",
                        utf16_len(&new)
                    )),
                    Err(e) => ToolResult::error(format!("Error editing file: {e}")),
                };
            }
            return ToolResult::error(format!(
                "File not found: {shown}. Use read_file to verify the path, or set create_if_missing=true to create it."
            ));
        }

        let content = match tokio::fs::read(&full).await {
            Ok(b) => decode_text(&b),
            Err(e) => return ToolResult::error(format!("Error editing file: {e}")),
        };

        if old.is_empty() {
            if !replace_all && !content.is_empty() {
                return ToolResult::error(
                    "old_string is empty but the file already has content. To replace specific text, set old_string to the text you want to replace. To overwrite the entire file, use write_file instead.",
                );
            }
            return match retry_io(|| tokio::fs::write(&full, new.as_bytes())).await {
                Ok(()) => ToolResult::success(format!(
                    "Replaced entire content of {shown} ({} chars)",
                    utf16_len(&new)
                )),
                Err(e) => ToolResult::error(format!("Error editing file: {e}")),
            };
        }

        let (old, new) = if content.contains(&old) {
            (old, new)
        } else {
            match try_multi_stage_match(&content, &old, &new) {
                Some(m) => (m.matched_old, m.adjusted_new),
                None => {
                    let preview = if utf16_len(&content) <= 10_000 {
                        content.clone()
                    } else {
                        let head: String = content.chars().take(10_000).collect();
                        format!(
                            "{head}\n... ({} chars truncated)",
                            utf16_len(&content) - 10_000
                        )
                    };
                    return ToolResult::error(format!(
                        "old_string not found in {shown}. Current file content:\n\n{preview}"
                    ));
                }
            }
        };

        let count = content.matches(old.as_str()).count();
        if !replace_all && count > 1 {
            return ToolResult::error(format!(
                "old_string appears {count} times in {shown}. Provide more context to make it unique, or set replace_all to true."
            ));
        }
        let new_content = if replace_all {
            content.replace(&old, &new)
        } else {
            content.replacen(&old, &new, 1)
        };
        if let Err(e) = retry_io(|| tokio::fs::write(&full, new_content.as_bytes())).await {
            return ToolResult::error(format!("Error editing file: {e}"));
        }
        let change_count = if replace_all { count } else { 1 };
        let diff = get_edit_diff(&content, &old, &new);
        ToolResult::success(format!(
            "Replaced {change_count} occurrence(s) in {shown}\n\n{diff}"
        ))
    }
}

fn split_any_newline(s: &str) -> Vec<&str> {
    // string.Split(["\r\n", "\r", "\n"]): every break counts, a trailing one yields "".
    let mut out = Vec::new();
    let b = s.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b'\r' || b[i] == b'\n' {
            out.push(&s[start..i]);
            if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// `GetEditDiff`: the first change with three lines of context.
pub fn get_edit_diff(original: &str, old: &str, new: &str) -> String {
    let Some(pos) = original.find(old) else {
        return String::new();
    };
    let all_lines = split_any_newline(original);
    let start_line = original[..pos].split('\n').count() - 1;
    let old_lines = split_any_newline(old);
    let new_lines = split_any_newline(new);
    const CONTEXT: usize = 3;
    let context_start = start_line.saturating_sub(CONTEXT);
    let end_line = start_line + old_lines.len() - 1;
    let shift = new_lines.len() as i64 - old_lines.len() as i64;
    let context_end = (all_lines.len() as i64 - 1).min((end_line + CONTEXT) as i64);

    let mut sb = String::new();
    append_line(&mut sb, "--- diff ---");
    for (i, line) in all_lines
        .iter()
        .enumerate()
        .take(start_line)
        .skip(context_start)
    {
        append_line(&mut sb, &format!("  {:>4}   {line}", i + 1));
    }
    for (i, line) in old_lines.iter().enumerate() {
        append_line(&mut sb, &format!("  {:>4} - {line}", start_line + i + 1));
    }
    for (i, line) in new_lines.iter().enumerate() {
        append_line(&mut sb, &format!("  {:>4} + {line}", start_line + i + 1));
    }
    let mut i = end_line as i64 + 1;
    while i <= context_end {
        append_line(
            &mut sb,
            &format!("  {:>4}   {}", i + 1 + shift, all_lines[i as usize]),
        );
        i += 1;
    }
    sb
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchResult {
    pub matched_old: String,
    pub adjusted_new: String,
}

/// `TryMultiStageMatch`: escape-normalized, line-trimmed, whitespace-normalized,
/// indentation-flexible, then block-anchor matching.
pub fn try_multi_stage_match(content: &str, old: &str, new: &str) -> Option<MatchResult> {
    let unescaped = unescape_literal_sequences(old);
    if unescaped != old && content.contains(&unescaped) {
        return Some(MatchResult {
            matched_old: unescaped,
            adjusted_new: unescape_literal_sequences(new),
        });
    }
    if let Some(m) = try_line_trimmed_match(content, old) {
        return Some(MatchResult {
            matched_old: m,
            adjusted_new: new.to_string(),
        });
    }
    for stage in [
        try_whitespace_normalized_match,
        try_indentation_flexible_match,
        try_block_anchor_match,
    ] {
        if let Some(m) = stage(content, old) {
            let adjusted = adjust_indentation(new, &m, old);
            return Some(MatchResult {
                matched_old: m,
                adjusted_new: adjusted,
            });
        }
    }
    None
}

/// The unique run of content lines equal to `old`'s lines after `norm`; `None` when there are
/// none, several, or the run is `old` itself.
fn unique_line_match(content: &str, old: &str, norm: impl Fn(&str) -> &str) -> Option<String> {
    let old_norm: Vec<&str> = old.split('\n').map(&norm).collect();
    if old_norm.is_empty() || (old_norm.len() == 1 && old_norm[0].is_empty()) {
        return None;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() < old_norm.len() {
        return None;
    }
    let mut first = None;
    let mut count = 0;
    for i in 0..=lines.len() - old_norm.len() {
        if (0..old_norm.len()).all(|j| norm(lines[i + j]) == old_norm[j]) {
            count += 1;
            if count > 1 {
                return None;
            }
            first = Some(lines[i..i + old_norm.len()].join("\n"));
        }
    }
    first.filter(|m| m != old)
}

pub fn try_line_trimmed_match(content: &str, old: &str) -> Option<String> {
    unique_line_match(content, old, |l| l.trim_end_matches(['\r', ' ', '\t']))
}

pub fn try_whitespace_normalized_match(content: &str, old: &str) -> Option<String> {
    unique_line_match(content, old, |l| l.trim_start().trim_end_matches('\r'))
}

pub fn try_indentation_flexible_match(content: &str, old: &str) -> Option<String> {
    let lines: Vec<&str> = old.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().count() - l.trim_start().chars().count())
        .min()?;
    if min_indent == 0 {
        return None;
    }
    let stripped: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.chars().count() > min_indent {
                l.chars().skip(min_indent).collect()
            } else {
                (*l).to_string()
            }
        })
        .collect();
    try_whitespace_normalized_match(content, &stripped.join("\n"))
}

pub fn try_block_anchor_match(content: &str, old: &str) -> Option<String> {
    let old_lines: Vec<&str> = old.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    if old_lines.len() < 3 {
        return None;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let first = old_lines[0].trim();
    let last = old_lines[old_lines.len() - 1].trim();
    if first.chars().count() < 2 || last.is_empty() || lines.len() < old_lines.len() {
        return None;
    }
    let mut best: Option<String> = None;
    let mut best_similarity = 0.0;
    for i in 0..=lines.len() - old_lines.len() {
        if lines[i].trim() != first {
            continue;
        }
        let end = i + old_lines.len() - 1;
        if lines[end].trim() != last {
            continue;
        }
        let matched = &lines[i..=end];
        let matches = old_lines
            .iter()
            .zip(matched)
            .filter(|(e, a)| e.trim() == a.trim_end_matches('\r').trim())
            .count();
        let similarity = matches as f64 / old_lines.len() as f64;
        if similarity > 0.6 && similarity > best_similarity {
            best_similarity = similarity;
            best = Some(matched.join("\n"));
        }
    }
    best
}

/// Re-indent `new` by the indentation the file has and the model left out.
pub fn adjust_indentation(new: &str, matched_old: &str, original_old: &str) -> String {
    let lead = |s: &str| -> String {
        let first = s.split('\n').next().unwrap_or("");
        first[..first.len() - first.trim_start().len()].to_string()
    };
    let matched_indent = lead(matched_old);
    let old_indent = lead(original_old);
    if old_indent.len() >= matched_indent.len() {
        return new.to_string();
    }
    let delta = &matched_indent[old_indent.len()..];
    new.split('\n')
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("{delta}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
