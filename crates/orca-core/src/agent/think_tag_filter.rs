//! Stream-time state machine that separates reasoning tags from visible response text
//! (`ThinkTagFilter.cs`). Recognizes several tag families (`<think>`, `<thinking>`,
//! `<reasoning>`, `<thought>`) and detects them mid-stream. The turn loop uses it to count
//! thinking chunks in the text (the thinking budget and the runaway guard).
//!
//! Tags are matched ASCII case-insensitively (C# uses `OrdinalIgnoreCase`, which would also
//! match e.g. a dotless `ı` for `i`; no model writes tags like that).

const DETECTION_LIMIT: usize = 30;

const TAG_FAMILIES: [(&str, &str); 4] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
    ("<thought>", "</thought>"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Detecting,
    InsideThink,
    Response,
}

#[derive(Debug, Clone)]
pub struct ThinkTagFilter {
    state: State,
    buffer: String,
    close_tag: &'static str,
    emitted_thinking_lines: usize,
    /// All thinking text so far (tags and API reasoning).
    pub accumulated_thinking: String,
    /// Most thinking lines to emit (default unlimited).
    pub max_thinking_lines: usize,
}

impl Default for ThinkTagFilter {
    fn default() -> Self {
        Self {
            state: State::Detecting,
            buffer: String::new(),
            close_tag: "</think>",
            emitted_thinking_lines: 0,
            accumulated_thinking: String::new(),
            max_thinking_lines: usize::MAX,
        }
    }
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn index_of_ci(s: &str, needle: &str) -> Option<usize> {
    if needle.len() > s.len() {
        return None;
    }
    (0..=s.len() - needle.len()).find(|&i| {
        s.is_char_boundary(i)
            && s.is_char_boundary(i + needle.len())
            && s[i..i + needle.len()].eq_ignore_ascii_case(needle)
    })
}

fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

impl ThinkTagFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The filter is in the response phase (answer text is flowing).
    pub fn in_response_phase(&self) -> bool {
        self.state == State::Response
    }

    /// Reasoning that arrived on the API's reasoning channel.
    pub fn append_external_thinking(&mut self, text: &str) {
        self.accumulated_thinking.push_str(text);
    }

    /// One streamed token: `(thinking text, response text)`.
    pub fn process(&mut self, token: &str) -> (String, String) {
        match self.state {
            State::Detecting => self.process_detecting(token),
            State::InsideThink => {
                self.buffer.push_str(token);
                self.drain_inside_think()
            }
            State::Response => self.process_response(token),
        }
    }

    fn process_detecting(&mut self, token: &str) -> (String, String) {
        self.buffer.push_str(token);
        let trimmed = self.buffer.trim_start().to_string();
        for (open, close) in TAG_FAMILIES {
            if starts_with_ci(&trimmed, open) {
                self.state = State::InsideThink;
                self.close_tag = close;
                self.buffer = trimmed[open.len()..].to_string();
                return self.drain_inside_think();
            }
        }
        // Enough content and no "<" at all: not a think model.
        if len16(&trimmed) >= DETECTION_LIMIT || (!trimmed.is_empty() && !trimmed.contains('<')) {
            self.state = State::Response;
            let response = std::mem::take(&mut self.buffer);
            return self.process_response(&response);
        }
        (String::new(), String::new())
    }

    fn drain_inside_think(&mut self) -> (String, String) {
        if let Some(close_idx) = index_of_ci(&self.buffer, self.close_tag) {
            let think_part = self.buffer[..close_idx].to_string();
            let response_part = self.buffer[close_idx + self.close_tag.len()..].to_string();
            self.accumulated_thinking.push_str(&think_part);
            self.buffer.clear();
            self.state = State::Response;
            let emitted = self.truncate_thinking(&think_part);
            let (more_think, response) = if response_part.is_empty() {
                (String::new(), String::new())
            } else {
                self.process_response(&response_part)
            };
            return (emitted + &more_think, response);
        }
        // Hold back a tail that could be the start of the close tag split across chunks.
        let holdback = partial_suffix_len(&self.buffer, &[self.close_tag]);
        let cut = self.buffer.len() - holdback;
        let thinking = self.buffer[..cut].to_string();
        self.accumulated_thinking.push_str(&thinking);
        self.buffer = self.buffer[cut..].to_string();
        (self.truncate_thinking(&thinking), String::new())
    }

    fn truncate_thinking(&mut self, text: &str) -> String {
        if self.emitted_thinking_lines >= self.max_thinking_lines {
            return String::new();
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let remaining = self.max_thinking_lines - self.emitted_thinking_lines;
        if lines.len() <= remaining {
            self.emitted_thinking_lines += lines.len();
            return text.to_string();
        }
        self.emitted_thinking_lines = self.max_thinking_lines;
        format!(
            "{}\n\u{22EF} (thinking truncated, Ctrl+O to expand)",
            lines[..remaining].join("\n")
        )
    }

    fn process_response(&mut self, token: &str) -> (String, String) {
        self.buffer.push_str(token);
        // A think block mid-stream (interleaved between tool calls).
        let mut earliest: Option<(usize, &'static str, usize)> = None;
        for (open, close) in TAG_FAMILIES {
            if let Some(idx) = index_of_ci(&self.buffer, open)
                && earliest.is_none_or(|(e, _, _)| idx < e)
            {
                earliest = Some((idx, close, open.len()));
            }
        }
        if let Some((idx, close, open_len)) = earliest {
            let response_part = self.buffer[..idx].to_string();
            self.buffer = self.buffer[idx + open_len..].to_string();
            self.close_tag = close;
            self.state = State::InsideThink;
            let (think, more_response) = self.drain_inside_think();
            return (think, response_part + &more_response);
        }
        let opens: Vec<&str> = TAG_FAMILIES.iter().map(|(o, _)| *o).collect();
        let holdback = partial_suffix_len(&self.buffer, &opens);
        let cut = self.buffer.len() - holdback;
        let emit = self.buffer[..cut].to_string();
        self.buffer = self.buffer[cut..].to_string();
        (String::new(), emit)
    }
}

/// Length of the buffer's suffix that is a strict prefix of one of `tags` (ASCII).
fn partial_suffix_len(buffer: &str, tags: &[&str]) -> usize {
    let max_tag = tags.iter().map(|t| t.len()).max().unwrap_or(0);
    let max_check = buffer.len().min(max_tag.saturating_sub(1));
    for len in (1..=max_check).rev() {
        let start = buffer.len() - len;
        if !buffer.is_char_boundary(start) {
            continue;
        }
        let suffix = &buffer[start..];
        if tags.iter().any(|t| starts_with_ci(t, suffix)) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    //! Ported from `ThinkTagFilterTests.cs`.
    use super::*;

    #[test]
    fn plain_text_goes_directly_to_response() {
        let mut f = ThinkTagFilter::new();
        let (think, response) = f.process("Hello world");
        assert_eq!(think, "");
        assert_eq!(response, "Hello world");
        assert!(f.in_response_phase());
    }

    #[test]
    fn plain_text_no_think_accumulated() {
        let mut f = ThinkTagFilter::new();
        f.process("Hello world");
        assert_eq!(f.accumulated_thinking, "");
    }

    #[test]
    fn leading_whitespace_then_plain_text_flushes_to_response() {
        let mut f = ThinkTagFilter::new();
        assert_eq!(f.process("  "), (String::new(), String::new()));
        let r2 = f.process("Hello");
        assert_eq!(r2.0, "");
        assert!(r2.1.contains("Hello"));
        assert!(f.in_response_phase());
    }

    #[test]
    fn detection_limit_forces_response_transition() {
        let mut f = ThinkTagFilter::new();
        let long = "x".repeat(35);
        let (think, response) = f.process(&long);
        assert_eq!(think, "");
        assert_eq!(response, long);
        assert!(f.in_response_phase());
    }

    #[test]
    fn think_tag_streamed_open_tag_then_content_then_close() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>");
        assert!(!f.in_response_phase());
        assert_eq!(f.process("step 1, "), ("step 1, ".into(), String::new()));
        assert_eq!(f.process("step 2"), ("step 2".into(), String::new()));
        let r4 = f.process("</think>The answer is 42");
        assert_eq!(r4.1, "The answer is 42");
        assert!(f.in_response_phase());
    }

    #[test]
    fn think_tag_streamed_accumulates_thinking() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>");
        f.process("reasoning ");
        f.process("here");
        f.process("</think>visible");
        assert!(f.accumulated_thinking.contains("reasoning "));
        assert!(f.accumulated_thinking.contains("here"));
    }

    #[test]
    fn think_tag_streamed_open_tag_split_across_tokens() {
        let mut f = ThinkTagFilter::new();
        assert_eq!(f.process("<thi").1, "");
        let r2 = f.process("nk>inner thoughts");
        assert!(r2.0.contains("inner thoughts"));
        assert!(!f.in_response_phase());
    }

    #[test]
    fn after_response_phase_all_tokens_are_response() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>");
        f.process("x");
        f.process("</think>first");
        assert!(f.in_response_phase());
        assert_eq!(
            f.process(" more text"),
            (String::new(), " more text".into())
        );
    }

    #[test]
    fn think_tag_single_token_response_part_correct() {
        let mut f = ThinkTagFilter::new();
        assert_eq!(f.process("<think>reasoning</think>visible").1, "visible");
        assert!(f.in_response_phase());
    }

    #[test]
    fn think_tag_single_token_accumulates_content() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>reasoning</think>visible");
        assert!(f.accumulated_thinking.contains("reasoning"));
    }

    #[test]
    fn max_thinking_lines_truncates_emitted_thinking() {
        let mut f = ThinkTagFilter {
            max_thinking_lines: 5,
            ..Default::default()
        };
        f.process("<think>");
        let r = f.process("line1\nline2\nline3\nline4\nline5\nline6");
        assert!(r.0.contains("line1"));
        assert!(r.0.contains("truncated"));
        assert!(!r.0.contains("line6"));
    }

    #[test]
    fn max_thinking_lines_still_accumulates_all() {
        let mut f = ThinkTagFilter {
            max_thinking_lines: 1,
            ..Default::default()
        };
        f.process("<think>");
        f.process("line1\nline2\nline3");
        f.process("</think>response");
        for l in ["line1", "line2", "line3"] {
            assert!(f.accumulated_thinking.contains(l));
        }
    }

    #[test]
    fn max_thinking_lines_subsequent_tokens_emit_empty() {
        let mut f = ThinkTagFilter {
            max_thinking_lines: 1,
            ..Default::default()
        };
        f.process("<think>");
        f.process("line1\nline2");
        assert_eq!(f.process("more thinking"), (String::new(), String::new()));
    }

    #[test]
    fn empty_think_block_streamed_transitions_to_response() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>");
        assert_eq!(f.process("</think>response").1, "response");
        assert!(f.in_response_phase());
    }

    #[test]
    fn no_close_tag_stays_inside_think() {
        let mut f = ThinkTagFilter::new();
        f.process("<think>");
        f.process("reasoning without end");
        assert!(!f.in_response_phase());
        assert!(f.accumulated_thinking.contains("reasoning without end"));
    }

    #[test]
    fn case_insensitive_streamed() {
        let mut f = ThinkTagFilter::new();
        f.process("<THINK>");
        f.process("reasoning");
        assert_eq!(f.process("</THINK>response").1, "response");
        assert!(f.in_response_phase());
    }

    #[test]
    fn leading_whitespace_before_think_tag_streamed() {
        let mut f = ThinkTagFilter::new();
        f.process("  \n");
        f.process("<think>");
        f.process("inner");
        assert_eq!(f.process("</think>outer").1, "outer");
        assert!(f.in_response_phase());
    }

    #[test]
    fn process_mid_stream_think_tag_does_not_leak_reasoning() {
        let mut f = ThinkTagFilter::new();
        let r1 = f.process("Okay, let me think about this carefully first. ");
        let r2 = f.process("<think>secret reasoning</think>The answer is 42.");
        let visible = r1.1 + &r2.1;
        assert!(!visible.contains("secret reasoning"));
        assert!(visible.contains("The answer is 42."));
        assert!(f.accumulated_thinking.contains("secret reasoning"));
    }

    #[test]
    fn process_thinking_tag_family_recognized() {
        let mut f = ThinkTagFilter::new();
        let r = f.process("<thinking>hidden</thinking>visible");
        assert_eq!(r.1, "visible");
        assert!(f.accumulated_thinking.contains("hidden"));
    }

    #[test]
    fn process_reasoning_tag_family_recognized() {
        let mut f = ThinkTagFilter::new();
        let r = f.process("<reasoning>ponder</reasoning>output");
        assert_eq!(r.1, "output");
        assert!(f.accumulated_thinking.contains("ponder"));
    }

    #[test]
    fn process_tag_split_across_chunks_mid_stream_still_caught() {
        let mut f = ThinkTagFilter::new();
        let mut visible = f.process("Answer coming. <th").1;
        visible += &f.process("ink>internal</think> Done.").1;
        assert!(!visible.contains("internal"));
        assert!(visible.contains("Answer coming."));
        assert!(visible.contains("Done."));
    }

    #[test]
    fn append_external_thinking_accumulates_for_expansion() {
        let mut f = ThinkTagFilter::new();
        f.append_external_thinking("api-channel reasoning");
        assert!(f.accumulated_thinking.contains("api-channel reasoning"));
    }
}
