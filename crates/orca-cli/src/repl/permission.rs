//! `ConsolePermissionPrompter`: asks at the console whether a tool call may run.

use orca_core::agent::permissions::{PermissionDecision, PermissionPrompter};

use crate::ui::Console;
use crate::ui::markup::escape;
use crate::ui::prompt;

pub const CHOICES: [&str; 3] = ["Yes", "Yes to all (this tool, this session)", "No"];

pub struct ConsolePermissionPrompter {
    console: Console,
}

impl ConsolePermissionPrompter {
    pub fn new(console: Console) -> Self {
        Self { console }
    }
}

/// `ToolCallRenderer.RenderPermissionPrompt`.
pub fn permission_prompt_markup(tool_name: &str, risk: &str) -> String {
    format!(
        "  [yellow]⎿  ⚠ [bold]{}[/] needs approval[/] [dim](risk: {})[/]",
        escape(tool_name),
        escape(&risk.to_lowercase())
    )
}

/// The decision for the picked choice (Ctrl+C / Esc / no answer = No).
pub fn decision_for(choice: Option<usize>) -> PermissionDecision {
    match choice {
        Some(0) => PermissionDecision::Approved,
        Some(1) => PermissionDecision::ApproveAll,
        _ => PermissionDecision::Denied,
    }
}

impl PermissionPrompter for ConsolePermissionPrompter {
    fn prompt(&mut self, tool_name: &str, risk: &str) -> PermissionDecision {
        let console = self.console.clone();
        let ask = move || {
            console.markup_line(&permission_prompt_markup(tool_name, risk));
            let choices: Vec<String> = CHOICES.iter().map(|c| c.to_string()).collect();
            decision_for(prompt::select(&console, "Allow this tool call?", &choices))
        };
        // The prompt blocks on the console; let the runtime move other tasks off this thread.
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(ask)
            }
            _ => ask(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_map_to_decisions() {
        assert_eq!(decision_for(Some(0)), PermissionDecision::Approved);
        assert_eq!(decision_for(Some(1)), PermissionDecision::ApproveAll);
        assert_eq!(decision_for(Some(2)), PermissionDecision::Denied);
        assert_eq!(decision_for(None), PermissionDecision::Denied);
    }

    #[test]
    fn prompt_line_names_tool_and_risk() {
        assert_eq!(
            crate::ui::markup::remove(&permission_prompt_markup("bash", "Dangerous")),
            "  ⎿  ⚠ bash needs approval (risk: dangerous)"
        );
    }
}
