//! `ReplState`: the interactive session's mode, output level and counters. The agent's own
//! counters (output tokens, last response, tool history) live on `AgentSession`.

use std::time::{Duration, Instant};

/// `InputMode`, cycled with Shift+Tab: Normal -> Plan -> Ask -> Normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    Plan,
    Ask,
}

#[derive(Debug, Clone)]
pub struct ReplState {
    /// 0 = print only, 1 = tool calls + preview, 2 = full tool output, 3 = thinking preview,
    /// 4 = full thinking (Ctrl+O cycles).
    pub verbosity: u8,
    pub mode: InputMode,
    pub total_turns: u32,
    pub started: Instant,
    pub current_session_id: Option<String>,
    /// The model's suggested next prompt (ghost text at the next prompt).
    pub suggested_next_prompt: Option<String>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            verbosity: 1,
            mode: InputMode::Normal,
            total_turns: 0,
            started: Instant::now(),
            current_session_id: None,
            suggested_next_prompt: None,
        }
    }
}

impl ReplState {
    pub fn show_print_only(&self) -> bool {
        self.verbosity == 0
    }
    pub fn show_tool_calls(&self) -> bool {
        self.verbosity >= 1
    }
    pub fn show_full_tool_output(&self) -> bool {
        self.verbosity >= 2
    }
    pub fn show_thinking(&self) -> bool {
        self.verbosity >= 3
    }
    pub fn show_full_thinking(&self) -> bool {
        self.verbosity >= 4
    }

    pub fn plan_mode(&self) -> bool {
        self.mode == InputMode::Plan
    }

    pub fn set_plan_mode(&mut self, on: bool) {
        self.mode = if on {
            InputMode::Plan
        } else {
            InputMode::Normal
        };
    }

    pub fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            InputMode::Normal => InputMode::Plan,
            InputMode::Plan => InputMode::Ask,
            InputMode::Ask => InputMode::Normal,
        };
    }

    /// Ctrl+O: the next output level (wraps after 4).
    pub fn cycle_verbosity(&mut self) {
        self.verbosity = (self.verbosity + 1) % 5;
    }

    pub fn session_duration(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn verbosity_label(&self) -> String {
        match self.verbosity {
            0 => "quiet".into(),
            1 => "normal".into(),
            2 => "full output".into(),
            3 => "thinking".into(),
            4 => "full thinking".into(),
            n => format!("level {n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ported from ReplStateTests.cs (the token / last-response counters live on AgentSession
    //! in Rust and are covered there).
    use super::*;

    #[test]
    fn default_state_show_thinking_is_false() {
        assert!(!ReplState::default().show_thinking());
    }

    #[test]
    fn default_state_plan_mode_is_false() {
        assert!(!ReplState::default().plan_mode());
    }

    #[test]
    fn default_state_tokens_and_turns_are_zero() {
        assert_eq!(ReplState::default().total_turns, 0);
    }

    #[test]
    fn verbosity_can_be_set_and_derived_properties_work() {
        let mut s = ReplState {
            verbosity: 4,
            ..Default::default()
        };
        assert!(s.show_thinking() && s.show_full_thinking() && s.show_full_tool_output());
        s.verbosity = 0;
        assert!(!s.show_thinking());
        assert!(s.show_print_only());
        s.verbosity = 1;
        assert!(s.show_tool_calls());
        assert!(!s.show_full_tool_output());
    }

    #[test]
    fn plan_mode_can_be_toggled() {
        let mut s = ReplState::default();
        s.set_plan_mode(true);
        assert!(s.plan_mode());
        s.set_plan_mode(false);
        assert!(!s.plan_mode());
    }

    #[test]
    fn total_turns_accumulates() {
        let mut s = ReplState::default();
        s.total_turns += 1;
        assert_eq!(s.total_turns, 1);
        s.total_turns = 5;
        assert_eq!(s.total_turns, 5);
    }

    #[test]
    fn session_stopwatch_is_running() {
        let s = ReplState::default();
        std::thread::sleep(Duration::from_millis(2));
        assert!(s.session_duration() > Duration::ZERO);
    }

    #[test]
    fn current_session_id_default_is_null() {
        assert!(ReplState::default().current_session_id.is_none());
    }

    #[test]
    fn default_state_mode_is_normal() {
        assert_eq!(ReplState::default().mode, InputMode::Normal);
    }

    #[test]
    fn cycle_mode_normal_to_plan_to_ask_to_normal() {
        let mut s = ReplState::default();
        s.cycle_mode();
        assert_eq!(s.mode, InputMode::Plan);
        s.cycle_mode();
        assert_eq!(s.mode, InputMode::Ask);
        s.cycle_mode();
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn plan_mode_set_true_sets_mode_to_plan() {
        let mut s = ReplState::default();
        s.set_plan_mode(true);
        assert_eq!(s.mode, InputMode::Plan);
        assert!(s.plan_mode());
    }

    #[test]
    fn plan_mode_set_false_sets_mode_to_normal() {
        let mut s = ReplState::default();
        s.set_plan_mode(true);
        s.set_plan_mode(false);
        assert_eq!(s.mode, InputMode::Normal);
        assert!(!s.plan_mode());
    }

    #[test]
    fn plan_mode_is_false_in_ask_mode() {
        let s = ReplState {
            mode: InputMode::Ask,
            ..Default::default()
        };
        assert!(!s.plan_mode());
    }

    #[test]
    fn mode_set_directly_reflects_in_plan_mode() {
        let mut s = ReplState {
            mode: InputMode::Plan,
            ..Default::default()
        };
        assert!(s.plan_mode());
        s.mode = InputMode::Normal;
        assert!(!s.plan_mode());
        s.mode = InputMode::Ask;
        assert!(!s.plan_mode());
    }
}
