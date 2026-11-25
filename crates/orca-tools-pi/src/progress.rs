//! Pi-style `progress` tool (an OpenOrca addition, `PiProgressTool`): moves the caller's
//! progress bar. The host wires [`PiProgressTool::set_report`] (the orchestrator knows which
//! agent the call came from); without it the tool answers "Progress is not available".

use std::sync::{Arc, RwLock};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use serde_json::Value;

use crate::{PiToolInfo, fail, fail_with, get_string};

pub const TOOL_NAME: &str = "progress";

const SCHEMA: &str = r#"{"type":"object","required":["percent"],"properties":{"percent":{"type":"number","description":"Estimated completion of the whole task, 0-100"},"note":{"type":"string","description":"What you are doing now, at most 4 words (optional)"}}}"#;

/// Host hook: (percent 0-100, note) -> result text.
pub type ReportFn = Arc<dyn Fn(i32, Option<&str>) -> String + Send + Sync>;

#[derive(Default)]
pub struct PiProgressTool {
    report: RwLock<Option<ReportFn>>,
}

impl PiProgressTool {
    pub fn set_report(&self, report: Option<ReportFn>) {
        *self.report.write().unwrap() = report;
    }

    pub fn has_report(&self) -> bool {
        self.report.read().unwrap().is_some()
    }
}

impl PiToolInfo for PiProgressTool {
    fn prompt_snippet(&self) -> String {
        "Show the user how far along you are (progress bar)".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use progress at real milestones of longer tasks, in the same response as another tool call; skip it for quick tasks.".into()]
    }
}

#[async_trait::async_trait]
impl Tool for PiProgressTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        "Update your progress bar shown to the user. percent is your estimate for the whole task (0-100); note is what you are doing now, at most 4 words."
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
        "percent"
    }
    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1F4CA}",
            short_description: "Progress",
            summary_arg_keys: &["percent", "note"],
            aliases: &[],
        })
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), SCHEMA, arguments_json))
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(report) = self.report.read().unwrap().clone() else {
            return fail("Progress is not available in this session.");
        };
        let Some(percent) = read_percent(args) else {
            return fail_with(
                "percent must be a number from 0 to 100.",
                ToolErrorCode::InvalidArguments,
            );
        };
        let note = get_string(args, "note").filter(|n| !n.trim().is_empty());
        ToolResult::success(report(percent, note))
    }
}

/// Accepts 40, 40.5, "40", "40%" and fractions 0..1 given as 0.4; clamps to 100.
pub fn read_percent(args: &Value) -> Option<i32> {
    let p = args.as_object()?.get("percent")?;
    let value = match p {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().trim_end_matches('%').parse::<f64>().ok()?,
        _ => return None,
    };
    if value.is_nan() || value < 0.0 {
        return None;
    }
    let value = if value > 0.0 && value < 1.0 {
        value * 100.0
    } else {
        value
    };
    Some(crate::job_tool::round_half_even(value.min(100.0)) as i32)
}
