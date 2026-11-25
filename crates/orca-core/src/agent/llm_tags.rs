//! Port of `LlmTagConstants` and `LlmTagPatterns`: the XML-style tags local models emit and
//! the shared patterns that strip them, plus the .NET `OrdinalIgnoreCase` substring test used
//! alongside them.

use std::sync::LazyLock;

use regex::Regex;

pub const THINK_OPEN: &str = "<think>";
pub const THINK_CLOSE: &str = "</think>";
pub const TOOL_CALL_OPEN: &str = "<tool_call>";
pub const TOOL_CALL_CLOSE: &str = "</tool_call>";
pub const TOOL_CALL_PIPE_OPEN: &str = "<|tool_call|>";
pub const TOOL_CALL_PIPE_CLOSE: &str = "<|/tool_call|>";
pub const ASSISTANT_OPEN: &str = "<assistant>";
pub const ASSISTANT_CLOSE: &str = "</assistant>";
pub const NEXT_PROMPT_OPEN: &str = "<next-prompt>";
pub const NEXT_PROMPT_CLOSE: &str = "</next-prompt>";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid LLM tag regex")
}

/// A complete `<think>...</think>` block (Singleline, IgnoreCase).
pub static THINK_BLOCK: LazyLock<Regex> = LazyLock::new(|| re(r"(?is)<think>.*?</think>"));
/// An unclosed `<think>` through the end of the text.
pub static THINK_UNCLOSED: LazyLock<Regex> = LazyLock::new(|| re(r"(?is)<think>.*$"));
/// A complete `<assistant>...</assistant>` block.
pub static ASSISTANT_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?is)<assistant>.*?</assistant>"));
/// An opening or closing `<assistant>` tag.
pub static ASSISTANT_TAG: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)</?assistant>"));

/// .NET `string.Contains(value, StringComparison.OrdinalIgnoreCase)`: both sides are compared
/// char by char after the invariant simple upper-case mapping (so `ı` matches `i` and `ſ`
/// matches `s`, as in .NET). Characters outside the BMP are compared as-is, since .NET maps
/// UTF-16 units and a lone surrogate has no case.
pub fn contains_ordinal_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        return haystack
            .to_ascii_uppercase()
            .contains(&needle.to_ascii_uppercase());
    }
    let hay: Vec<char> = haystack.chars().map(simple_upper).collect();
    let pat: Vec<char> = needle.chars().map(simple_upper).collect();
    hay.windows(pat.len()).any(|w| w == pat.as_slice())
}

fn simple_upper(c: char) -> char {
    if (c as u32) > 0xFFFF {
        return c;
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) if (u as u32) <= 0xFFFF => u,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinal_ignore_case_contains() {
        assert!(contains_ordinal_ignore_case(
            "abc <TOOL_CALL> x",
            "<tool_call>"
        ));
        assert!(!contains_ordinal_ignore_case(
            "abc <tool call>",
            "<tool_call>"
        ));
        assert!(contains_ordinal_ignore_case("Ünïcode I CANNOT", "i cannot"));
        // .NET: ToUpperInvariant('ı') == 'I'.
        assert!(contains_ordinal_ignore_case("ı cannot", "i cannot"));
        assert!(contains_ordinal_ignore_case("anything", ""));
    }

    #[test]
    fn tag_patterns_strip() {
        assert_eq!(THINK_BLOCK.replace_all("a<THINK>x\ny</think>b", ""), "ab");
        assert_eq!(THINK_UNCLOSED.replace_all("a<think>x\ny", ""), "a");
        assert_eq!(
            ASSISTANT_BLOCK.replace_all("<assistant>hi\n</Assistant>z", ""),
            "z"
        );
        assert_eq!(
            ASSISTANT_TAG.replace_all("<assistant>hi</assistant>", ""),
            "hi"
        );
    }
}
