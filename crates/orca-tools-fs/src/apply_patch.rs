//! `apply_patch` and `UnifiedDiffParser`: unified diffs with fuzzy hunk placement.

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{
    NEWLINE, append_line, decode_text, get_str, io_message, required_str, retry_io,
};
use serde_json::Value;

use crate::util::{display, schema};

/// `UnifiedDiffParser` (public for tests).
pub mod unified_diff {
    /// Max lines of offset for fuzzy matching.
    const MAX_FUZZ: i64 = 3;

    #[derive(Debug, Clone, Default)]
    pub struct PatchFile {
        pub source_path: String,
        pub target_path: String,
        pub hunks: Vec<Hunk>,
    }

    impl PatchFile {
        pub fn is_new_file(&self) -> bool {
            matches!(self.source_path.as_str(), "/dev/null" | "")
        }
        pub fn is_delete_file(&self) -> bool {
            matches!(self.target_path.as_str(), "/dev/null" | "")
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct Hunk {
        pub source_start: i64,
        pub source_count: i64,
        pub target_start: i64,
        pub target_count: i64,
        pub lines: Vec<DiffLine>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DiffLineType {
        Context,
        Add,
        Remove,
    }

    #[derive(Debug, Clone)]
    pub struct DiffLine {
        pub kind: DiffLineType,
        pub content: String,
    }

    impl DiffLine {
        pub fn new(kind: DiffLineType, content: &str) -> Self {
            Self {
                kind,
                content: content.to_string(),
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct ApplyResult {
        pub success: bool,
        pub error: Option<String>,
        pub hunks_applied: usize,
        pub hunks_total: usize,
        pub result_content: Option<String>,
    }

    /// `string.Split(["\r\n", "\r", "\n"], None)`: every break splits, keeping empty pieces.
    fn split_any_newline(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let (mut start, mut i) = (0, 0);
        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    out.push(&text[start..i]);
                    if bytes.get(i + 1) == Some(&b'\n') {
                        i += 1;
                    }
                    start = i + 1;
                }
                b'\n' => {
                    out.push(&text[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        out.push(&text[start..]);
        out
    }

    /// Parses a unified diff into one entry per `---`/`+++` header pair.
    pub fn parse(patch: &str) -> Vec<PatchFile> {
        let lines = split_any_newline(patch);
        let mut files = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if i + 1 < lines.len() && lines[i].starts_with("---") && lines[i + 1].starts_with("+++")
            {
                let mut file = PatchFile {
                    source_path: extract_path(lines[i]),
                    target_path: extract_path(lines[i + 1]),
                    hunks: Vec::new(),
                };
                i += 2;
                while i < lines.len() && lines[i].starts_with("@@") {
                    if let Some(h) = parse_hunk(&lines, &mut i) {
                        file.hunks.push(h);
                    }
                }
                files.push(file);
            } else {
                i += 1;
            }
        }
        files
    }

    /// Applies one file's hunks to `content`, each within [`MAX_FUZZ`] lines of its position.
    pub fn apply(content: &str, patch: &PatchFile) -> ApplyResult {
        let mut lines: Vec<String> = split_any_newline(content)
            .into_iter()
            .map(str::to_string)
            .collect();
        // A final newline leaves a trailing empty piece; drop it to match the real line count.
        if content.ends_with('\n') && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }

        let mut offset: i64 = 0;
        let mut applied = 0;
        for hunk in &patch.hunks {
            let target = hunk.source_start - 1 + offset;
            let expected: Vec<&str> = hunk
                .lines
                .iter()
                .filter(|l| l.kind != DiffLineType::Add)
                .map(|l| l.content.as_str())
                .collect();
            let Some(pos) = find_hunk_position(&lines, &expected, target) else {
                return ApplyResult {
                    success: false,
                    error: Some(format!(
                        "Hunk at line {} could not be applied \u{2014} context does not match",
                        hunk.source_start
                    )),
                    hunks_applied: applied,
                    hunks_total: patch.hunks.len(),
                    result_content: None,
                };
            };
            let new_lines: Vec<String> = hunk
                .lines
                .iter()
                .filter(|l| l.kind != DiffLineType::Remove)
                .map(|l| l.content.clone())
                .collect();
            let added = new_lines.len() as i64;
            lines.splice(pos..pos + expected.len(), new_lines);
            offset += added - expected.len() as i64;
            applied += 1;
        }

        // Rebuild with the original line ending style.
        let ending = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut result = lines.join(ending);
        if content.ends_with(ending) {
            result.push_str(ending);
        }
        ApplyResult {
            success: true,
            error: None,
            hunks_applied: applied,
            hunks_total: patch.hunks.len(),
            result_content: Some(result),
        }
    }

    fn find_hunk_position(lines: &[String], expected: &[&str], target: i64) -> Option<usize> {
        if matches_at(lines, expected, target) {
            return Some(target as usize);
        }
        for fuzz in 1..=MAX_FUZZ {
            if target - fuzz >= 0 && matches_at(lines, expected, target - fuzz) {
                return Some((target - fuzz) as usize);
            }
            if matches_at(lines, expected, target + fuzz) {
                return Some((target + fuzz) as usize);
            }
        }
        None
    }

    fn matches_at(lines: &[String], expected: &[&str], pos: i64) -> bool {
        if pos < 0 || pos as usize + expected.len() > lines.len() {
            return false;
        }
        let pos = pos as usize;
        expected
            .iter()
            .enumerate()
            .all(|(i, e)| lines[pos + i] == *e)
    }

    /// `--- a/path` / `+++ b/path` / `--- /dev/null`: the path with an `a/` or `b/` prefix
    /// removed; empty when the header has no path.
    fn extract_path(header: &str) -> String {
        let trimmed = header.trim_start_matches([' ', '\t']);
        let Some(sep) = trimmed.find([' ', '\t']) else {
            return String::new();
        };
        // Split(' ', '\t', 2, RemoveEmptyEntries): the rest after the first separator run.
        let rest = trimmed[sep..].trim_start_matches([' ', '\t']);
        if rest.is_empty() {
            return String::new();
        }
        let path = rest;
        if path.starts_with("a/") || path.starts_with("b/") {
            path[2..].to_string()
        } else {
            path.to_string()
        }
    }

    fn parse_int(s: &str) -> Option<i64> {
        s.parse().ok()
    }

    /// `^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`
    fn parse_header(line: &str) -> Option<(i64, i64, i64, i64)> {
        fn range(s: &str) -> Option<(i64, i64, &str)> {
            let digits = s.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            let start = parse_int(&s[..digits])?;
            let rest = &s[digits..];
            if let Some(after) = rest.strip_prefix(',') {
                let d2 = after.bytes().take_while(u8::is_ascii_digit).count();
                if d2 > 0 {
                    return Some((start, parse_int(&after[..d2])?, &after[d2..]));
                }
            }
            Some((start, 1, rest))
        }
        let s = line.strip_prefix("@@ -")?;
        let (ss, sc, rest) = range(s)?;
        let rest = rest.strip_prefix(" +")?;
        let (ts, tc, rest) = range(rest)?;
        rest.starts_with(" @@").then_some((ss, sc, ts, tc))
    }

    fn parse_hunk(lines: &[&str], i: &mut usize) -> Option<Hunk> {
        let Some((ss, sc, ts, tc)) = parse_header(lines[*i]) else {
            *i += 1;
            return None;
        };
        let mut hunk = Hunk {
            source_start: ss,
            source_count: sc,
            target_start: ts,
            target_count: tc,
            lines: Vec::new(),
        };
        *i += 1;
        while *i < lines.len() && !lines[*i].starts_with("@@") && !lines[*i].starts_with("---") {
            let line = lines[*i];
            if let Some(rest) = line.strip_prefix('+') {
                hunk.lines.push(DiffLine::new(DiffLineType::Add, rest));
            } else if let Some(rest) = line.strip_prefix('-') {
                hunk.lines.push(DiffLine::new(DiffLineType::Remove, rest));
            } else if let Some(rest) = line.strip_prefix(' ') {
                hunk.lines.push(DiffLine::new(DiffLineType::Context, rest));
            } else if line.is_empty() {
                hunk.lines.push(DiffLine::new(DiffLineType::Context, ""));
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
            } else {
                break;
            }
            *i += 1;
        }
        Some(hunk)
    }
}

pub struct ApplyPatchTool;

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff patch to one or more files. Supports multi-file and multi-hunk patches with fuzzy line matching (up to 3 lines offset). Can create new files (diff against /dev/null) and delete files (diff to /dev/null)."
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
            emoji: "\u{1fa79}",
            short_description: "Apply patch",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "patch": {
                "type": "string",
                "description": "The unified diff content to apply. Use standard unified diff format with --- and +++ headers, @@ hunk headers, and +/- line prefixes."
            },
            "path": {
                "type": "string",
                "description": "Optional file path override for single-file patches. If provided, overrides the path in the diff headers."
            }
        },
        "required": ["patch"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let patch = match required_str(args, "patch") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let path_override = get_str(args, "path").filter(|p| !p.is_empty());
        let fail = |e: std::io::Error| {
            ToolResult::error(format!("Error applying patch: {}", io_message(&e)))
        };

        let files = unified_diff::parse(patch);
        if files.is_empty() {
            return ToolResult::error(
                "Could not parse any file diffs from the patch. Ensure you use standard unified diff format with --- and +++ headers.",
            );
        }

        let mut sb = String::new();
        let (mut total, mut applied) = (0, 0);
        for file in &files {
            let target = match path_override {
                Some(p) if files.len() == 1 => p,
                _ if file.is_new_file() => file.target_path.as_str(),
                _ => file.source_path.as_str(),
            };
            if target.is_empty() {
                return ToolResult::error(
                    "Could not determine target file path from the patch. Provide a 'path' parameter or use standard --- a/file +++ b/file headers.",
                );
            }
            let target = ctx.full_path(target);
            let shown = display(&target);
            total += file.hunks.len();

            if file.is_new_file() {
                if let Some(dir) = target.parent()
                    && let Err(e) = std::fs::create_dir_all(dir)
                {
                    return fail(e);
                }
                let new_content = file
                    .hunks
                    .iter()
                    .flat_map(|h| &h.lines)
                    .filter(|l| l.kind == unified_diff::DiffLineType::Add)
                    .map(|l| l.content.as_str())
                    .collect::<Vec<_>>()
                    .join(NEWLINE);
                let text = format!("{new_content}{NEWLINE}");
                if let Err(e) = retry_io(|| tokio::fs::write(&target, text.as_bytes())).await {
                    return fail(e);
                }
                applied += file.hunks.len();
                append_line(
                    &mut sb,
                    &format!(
                        "Created: {shown} ({} lines)",
                        new_content.split('\n').count()
                    ),
                );
                continue;
            }

            if file.is_delete_file() {
                if target.is_file() {
                    if let Err(e) = std::fs::remove_file(&target) {
                        return fail(e);
                    }
                    applied += file.hunks.len();
                    append_line(&mut sb, &format!("Deleted: {shown}"));
                } else {
                    append_line(&mut sb, &format!("Skipped delete (not found): {shown}"));
                }
                continue;
            }

            if !target.is_file() {
                return ToolResult::error(format!("File not found: {shown}"));
            }
            let content = match tokio::fs::read(&target).await {
                Ok(b) => decode_text(&b),
                Err(e) => return fail(e),
            };
            let result = unified_diff::apply(&content, file);
            if !result.success {
                return ToolResult::error(format!(
                    "Patch failed for {shown}: {} ({}/{} hunks applied before failure)",
                    result.error.unwrap_or_default(),
                    result.hunks_applied,
                    result.hunks_total
                ));
            }
            let new_text = result.result_content.unwrap_or_default();
            if let Err(e) = retry_io(|| tokio::fs::write(&target, new_text.as_bytes())).await {
                return fail(e);
            }
            applied += result.hunks_applied;
            append_line(
                &mut sb,
                &format!(
                    "Patched: {shown} ({} hunk(s) applied)",
                    result.hunks_applied
                ),
            );
        }
        ToolResult::success(format!(
            "Applied {applied}/{total} hunk(s) across {} file(s)\n\n{sb}",
            files.len()
        ))
    }
}
