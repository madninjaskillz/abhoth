//! Conversation history and context management (port of `OpenOrca.Core/Chat`): token
//! estimates against the server's reported prompt size, the compaction planner, Pi-style
//! compaction (cut points, checkpoints, file lists), the LLM summariser, the repeat-call note
//! and the per-step reasoning cap.
//!
//! Lengths and limits count UTF-16 units, as C# `string.Length` does.

pub mod adaptive_reasoning;
pub mod compaction;
pub mod compaction_prompts;
pub mod compactor;
pub mod conversation;
pub mod early_tool_starter;
pub mod planner;
pub mod reasoning_cap;
pub mod repeat_call_note;

pub use adaptive_reasoning::AdaptiveReasoning;
pub use compaction::{
    CompactionCutPoint, CompactionFileOps, CompactionPreparation, CompactionSettings,
};
pub use compactor::{CompactionReport, ConversationCompactor, LlmCompactor};
pub use conversation::{ContextUsage, Conversation};
pub use early_tool_starter::EarlyToolStarter;
pub use planner::{CompactionAction, CompactionCostModel, CompactionPlan};
pub use reasoning_cap::ReasoningCap;

/// The chars-per-token ratio used when a configured one is not positive.
pub const DEFAULT_CHARS_PER_TOKEN: f32 = 3.5;

/// `charsPerToken`, or the default when it is not positive.
pub(crate) fn cpt_or_default(chars_per_token: f32) -> f32 {
    if chars_per_token > 0.0 {
        chars_per_token
    } else {
        DEFAULT_CHARS_PER_TOKEN
    }
}

/// `string.Length`: UTF-16 code units.
pub(crate) fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s[..n]` in UTF-16 units (a character that would straddle the cut is left out).
pub(crate) fn head16(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if units + w > n {
            return &s[..i];
        }
        units += w;
    }
    s
}

/// `s[^n..]` in UTF-16 units (a character that would straddle the cut is left out).
pub(crate) fn tail16(s: &str, n: usize) -> &str {
    let total = len16(s);
    if n >= total {
        return s;
    }
    let skip = total - n;
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= skip {
            return &s[i..];
        }
        units += c.len_utf16();
    }
    ""
}

/// `{n:N0}`: the number with thousands separators (en-US / invariant culture).
pub(crate) fn n0(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_helpers_count_and_cut_in_utf16_units() {
        assert_eq!(len16("a😀b"), 4);
        assert_eq!(head16("a😀b", 2), "a");
        assert_eq!(head16("a😀b", 3), "a😀");
        assert_eq!(tail16("a😀b", 1), "b");
        assert_eq!(tail16("a😀b", 3), "😀b");
        assert_eq!(tail16("ab", 5), "ab");
        assert_eq!(n0(0), "0");
        assert_eq!(n0(999), "999");
        assert_eq!(n0(1000), "1,000");
        assert_eq!(n0(1234567), "1,234,567");
        assert_eq!(n0(-4096), "-4,096");
    }
}
