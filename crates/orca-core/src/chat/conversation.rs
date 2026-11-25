//! `Conversation`: the history plus what compaction needs: token estimates based on the
//! server-reported prompt size, a rewrite counter (the prompt-cache prefix is stable between
//! rewrites) and the deterministic shrinking passes (pruning, thinking-aware compaction,
//! checkpoint insertion).
//!
//! The C# message model has content parts; the Rust [`ChatMessage`] maps them as: text =
//! `content`, reasoning = `reasoning`, function calls = `tool_calls` (arguments as JSON text),
//! a function result = a `Role::Tool` message's `content`. Images (`DataContent`) and a
//! provider's raw message form do not exist in the Rust model, so the image-dropping part of
//! `CompactLight` has nothing to do.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde_json::{Map, Value};

use super::compaction;
use super::{cpt_or_default, head16, len16, n0, tail16};
use crate::client::{ChatMessage, Role, Usage};
use crate::config::dotnet_json;

/// Prefix of the user message that carries text-mode tool results.
pub const TEXT_TOOL_RESULTS_PREFIX: &str = "Here are the results of the tool calls";

/// Smallest estimated prompt a calibration is taken from (small prompts are mostly fixed
/// overhead: tool schemas).
pub const MIN_CALIBRATION_TOKENS: i32 = 8000;

/// Tool results longer than this are cut to a head by a thinking-aware compaction.
pub const LIGHT_RESULT_MAX_CHARS: usize = 1500;
/// Characters of a cut tool result that a thinking-aware compaction keeps.
pub const LIGHT_RESULT_HEAD_CHARS: usize = 400;
/// Write/edit call arguments longer than this lose their text to a thinking-aware compaction.
pub const LIGHT_ARG_MAX_CHARS: usize = 800;
/// Note left in place of a tool result's image by a thinking-aware compaction (kept for
/// parity; the Rust message model has no images yet).
pub const IMAGE_DROPPED_NOTE: &str =
    "[screenshot dropped at compaction; take a new one if you need to see the window]";

/// The usage bookkeeping of a conversation, separable from its messages so an
/// [`crate::agent::AgentSession`] (which keeps its history as a plain `Vec<ChatMessage>`,
/// system prompt first) can carry it between compaction checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsage {
    /// Server-reported prompt size for the last request, and how many messages it covered.
    usage_tokens: Option<i32>,
    usage_message_count: usize,
    usage_rewrite_count: u64,
    /// Fingerprint of the system prompt and the covered messages when the usage was taken:
    /// a history changed in place by code that does not bump the rewrite counter still
    /// invalidates the usage base.
    usage_fingerprint: u64,
    calibration: f64,
    rewrite_count: u64,
}

impl Default for ContextUsage {
    fn default() -> Self {
        Self {
            usage_tokens: None,
            usage_message_count: 0,
            usage_rewrite_count: 0,
            usage_fingerprint: 0,
            calibration: 1.0,
            rewrite_count: 0,
        }
    }
}

impl ContextUsage {
    /// See [`Conversation::rewrite_count`].
    pub fn rewrite_count(&self) -> u64 {
        self.rewrite_count
    }
}

/// What a request covered, taken right before it is sent ([`Conversation::mark`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMark {
    /// Messages sent after the system prompt.
    pub message_count: usize,
    /// [`Conversation::rewrite_count`] when the request was built.
    pub rewrite_count: u64,
    /// Fingerprint of what the request carried (catches in-place changes made since).
    pub fingerprint: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conversation {
    system_prompt: Option<String>,
    messages: Vec<ChatMessage>,
    usage: ContextUsage,
    /// The bridge engine that ran this conversation's turns ("claude", "codex"; None = native).
    pub engine_id: Option<String>,
    /// A bridge CLI's own session id.
    pub engine_session_id: Option<String>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// A conversation over an API-shaped history (system prompt first, if any) and the usage
    /// bookkeeping carried from earlier checks. A usage base whose messages changed in place
    /// since it was taken is dropped (counted as a rewrite).
    pub fn from_api_messages(mut messages: Vec<ChatMessage>, usage: ContextUsage) -> Self {
        let system_prompt = match messages.first() {
            Some(m) if m.role == Role::System => messages.remove(0).content,
            _ => None,
        };
        let mut c = Self {
            system_prompt,
            messages,
            usage,
            engine_id: None,
            engine_session_id: None,
        };
        if c.usage.usage_tokens.is_some()
            && c.usage.usage_rewrite_count == c.usage.rewrite_count
            && (c.usage.usage_message_count > c.messages.len()
                || c.fingerprint(c.usage.usage_message_count) != c.usage.usage_fingerprint)
        {
            c.usage.rewrite_count += 1;
        }
        c
    }

    /// Back to the API shape (system prompt first) and the usage bookkeeping.
    pub fn into_api_messages(self) -> (Vec<ChatMessage>, ContextUsage) {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if let Some(s) = self.system_prompt.filter(|s| !s.is_empty()) {
            out.push(ChatMessage::system(s));
        }
        out.extend(self.messages);
        (out, self.usage)
    }

    pub fn usage(&self) -> &ContextUsage {
        &self.usage
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// Setting a different system prompt is a rewrite.
    pub fn set_system_prompt(&mut self, value: Option<String>) {
        if self.system_prompt != value {
            self.usage.rewrite_count += 1;
        }
        self.system_prompt = value;
    }

    /// Bumped whenever anything other than an append changes what the model would see
    /// (compaction, pruning, undo, clear, a new system prompt). Between bumps the request
    /// prefix is byte-stable, so a server-side prompt cache stays warm.
    pub fn rewrite_count(&self) -> u64 {
        self.usage.rewrite_count
    }

    /// Count a change made through [`Self::messages_mut`] that was not an append.
    pub fn note_rewrite(&mut self) {
        self.usage.rewrite_count += 1;
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Direct access. Anything but appending must be followed by [`Self::note_rewrite`].
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    pub fn add_system_message(&mut self, content: impl Into<String>) {
        self.set_system_prompt(Some(content.into()));
    }

    pub fn add_user_message(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::user(content));
    }

    pub fn add_assistant_message(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::assistant(content));
    }

    pub fn add_message(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    pub fn add_messages(&mut self, messages: impl IntoIterator<Item = ChatMessage>) {
        self.messages.extend(messages);
    }

    /// The request's messages: the system prompt (when set) first.
    pub fn get_messages_for_api(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if let Some(s) = self.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            out.push(ChatMessage::system(s));
        }
        out.extend(self.messages.iter().cloned());
        out
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.system_prompt = None;
        self.engine_id = None;
        self.engine_session_id = None;
        self.usage.rewrite_count += 1;
    }

    /// What the next request will cover; pass it to [`Self::record_prompt_usage`] with the
    /// server's count afterwards.
    pub fn mark(&self) -> ContextMark {
        ContextMark {
            message_count: self.messages.len(),
            rewrite_count: self.usage.rewrite_count,
            fingerprint: self.fingerprint(self.messages.len()),
        }
    }

    /// [`Self::record_prompt_usage`] for the request taken at `mark`; ignored when what it
    /// carried has changed since (rewritten, or edited in place).
    pub fn record_prompt_usage_at(&mut self, prompt_tokens: i32, mark: ContextMark) {
        if mark.message_count > self.messages.len()
            || self.fingerprint(mark.message_count) != mark.fingerprint
        {
            return;
        }
        self.record_prompt_usage(prompt_tokens, mark.message_count, Some(mark.rewrite_count));
    }

    /// Real prompt tokens per estimated token, from the last server-reported prompt of at
    /// least [`MIN_CALIBRATION_TOKENS`] estimated tokens (1 until then; clamped to 0.5..4).
    /// Estimates of a rewritten history (no usage base) and compaction plans are scaled by it.
    pub fn estimate_calibration(&self) -> f64 {
        self.usage.calibration
    }

    /// Record the server's prompt-token count for a request that carried the system prompt
    /// plus the first `message_count` messages. Later estimates use it as the base and only
    /// estimate what was appended since (Pi's estimateContextTokens). Usage for a history
    /// rewritten since `rewrite_count_at_request` is ignored.
    pub fn record_prompt_usage(
        &mut self,
        prompt_tokens: i32,
        message_count: usize,
        rewrite_count_at_request: Option<u64>,
    ) {
        if prompt_tokens <= 0 || message_count > self.messages.len() {
            return;
        }
        if rewrite_count_at_request.is_some_and(|rc| rc != self.usage.rewrite_count) {
            return;
        }
        self.usage.usage_tokens = Some(prompt_tokens);
        self.usage.usage_message_count = message_count;
        self.usage.usage_rewrite_count = self.usage.rewrite_count;
        self.usage.usage_fingerprint = self.fingerprint(message_count);
        let estimated = self.estimate_prefix_tokens(message_count, 3.5);
        if estimated >= f64::from(MIN_CALIBRATION_TOKENS) {
            self.usage.calibration = (f64::from(prompt_tokens) / estimated).clamp(0.5, 4.0);
        }
    }

    /// The whole prompt a request carried, from its usage: input tokens counting cache reads.
    /// Servers that report the prompt as the uncached part only (cache reads larger than it)
    /// get the cache count added back. None without usage.
    pub fn prompt_tokens_of(usage: &Usage) -> Option<i32> {
        let mut input = usage.prompt_tokens?;
        let cached = usage.cached_tokens.unwrap_or(0);
        if cached > input {
            input += cached + usage.cache_creation_tokens.unwrap_or(0);
        }
        (input > 0).then(|| input.min(i64::from(i32::MAX)) as i32)
    }

    fn fingerprint(&self, message_count: usize) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.system_prompt.hash(&mut h);
        for m in self.messages.iter().take(message_count) {
            m.role.as_str().hash(&mut h);
            m.content.hash(&mut h);
            m.reasoning.hash(&mut h);
            m.tool_call_id.hash(&mut h);
            for c in &m.tool_calls {
                c.id.hash(&mut h);
                c.name.hash(&mut h);
                c.arguments.hash(&mut h);
            }
        }
        h.finish()
    }

    /// Char-based estimate of the system prompt plus the first `message_count` messages.
    fn estimate_prefix_tokens(&self, message_count: usize, chars_per_token: f32) -> f64 {
        let mut total = self.system_len() as f64 / f64::from(chars_per_token);
        for m in self.messages.iter().take(message_count) {
            total += estimate_message_tokens_exact(m, chars_per_token);
        }
        total
    }

    fn system_len(&self) -> usize {
        self.system_prompt.as_deref().map(len16).unwrap_or(0)
    }

    /// Calibrated estimate of the system prompt plus messages `[0, end_index)`: the prefix a
    /// rewrite after it keeps.
    pub fn estimate_prefix_tokens_calibrated(&self, end_index: usize, chars_per_token: f32) -> i32 {
        let cpt = cpt_or_default(chars_per_token);
        (self.estimate_prefix_tokens(end_index, cpt) * self.usage.calibration) as i32
    }

    /// Calibrated estimate of what a request would carry if only the system prompt and the
    /// messages from `from_index` on were kept.
    pub fn estimate_tail_tokens(&self, from_index: usize, chars_per_token: f32) -> i32 {
        let cpt = cpt_or_default(chars_per_token);
        let mut total = self.system_len() as f64 / f64::from(cpt);
        for m in self.messages.iter().skip(from_index) {
            total += estimate_message_tokens_exact(m, cpt);
        }
        (total * self.usage.calibration) as i32
    }

    /// Whether [`Self::estimate_context_tokens`] currently has a usage base.
    pub fn has_usage_base(&self) -> bool {
        self.usage.usage_tokens.is_some()
            && self.usage.usage_rewrite_count == self.usage.rewrite_count
            && self.usage.usage_message_count <= self.messages.len()
    }

    /// Best estimate of the next request's prompt size: the last server-reported usage plus
    /// a chars-per-token estimate of the messages appended since. Falls back to a full
    /// (calibrated) estimate when there is no usage or the history was rewritten after it.
    pub fn estimate_context_tokens(&self, chars_per_token: f32) -> i32 {
        if !self.has_usage_base() {
            return (f64::from(self.estimate_token_count(chars_per_token)) * self.usage.calibration)
                as i32;
        }
        let trailing: i32 = self.messages[self.usage.usage_message_count..]
            .iter()
            .map(|m| estimate_message_tokens(m, chars_per_token))
            .sum();
        self.usage.usage_tokens.unwrap_or(0) + trailing
    }

    /// Estimate with a chars-per-token ratio. Tool call/result content uses 75% of it (JSON
    /// tokenizes less efficiently than prose).
    pub fn estimate_token_count(&self, chars_per_token: f32) -> i32 {
        let cpt = cpt_or_default(chars_per_token);
        self.estimate_prefix_tokens(self.messages.len(), cpt) as i32
    }

    /// Removes the oldest messages (keeping at least two) to fit within `max_tokens`. True when
    /// the conversation fits afterwards.
    pub fn truncate_to_fit(&mut self, max_tokens: i32) -> bool {
        while self.messages.len() > 2 && self.estimate_token_count(3.5) > max_tokens {
            self.messages.remove(0);
            self.usage.rewrite_count += 1;
        }
        self.estimate_token_count(3.5) <= max_tokens
    }

    /// Deterministically shrink tool results older than the last `keep_last_n_turns` user
    /// turns. Returns the characters reclaimed.
    pub fn prune_old_tool_results(
        &mut self,
        keep_last_n_turns: i32,
        stub_max_chars: usize,
    ) -> usize {
        let end = self.find_preserve_start_index(keep_last_n_turns);
        self.prune_tool_results_before(end, stub_max_chars)
    }

    /// Stub every tool result (native or text-mode) longer than `stub_max_chars` in messages
    /// before `end_index`, and drop those messages' reasoning. Returns the characters reclaimed.
    pub fn prune_tool_results_before(&mut self, end_index: usize, stub_max_chars: usize) -> usize {
        let end = end_index.min(self.messages.len());
        let mut reclaimed = 0;
        for m in &mut self.messages[..end] {
            if let Some(r) = m.reasoning.take() {
                reclaimed += len16(&r);
            }
            m.reasoning_signature = None;
        }
        for m in &mut self.messages[..end] {
            reclaimed += shrink_tool_results(m, stub_max_chars, |text, text_mode| {
                let len = len16(text);
                if text_mode {
                    format!(
                        "[tool results pruned — {} chars from an earlier step. Head: {}…]",
                        n0(len as i64),
                        head16(text, 160)
                    )
                } else {
                    format!(
                        "[tool result pruned — {} chars. Head: {}…]",
                        n0(len as i64),
                        head16(text, 120)
                    )
                }
            });
        }
        if reclaimed > 0 {
            self.usage.rewrite_count += 1;
        }
        reclaimed
    }

    /// Cut every tool result from `start_index` on to a head + tail of about `max_chars`. A
    /// last-resort squeeze, only during a compaction event.
    pub fn truncate_tool_results_from(&mut self, start_index: usize, max_chars: usize) -> usize {
        let max_chars = max_chars.max(200);
        let mut reclaimed = 0;
        let start = start_index.min(self.messages.len());
        for m in &mut self.messages[start..] {
            reclaimed += shrink_tool_results(m, max_chars, |text, _| {
                let head = max_chars * 2 / 5;
                let tail = max_chars - head;
                format!(
                    "{}\n[... {} chars truncated to fit the context window ...]\n{}",
                    head16(text, head),
                    n0((len16(text) - head - tail) as i64),
                    tail16(text, tail)
                )
            });
        }
        if reclaimed > 0 {
            self.usage.rewrite_count += 1;
        }
        reclaimed
    }

    /// Thinking-aware compaction of the messages in `[start_index, end_index)`, with no model
    /// call: their reasoning goes, tool results over [`LIGHT_RESULT_MAX_CHARS`] keep a head and
    /// a note, and long string arguments of tool calls (a write's file text, an edit's old/new
    /// text) keep a head and a note. User turns, assistant text and the call names, ids and
    /// paths stay. Returns the estimated tokens reclaimed; with `dry_run` nothing changes.
    pub fn compact_light(
        &mut self,
        end_index: usize,
        chars_per_token: f32,
        dry_run: bool,
        start_index: usize,
    ) -> i32 {
        if dry_run {
            return self.compact_light_estimate(end_index, chars_per_token, start_index);
        }
        let cpt = cpt_or_default(chars_per_token);
        let end = end_index.min(self.messages.len());
        let start = start_index.min(end);
        let mut tokens = 0.0;
        let mut changed = false;
        for m in &mut self.messages[start..end] {
            let (t, c) = light_pass(m, cpt, true);
            tokens += t;
            changed |= c;
        }
        if changed {
            self.usage.rewrite_count += 1;
        }
        tokens as i32
    }

    /// [`Self::compact_light`] as a dry run (the estimate for planning), on a shared borrow.
    pub fn compact_light_estimate(
        &self,
        end_index: usize,
        chars_per_token: f32,
        start_index: usize,
    ) -> i32 {
        let cpt = cpt_or_default(chars_per_token);
        let end = end_index.min(self.messages.len());
        let start = start_index.min(end);
        let mut tokens = 0.0;
        for m in &self.messages[start..end] {
            let mut copy = m.clone();
            tokens += light_pass(&mut copy, cpt, false).0;
        }
        tokens as i32
    }

    /// Messages that would be summarized during compaction (all but the last N user turns).
    pub fn get_messages_for_compaction(&self, preserve_last_n: i32) -> Vec<ChatMessage> {
        if preserve_last_n >= 0 && preserve_last_n as usize >= self.messages.len() {
            return Vec::new();
        }
        let start = self.find_preserve_start_index(preserve_last_n);
        self.messages[..start].to_vec()
    }

    /// Replace old messages with a legacy summary message, preserving the last N turns.
    /// Returns the number of messages removed.
    pub fn compact_with_summary(&mut self, summary: &str, preserve_last_n: i32) -> usize {
        let start = self.find_preserve_start_index(preserve_last_n);
        if start == 0 {
            return 0;
        }
        self.messages.drain(..start);
        self.messages.insert(
            0,
            ChatMessage::user(format!("{}{summary}", compaction::LEGACY_SUMMARY_PREFIX)),
        );
        self.usage.rewrite_count += 1;
        start
    }

    /// Replace `[head_end, first_kept_index)` (including any previous checkpoint right after
    /// the kept head) with a Pi-format checkpoint message. Returns the messages removed.
    pub fn apply_compaction(
        &mut self,
        summary: &str,
        first_kept_index: usize,
        head_end: usize,
    ) -> usize {
        let first_kept = first_kept_index.min(self.messages.len());
        let head = head_end.min(first_kept);
        self.messages.drain(head..first_kept);
        self.messages
            .insert(head, compaction::create_summary_message(summary));
        self.usage.rewrite_count += 1;
        first_kept - head
    }

    /// Remove the last N turns (a user message and everything after it). Returns the
    /// messages removed.
    pub fn remove_last_turns(&mut self, turn_count: usize) -> usize {
        let mut removed = 0;
        for _ in 0..turn_count {
            if self.messages.is_empty() {
                break;
            }
            while self.messages.last().is_some_and(|m| m.role != Role::User) {
                self.messages.pop();
                removed += 1;
            }
            if self.messages.last().is_some_and(|m| m.role == Role::User) {
                self.messages.pop();
                removed += 1;
            }
        }
        if removed > 0 {
            self.usage.rewrite_count += 1;
        }
        removed
    }

    /// Message counts by role ("user", "assistant", "tool", "system").
    pub fn get_message_count_by_role(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for m in &self.messages {
            *counts.entry(m.role.as_str()).or_insert(0) += 1;
        }
        counts
    }

    fn find_preserve_start_index(&self, preserve_last_n: i32) -> usize {
        if preserve_last_n <= 0 {
            return self.messages.len();
        }
        let mut users = 0;
        for i in (0..self.messages.len()).rev() {
            if self.messages[i].role == Role::User {
                users += 1;
                if users >= preserve_last_n {
                    return i;
                }
            }
        }
        0
    }
}

/// Estimated tokens for one message (rounded up).
pub fn estimate_message_tokens(message: &ChatMessage, chars_per_token: f32) -> i32 {
    let cpt = cpt_or_default(chars_per_token);
    estimate_message_tokens_exact(message, cpt).ceil() as i32
}

fn estimate_message_tokens_exact(message: &ChatMessage, chars_per_token: f32) -> f64 {
    let tool_cpt = f64::from(chars_per_token) * 0.75;
    let mut text = 0usize;
    let mut tool = 0usize;
    let content = message.content.as_deref().map(len16).unwrap_or(0);
    if message.role == Role::Tool {
        tool += content;
    } else {
        text += content;
    }
    text += message.reasoning.as_deref().map(len16).unwrap_or(0);
    for c in &message.tool_calls {
        tool += len16(&c.name);
        tool += arguments_json_len(&c.arguments);
    }
    text as f64 / f64::from(chars_per_token) + tool as f64 / tool_cpt
}

/// Length of the arguments as C# serializes the parsed dictionary (`JsonDefaults`: indented,
/// default encoder). Arguments that are not a JSON object count nothing (null in C#).
fn arguments_json_len(arguments: &str) -> usize {
    match serde_json::from_str::<Value>(arguments) {
        Ok(v @ Value::Object(_)) => dotnet_json::to_string_dotnet(&v)
            .map(|s| len16(&s))
            .unwrap_or(0),
        _ => 0,
    }
}

/// Whether a user message carries text-mode tool results.
pub(crate) fn is_text_mode_tool_result(message: &ChatMessage) -> bool {
    message.role == Role::User
        && message
            .content
            .as_deref()
            .is_some_and(|t| t.starts_with(TEXT_TOOL_RESULTS_PREFIX))
}

fn shrink_tool_results(
    m: &mut ChatMessage,
    max_chars: usize,
    shrink: impl Fn(&str, bool) -> String,
) -> usize {
    let text_mode = match m.role {
        Role::Tool => false,
        Role::User if is_text_mode_tool_result(m) => true,
        _ => return 0,
    };
    let Some(text) = m.content.as_deref() else {
        return 0;
    };
    let len = len16(text);
    if len <= max_chars {
        return 0;
    }
    let stub = shrink(text, text_mode);
    let stub_len = len16(&stub);
    if stub_len >= len {
        return 0;
    }
    m.content = Some(stub);
    len - stub_len
}

fn light_stub(text: &str) -> String {
    format!(
        "{}\n[... {} more chars dropped at compaction; run or read it again if you need them]",
        head16(text, LIGHT_RESULT_HEAD_CHARS),
        n0((len16(text) - LIGHT_RESULT_HEAD_CHARS) as i64)
    )
}

/// The arguments with every long string value cut to a head, or None when none is long.
fn shrink_arguments(args: &Map<String, Value>) -> Option<(Map<String, Value>, i64)> {
    let mut saved = 0i64;
    let mut result: Option<Map<String, Value>> = None;
    for (key, value) in args {
        let Some(text) = value.as_str() else {
            continue;
        };
        let len = len16(text);
        if len <= LIGHT_ARG_MAX_CHARS {
            continue;
        }
        let replacement = format!(
            "{}\n[... {} more chars of this argument dropped at compaction; the file on disk has the current text]",
            head16(text, 200),
            n0((len - 200) as i64)
        );
        saved += len as i64 - len16(&replacement) as i64;
        result.get_or_insert_with(|| args.clone())[key] = Value::String(replacement);
    }
    result.map(|r| (r, saved))
}

/// One message of a thinking-aware compaction: the tokens it reclaims and whether anything
/// changes. Applies the change when `apply`.
fn light_pass(m: &mut ChatMessage, cpt: f32, apply: bool) -> (f64, bool) {
    let tool_cpt = f64::from(cpt) * 0.75;
    let mut tokens = 0.0;
    let mut changed = false;
    // C# walks the content parts last to first: calls, then text, then reasoning.
    if m.role == Role::Assistant {
        for i in (0..m.tool_calls.len()).rev() {
            let Ok(Value::Object(args)) = serde_json::from_str::<Value>(&m.tool_calls[i].arguments)
            else {
                continue;
            };
            if args.is_empty() {
                continue;
            }
            if let Some((shrunk, saved)) = shrink_arguments(&args) {
                tokens += saved as f64 / tool_cpt;
                if apply {
                    m.tool_calls[i].arguments =
                        serde_json::to_string(&Value::Object(shrunk)).unwrap_or_default();
                }
                changed = true;
            }
        }
    }
    let content = m.content.as_deref().unwrap_or("");
    let cut_result = match m.role {
        Role::Tool => len16(content) > LIGHT_RESULT_MAX_CHARS,
        Role::User => is_text_mode_tool_result(m) && len16(content) > LIGHT_RESULT_MAX_CHARS,
        _ => false,
    };
    if cut_result {
        let stub = light_stub(content);
        tokens += (len16(content) - len16(&stub)) as f64 / tool_cpt;
        if apply {
            m.content = Some(stub);
        }
        changed = true;
    }
    if m.role == Role::Assistant
        && let Some(r) = m.reasoning.as_deref().filter(|r| !r.is_empty())
    {
        tokens += len16(r) as f64 / f64::from(cpt);
        if apply {
            m.reasoning = None;
            m.reasoning_signature = None;
        }
        changed = true;
    }
    (tokens, changed)
}
