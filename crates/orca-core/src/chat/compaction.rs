//! Pure compaction logic (`ConversationCompaction.cs`), ported from Pi 0.87.1 core/compaction:
//! the trigger settings, cut-point selection on valid boundaries, split-turn detection,
//! conversation serialization, file-operation tracking and the checkpoint message format.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compaction_prompts::{SUMMARY_PREFIX, SUMMARY_SUFFIX};
use super::conversation::{estimate_message_tokens, is_text_mode_tool_result};
use super::{head16, len16};
use crate::client::{ChatMessage, Role, ToolCall};
use crate::config::ContextConfig;

/// Pi TOOL_RESULT_MAX_CHARS: tool results are cut to this in the summarization input.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Checkpoint prefix written by OpenOrca before Pi-style compaction (still recognised).
pub const LEGACY_SUMMARY_PREFIX: &str = "[Conversation summary]\n";

/// Pi DEFAULT_COMPACTION_SETTINGS.reserveTokens.
pub const PI_RESERVE_TOKENS: i32 = 16384;
/// Pi DEFAULT_COMPACTION_SETTINGS.keepRecentTokens.
pub const PI_KEEP_RECENT_TOKENS: i32 = 20000;

/// A text-mode tool-call parser (assistant text to the calls in it), for the file lists.
pub type TextToolCallParser<'a> = &'a (dyn Fn(&str) -> Vec<ToolCall> + Send + Sync);

/// When and how much to compact (Pi's `shouldCompact`): compact once context tokens exceed
/// `window - reserve_tokens`, keeping roughly `keep_recent_tokens` of the newest messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub context_window: i32,
    pub reserve_tokens: i32,
    pub keep_recent_tokens: i32,
    /// The planned compaction point (`context.compactAtRatio` x window), at most the hard
    /// [`Self::trigger_tokens`]; equal to it when the planned point is off.
    pub planned_trigger_tokens: i32,
}

impl CompactionSettings {
    pub fn new(context_window: i32, reserve_tokens: i32, keep_recent_tokens: i32) -> Self {
        Self {
            context_window,
            reserve_tokens,
            keep_recent_tokens,
            planned_trigger_tokens: context_window - reserve_tokens,
        }
    }

    /// Context size above which compaction fires (the hard limit).
    pub fn trigger_tokens(&self) -> i32 {
        self.context_window - self.reserve_tokens
    }

    pub fn should_compact(&self, context_tokens: i32) -> bool {
        context_tokens > self.trigger_tokens()
    }

    /// Past the planned point: a planned (cost-gated) compaction may run.
    pub fn should_plan_compaction(&self, context_tokens: i32) -> bool {
        context_tokens > self.planned_trigger_tokens
    }

    /// Resolve settings from config. Unset (0) values are derived so Pi's defaults apply on
    /// large windows and scale down on small ones: reserve = min(16384, window x (1 -
    /// autoCompactThreshold)); keep = min(20000, usable / 4). Explicit values are clamped so a
    /// compaction can always make room.
    pub fn from_config(config: &ContextConfig) -> Self {
        let window = if config.context_window_size > 0 {
            config.context_window_size
        } else {
            8192
        };
        let threshold =
            if config.auto_compact_threshold > 0.0 && config.auto_compact_threshold <= 1.0 {
                config.auto_compact_threshold
            } else {
                0.8
            };
        let reserve = if config.reserve_tokens > 0 {
            config.reserve_tokens
        } else {
            PI_RESERVE_TOKENS.min((window as f32 * (1.0 - threshold)) as i32)
        };
        let reserve = reserve.clamp(0, window / 2);

        let usable = window - reserve;
        let keep = if config.keep_recent_tokens > 0 {
            config.keep_recent_tokens
        } else {
            PI_KEEP_RECENT_TOKENS.min(usable / 4)
        };
        let keep = keep.clamp(1, (usable / 2).max(1));

        let hard = window - reserve;
        let ratio = config.compact_at_ratio;
        let mut planned = if ratio > 0.0 && ratio < 1.0 {
            hard.min((window as f32 * ratio) as i32)
        } else {
            hard
        };
        // The kept tail must fit well under the planned point, or every planned compaction
        // would leave the history above it again.
        if planned < hard {
            planned = planned.max(hard.min(keep * 2));
        }
        Self {
            planned_trigger_tokens: planned,
            ..Self::new(window, reserve, keep)
        }
    }
}

/// `context.keepFirstTokens`, at most a tenth of the window (`EffectiveKeepFirstTokens`).
pub fn effective_keep_first_tokens(config: &ContextConfig) -> i32 {
    config.compute_effective_keep_first_tokens()
}

/// What a compaction will summarize and where the verbatim tail starts.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPreparation {
    pub first_kept_index: usize,
    pub is_split_turn: bool,
    pub messages_to_summarize: Vec<ChatMessage>,
    pub turn_prefix_messages: Vec<ChatMessage>,
    pub previous_summary: Option<String>,
    pub file_ops: CompactionFileOps,
    pub head_end: usize,
}

/// Result of the cut-point search (Pi's `findCutPoint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionCutPoint {
    pub first_kept_index: usize,
    /// The user message that starts the turn the cut is in (None when the cut starts a turn
    /// or no turn start was found).
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// Files touched by tool calls in the summarized span (Pi's `FileOperations`), appended to
/// the checkpoint as `<read-files>` / `<modified-files>`. Covers Pi's tools (read/write/edit)
/// and OpenOrca's (read_file/write_file/edit_file/...).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionFileOps {
    pub read: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

impl CompactionFileOps {
    /// Record one call (`args` is its arguments object; anything else is ignored).
    pub fn add_tool_call(&mut self, tool_name: &str, args: &Value) {
        let Some(args) = args.as_object() else {
            return;
        };
        match tool_name {
            "read" | "read_file" => add(&mut self.read, args, &["path", "file_path"]),
            "write" | "write_file" => add(&mut self.written, args, &["path", "file_path"]),
            "copy_file" => add(&mut self.written, args, &["destination"]),
            "edit" | "edit_file" | "delete_file" => {
                add(&mut self.edited, args, &["path", "file_path"])
            }
            "move_file" | "rename_file" => add(
                &mut self.edited,
                args,
                &["source", "destination", "path", "new_path", "old_path"],
            ),
            "multi_edit" => {
                add(&mut self.edited, args, &["path", "file_path"]);
                if let Some(Value::Array(edits)) = args.get("edits") {
                    for e in edits {
                        if let Some(p) = e.get("path").and_then(Value::as_str)
                            && !p.is_empty()
                        {
                            self.edited.insert(p.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Pi `computeFileLists`: read-only files and modified files, both sorted.
    pub fn compute_file_lists(&self) -> (Vec<String>, Vec<String>) {
        let modified: BTreeSet<&String> = self.edited.iter().chain(&self.written).collect();
        let read_only = self
            .read
            .iter()
            .filter(|f| !modified.contains(f))
            .cloned()
            .collect();
        (read_only, modified.into_iter().cloned().collect())
    }

    /// Pi `formatFileOperations`.
    pub fn format(read_files: &[String], modified_files: &[String]) -> String {
        let mut sections = Vec::new();
        if !read_files.is_empty() {
            sections.push(format!(
                "<read-files>\n{}\n</read-files>",
                read_files.join("\n")
            ));
        }
        if !modified_files.is_empty() {
            sections.push(format!(
                "<modified-files>\n{}\n</modified-files>",
                modified_files.join("\n")
            ));
        }
        if sections.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", sections.join("\n\n"))
        }
    }
}

fn add(set: &mut BTreeSet<String>, args: &Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(s) = args.get(*key).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            set.insert(s.to_string());
        }
    }
}

/// The user message that carries a checkpoint (Pi's compactionSummary as a user message).
pub fn create_summary_message(summary: &str) -> ChatMessage {
    ChatMessage::user(format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"))
}

/// The summary text when `message` is a compaction checkpoint.
pub fn try_get_summary(message: &ChatMessage) -> Option<String> {
    if message.role != Role::User {
        return None;
    }
    let text = message.content.as_deref().unwrap_or("");
    if let Some(body) = text.strip_prefix(SUMMARY_PREFIX) {
        return Some(
            body.strip_suffix(SUMMARY_SUFFIX)
                .unwrap_or(body)
                .to_string(),
        );
    }
    text.strip_prefix(LEGACY_SUMMARY_PREFIX).map(str::to_string)
}

/// Text-mode tool results are injected as user messages with a fixed prefix.
pub fn is_text_mode_tool_result_message(message: &ChatMessage) -> bool {
    is_text_mode_tool_result(message)
}

/// Pi `isCutPointMessage`: the kept tail may start at a user or assistant message, never at
/// a tool result (it must stay with its call). Text-mode results count as tool results.
pub fn is_cut_point(message: &ChatMessage) -> bool {
    message.role == Role::Assistant
        || (message.role == Role::User && !is_text_mode_tool_result(message))
}

/// Pi `isTurnStartMessage`: a user message (not a text-mode tool result).
pub fn is_turn_start(message: &ChatMessage) -> bool {
    message.role == Role::User && !is_text_mode_tool_result(message)
}

/// Pi `findCutPoint`: walk back from the newest message accumulating estimated tokens; once
/// the budget is reached cut at the nearest valid cut point at or after that message.
/// `first_kept_index == start_index` when there is nothing to cut.
pub fn find_cut_point(
    messages: &[ChatMessage],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: i32,
    chars_per_token: f32,
) -> CompactionCutPoint {
    let end = end_index.min(messages.len());
    let cut_points: Vec<usize> = (start_index..end)
        .filter(|&i| is_cut_point(&messages[i]))
        .collect();
    let Some(&first) = cut_points.first() else {
        return CompactionCutPoint {
            first_kept_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    };

    let mut accumulated = 0;
    let mut cut_index = first;
    for i in (start_index..end).rev() {
        let tokens = estimate_message_tokens(&messages[i], chars_per_token);
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            // Closest valid cut point at or after this message; if the trailing tool results
            // alone exceed the budget, keep their preceding assistant tool call.
            cut_index = cut_points
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(*cut_points.last().unwrap_or(&first));
            break;
        }
    }

    let starts_turn = is_turn_start(&messages[cut_index]);
    let turn_start = if starts_turn {
        None
    } else {
        (start_index..=cut_index)
            .rev()
            .find(|&i| is_turn_start(&messages[i]))
    };
    CompactionCutPoint {
        first_kept_index: cut_index,
        turn_start_index: turn_start,
        is_split_turn: !starts_turn && turn_start.is_some(),
    }
}

/// Pi `prepareCompaction`. A checkpoint right after the kept head is the previous summary: it
/// is not re-summarized, it feeds the incremental UPDATE prompt and its file lists carry over.
/// None when there is nothing to summarize.
pub fn prepare(
    messages: &[ChatMessage],
    keep_recent_tokens: i32,
    chars_per_token: f32,
    text_tool_call_parser: Option<TextToolCallParser<'_>>,
    keep_first_tokens: i32,
) -> Option<CompactionPreparation> {
    if messages.is_empty() {
        return None;
    }
    let mut previous_summary = None;
    let mut file_ops = CompactionFileOps::default();
    let head_end = head_end(messages, keep_first_tokens, chars_per_token);
    let mut boundary_start = head_end;
    if let Some(prev) = messages.get(head_end).and_then(try_get_summary) {
        previous_summary = Some(extract_file_lists(&prev, &mut file_ops));
        boundary_start = head_end + 1;
    }

    let cut = find_cut_point(
        messages,
        boundary_start,
        messages.len(),
        keep_recent_tokens,
        chars_per_token,
    );
    let history_end = match (cut.is_split_turn, cut.turn_start_index) {
        (true, Some(t)) => t,
        _ => cut.first_kept_index,
    };
    let to_summarize = slice(messages, boundary_start, history_end);
    let turn_prefix = match (cut.is_split_turn, cut.turn_start_index) {
        (true, Some(t)) => slice(messages, t, cut.first_kept_index),
        _ => Vec::new(),
    };
    if to_summarize.is_empty() && turn_prefix.is_empty() {
        return None;
    }
    for m in to_summarize.iter().chain(&turn_prefix) {
        extract_file_ops(m, &mut file_ops, text_tool_call_parser);
    }
    Some(CompactionPreparation {
        first_kept_index: cut.first_kept_index,
        is_split_turn: cut.is_split_turn,
        messages_to_summarize: to_summarize,
        turn_prefix_messages: turn_prefix,
        previous_summary,
        file_ops,
        head_end,
    })
}

/// Index of the first compaction checkpoint in `messages`.
pub fn summary_index(messages: &[ChatMessage]) -> Option<usize> {
    messages.iter().position(|m| try_get_summary(m).is_some())
}

/// End (exclusive) of the head every compaction keeps verbatim: the task (first user turn)
/// and the messages after it up to `keep_first_tokens` estimated tokens in all, ending before
/// a cut point. Once a checkpoint exists the head is what precedes it (fixed at the first
/// compaction). 0 tokens = no head (index 0).
pub fn head_end(messages: &[ChatMessage], keep_first_tokens: i32, chars_per_token: f32) -> usize {
    if let Some(s) = summary_index(messages) {
        return s;
    }
    if keep_first_tokens <= 0 {
        return 0;
    }
    let Some(task) = messages.iter().position(is_turn_start) else {
        return 0;
    };
    let mut end = task + 1;
    let mut tokens = 0.0f64;
    for m in &messages[..=task] {
        tokens += f64::from(estimate_message_tokens(m, chars_per_token));
    }
    let limit = f64::from(keep_first_tokens);
    for (i, m) in messages.iter().enumerate().skip(task + 1) {
        if is_cut_point(m) && tokens <= limit {
            end = i;
        }
        tokens += f64::from(estimate_message_tokens(m, chars_per_token));
        if tokens > limit {
            break;
        }
    }
    end
}

/// Pi `extractFileOpsFromMessage`, plus text-mode tool calls when a parser is given.
pub fn extract_file_ops(
    message: &ChatMessage,
    file_ops: &mut CompactionFileOps,
    text_tool_call_parser: Option<TextToolCallParser<'_>>,
) {
    if message.role != Role::Assistant {
        return;
    }
    for c in &message.tool_calls {
        file_ops.add_tool_call(&c.name, &parse_args(&c.arguments));
    }
    if message.tool_calls.is_empty()
        && let (Some(parser), Some(text)) = (text_tool_call_parser, message.content.as_deref())
        && !text.is_empty()
    {
        for c in parser(text) {
            file_ops.add_tool_call(&c.name, &parse_args(&c.arguments));
        }
    }
}

fn parse_args(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or(Value::Null)
}

/// Pi `serializeConversation`: flatten messages to plain text so the summarizer reads a
/// transcript instead of continuing a conversation. Tool results are cut to 2000 chars.
pub fn serialize_conversation<'a>(messages: impl IntoIterator<Item = &'a ChatMessage>) -> String {
    let mut parts = Vec::new();
    for m in messages {
        let text = m.content.as_deref().unwrap_or("");
        match m.role {
            Role::User => {
                if text.is_empty() {
                    continue;
                }
                parts.push(if is_text_mode_tool_result(m) {
                    format!(
                        "[Tool result]: {}",
                        truncate_for_summary(text, TOOL_RESULT_MAX_CHARS)
                    )
                } else {
                    format!("[User]: {text}")
                });
            }
            Role::Assistant => {
                if let Some(r) = m.reasoning.as_deref().filter(|r| !r.is_empty()) {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        truncate_for_summary(r, TOOL_RESULT_MAX_CHARS)
                    ));
                }
                // Only non-empty text counts (a tool-call turn has none).
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !m.tool_calls.is_empty() {
                    let calls: Vec<String> = m
                        .tool_calls
                        .iter()
                        .map(|c| format!("{}({})", c.name, format_args(&c.arguments)))
                        .collect();
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Role::Tool => {
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            Role::System => {}
        }
    }
    parts.join("\n\n")
}

/// Pi `truncateForSummary`.
pub fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let len = len16(text);
    if len <= max_chars {
        return text.to_string();
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        head16(text, max_chars),
        len - max_chars
    )
}

static READ_FILES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<read-files>\s*\n(.*?)</read-files>\s*").unwrap());
static MODIFIED_FILES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<modified-files>\s*\n(.*?)</modified-files>\s*").unwrap());

/// Pull `<read-files>`/`<modified-files>` out of a previous summary into `file_ops` and return
/// the summary without them, so the lists are re-appended once, never duplicated.
pub fn extract_file_lists(summary: &str, file_ops: &mut CompactionFileOps) -> String {
    for c in READ_FILES.captures_iter(summary) {
        file_ops.read.extend(split_lines(&c[1]));
    }
    for c in MODIFIED_FILES.captures_iter(summary) {
        file_ops.edited.extend(split_lines(&c[1]));
    }
    strip_file_lists(summary)
}

/// Remove any file-list blocks (models sometimes echo them back).
pub fn strip_file_lists(summary: &str) -> String {
    let s = READ_FILES.replace_all(summary, "");
    MODIFIED_FILES.replace_all(&s, "").trim_end().to_string()
}

fn split_lines(block: &str) -> impl Iterator<Item = String> + '_ {
    block
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
}

fn slice(messages: &[ChatMessage], start: usize, end: usize) -> Vec<ChatMessage> {
    let end = end.min(messages.len());
    if start >= end {
        return Vec::new();
    }
    messages[start..end].to_vec()
}

/// `name=value` pairs, values as compact JSON (relaxed escaping).
fn format_args(arguments: &str) -> String {
    let Ok(Value::Object(args)) = serde_json::from_str::<Value>(arguments) else {
        return String::new();
    };
    args.iter()
        .map(|(k, v)| format!("{k}={}", serde_json::to_string(v).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(", ")
}
