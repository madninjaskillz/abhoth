//! A sub-agent's step (iteration) budget (port of `StepBudget.cs`): a warning a few steps
//! before the limit, and up to `agent.iterationExtensions` extensions of half the budget each
//! while the agent is still doing real work (changing files or running commands, not
//! repeating itself).
//!
//! Why (launcher bench, local model): three of four workers stopped at 40 steps with their
//! piece almost done — the scaffold worker wrote 28 files one per step, the discovery worker
//! had fixed its last failing fixture and only needed to re-run the tests, and the UI worker
//! had built and smoke-run the app. None knew the limit was near. Haiku workers on the same
//! split used 13-31 steps, so a warning plus a bounded extension costs nothing when work fits
//! and saves a re-brief when it nearly does. A looping agent gets no extension.

use std::collections::{HashSet, VecDeque};

use super::types::eq_ci;
use crate::client::ToolCall;

/// Steps left when the agent is told to wrap up.
pub const WARN_STEPS_LEFT: i32 = 5;

/// Recent tool calls considered for "still making progress".
pub const RECENT_WINDOW: usize = 10;

const WORK_TOOLS: [&str; 18] = [
    "write",
    "edit",
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "create_directory",
    "mkdir",
    "delete_file",
    "move_file",
    "rename_file",
    "copy_file",
    "bash",
    "shell",
    "run_command",
    "execute_command",
    "start_background_process",
    "job",
];

/// A sub-agent's step budget.
#[derive(Debug, Clone)]
pub struct StepBudget {
    recent: VecDeque<String>,
    extension: i32,
    max_extensions: i32,
    warned_for: i32,
    max: i32,
    extensions: i32,
}

impl StepBudget {
    /// A budget of `max` steps (at least 1) with up to `max_extensions` extensions of half of
    /// it (at least 1 step) each.
    pub fn new(max: i32, max_extensions: i32) -> Self {
        let max = max.max(1);
        Self {
            recent: VecDeque::new(),
            extension: (max / 2).max(1),
            max_extensions: max_extensions.max(0),
            warned_for: -1,
            max,
            extensions: 0,
        }
    }

    /// The current limit (raised by each extension).
    pub fn max(&self) -> i32 {
        self.max
    }

    /// Extensions granted so far.
    pub fn extensions(&self) -> i32 {
        self.extensions
    }

    /// Note one tool call the agent made. `args_json` is the compact JSON of its arguments (C#
    /// serializes the argument dictionary; "" for none). Only equality between calls matters.
    pub fn record(&mut self, name: &str, args_json: &str) {
        self.recent.push_back(format!("{name}\u{1}{args_json}"));
        while self.recent.len() > RECENT_WINDOW {
            self.recent.pop_front();
        }
    }

    /// Note the tool calls of one response. Each call's arguments are canonicalised to
    /// compact JSON (invalid JSON is kept as its raw text).
    pub fn record_calls(&mut self, calls: &[ToolCall]) {
        for c in calls {
            let args = match serde_json::from_str::<serde_json::Value>(&c.arguments) {
                Ok(v) => v.to_string(),
                Err(_) => c.arguments.clone(),
            };
            self.record(&c.name, &args);
        }
    }

    /// Active and not looping: at least 4 recent calls, at least half of them distinct, and at
    /// least one that changes files or runs a command.
    pub fn making_progress(&self) -> bool {
        if self.recent.len() < 4 {
            return false;
        }
        let distinct = self.recent.iter().collect::<HashSet<_>>().len();
        distinct * 2 >= self.recent.len()
            && self.recent.iter().any(|s| {
                let name = s.split('\u{1}').next().unwrap_or("");
                WORK_TOOLS.iter().any(|w| eq_ci(w, name))
            })
    }

    /// The wrap-up notice to add before step `step` (1-based), or `None`. Sent once per limit,
    /// when exactly [`WARN_STEPS_LEFT`] steps remain including this one.
    pub fn warning_before(&mut self, step: i32) -> Option<String> {
        let left = self.max - step + 1;
        if left != WARN_STEPS_LEFT || self.warned_for == self.max {
            return None;
        }
        self.warned_for = self.max;
        Some(format!(
            "[Step budget] {left} steps left (this is step {step} of {}). Wrap up now: finish the change you are on, run your piece's check once, then reply with your report. If anything is unfinished, end the report with a \"## Remaining\" list saying exactly what is left, so the work can be continued from it.",
            self.max
        ))
    }

    /// At the limit: grant one more extension (half the original budget) when the agent is
    /// still making progress and extensions remain. Returns the notice to send, or `None` for
    /// no extension.
    pub fn try_extend(&mut self) -> Option<String> {
        if self.extensions >= self.max_extensions || !self.making_progress() {
            return None;
        }
        self.extensions += 1;
        self.max += self.extension;
        Some(format!(
            "[Step budget] You reached your step limit while still making progress, so you get {} more steps (new limit {}{}). Finish your piece, run its check, and reply with your report.",
            self.extension,
            self.max,
            if self.extensions >= self.max_extensions {
                ", the last extension"
            } else {
                ""
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_once_five_steps_before_the_limit() {
        let mut b = StepBudget::new(20, 1);
        assert_eq!(b.warning_before(15), None);
        let w = b.warning_before(16).unwrap();
        assert!(w.starts_with("[Step budget] 5 steps left (this is step 16 of 20). Wrap up now"));
        assert!(w.contains("\"## Remaining\" list"));
        assert_eq!(b.warning_before(16), None);
    }

    #[test]
    fn extends_only_while_making_progress() {
        let mut b = StepBudget::new(40, 2);
        assert_eq!(b.try_extend(), None); // no calls yet
        for i in 0..4 {
            b.record("write", &format!("{{\"path\":\"f{i}.py\"}}"));
        }
        let notice = b.try_extend().unwrap();
        assert_eq!(
            notice,
            "[Step budget] You reached your step limit while still making progress, so you get 20 more steps (new limit 60). Finish your piece, run its check, and reply with your report."
        );
        assert_eq!(b.max(), 60);
        // The warning comes again for the new limit.
        assert!(b.warning_before(56).is_some());
        assert!(
            b.try_extend()
                .unwrap()
                .contains("(new limit 80, the last extension)")
        );
        assert_eq!(b.extensions(), 2);
        assert_eq!(b.try_extend(), None);
    }

    #[test]
    fn a_looping_or_read_only_agent_gets_no_extension() {
        let mut looping = StepBudget::new(10, 3);
        for _ in 0..6 {
            looping.record("bash", "{\"command\":\"dotnet test\"}");
        }
        assert!(!looping.making_progress());

        let mut reading = StepBudget::new(10, 3);
        for i in 0..6 {
            reading.record("read", &format!("{{\"path\":\"{i}\"}}"));
        }
        assert!(!reading.making_progress());
        assert_eq!(reading.try_extend(), None);
    }

    #[test]
    fn record_calls_canonicalises_arguments() {
        let call = |args: &str| ToolCall {
            id: "c".into(),
            name: "EDIT".into(),
            arguments: args.into(),
        };
        let mut b = StepBudget::new(10, 1);
        b.record_calls(&[
            call("{ \"a\": 1 }"),
            call("{\"a\":1}"),
            call("not json"),
            call("{\"a\":2}"),
        ]);
        // Two of the four are the same call: still half distinct; EDIT is a work tool.
        assert!(b.making_progress());
        assert_eq!(StepBudget::new(0, -1).max(), 1);
    }
}
