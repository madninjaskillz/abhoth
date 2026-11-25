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
        }
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

    /// The turn was cancelled: the partial reply stays as it is on screen.
    pub fn cancel(&mut self) {
        self.markdown.clear();
        if self.in_reasoning {
            self.console.raw("[0m");
            self.in_reasoning = false;
        }
    }

    /// End the streamed output (before the turn footer); the suggested next prompt, if the
    /// model gave one.
    pub fn finish(&mut self) -> Option<String> {
        self.end_line();
        self.next_prompt.captured_prompt()
    }
}

impl AgentObserver for TurnObserver {
    fn on_text(&mut self, text: &str) {
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
        self.markdown.append(&shown);
    }

    fn on_reasoning(&mut self, text: &str) {
        if !self.state.show_thinking() {
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

    fn on_tool_end(&mut self, call: &ToolCall, result: &ToolResult) {
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
