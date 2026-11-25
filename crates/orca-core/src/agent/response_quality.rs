//! Port of `ResponseQualityChecker`: whether a reply is a refusal or too short, so the caller
//! can switch to the backup model (`lmStudio.backupOnRefusal`,
//! `lmStudio.backupMinResponseLength`).

use super::llm_tags::{THINK_BLOCK, THINK_UNCLOSED, TOOL_CALL_OPEN, contains_ordinal_ignore_case};
use crate::config::OrcaConfig;

const REFUSAL_PATTERNS: &[&str] = &[
    "i cannot",
    "i can't",
    "i am unable",
    "i'm unable",
    "i don't have the ability",
    "i do not have the ability",
    "as an ai",
    "as a language model",
    "i'm sorry, but i",
    "i apologize, but i",
    "i'm not able to",
    "i am not able to",
];

/// C# `QualityVerdict`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityVerdict {
    pub should_switch_to_backup: bool,
    pub reason: Option<String>,
}

/// `ResponseQualityChecker.Check`: too short (UTF-16 length of the text outside `<think>`,
/// when a minimum is set) or a refusal (when `backupOnRefusal` is on).
pub fn check(config: &OrcaConfig, full_text: &str) -> QualityVerdict {
    let stripped = strip_think_blocks(full_text);

    let min_len = config.lm_studio.backup_min_response_length;
    let len = orca_tools_common::utf16_len(&stripped);
    if min_len > 0 && len < min_len as usize {
        tracing::warn!("Response too short ({len} chars, minimum {min_len}) — recommending backup");
        return QualityVerdict {
            should_switch_to_backup: true,
            reason: Some(format!(
                "Response too short ({len} chars, minimum {min_len})"
            )),
        };
    }

    if config.lm_studio.backup_on_refusal && is_refusal_response(full_text) {
        tracing::warn!("Refusal pattern detected — recommending backup");
        return QualityVerdict {
            should_switch_to_backup: true,
            reason: Some("Refusal detected".to_string()),
        };
    }

    QualityVerdict {
        should_switch_to_backup: false,
        reason: None,
    }
}

/// `IsRefusalResponse`: a refusal phrase outside `<think>`, unless the reply has a tool call.
pub fn is_refusal_response(full_text: &str) -> bool {
    if contains_ordinal_ignore_case(full_text, TOOL_CALL_OPEN) {
        return false;
    }
    let stripped = strip_think_blocks(full_text);
    if stripped.trim().is_empty() {
        return false;
    }
    REFUSAL_PATTERNS
        .iter()
        .any(|p| contains_ordinal_ignore_case(&stripped, p))
}

/// `StripThinkBlocks`: closed and unclosed `<think>` blocks removed, trimmed.
pub fn strip_think_blocks(text: &str) -> String {
    let stripped = THINK_BLOCK.replace_all(text, "");
    THINK_UNCLOSED.replace_all(&stripped, "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_refusal_response_detects_refusal_patterns() {
        for text in [
            "I cannot help with that request.",
            "I can't do that.",
            "I am unable to assist.",
            "I'm unable to perform this task.",
            "I don't have the ability to do that.",
            "I do not have the ability to execute code.",
            "As an AI, I cannot modify files.",
            "As a language model, I have limitations.",
            "I'm sorry, but I can't help with that.",
            "I apologize, but I cannot proceed.",
            "I'm not able to complete this task.",
            "I am not able to access the file system.",
        ] {
            assert!(is_refusal_response(text), "{text}");
        }
    }

    #[test]
    fn is_refusal_response_accepts_normal_responses() {
        for text in [
            "Here's how to implement that feature.",
            "I'll help you with that.",
            "Let me read the file first.",
            "", // empty
        ] {
            assert!(!is_refusal_response(text), "{text}");
        }
    }

    #[test]
    fn is_refusal_response_ignores_refusals_with_tool_calls() {
        let text = "I cannot do that directly, but <tool_call>{\"name\":\"bash\"}</tool_call>";
        assert!(!is_refusal_response(text));
    }

    #[test]
    fn is_refusal_response_ignores_refusals_inside_think_blocks() {
        let text = "<think>I cannot figure this out</think>Here's the solution:";
        assert!(!is_refusal_response(text));
    }

    #[test]
    fn strip_think_blocks_removes_closed_blocks() {
        assert_eq!(
            strip_think_blocks("<think>some reasoning</think>actual response"),
            "actual response"
        );
    }

    #[test]
    fn strip_think_blocks_removes_unclosed_blocks() {
        assert_eq!(
            strip_think_blocks("prefix <think>unclosed reasoning that goes on"),
            "prefix"
        );
    }

    // Rust-only: the Check verdicts (C# has no direct tests for Check).

    #[test]
    fn check_verdicts() {
        let mut config = OrcaConfig::default();
        assert_eq!(
            check(&config, "I cannot do that."),
            QualityVerdict {
                should_switch_to_backup: false,
                reason: None
            }
        );

        config.lm_studio.backup_on_refusal = true;
        assert_eq!(
            check(&config, "I cannot do that.").reason.as_deref(),
            Some("Refusal detected")
        );

        config.lm_studio.backup_min_response_length = 20;
        let v = check(&config, "<think>long hidden reasoning here</think>short 😀");
        assert!(v.should_switch_to_backup);
        // Length counts UTF-16 units of the text outside <think>: "short 😀" is 8.
        assert_eq!(
            v.reason.as_deref(),
            Some("Response too short (8 chars, minimum 20)")
        );
    }
}
