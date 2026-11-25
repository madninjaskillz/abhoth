//! Adaptive reasoning for one actor (`AdaptiveReasoning.cs`, `agent.reasoning.adaptive`):
//! decides, request by request, whether the model thinks before it answers. Thinking is
//! skipped only for a step whose next action is obvious: the request directly follows a tool
//! round in which every call succeeded and every call was one of the configured tools (by
//! default the file writes and edits of a plan the model already made). Everything else
//! thinks: the first request of a turn, a round with an error, a read or a command, a harness
//! note or user message after the round, and every request after `max_skips` skipped ones in a
//! row.
//!
//! A skipped step sends `chat_template_kwargs.enable_thinking = false`
//! ([`crate::client::request_parameters::skip_thinking`]).

use std::collections::HashSet;

use crate::client::{ChatMessage, Role};
use crate::config::AgentReasoningConfig;

/// The effort that renders nothing into the Qwen system prompt.
pub const CACHE_STABLE_EFFORT: &str = "medium";

/// What the "low" effort line of the Qwen template says; added to the prompt of an adaptive
/// actor (`agent.reasoning.adaptiveBriefRule`) since its thinking steps run at medium. Only
/// the Pi system prompt carries it (not ported yet).
pub const BRIEF_RULE: &str = "Keep your thinking brief and focused, moving directly to the conclusion without unnecessary elaboration.";

/// `AgentReasoningConfig.DefaultAdaptiveSkipAfter`.
pub const DEFAULT_SKIP_AFTER: [&str; 6] = [
    "write",
    "edit",
    "write_file",
    "edit_file",
    "multi_edit",
    "create_file",
];

#[derive(Debug, Clone)]
pub struct AdaptiveReasoning {
    /// Lowercase tool names.
    skip_after: HashSet<String>,
    max_skips: i32,
    round_eligible: bool,
    messages_at_round: Option<usize>,
    last_result_at_round: Option<String>,
    skips_in_a_row: i32,
    skipped: i32,
    thought: i32,
}

impl AdaptiveReasoning {
    pub fn new(skip_after: Option<&[String]>, max_skips: i32) -> Self {
        let skip_after = match skip_after {
            Some(list) => list.iter().map(|s| s.to_lowercase()).collect(),
            None => DEFAULT_SKIP_AFTER.iter().map(|s| s.to_string()).collect(),
        };
        Self {
            skip_after,
            max_skips: max_skips.max(1),
            round_eligible: false,
            messages_at_round: None,
            last_result_at_round: None,
            skips_in_a_row: 0,
            skipped: 0,
            thought: 0,
        }
    }

    /// An adaptive policy for this role, or None when `agent.reasoning.adaptive` does not
    /// cover it.
    pub fn for_config(config: &AgentReasoningConfig, lead: bool) -> Option<Self> {
        config.is_adaptive(lead).then(|| {
            Self::new(
                config.adaptive_skip_after.as_deref(),
                config.adaptive_max_skips,
            )
        })
    }

    /// Most requests in a row that may skip thinking.
    pub fn max_skips(&self) -> i32 {
        self.max_skips
    }

    /// Requests that skipped thinking so far.
    pub fn skipped(&self) -> i32 {
        self.skipped
    }

    /// Requests that kept thinking so far.
    pub fn thought(&self) -> i32 {
        self.thought
    }

    /// A new turn: its first request thinks.
    pub fn reset(&mut self) {
        self.round_eligible = false;
        self.skips_in_a_row = 0;
    }

    /// Record the tool round just committed to `messages` (its results are the last
    /// messages). Call it right after the results were added and before any note.
    pub fn note_round(&mut self, calls: &[(String, bool)], messages: &[ChatMessage]) {
        self.round_eligible = Self::is_obvious_next(calls, &self.skip_after);
        self.messages_at_round = Some(messages.len());
        self.last_result_at_round = last_result(messages);
    }

    /// Whether the next request skips thinking. Consumes the recorded round: a second request
    /// without a new round (a retry, a no-tool reply) thinks. Not asked for a reasoning-cap
    /// re-ask, which repeats the step's request with the same thinking setting.
    pub fn should_skip(&mut self, messages: &[ChatMessage]) -> bool {
        // C# compares the last result by reference: a note appended to it replaces the string.
        // Comparing the text is the same test (a note always changes it).
        let skip = self.round_eligible
            && self.skips_in_a_row < self.max_skips
            && self.messages_at_round == Some(messages.len())
            && messages.last().is_some_and(|m| m.role == Role::Tool)
            && last_result(messages) == self.last_result_at_round;
        self.round_eligible = false;
        if skip {
            self.skips_in_a_row += 1;
            self.skipped += 1;
        } else {
            self.skips_in_a_row = 0;
            self.thought += 1;
        }
        skip
    }

    /// A round with an obvious next action: at least one call, none failed, all in
    /// `skip_after` (lowercase names; the check ignores case).
    pub fn is_obvious_next(calls: &[(String, bool)], skip_after: &HashSet<String>) -> bool {
        !calls.is_empty()
            && calls
                .iter()
                .all(|(tool, is_error)| !is_error && skip_after.contains(&tool.to_lowercase()))
    }
}

fn last_result(messages: &[ChatMessage]) -> Option<String> {
    messages
        .last()
        .filter(|m| m.role == Role::Tool)
        .and_then(|m| m.content.clone())
}

#[cfg(test)]
mod tests {
    //! Ported from `AdaptiveReasoningTests.cs` (the policy and config parts; the worker tests
    //! need the orchestrator, phase 3).
    use super::*;
    use crate::client::ToolCall;

    fn with_round(results: &[&str]) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user("do it")];
        let mut assistant = ChatMessage::assistant("");
        for i in 0..results.len() {
            assistant.tool_calls.push(ToolCall {
                id: format!("c{i}"),
                name: "write".into(),
                arguments: "{}".into(),
            });
        }
        messages.push(assistant);
        for (i, r) in results.iter().enumerate() {
            messages.push(ChatMessage::tool_result(format!("c{i}"), *r));
        }
        messages
    }

    fn calls(c: &[(&str, bool)]) -> Vec<(String, bool)> {
        c.iter().map(|(t, e)| (t.to_string(), *e)).collect()
    }

    #[test]
    fn first_request_thinks() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let messages = vec![ChatMessage::user("do it")];
        assert!(!policy.should_skip(&messages));
        assert_eq!(policy.thought(), 1);
    }

    #[test]
    fn after_successful_writes_and_edits_skips_once() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let messages = with_round(&["ok", "ok"]);
        policy.note_round(&calls(&[("write", false), ("edit", false)]), &messages);
        assert!(policy.should_skip(&messages));
        assert!(!policy.should_skip(&messages));
        assert_eq!(policy.skipped(), 1);
    }

    #[test]
    fn after_an_error_or_another_tool_thinks() {
        for (tool, is_error) in [("write", true), ("bash", false), ("read", false)] {
            let mut policy = AdaptiveReasoning::new(None, 3);
            let messages = with_round(&["ok", "ok"]);
            policy.note_round(&calls(&[("write", false), (tool, is_error)]), &messages);
            assert!(!policy.should_skip(&messages), "{tool} {is_error}");
        }
    }

    #[test]
    fn empty_round_thinks() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let messages = with_round(&["ok"]);
        policy.note_round(&[], &messages);
        assert!(!policy.should_skip(&messages));
    }

    #[test]
    fn skip_after_is_configurable() {
        let list = vec!["write".to_string(), "edit".into(), "bash".into()];
        let mut policy = AdaptiveReasoning::new(Some(&list), 3);
        let messages = with_round(&["ok"]);
        policy.note_round(&calls(&[("BASH", false)]), &messages);
        assert!(policy.should_skip(&messages));
    }

    #[test]
    fn harness_note_on_the_last_result_thinks() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let mut messages = with_round(&["ok"]);
        policy.note_round(&calls(&[("write", false)]), &messages);
        let last = messages.last_mut().unwrap();
        last.content = Some(format!(
            "{}\n\n[Task reminder] ...",
            last.content.clone().unwrap()
        ));
        assert!(!policy.should_skip(&messages));
    }

    #[test]
    fn message_after_the_round_thinks() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let mut messages = with_round(&["ok"]);
        policy.note_round(&calls(&[("write", false)]), &messages);
        messages.push(ChatMessage::user("Only 3 steps remain."));
        assert!(!policy.should_skip(&messages));
    }

    #[test]
    fn at_most_max_skips_in_a_row_then_thinks_and_starts_over() {
        let mut policy = AdaptiveReasoning::new(None, 2);
        let mut skipped = Vec::new();
        for _ in 0..5 {
            let messages = with_round(&["ok"]);
            policy.note_round(&calls(&[("edit", false)]), &messages);
            skipped.push(policy.should_skip(&messages));
        }
        assert_eq!(skipped, vec![true, true, false, true, true]);
    }

    #[test]
    fn reset_makes_the_next_request_think() {
        let mut policy = AdaptiveReasoning::new(None, 3);
        let messages = with_round(&["ok"]);
        policy.note_round(&calls(&[("write", false)]), &messages);
        policy.reset();
        assert!(!policy.should_skip(&messages));
    }

    #[test]
    fn config_says_which_roles_are_adaptive() {
        for (adaptive, lead, worker) in [
            (None, false, false),
            (Some("off"), false, false),
            (Some("lead"), true, false),
            (Some("worker"), false, true),
            (Some(" All "), true, true),
        ] {
            let config = AgentReasoningConfig {
                adaptive: adaptive.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(config.is_adaptive(true), lead);
            assert_eq!(config.is_adaptive(false), worker);
            assert_eq!(AdaptiveReasoning::for_config(&config, true).is_some(), lead);
            assert_eq!(
                AdaptiveReasoning::for_config(&config, false).is_some(),
                worker
            );
        }
    }

    #[test]
    fn config_defaults_are_off_and_writes_and_edits() {
        let config = AgentReasoningConfig::default();
        assert!(config.adaptive.is_none());
        assert!(config.adaptive_skip_after.is_none());
        assert_eq!(config.adaptive_max_skips, 3);
        assert!(config.adaptive_brief_rule);
        assert!(DEFAULT_SKIP_AFTER.contains(&"write"));
        assert!(DEFAULT_SKIP_AFTER.contains(&"edit"));
        assert!(!DEFAULT_SKIP_AFTER.contains(&"bash"));
    }

    #[test]
    fn effort_for_adaptive_role_without_an_effort_is_medium_others_keep_theirs() {
        let cfg = |adaptive: Option<&str>, lead: Option<&str>, worker: Option<&str>| {
            AgentReasoningConfig {
                adaptive: adaptive.map(str::to_string),
                lead: lead.map(str::to_string),
                worker: worker.map(str::to_string),
                ..Default::default()
            }
        };
        assert_eq!(
            cfg(Some("all"), None, None).effort_for(true).as_deref(),
            Some("medium")
        );
        assert_eq!(
            cfg(Some("all"), None, Some("off"))
                .effort_for(false)
                .as_deref(),
            Some("medium")
        );
        assert_eq!(
            cfg(Some("all"), Some("low"), None)
                .effort_for(true)
                .as_deref(),
            Some("low")
        );
        assert_eq!(cfg(None, None, None).effort_for(true), None);
        assert_eq!(
            cfg(Some("lead"), None, Some("off"))
                .effort_for(false)
                .as_deref(),
            Some("off")
        );
    }

    #[test]
    fn skip_thinking_turns_thinking_off_keeping_the_effort_and_other_kwargs() {
        use crate::client::ChatOptions;
        use serde_json::json;
        let mut options = ChatOptions::default();
        options.set("reasoning_effort", json!("medium"));
        options.set("chat_template_kwargs", json!({"x": 1}));
        crate::client::request_parameters::skip_thinking(&mut options);
        assert_eq!(options.additional["reasoning_effort"], json!("medium"));
        assert_eq!(
            options.additional["chat_template_kwargs"],
            json!({"enable_thinking": false, "x": 1})
        );
    }

    #[test]
    fn skip_thinking_puts_the_thinking_switch_before_kwargs_the_request_already_has() {
        use crate::client::ChatOptions;
        use serde_json::json;
        let mut options = ChatOptions::default();
        options.set(
            "chat_template_kwargs",
            json!({"preserve_thinking": true, "enable_thinking": true}),
        );
        crate::client::request_parameters::skip_thinking(&mut options);
        assert_eq!(
            options.additional["chat_template_kwargs"].to_string(),
            r#"{"enable_thinking":false,"preserve_thinking":true}"#
        );
        let mut off = ChatOptions::default();
        off.set("chat_template_kwargs", json!({"preserve_thinking": true}));
        crate::client::request_parameters::apply_effort(&mut off, Some("off"));
        assert_eq!(
            off.additional["chat_template_kwargs"].to_string(),
            r#"{"enable_thinking":false,"preserve_thinking":true}"#
        );
    }
}
