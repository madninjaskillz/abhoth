//! Stream-time display filters for the REPL (`ToolCallTagFilter.cs`, `NextPromptTagFilter.cs`).
//! The agent loop hands the observer raw text; these keep `<tool_call>` blocks and the
//! `<next-prompt>` suggestion off the screen. The `<think>` filter is
//! `orca_core::agent::think_tag_filter::ThinkTagFilter` (ported there with its tests).
//! Tags match ASCII case-insensitively (C# `OrdinalIgnoreCase`).

use orca_core::agent::llm_tags::{
    NEXT_PROMPT_CLOSE, NEXT_PROMPT_OPEN, TOOL_CALL_CLOSE, TOOL_CALL_OPEN, TOOL_CALL_PIPE_CLOSE,
    TOOL_CALL_PIPE_OPEN,
};

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn find_ci(s: &str, needle: &str) -> Option<usize> {
    s.to_ascii_lowercase().find(&needle.to_ascii_lowercase())
}

/// Byte index where the last `n` chars of `s` start.
fn tail_start(s: &str, n: usize) -> usize {
    s.char_indices()
        .rev()
        .nth(n.saturating_sub(1))
        .map_or(0, |(i, _)| i)
}

/// The text after the first char (C# `IndexOf('<', 1)` searches from index 1).
fn next_lt(s: &str) -> Option<usize> {
    let first = s.chars().next()?.len_utf8();
    s[first..].find('<').map(|i| i + first)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Normal,
    Buffering,
    Inside,
}

/// Suppresses `<tool_call>…</tool_call>` and `<|tool_call|>…<|/tool_call|>` from visible output.
#[derive(Debug, Default)]
pub struct ToolCallTagFilter {
    state: State,
    buffer: String,
    active_tag: usize,
}

const OPEN_TAGS: [&str; 2] = [TOOL_CALL_OPEN, TOOL_CALL_PIPE_OPEN];
const CLOSE_TAGS: [&str; 2] = [TOOL_CALL_CLOSE, TOOL_CALL_PIPE_CLOSE];

impl ToolCallTagFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The part of `token` to display.
    pub fn filter(&mut self, token: &str) -> String {
        if token.is_empty() {
            return String::new();
        }
        match self.state {
            State::Normal => self.process_normal(token),
            State::Buffering => {
                self.buffer.push_str(token);
                self.resolve_buffer()
            }
            State::Inside => self.process_suppressing(token),
        }
    }

    fn process_normal(&mut self, token: &str) -> String {
        let Some(lt) = token.find('<') else {
            return token.to_string();
        };
        self.buffer = token[lt..].to_string();
        self.state = State::Buffering;
        format!("{}{}", &token[..lt], self.resolve_buffer())
    }

    fn resolve_buffer(&mut self) -> String {
        for (i, open) in OPEN_TAGS.iter().enumerate() {
            if starts_with_ci(&self.buffer, open) {
                self.state = State::Inside;
                self.active_tag = i;
                let remainder = self.buffer[open.len()..].to_string();
                self.buffer.clear();
                return self.process_suppressing(&remainder);
            }
            if starts_with_ci(open, &self.buffer) {
                return String::new();
            }
        }
        // No match: flush the buffer as visible text, re-scanning for more '<'.
        self.state = State::Normal;
        let flushed = std::mem::take(&mut self.buffer);
        if let Some(n) = next_lt(&flushed) {
            self.buffer = flushed[n..].to_string();
            self.state = State::Buffering;
            return format!("{}{}", &flushed[..n], self.resolve_buffer());
        }
        flushed
    }

    fn process_suppressing(&mut self, token: &str) -> String {
        self.buffer.push_str(token);
        let close = CLOSE_TAGS[self.active_tag];
        if let Some(idx) = find_ci(&self.buffer, close) {
            let remainder = self.buffer[idx + close.len()..].to_string();
            self.buffer.clear();
            self.state = State::Normal;
            return if remainder.is_empty() {
                String::new()
            } else {
                self.process_normal(&remainder)
            };
        }
        // Keep enough trailing chars to detect a close tag split across tokens.
        if self.buffer.chars().count() > close.len() * 2 {
            self.buffer = self.buffer[tail_start(&self.buffer, close.len())..].to_string();
        }
        String::new()
    }
}

/// Captures `<next-prompt>…</next-prompt>` (the suggested next prompt) and keeps it off screen.
#[derive(Debug, Default)]
pub struct NextPromptTagFilter {
    state: State,
    buffer: String,
    captured: String,
}

impl NextPromptTagFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The captured suggestion (trimmed), or `None` when there was none.
    pub fn captured_prompt(&self) -> Option<String> {
        let t = self.captured.trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    /// The part of `token` to display.
    pub fn filter(&mut self, token: &str) -> String {
        if token.is_empty() {
            return String::new();
        }
        match self.state {
            State::Normal => self.process_normal(token),
            State::Buffering => {
                self.buffer.push_str(token);
                self.resolve_buffer()
            }
            State::Inside => self.process_inside(token),
        }
    }

    fn process_normal(&mut self, token: &str) -> String {
        let Some(lt) = token.find('<') else {
            return token.to_string();
        };
        self.buffer = token[lt..].to_string();
        self.state = State::Buffering;
        format!("{}{}", &token[..lt], self.resolve_buffer())
    }

    fn resolve_buffer(&mut self) -> String {
        if starts_with_ci(&self.buffer, NEXT_PROMPT_OPEN) {
            // Full match: capture (a later tag replaces an earlier one).
            self.state = State::Inside;
            self.captured.clear();
            let remainder = self.buffer[NEXT_PROMPT_OPEN.len()..].to_string();
            self.buffer.clear();
            return self.process_inside(&remainder);
        }
        if starts_with_ci(NEXT_PROMPT_OPEN, &self.buffer) {
            return String::new();
        }
        self.state = State::Normal;
        let flushed = std::mem::take(&mut self.buffer);
        if let Some(n) = next_lt(&flushed) {
            self.buffer = flushed[n..].to_string();
            self.state = State::Buffering;
            return format!("{}{}", &flushed[..n], self.resolve_buffer());
        }
        flushed
    }

    fn process_inside(&mut self, token: &str) -> String {
        self.buffer.push_str(token);
        if let Some(idx) = find_ci(&self.buffer, NEXT_PROMPT_CLOSE) {
            self.captured.push_str(&self.buffer[..idx]);
            let remainder = self.buffer[idx + NEXT_PROMPT_CLOSE.len()..].to_string();
            self.buffer.clear();
            self.state = State::Normal;
            return if remainder.is_empty() {
                String::new()
            } else {
                self.process_normal(&remainder)
            };
        }
        // No close tag yet: keep the tail for split-tag detection.
        if self.buffer.chars().count() > NEXT_PROMPT_CLOSE.len() {
            let safe = tail_start(&self.buffer, NEXT_PROMPT_CLOSE.len());
            self.captured.push_str(&self.buffer[..safe]);
            self.buffer = self.buffer[safe..].to_string();
        }
        String::new()
    }
}

#[cfg(test)]
mod tests {
    //! Ported from NextPromptTagFilterTests.cs and ToolCallTagFilterTests.cs. The
    //! `NullString_PassesThrough` cases have no Rust equivalent (`&str` can't be null).
    use super::*;

    // ── NextPromptTagFilter ──

    #[test]
    fn next_prompt_plain_text_passes_through() {
        assert_eq!(
            NextPromptTagFilter::new().filter("Hello world"),
            "Hello world"
        );
    }

    #[test]
    fn next_prompt_empty_string_passes_through() {
        assert_eq!(NextPromptTagFilter::new().filter(""), "");
    }

    #[test]
    fn no_captured_prompt_when_no_tag() {
        let mut f = NextPromptTagFilter::new();
        f.filter("just regular text");
        assert_eq!(f.captured_prompt(), None);
    }

    #[test]
    fn full_tag_single_token_captures_and_suppresses() {
        let mut f = NextPromptTagFilter::new();
        assert_eq!(
            f.filter("visible<next-prompt>suggest this</next-prompt>"),
            "visible"
        );
        assert_eq!(f.captured_prompt().as_deref(), Some("suggest this"));
    }

    #[test]
    fn full_tag_single_token_with_trailing_text() {
        let mut f = NextPromptTagFilter::new();
        assert_eq!(
            f.filter("before<next-prompt>prompt</next-prompt>after"),
            "beforeafter"
        );
        assert_eq!(f.captured_prompt().as_deref(), Some("prompt"));
    }

    #[test]
    fn tag_streamed_across_multiple_tokens() {
        let mut f = NextPromptTagFilter::new();
        assert_eq!(f.filter("Hello "), "Hello ");
        assert_eq!(f.filter("<next-prompt>"), "");
        assert_eq!(f.filter("suggested prompt"), "");
        assert_eq!(f.filter("</next-prompt>"), "");
        assert_eq!(f.captured_prompt().as_deref(), Some("suggested prompt"));
    }

    #[test]
    fn tag_open_tag_split_across_tokens() {
        let mut f = NextPromptTagFilter::new();
        assert_eq!(f.filter("text<next"), "text");
        assert_eq!(f.filter("-prompt>captured</next-prompt>"), "");
        assert_eq!(f.captured_prompt().as_deref(), Some("captured"));
    }

    #[test]
    fn tag_close_tag_split_across_tokens() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<next-prompt>inner");
        f.filter("</next-");
        f.filter("prompt>");
        assert_eq!(f.captured_prompt().as_deref(), Some("inner"));
    }

    #[test]
    fn text_after_closing_tag_is_visible() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<next-prompt>captured</next-prompt>");
        assert_eq!(f.filter("more visible text"), "more visible text");
    }

    #[test]
    fn less_than_not_a_tag_flushes_buffer() {
        let r = NextPromptTagFilter::new().filter("a < b");
        assert!(r.contains('a') && r.contains("< b"));
    }

    #[test]
    fn partial_match_then_divergent_flushes_buffer() {
        let mut f = NextPromptTagFilter::new();
        assert!(f.filter("<nextz>not a tag").contains("<nextz>not a tag"));
        assert_eq!(f.captured_prompt(), None);
    }

    #[test]
    fn case_insensitive_open_tag() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<NEXT-PROMPT>captured</NEXT-PROMPT>");
        assert_eq!(f.captured_prompt().as_deref(), Some("captured"));
    }

    #[test]
    fn captured_prompt_is_trimmed() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<next-prompt>  suggest this  </next-prompt>");
        assert_eq!(f.captured_prompt().as_deref(), Some("suggest this"));
    }

    #[test]
    fn whitespace_only_captured_prompt_returns_null() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<next-prompt>   </next-prompt>");
        assert_eq!(f.captured_prompt(), None);
    }

    #[test]
    fn second_tag_overwrites_first() {
        let mut f = NextPromptTagFilter::new();
        f.filter("<next-prompt>first</next-prompt>middle<next-prompt>second</next-prompt>");
        assert_eq!(f.captured_prompt().as_deref(), Some("second"));
    }

    #[test]
    fn long_prompt_streamed_in_small_pieces() {
        let mut f = NextPromptTagFilter::new();
        let mut shown = String::new();
        for c in "ok<next-prompt>run the tests again — überall</next-prompt>!".chars() {
            shown.push_str(&f.filter(&c.to_string()));
        }
        assert_eq!(shown, "ok!");
        assert_eq!(
            f.captured_prompt().as_deref(),
            Some("run the tests again — überall")
        );
    }

    // ── ToolCallTagFilter ──

    fn feed(f: &mut ToolCallTagFilter, parts: &[&str]) -> String {
        parts.iter().map(|p| f.filter(p)).collect()
    }

    #[test]
    fn tool_call_plain_text_passes_through() {
        assert_eq!(
            ToolCallTagFilter::new().filter("Hello world"),
            "Hello world"
        );
    }

    #[test]
    fn tool_call_empty_string_passes_through() {
        assert_eq!(ToolCallTagFilter::new().filter(""), "");
    }

    #[test]
    fn tool_call_tag_suppressed_single_token() {
        assert_eq!(
            ToolCallTagFilter::new().filter("<tool_call>{\"name\":\"read_file\"}</tool_call>"),
            ""
        );
    }

    #[test]
    fn pipe_tool_call_tag_suppressed_single_token() {
        assert_eq!(
            ToolCallTagFilter::new().filter("<|tool_call|>{\"name\":\"read_file\"}<|/tool_call|>"),
            ""
        );
    }

    #[test]
    fn text_before_tool_call_preserved() {
        assert_eq!(
            ToolCallTagFilter::new()
                .filter("Here is my plan: <tool_call>{\"name\":\"bash\"}</tool_call>"),
            "Here is my plan: "
        );
    }

    #[test]
    fn text_after_tool_call_preserved() {
        assert_eq!(
            ToolCallTagFilter::new().filter("<tool_call>{\"name\":\"bash\"}</tool_call>Done!"),
            "Done!"
        );
    }

    #[test]
    fn text_around_tool_call_preserved() {
        assert_eq!(
            ToolCallTagFilter::new()
                .filter("Before <tool_call>{\"name\":\"bash\"}</tool_call> After"),
            "Before  After"
        );
    }

    #[test]
    fn tool_call_tag_suppressed_token_by_token() {
        let mut f = ToolCallTagFilter::new();
        let out = feed(
            &mut f,
            &[
                "<",
                "tool_call",
                ">",
                "{\"name\":\"read_file\"}",
                "</tool_call>",
            ],
        );
        assert_eq!(out, "");
    }

    #[test]
    fn tool_call_tag_suppressed_char_by_char() {
        let mut f = ToolCallTagFilter::new();
        let out: String = "<tool_call>{\"name\":\"x\"}</tool_call>"
            .chars()
            .map(|c| f.filter(&c.to_string()))
            .collect();
        assert_eq!(out, "");
    }

    #[test]
    fn mixed_text_and_tool_call_token_by_token() {
        let mut f = ToolCallTagFilter::new();
        let out = feed(
            &mut f,
            &[
                "I will ",
                "do this: ",
                "<tool_call>",
                "{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}",
                "</tool_call>",
                " and done",
            ],
        );
        assert_eq!(out, "I will do this:  and done");
    }

    #[test]
    fn multiple_tool_calls_all_suppressed() {
        let mut f = ToolCallTagFilter::new();
        let out = feed(
            &mut f,
            &[
                "A <tool_call>{\"name\":\"a\"}</tool_call>",
                " B <tool_call>{\"name\":\"b\"}</tool_call> C",
            ],
        );
        assert_eq!(out, "A  B  C");
    }

    #[test]
    fn non_tool_call_angle_bracket_preserved() {
        let mut f = ToolCallTagFilter::new();
        assert_eq!(
            feed(&mut f, &["x < y", " and ", "z > w"]),
            "x < y and z > w"
        );
    }

    #[test]
    fn html_tags_preserved() {
        assert_eq!(
            ToolCallTagFilter::new().filter("<div>hello</div>"),
            "<div>hello</div>"
        );
    }

    #[test]
    fn case_insensitive_tool_call_tag() {
        assert_eq!(
            ToolCallTagFilter::new().filter("<TOOL_CALL>{\"name\":\"x\"}</TOOL_CALL>"),
            ""
        );
    }

    #[test]
    fn pipe_tag_token_by_token() {
        let mut f = ToolCallTagFilter::new();
        let out = feed(
            &mut f,
            &["<|", "tool_call|>", "{\"name\":\"x\"}", "<|/tool_call|>"],
        );
        assert_eq!(out, "");
    }

    #[test]
    fn split_close_tag_across_tokens() {
        let mut f = ToolCallTagFilter::new();
        let out = feed(
            &mut f,
            &["<tool_call>{\"name\":\"x\"}</", "tool_call>", "visible"],
        );
        assert_eq!(out, "visible");
    }

    #[test]
    fn text_with_multiple_angle_brackets_preserved() {
        assert_eq!(
            ToolCallTagFilter::new().filter("if (a < b && c > d) return true;"),
            "if (a < b && c > d) return true;"
        );
    }
}
