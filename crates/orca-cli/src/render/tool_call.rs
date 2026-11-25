//! `ToolCallRenderer`: tool calls and results in a compact, Claude-Code-like layout:
//!
//! ```text
//!   ● bash  pytest -q
//!     ⎿  3 passed in 0.41s
//!   ● edit  src/calc.py
//!     ⎿  Edited src/calc.py  +1 −0
//!             4   def sub(a, b):
//!             5 +     # subtract b from a
//! ```
//!
//! Purely presentation: what the model sees is never changed here. The tool is passed in by
//! the caller (`registry.resolve(name).as_deref()`) instead of a registry held here.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use orca_abstractions::{Tool, ToolCapabilities, ToolContext};
use orca_core::attachments::{file_link_resolver, image_files};
use orca_core::live::LiveEventBus;
use regex::Regex;
use serde_json::{Map, Value};

use super::links;
use super::text::{is_blank, sanitize};
use crate::repl::state::ReplState;
use crate::ui::console::{Console, cell_width, ellipsize};
use crate::ui::markup::escape;

const RESULT_LEAD: &str = "  [dim]⎿[/]  "; // "  ⎿  "
const RESULT_INDENT: &str = "     ";
const RESULT_INDENT_CELLS: usize = 5;
const MAX_COLLAPSED_DIFF_LINES: usize = 24;
const MAX_IMAGE_RESULT_LINES: usize = 4;

/// `CliConstants.ToolOutputPreviewLines`.
pub const TOOL_OUTPUT_PREVIEW_LINES: usize = 7;
/// `CliConstants.ToolErrorDisplayMaxChars`.
pub const TOOL_ERROR_DISPLAY_MAX_CHARS: usize = 120;

// Pi bash appends its status as the last line ("Command exited with code 1").
static SHELL_STATUS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(Command exited with code -?\d+|Command timed out.*|Command aborted.*)$")
        .expect("static regex")
});
static READ_CONTINUATION_NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[Showing lines (\d+)-(\d+) of (\d+)").expect("static regex")
});

pub struct ToolCallRenderer {
    console: Console,
    /// When true, all rendering is suppressed (benchmark mode).
    pub suppressed: bool,
    /// The folder tool paths resolve against (the Pi edit diff reads the edited file).
    pub cwd: PathBuf,
    /// Terminal width override (tests); `None` = the console's width.
    pub width: Option<usize>,
    // Calls in the current batch whose results haven't been rendered yet. Results of a
    // multi-call batch render after all headers, so each one gets a label naming its call.
    pending_results: usize,
    multi_call_batch: bool,
}

impl ToolCallRenderer {
    pub fn new(console: Console) -> Self {
        Self {
            console,
            suppressed: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            width: None,
            pending_results: 0,
            multi_call_batch: false,
        }
    }

    fn width(&self) -> usize {
        self.width.unwrap_or_else(|| self.console.width()).max(40)
    }

    /// Start of a tool-call batch: forget results that were never rendered (streamed tools,
    /// cancelled batches) so they can't mislabel the next batch.
    pub fn begin_batch(&mut self) {
        self.pending_results = 0;
        self.multi_call_batch = false;
    }

    /// `RenderToolCall`: the `● name  summary  reason` header.
    pub fn render_tool_call(
        &mut self,
        tool_name: &str,
        arguments: &str,
        tool: Option<&dyn Tool>,
        state: &ReplState,
    ) {
        LiveEventBus::shared().publish_tool_call(tool_name, arguments);
        if self.suppressed || !state.show_tool_calls() {
            return; // Level 0: suppress tool call rendering
        }
        if self.pending_results > 0 {
            self.multi_call_batch = true;
        }
        self.pending_results += 1;
        let line = self.call_header(tool_name, arguments, tool);
        self.console.markup_line(&line);
    }

    /// The header line's markup.
    pub fn call_header(&self, tool_name: &str, arguments: &str, tool: Option<&dyn Tool>) -> String {
        let summary = format_call_summary(tool_name, tool, arguments);
        let reason = extract_reason(arguments);
        let mut budget = self.width() as i64 - 6 - cell_width(tool_name) as i64;
        let mut line = format!("  [cyan]●[/] [bold]{}[/]", escape(tool_name));
        if !summary.is_empty() {
            let shown = ellipsize(&summary, budget.max(10) as usize);
            let path = if links::enabled() {
                path_argument(arguments)
            } else {
                None
            };
            line.push_str(&format!(
                "  {}",
                links::summary_markup(&shown, path.as_deref())
            ));
            budget -= cell_width(&shown) as i64 + 2;
        }
        if let Some(reason) = reason.filter(|r| !r.is_empty())
            && budget > 12
        {
            line.push_str(&format!(
                "  [dim]{}[/]",
                escape(&ellipsize(&one_line(&reason), (budget - 2) as usize))
            ));
        }
        line
    }

    /// `RenderToolResult`. `arguments` (the call's JSON) gives the batch label and the
    /// edit / write previews; results render without it too. `elapsed` goes to the live
    /// event bus.
    #[allow(clippy::too_many_arguments)]
    pub fn render_tool_result(
        &mut self,
        tool_name: &str,
        result: &str,
        is_error: bool,
        elapsed: Option<Duration>,
        arguments: Option<&str>,
        tool: Option<&dyn Tool>,
        state: &ReplState,
    ) {
        LiveEventBus::shared().publish_tool_result(
            tool_name,
            result,
            is_error,
            elapsed.map(|e| e.as_millis() as i64),
        );
        if self.suppressed {
            return;
        }
        for line in self.result_lines(tool_name, result, is_error, arguments, tool, state) {
            self.console.markup_line(&line);
        }
    }

    /// The markup lines `render_tool_result` writes (with the closing blank line), and the
    /// batch bookkeeping. Empty when nothing is shown.
    pub fn result_lines(
        &mut self,
        tool_name: &str,
        result: &str,
        is_error: bool,
        arguments: Option<&str>,
        tool: Option<&dyn Tool>,
        state: &ReplState,
    ) -> Vec<String> {
        let had_header = self.pending_results > 0;
        let label_needed = self.multi_call_batch && had_header;
        if self.pending_results > 0 {
            self.pending_results -= 1;
            if self.pending_results == 0 {
                self.multi_call_batch = false;
            }
        }
        if is_blank(result) {
            return Vec::new();
        }

        // CRLF normalised; escape sequences and control characters stripped so tool output
        // can't recolour or move the cursor over the input panel.
        let display = sanitize(&normalize_line_endings(result));
        let caps = tool.map(|t| t.capabilities()).unwrap_or_default();
        let is_print = caps.contains(ToolCapabilities::ALWAYS_VISIBLE);

        // At level 0, only print tool output is shown.
        if state.show_print_only() && !is_print {
            return Vec::new();
        }
        if is_print {
            // Print tool: always render prominently (not dim).
            return vec![format!("  [white]{}[/]", escape(&display))];
        }

        let expanded = state.show_full_tool_output();
        let max_lines = TOOL_OUTPUT_PREVIEW_LINES;
        let mut body: Vec<String> = Vec::new(); // markup lines; the first gets the ⎿ lead

        if label_needed && let Some(args) = arguments {
            let label = format_call_summary(tool_name, tool, args);
            let text = format!("{tool_name} {label}");
            body.push(format!(
                "[dim]{}[/]",
                escape(&ellipsize(text.trim_end(), self.width() - 8))
            ));
        }

        let pi_edit = (!is_error && is_pi_tool(tool, "edit"))
            .then(|| arguments.and_then(|a| self.try_build_pi_edit_diff(tool?, a)))
            .flatten();
        let pi_write = (!is_error && pi_edit.is_none() && is_pi_tool(tool, "write"))
            .then(|| arguments.and_then(try_get_write_content))
            .flatten();

        if is_error {
            self.build_error_body(&mut body, tool_name, &display, had_header, max_lines, expanded);
        } else if let Some(diff) = pi_edit {
            let counts = format!("[green]+{}[/] [red]−{}[/]", diff.added, diff.removed);
            body.push(format!(
                "Edited {}  {counts}",
                links::path_markup(&diff.path, None)
            ));
            if !expanded && diff.lines.len() > MAX_COLLAPSED_DIFF_LINES {
                add_diff_lines(&mut body, &diff.lines[..MAX_COLLAPSED_DIFF_LINES], max_lines, true);
                body.push(more_lines_hint(diff.lines.len() - MAX_COLLAPSED_DIFF_LINES));
            } else {
                add_diff_lines(&mut body, &diff.lines, max_lines, expanded);
            }
        } else if let Some((path, content)) = pi_write {
            let sanitized = sanitize(&normalize_line_endings(&content));
            let all: Vec<&str> = sanitized.split('\n').collect();
            let content_lines = trim_blank_edges(&all);
            body.push(format!(
                "Wrote {} line{} to {}",
                content_lines.len(),
                if content_lines.len() == 1 { "" } else { "s" },
                links::path_markup(&path, None)
            ));
            self.add_preview_lines(&mut body, content_lines, max_lines, expanded);
        } else if !expanded && is_file_read_tool(tool) {
            body.push(format!("[dim]{}[/]", escape(&summarize_read(&display))));
        } else if caps.contains(ToolCapabilities::HAS_DIFF_OUTPUT) {
            let lines: Vec<&str> = display.split('\n').collect();
            add_diff_lines(&mut body, &lines, max_lines, expanded);
        } else {
            let lines: Vec<&str> = display.split('\n').collect();
            let collapsed = collapse_blank_runs(&lines);
            self.add_preview_lines(&mut body, &collapsed, max_lines, expanded);
        }

        // Thumbnails (InlineImages) are not ported: the body always closes with a blank line.
        let mut out: Vec<String> = body
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let lead = if i == 0 { RESULT_LEAD } else { RESULT_INDENT };
                if b.is_empty() {
                    String::new()
                } else {
                    format!("{lead}{b}")
                }
            })
            .collect();
        if !out.is_empty() {
            // A blank line closes each call block, separating it from the next call or the reply.
            out.push(String::new());
        }
        out
    }

    fn build_error_body(
        &self,
        body: &mut Vec<String>,
        tool_name: &str,
        display: &str,
        had_header: bool,
        max_lines: usize,
        expanded: bool,
    ) {
        let all: Vec<&str> = display.split('\n').collect();
        let lines = trim_blank_edges(&all);
        if lines.is_empty() {
            return;
        }
        let last = lines[lines.len() - 1].trim();
        // Shell failures: the output is the interesting part; the status goes last, in red.
        if lines.len() > 1 && SHELL_STATUS_LINE.is_match(last) {
            let output = collapse_blank_runs(trim_blank_edges(&lines[..lines.len() - 1]));
            self.add_preview_lines(body, &output, max_lines, expanded);
            body.push(format!("[red]✗ {}[/]", escape(last)));
            return;
        }
        // Without a header right above (e.g. a call blocked before it ran), name the tool.
        let first = ellipsize(lines[0], TOOL_ERROR_DISPLAY_MAX_CHARS);
        let prefix = if had_header {
            String::new()
        } else {
            format!("{tool_name}: ")
        };
        body.push(format!("[red]✗ {}[/]", escape(&(prefix + &first))));
        if lines.len() > 1 {
            let rest = collapse_blank_runs(&lines[1..]);
            self.add_preview_lines(body, &rest, max_lines, expanded);
        }
    }

    fn add_preview_lines<S: AsRef<str>>(
        &self,
        body: &mut Vec<String>,
        lines: &[S],
        max_lines: usize,
        expanded: bool,
    ) {
        let to_show = if expanded {
            lines.len()
        } else {
            lines.len().min(max_lines)
        };
        let cells = self.width() - RESULT_INDENT_CELLS - 1;
        for line in &lines[..to_show] {
            let line = line.as_ref();
            // Collapsed previews cut long lines instead of letting them wrap to column 0.
            // (Agent badges — AgentBadge.DecorateMarkup — need the orchestrator: not ported.)
            let text = if expanded {
                line.to_string()
            } else {
                ellipsize(line, cells.max(8))
            };
            body.push(if text.is_empty() {
                String::new()
            } else {
                format!("[dim]{}[/]", escape(&text))
            });
        }
        if !expanded && lines.len() > max_lines {
            body.push(more_lines_hint(lines.len() - max_lines));
        }
    }

    /// `TryBuildPiEditDiff`: a display diff for a successful Pi edit from its arguments and
    /// the file as it now is (Pi's result text is only "Successfully replaced N block(s)").
    fn try_build_pi_edit_diff(&self, tool: &dyn Tool, args_json: &str) -> Option<EditDiff> {
        let root: Value = serde_json::from_str(args_json).ok()?;
        let obj = root.as_object()?;
        let path = try_get_string(obj, "path");
        let edits_el = obj.get("edits")?.as_array()?;
        if path.is_empty() {
            return None;
        }
        let edits: Vec<(String, String)> = edits_el
            .iter()
            .filter_map(Value::as_object)
            .map(|e| (try_get_string(e, "oldText"), try_get_string(e, "newText")))
            .collect();
        let ctx = ToolContext::new(self.cwd.clone());
        let abs = tool.affected_file_paths(&root, &ctx).into_iter().next()?;
        let meta = std::fs::metadata(&abs).ok()?;
        if !meta.is_file() || meta.len() > 4_000_000 {
            return None;
        }
        let text = std::fs::read_to_string(&abs).ok()?;
        let mut diff = build_edit_diff(&text, &edits)?;
        diff.path = path;
        Some(diff)
    }

    /// `RenderPermissionPrompt`: indented under the `● tool  summary` header it belongs to.
    pub fn render_permission_prompt(&self, tool_name: &str, risk_level: &str) {
        if self.suppressed {
            return;
        }
        self.console.markup_line(&format!(
            "  [yellow]⎿  ⚠ [bold]{}[/] needs approval[/] [dim](risk: {})[/]",
            escape(tool_name),
            escape(&risk_level.to_lowercase())
        ));
    }

    pub fn render_plan_mode_toggle(&self, enabled: bool) {
        if enabled {
            self.console.markup_line(
                "[cyan]⏸ Plan mode enabled[/] — the model will plan without making changes.",
            );
        } else {
            self.console
                .markup_line("[green]▶ Plan mode disabled[/] — the model can now execute changes.");
        }
    }

    /// `risk_level` as C# prints the enum (`ToolRiskLevel.ToString()`: "ReadOnly",
    /// "Moderate", "Dangerous").
    pub fn render_plan_tool_blocked(&self, tool_name: &str, risk_level: &str) {
        self.console.markup_line(&format!(
            "  [cyan]⏸ {}[/] [dim]blocked in plan mode (risk: {})[/]",
            escape(tool_name),
            escape(risk_level)
        ));
    }
}

/// The file a call works on (its "path" / "file_path" argument), for linking the header.
pub fn path_argument(args_json: &str) -> Option<String> {
    if is_blank(args_json) || args_json == "{}" {
        return None;
    }
    let root: Value = serde_json::from_str(args_json).ok()?;
    let obj = root.as_object()?;
    ["path", "file_path", "filePath", "file"]
        .iter()
        .map(|k| try_get_string(obj, k))
        .find(|v| !v.is_empty())
}

/// The optional `_reason` field of the arguments.
fn extract_reason(args_json: &str) -> Option<String> {
    if is_blank(args_json) || args_json == "{}" {
        return None;
    }
    let root: Value = serde_json::from_str(args_json).ok()?;
    root.as_object()?
        .get("_reason")?
        .as_str()
        .map(str::to_string)
}

/// Image files a short result names ("Image saved: out.png", a glob with a match or two).
/// Long results (directory listings, searches) give none, so a listing of a folder of
/// pictures doesn't turn into a gallery.
pub fn image_paths_in(result: &str, working_directory: &str) -> Vec<String> {
    // C# measures UTF-16 units; bytes here (only the order of magnitude matters).
    if result.len() > 4096
        || result.split('\n').filter(|l| !is_blank(l)).count() > MAX_IMAGE_RESULT_LINES
    {
        return Vec::new();
    }
    file_link_resolver::find_paths(result, working_directory)
        .into_iter()
        .map(|r| r.full_path)
        .filter(|p| image_files::is_image(p))
        .collect()
}

fn more_lines_hint(remaining: usize) -> String {
    format!(
        "[dim]⋯ {remaining} more line{} (ctrl+o to expand)[/]",
        if remaining == 1 { "" } else { "s" }
    )
}

fn add_diff_lines<S: AsRef<str>>(body: &mut Vec<String>, lines: &[S], max_lines: usize, expanded: bool) {
    if expanded {
        body.extend(lines.iter().map(|l| diff_line_markup(l.as_ref())));
        return;
    }
    // Lines that must always be shown: change lines (+ / -) and up to 2 context lines
    // before/after each change.
    let n = lines.len();
    let mut pinned = vec![false; n];
    for i in 0..n {
        if !matches!(diff_line_marker(lines[i].as_ref()), Some('-' | '+')) {
            continue;
        }
        pinned[i] = true;
        for p in pinned.iter_mut().take(i).skip(i.saturating_sub(2)) {
            *p = true;
        }
        for p in pinned.iter_mut().take((i + 2).min(n - 1) + 1).skip(i + 1) {
            *p = true;
        }
    }
    // Pinned lines always shown, others count toward the budget. When lines are skipped
    // between shown sections, a blank separator goes in.
    let mut budget = max_lines;
    let mut truncated = 0;
    let mut skipped_since_last_shown = false;
    let mut any_shown = false;
    for i in 0..n {
        if pinned[i] || budget > 0 {
            if skipped_since_last_shown && any_shown {
                body.push(String::new());
            }
            body.push(diff_line_markup(lines[i].as_ref()));
            any_shown = true;
            skipped_since_last_shown = false;
            if !pinned[i] {
                budget -= 1;
            }
        } else {
            skipped_since_last_shown = true;
            truncated += 1;
        }
    }
    if truncated > 0 {
        body.push(more_lines_hint(truncated));
    }
}

fn diff_line_markup(line: &str) -> String {
    let escaped = escape(line);
    match diff_line_marker(line) {
        Some('-') => format!("[#ff9999 on #3d0000]{escaped}[/]"),
        Some('+') => format!("[#99ff99 on #003d00]{escaped}[/]"),
        _ => format!("[dim]{escaped}[/]"),
    }
}

/// The diff line type of `"  {lineNum} - {text}"` / `"  {lineNum} + {text}"`: `-`, `+`, or
/// `None` for context and other lines.
fn diff_line_marker(line: &str) -> Option<char> {
    let trimmed: Vec<char> = line.trim_start().chars().collect();
    if trimmed.first().is_none_or(|c| !c.is_ascii_digit()) {
        return None;
    }
    let mut idx = 0;
    while idx < trimmed.len() && trimmed[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx + 3 <= trimmed.len() && trimmed[idx] == ' ' && trimmed[idx + 2] == ' ' {
        let marker = trimmed[idx + 1];
        if marker == '-' || marker == '+' {
            return Some(marker);
        }
    }
    None
}

// ── Call summaries ───────────────────────────────────────────────────────────

/// `FormatCallSummary`: the one-line argument summary shown after the tool name: the path
/// for file tools, the command for bash, the pattern for search tools.
pub fn format_call_summary(tool_name: &str, tool: Option<&dyn Tool>, args_json: &str) -> String {
    if is_blank(args_json) || args_json == "{}" {
        return String::new();
    }
    let Ok(root) = serde_json::from_str::<Value>(args_json) else {
        return String::new();
    };
    let Some(obj) = root.as_object() else {
        return String::new();
    };
    let mut summary = if is_pi(tool) {
        pi_summary(tool_name, obj)
    } else if let Some(hints) = tool.and_then(|t| t.render_hints()) {
        hints
            .summary_arg_keys
            .iter()
            .map(|k| try_get_string(obj, k))
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        String::new()
    };
    if summary.is_empty() {
        summary = first_string_value(obj);
    }
    one_line(&sanitize(&summary))
}

fn pi_summary(tool_name: &str, root: &Map<String, Value>) -> String {
    match tool_name {
        "read" => {
            let path = try_get_string(root, "path");
            let offset = try_get_number(root, "offset");
            let limit = try_get_number(root, "limit");
            if offset.is_none() && limit.is_none() {
                return path;
            }
            let start = offset.unwrap_or(1.0) as i64;
            match limit {
                Some(l) => format!("{path}:{start}-{}", start + l as i64 - 1),
                None => format!("{path}:{start}-"),
            }
        }
        "bash" => try_get_string(root, "command"),
        "grep" => {
            let pattern = try_get_string(root, "pattern");
            let quoted = if pattern.is_empty() {
                String::new()
            } else {
                format!("\"{pattern}\"")
            };
            join_non_empty(&[quoted, try_get_string(root, "path"), try_get_string(root, "glob")])
        }
        "find" => join_non_empty(&[try_get_string(root, "pattern"), try_get_string(root, "path")]),
        _ => try_get_string(root, "path"),
    }
}

fn join_non_empty(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// First line of a (possibly multi-line) value, with a "(+N lines)" marker.
fn one_line(text: &str) -> String {
    let trimmed = text.trim();
    let Some(nl) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    let extra = trimmed.matches('\n').count();
    format!(
        "{} … (+{extra} line{})",
        trimmed[..nl].trim_end(),
        if extra == 1 { "" } else { "s" }
    )
}

fn try_get_string(root: &Map<String, Value>, name: &str) -> String {
    root.get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn try_get_number(root: &Map<String, Value>, name: &str) -> Option<f64> {
    root.get(name).and_then(Value::as_f64)
}

fn first_string_value(root: &Map<String, Value>) -> String {
    root.iter()
        .find_map(|(k, v)| (k != "_reason").then(|| v.as_str()).flatten())
        .unwrap_or("")
        .to_string()
}

// ── Result helpers ───────────────────────────────────────────────────────────

/// A Pi tool (C# `PiToolBase`): in the Rust port every Pi tool, and only those, has
/// `VERBATIM_OUTPUT`.
fn is_pi(tool: Option<&dyn Tool>) -> bool {
    tool.is_some_and(|t| t.capabilities().contains(ToolCapabilities::VERBATIM_OUTPUT))
}

fn is_pi_tool(tool: Option<&dyn Tool>, name: &str) -> bool {
    is_pi(tool) && tool.is_some_and(|t| t.name() == name)
}

fn is_file_read_tool(tool: Option<&dyn Tool>) -> bool {
    tool.is_some_and(|t| {
        let c = t.capabilities();
        c.contains(ToolCapabilities::READS_FILE_CONTENT)
            && !c.contains(ToolCapabilities::MODIFIES_FILES)
    })
}

/// `SummarizeRead`: "Read 42 lines" — or "Read lines 1-2000 of 5120" when the tool paged.
pub fn summarize_read(display: &str) -> String {
    let lines: Vec<&str> = display.trim_end_matches('\n').split('\n').collect();
    if let Some(m) = lines
        .last()
        .and_then(|l| READ_CONTINUATION_NOTICE.captures(l))
    {
        return format!("Read lines {}-{} of {}", &m[1], &m[2], &m[3]);
    }
    format!(
        "Read {} line{}",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" }
    )
}

fn trim_blank_edges<'a, S: AsRef<str>>(lines: &'a [S]) -> &'a [S] {
    let mut start = 0;
    let mut end = lines.len();
    while start < end && is_blank(lines[start].as_ref()) {
        start += 1;
    }
    while end > start && is_blank(lines[end - 1].as_ref()) {
        end -= 1;
    }
    &lines[start..end]
}

/// Runs of blank lines collapse to one; leading/trailing blanks are dropped.
fn collapse_blank_runs<S: AsRef<str>>(lines: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in trim_blank_edges(lines) {
        let line = line.as_ref();
        if is_blank(line) && out.last().is_some_and(|l| is_blank(l)) {
            continue;
        }
        out.push(line.to_string());
    }
    out
}

fn try_get_write_content(args_json: &str) -> Option<(String, String)> {
    let root: Value = serde_json::from_str(args_json).ok()?;
    let obj = root.as_object()?;
    let path = try_get_string(obj, "path");
    let content = obj.get("content")?.as_str()?;
    (!path.is_empty()).then(|| (path, content.to_string()))
}

/// `ReplaceLineEndings("\n")` for CRLF, CR and the Unicode line breaks .NET recognises.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace(['\r', '\u{85}', '\u{2028}', '\u{2029}', '\u{c}'], "\n")
}

/// A display diff of a Pi edit (`ToolCallRenderer.EditDiff`).
#[derive(Debug, Clone, PartialEq)]
pub struct EditDiff {
    pub path: String,
    pub lines: Vec<String>,
    pub added: usize,
    pub removed: usize,
}

/// `BuildEditDiff`: `"  N - old"` / `"  N + new"` / `"  N   context"` lines for each edit,
/// located by its newText in the edited file. `None` when an edit can't be located.
pub fn build_edit_diff(file_after: &str, edits: &[(String, String)]) -> Option<EditDiff> {
    let file = normalize_line_endings(file_after);
    let file_lines: Vec<&str> = file.split('\n').collect();
    let mut hunks: Vec<(usize, Vec<String>)> = Vec::new();
    let (mut added, mut removed) = (0, 0);

    for (old_raw, new_raw) in edits {
        let old_text = normalize_line_endings(old_raw);
        let new_text = normalize_line_endings(new_raw);
        if new_text.is_empty() {
            return None;
        }
        let idx = file.find(&new_text)?;
        let start_line = 1 + file[..idx].matches('\n').count();

        let old_lines: Vec<&str> = old_text.split('\n').collect();
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        let mut prefix = 0;
        while prefix < old_lines.len()
            && prefix < new_lines.len()
            && old_lines[prefix] == new_lines[prefix]
        {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_lines.len() - prefix
            && suffix < new_lines.len() - prefix
            && old_lines[old_lines.len() - suffix - 1] == new_lines[new_lines.len() - suffix - 1]
        {
            suffix += 1;
        }

        let change_line = start_line + prefix; // first changed line, new numbering
        let width = (start_line + new_lines.len() + 2).to_string().len().max(4);
        let fmt = |n: usize, m: char, t: &str| format!("{n:>width$} {m} {}", sanitize(t));
        let mut lines = Vec::new();

        // Up to 2 lines of context before the change (from the file).
        for n in change_line.saturating_sub(2).max(1)..change_line {
            if n - 1 < file_lines.len() {
                lines.push(fmt(n, ' ', file_lines[n - 1]));
            }
        }
        let old_mid = &old_lines[prefix..old_lines.len() - suffix];
        let new_mid = &new_lines[prefix..new_lines.len() - suffix];
        for (i, l) in old_mid.iter().enumerate() {
            lines.push(fmt(change_line + i, '-', l));
        }
        for (i, l) in new_mid.iter().enumerate() {
            lines.push(fmt(change_line + i, '+', l));
        }
        removed += old_mid.len();
        added += new_mid.len();

        let after_change = change_line + new_mid.len();
        let mut n = after_change;
        while n < after_change + 2 && n - 1 < file_lines.len() {
            if n == file_lines.len() && file_lines[n - 1].is_empty() {
                break; // the empty "line" after a trailing newline
            }
            lines.push(fmt(n, ' ', file_lines[n - 1]));
            n += 1;
        }
        hunks.push((change_line, lines));
    }

    hunks.sort_by_key(|h| h.0);
    let mut all = Vec::new();
    for (_, lines) in hunks {
        if !all.is_empty() {
            all.push("     ⋮".to_string());
        }
        all.extend(lines);
    }
    Some(EditDiff {
        path: String::new(),
        lines: all,
        added,
        removed,
    })
}

#[cfg(test)]
mod tests;
