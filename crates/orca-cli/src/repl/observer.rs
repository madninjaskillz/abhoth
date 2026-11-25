//! What the user sees during a turn: streamed text (tool-call and next-prompt tags filtered
//! out), thinking, tool calls and their results (`ToolCallRenderer`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use orca_abstractions::{Tool, ToolResult};
use orca_core::agent::AgentObserver;
use orca_core::client::ToolCall;

use crate::render::markdown::MarkdownStreamRenderer;
use crate::render::tag_filters::{NextPromptTagFilter, ToolCallTagFilter};
use crate::render::tool_call::ToolCallRenderer;
use crate::ui::Console;
use crate::ui::markup::escape;

use super::state::ReplState;

pub struct TurnObserver {
    console: Console,
    state: ReplState,
    tools: ToolCallRenderer,
    /// The tools by name (for summaries), taken from the session's registry.
    registry: HashMap<String, Arc<dyn Tool>>,
    started: HashMap<String, Instant>,
    think: orca_core::agent::think_tag_filter::ThinkTagFilter,
    tag_filter: ToolCallTagFilter,
    next_prompt: NextPromptTagFilter,
    markdown: MarkdownStreamRenderer,
    /// Text streamed since the last line break (to end the line before a tool header).
    mid_line: bool,
    in_reasoning: bool,
    /// `StreamingRenderer.Suppressed`: model text and thinking are not shown (the benchmark
    /// runner); tool calls still are.
    suppressed: bool,
    /// A tool ran this turn: from then on the text is collected in `wrap_up` and shown once
    /// at the end of the turn (C# `ToolsExecuted` / `SummaryBuffer` / `RenderFinalSummary`).
    tools_ran: bool,
    wrap_up: String,
}

impl TurnObserver {
    pub fn new(console: Console, state: ReplState, tools: &[Arc<dyn Tool>]) -> Self {
        Self {
            tools: ToolCallRenderer::new(console.clone()),
            markdown: MarkdownStreamRenderer::new(console.clone()),
            console,
            state,
            registry: tools
                .iter()
                .map(|t| (t.name().to_string(), t.clone()))
                .collect(),
            started: HashMap::new(),
            think: orca_core::agent::think_tag_filter::ThinkTagFilter::new(),
            tag_filter: ToolCallTagFilter::new(),
            next_prompt: NextPromptTagFilter::new(),
            mid_line: false,
            in_reasoning: false,
            suppressed: false,
            tools_ran: false,
            wrap_up: String::new(),
        }
    }

    /// Hide streamed text and thinking (`StreamingRenderer.Suppressed = true`).
    pub fn suppressed(mut self) -> Self {
        self.suppressed = true;
        self
    }

    fn end_line(&mut self) {
        self.markdown.finish();
        if self.in_reasoning {
            self.console.raw("\x1b[0m");
            self.in_reasoning = false;
        }
        if self.mid_line {
            self.console.raw("\n");
            self.mid_line = false;
        }
    }

    /// The turn was cancelled: the partial reply stays as it is on screen (the collected
    /// wrap-up is shown, as C# renders its summary buffer in `finally`).
    pub fn cancel(&mut self) {
        self.markdown.clear();
        if self.in_reasoning {
            self.console.raw("[0m");
            self.in_reasoning = false;
        }
        self.show_wrap_up();
    }

    /// End the streamed output (before the turn footer); the suggested next prompt, if the
    /// model gave one.
    pub fn finish(&mut self) -> Option<String> {
        self.end_line();
        self.show_wrap_up();
        self.next_prompt.captured_prompt()
    }

    /// `RenderFinalSummary`: the text collected since a tool ran, rendered once.
    fn show_wrap_up(&mut self) {
        let text = std::mem::take(&mut self.wrap_up);
        if !text.is_empty() && !self.suppressed {
            self.markdown.append(&text);
            self.markdown.finish();
        }
    }
}

impl AgentObserver for TurnObserver {
    fn on_text(&mut self, text: &str) {
        if self.suppressed {
            return;
        }
        if self.in_reasoning {
            self.end_line();
        }
        let (thinking, visible) = self.think.process(text);
        if !thinking.is_empty() {
            self.on_reasoning(&thinking);
            if self.in_reasoning {
                self.end_line();
            }
        }
        let shown = self.tag_filter.filter(&visible);
        let shown = self.next_prompt.filter(&shown);
        if shown.is_empty() {
            return;
        }
        if self.tools_ran {
            self.wrap_up.push_str(&shown);
            return;
        }
        self.markdown.append(&shown);
    }

    fn on_reasoning(&mut self, text: &str) {
        if self.suppressed || !self.state.show_thinking() {
            return;
        }
        if !self.in_reasoning {
            self.end_line();
            self.console.raw("\x1b[2m");
            self.in_reasoning = true;
        }
        self.console.raw(text);
        self.mid_line = !text.ends_with('\n');
    }

    fn on_tool_start(&mut self, call: &ToolCall) {
        self.end_line();
        self.started.insert(call.id.clone(), Instant::now());
        let tool = self.registry.get(&call.name).cloned();
        self.tools
            .render_tool_call(&call.name, &call.arguments, tool.as_deref(), &self.state);
    }

    fn on_tool_output(&mut self, line: &str) {
        if self.state.show_full_tool_output() {
            self.console
                .markup_line(&format!("    [dim]{}[/]", escape(line)));
        }
    }

    fn on_wrap_up(&mut self, replacement: Option<&str>) {
        self.wrap_up.clear();
        if let Some(text) = replacement {
            self.wrap_up.push_str(text);
        }
    }

    fn on_tool_end(&mut self, call: &ToolCall, result: &ToolResult) {
        self.tools_ran = true;
        let elapsed = self.started.remove(&call.id).map(|t| t.elapsed());
        let tool = self.registry.get(&call.name).cloned();
        self.tools.render_tool_result(
            &call.name,
            &result.content,
            result.is_error,
            elapsed,
            Some(&call.arguments),
            tool.as_deref(),
            &self.state,
        );
    }

    fn on_notice(&mut self, message: &str) {
        self.end_line();
        self.console
            .markup_line(&format!("[yellow]{}[/]", escape(message)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }
    }

    /// Text after a tool ran is collected and shown once at the end (C# `SummaryBuffer`): a
    /// requested summary replaces the wrap-up streamed before it (cross-check 225).
    #[test]
    fn text_after_a_tool_shows_once_at_the_end() {
        let (console, buf) = Console::buffer();
        let mut o = TurnObserver::new(console, ReplState::default(), &[]);
        o.on_text("Reading. ");
        o.on_tool_end(&call(), &ToolResult::success("ok"));
        o.on_text("Wrap-up one.");
        o.on_wrap_up(None);
        o.on_text("Summary two.");
        let before = Console::buffer_text(&buf);
        assert!(!before.contains("Wrap-up one.") && !before.contains("Summary two."));
        o.finish();
        let after = Console::buffer_text(&buf);
        assert!(after.contains("Reading.") && !after.contains("Wrap-up one."));
        assert_eq!(after.matches("Summary two.").count(), 1);
    }

    #[test]
    fn a_standing_wrap_up_replaces_the_collected_text() {
        let (console, buf) = Console::buffer();
        let mut o = TurnObserver::new(console, ReplState::default(), &[]);
        o.on_tool_end(&call(), &ToolResult::success("ok"));
        o.on_text("Verified.");
        o.on_wrap_up(Some("The earlier wrap-up."));
        o.finish();
        let out = Console::buffer_text(&buf);
        assert!(out.contains("The earlier wrap-up.") && !out.contains("Verified."));
    }
}
