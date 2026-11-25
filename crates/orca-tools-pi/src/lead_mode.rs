//! agent.leadMode "auto" (`PiLeadModeTool`): the Lead sizes each new user request with one
//! call before it starts work. The host wires [`PiLeadModeTool::set_choose`]; without it the
//! tool answers "lead_mode is not available in this session."

use std::sync::{Arc, RwLock};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use serde_json::Value;

use crate::{PiToolInfo, fail, fail_with, get_string};

pub const TOOL_NAME: &str = "lead_mode";
pub const DELEGATE: &str = "delegate";
pub const HANDS_ON: &str = "hands-on";
pub const ANSWER: &str = "answer";

const DESCRIPTION: &str = concat!(
    "Call this once for every new user request, before you write, edit or delegate, to choose how you work on it (it can go in the same turn as reads). ",
    "delegate: big multi-part work \u{2014} 3 or more files and roughly 300 or more new lines, or an investigation plus an implementation; you split it, brief subagents, review their work with bash, and cannot write or edit files yourself. ",
    "hands-on: a job you can finish yourself \u{2014} one file, a small fix, or tightly coupled work under roughly 300 new lines even across several files (every part needs the same new core, so splitting costs more than it saves); you write and edit directly. ",
    "answer: a question or an explanation that changes nothing."
);

const SCHEMA: &str = r#"{"type":"object","required":["mode"],"properties":{"mode":{"type":"string","enum":["delegate","hands-on","answer"],"description":"delegate (big multi-part: 3+ files and ~300+ new lines, or investigation plus implementation), hands-on (one file, a small fix, or coupled work under ~300 new lines), answer (a question; nothing changes)"},"reason":{"type":"string","description":"Why, in at most 10 words"}}}"#;

/// The Lead's system-prompt addendum in auto mode, with "\n" line breaks (C# sends a raw
/// string literal: the host converts to the platform line break).
pub const AUTO_INSTRUCTIONS: &str = "You lead a team of subagents. Every new user request gets ONE lead_mode call that sizes it, before you write, edit or delegate:
- delegate: big multi-part work \u{2014} 3 or more files and roughly 300 or more new lines, or an investigation plus an implementation. Its result gives you the orchestrator workflow, and you cannot write or edit files for the rest of that request.
- hands-on: one file, a small fix, or tightly coupled work under roughly 300 new lines even across several files (every part needs the same new core). You do it yourself.
- answer: a question or an explanation; nothing changes.
Wall-clock time matters most, so:
- Read in parallel: put every read you need into ONE turn (several read/bash calls at once). Start with the file list and the spec, size the request from them, and call lead_mode in your next turn together with any reads you still need.
- Think briefly. To delegate, plan only the split: the pieces, the files each owns, the spec section each follows. The workers read the code and design it; do not work out code, formats or test cases yourself, and read only what you need to split the work.
- Hands-on: write whole files (several writes in one turn), then run the tests and the spec's sample in one bash call before you change anything. When they pass, stop and answer in at most 3 short lines, no lists.";

/// Result text for a hands-on choice.
pub const HANDS_ON_INSTRUCTIONS: &str = "Hands-on for this request: make the change yourself with write/edit and check it with bash (the full test suite and the spec's sample command in one call). When that passes, answer in at most 3 short lines. If it turns out big (roughly 300 or more new lines in parts that can be split), call lead_mode with delegate instead.";

/// Result text for an answer choice.
pub const ANSWER_INSTRUCTIONS: &str = "Answer directly from what you read; change nothing. If the user does want a change, call lead_mode again.";

/// Host hook: (normalized mode, reason) -> instructions for this request (async: the host may
/// ask the slot registry which model copies are free).
pub type ChooseFn =
    Arc<dyn Fn(String, Option<String>) -> crate::subagent::BoxFuture<String> + Send + Sync>;

#[derive(Default)]
pub struct PiLeadModeTool {
    choose: RwLock<Option<ChooseFn>>,
}

impl PiLeadModeTool {
    pub fn set_choose(&self, choose: Option<ChooseFn>) {
        *self.choose.write().unwrap() = choose;
    }
}

/// "delegate"/"hands-on"/"answer" from loose spellings, or `None`.
pub fn normalize(mode: Option<&str>) -> Option<&'static str> {
    let m = mode?.trim().to_lowercase().replace(['_', ' '], "-");
    match m.as_str() {
        "delegate" | "orchestrate" | "orchestrator" | "delegation" => Some(DELEGATE),
        "hands-on" | "handson" | "direct" | "solo" => Some(HANDS_ON),
        "answer" | "question" | "explain" => Some(ANSWER),
        _ => None,
    }
}

impl PiToolInfo for PiLeadModeTool {
    fn prompt_snippet(&self) -> String {
        "Choose how to work on this request: delegate, hands-on or answer (before you write or delegate)".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Every new user request gets one lead_mode call before you write, edit or delegate; its result tells you how to proceed.".into()]
    }
}

#[async_trait::async_trait]
impl Tool for PiLeadModeTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        crate::schema(SCHEMA)
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT
    }
    fn permission_arg_name(&self) -> &str {
        "mode"
    }
    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1F9ED}",
            short_description: "Mode",
            summary_arg_keys: &["mode"],
            aliases: &[],
        })
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), SCHEMA, arguments_json))
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(choose) = self.choose.read().unwrap().clone() else {
            return fail("lead_mode is not available in this session.");
        };
        let Some(mode) = normalize(get_string(args, "mode")) else {
            return fail_with(
                "mode must be one of: delegate, hands-on, answer.",
                ToolErrorCode::InvalidArguments,
            );
        };
        let reason = get_string(args, "reason")
            .filter(|r| !r.trim().is_empty())
            .map(str::trim);
        ToolResult::success(choose(mode.to_string(), reason.map(str::to_string)).await)
    }
}
