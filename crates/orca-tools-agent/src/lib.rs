//! Agent tools (port of `OpenOrca.Tools.Agent`): spawn_agent, spawn_agents and report_progress.
//!
//! The tools are registered before the session exists; the host wires them to the session's
//! [`AgentManager`] afterwards through the shared [`AgentToolsHandle`] (C#'s `IConfigurable`).
//! Unwired, they answer as the C# tools do without a delegate.

use std::sync::{Arc, RwLock};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_core::orchestration::{AgentManager, CancelToken, ParallelAgentSpec};
use serde_json::Value;

/// Where the agent tools find the session's manager (set once the session is built).
#[derive(Clone, Default)]
pub struct AgentToolsHandle(Arc<RwLock<Option<Arc<AgentManager>>>>);

impl AgentToolsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, manager: Option<Arc<AgentManager>>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = manager;
    }

    pub fn get(&self) -> Option<Arc<AgentManager>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The three tools, wired to `handle`.
pub fn all_tools(handle: &AgentToolsHandle) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SpawnAgentTool {
            handle: handle.clone(),
        }),
        Arc::new(SpawnAgentsTool {
            handle: handle.clone(),
        }),
        Arc::new(ReportProgressTool {
            handle: handle.clone(),
        }),
    ]
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("static schema is valid JSON")
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

// ── spawn_agent ──────────────────────────────────────────────────────────────

pub struct SpawnAgentTool {
    handle: AgentToolsHandle,
}

pub const SPAWN_AGENT_DESCRIPTION: &str = concat!(
    "Launch a specialized sub-agent to handle a focused task autonomously with its own conversation context.\n",
    "\n",
    "Built-in agent types:\n",
    "- explore: Fast read-only codebase exploration specialist. Has access to: read_file, list_directory, glob, grep, think. ",
    "Use for finding files, searching code, understanding project structure, tracing usages, and answering questions about the codebase. ",
    "PREFER this over calling glob/grep/read_file directly when the task requires broad exploration or more than 3 search queries.\n",
    "- plan: Architecture and implementation planning. Has access to: read_file, list_directory, glob, grep, think, web_search, web_fetch. ",
    "Use for designing solutions, researching approaches, and producing step-by-step implementation plans.\n",
    "- bash: Command execution specialist. Has access to: bash, read_file, think, get_process_output, start_background_process, stop_process, env. ",
    "Use for running builds, tests, scripts, and managing processes.\n",
    "- review: Code review and diff analysis. Has access to: read_file, list_directory, glob, grep, git_diff, git_log, git_status, think. ",
    "Use for examining changes, checking code quality, and reviewing diffs.\n",
    "- general: Full tool access (default). Use for pieces of work that write or change code, or when no specialized type fits.\n",
    "\n",
    "Additional agent types may be available from .orca/agents/ definitions.\n",
    "\n",
    "When to use spawn_agent (prefer delegating over doing it yourself when any of these fit):\n",
    "- Multi-part work (several files or components): split it into independent pieces and run them with spawn_agents ",
    "(general agents for code), then integrate and verify the results yourself\n",
    "- Broad codebase exploration (\"find all usages of X\", \"what is the project structure?\", \"how does Y work?\")\n",
    "- Tasks requiring multiple rounds of search/read (more than 3 queries)\n",
    "- Independent subtasks that can run in parallel with your main work (use spawn_agents for 2+)\n",
    "- Research, builds/tests, or code review before/after making changes\n",
    "\n",
    "When NOT to use spawn_agent:\n",
    "- Reading a specific file you already know the path to \u{2014} use read_file directly\n",
    "- Searching for a specific class or function name \u{2014} use glob or grep directly\n",
    "- Investigating code within 2-3 known files \u{2014} use read_file directly\n",
    "- Simple single-step tasks that don't need their own conversation context\n",
    "\n",
    "Examples:\n",
    "- User asks \"find all usages of AgentOrchestrator\" \u{2192} spawn explore agent\n",
    "- User asks \"what is the codebase structure?\" \u{2192} spawn explore agent\n",
    "- User asks \"where are errors from the client handled?\" \u{2192} spawn explore agent\n",
    "- User asks \"run the tests and report results\" \u{2192} spawn bash agent\n",
    "- User asks \"review the changes on this branch\" \u{2192} spawn review agent\n",
    "- User asks \"design how we should implement feature X\" \u{2192} spawn plan agent\n",
    "\n",
    "Each agent gets a lettered, coloured progress bar in the footer (Agent A, Agent B, \u{2026}) labelled with `label`. ",
    "The result comes back tagged with the agent's letter \u{2014} refer to it as \"Agent A\" etc. when you report it.\n",
    "\n",
    "Follow-up: pass `to` with a finished agent's letter (e.g. \"A\") to send it more work, such as a fix to code it wrote; ",
    "it resumes with its own context, so the task only needs what is new."
);

#[async_trait::async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        SPAWN_AGENT_DESCRIPTION
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::LONG_RUNNING
    }

    fn affected_file_paths(&self, _args: &Value) -> Vec<String> {
        Vec::new()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f916}",
            short_description: "Spawn sub-agent",
            summary_arg_keys: &["label", "agent_type", "task"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "task": {
                "type": "string",
                "description": "A clear, detailed description of the task for the sub-agent. Include all necessary context — the agent starts a fresh conversation and cannot see your prior messages."
            },
            "agent_type": {
                "type": "string",
                "description": "The type of sub-agent to spawn. Built-in types: explore, plan, bash, review, general (default). Custom types from .orca/agents/ are also available. Determines the system prompt and available tools."
            },
            "label": {
                "type": "string",
                "description": "Optional short name for the agent's progress bar, at most 4 words (e.g. \"Scan auth code\"). Derived from the task when omitted."
            },
            "to": {
                "type": "string",
                "description": "Follow-up: the letter of an agent that already finished (e.g. \"A\"). The task is sent to it and it resumes with its own context instead of a new agent starting."
            }
        },
        "required": ["task"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let task = match args.get("task") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return ToolResult::error(format!(
                    "Sub-agent failed: The requested operation requires an element of type 'String', but the target element has type '{}'.",
                    json_kind(other)
                ));
            }
            None => return ToolResult::error("Sub-agent failed: 'task' is required."),
        };
        let agent_type = str_arg(args, "agent_type").unwrap_or("general");
        let label = str_arg(args, "label");
        let to = str_arg(args, "to");
        let Some(manager) = self.handle.get() else {
            return ToolResult::error("Agent spawning not configured.");
        };
        let ct = CancelToken::new();
        if let Some(to) = to.filter(|t| !t.trim().is_empty()) {
            return ToolResult::success(manager.run_follow_up_tool(to.trim(), &task, &ct).await);
        }
        ToolResult::success(manager.run_spawn_tool(&task, agent_type, label, &ct).await)
    }
}

/// `JsonValueKind` names (for the C# type-mismatch message).
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

// ── spawn_agents ─────────────────────────────────────────────────────────────

pub struct SpawnAgentsTool {
    handle: AgentToolsHandle,
}

pub const SPAWN_AGENTS_DESCRIPTION: &str = concat!(
    "Launch MULTIPLE sub-agents in parallel, each with its own task and optional agent type ",
    "(explore, plan, bash, review, general \u{2014} see spawn_agent for descriptions). ",
    "Use when 2+ independent subtasks can run at the same time, e.g. building the separate components ",
    "of multi-part work (general agents, one owner per file), exploring different parts of a codebase, ",
    "or researching while running tests. List the biggest task first: it starts first, on a model copy of its own. Each agent gets a fresh conversation ",
    "and cannot see your messages or each other \u{2014} include all needed context in each task. ",
    "Results return as one markdown report with a section per agent, each headed by the agent's footer letter ",
    "(Agent A, Agent B, \u{2026}) \u{2014} refer to agents that way when you report their results. ",
    "Prefer spawn_agent for a single task."
);

#[async_trait::async_trait]
impl Tool for SpawnAgentsTool {
    fn name(&self) -> &str {
        "spawn_agents"
    }

    fn description(&self) -> &str {
        SPAWN_AGENTS_DESCRIPTION
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::LONG_RUNNING
    }

    fn affected_file_paths(&self, _args: &Value) -> Vec<String> {
        Vec::new()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f465}",
            short_description: "Spawn parallel sub-agents",
            summary_arg_keys: &["agents"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "agents": {
                "type": "array",
                "description": "The agents to run in parallel (2-8).",
                "items": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Full, self-contained task description for this agent." },
                        "agent_type": { "type": "string", "description": "explore, plan, bash, review, or general (default)." },
                        "label": { "type": "string", "description": "Optional progress-bar name, at most 4 words. Derived from the task when omitted." }
                    },
                    "required": ["task"]
                }
            }
        },
        "required": ["agents"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(manager) = self.handle.get() else {
            return ToolResult::error("Parallel agent spawning not configured.");
        };
        let Some(Value::Array(entries)) = args.get("agents") else {
            return ToolResult::error_with(
                "'agents' must be an array of {task, agent_type} objects.",
                ToolErrorCode::InvalidArguments,
            );
        };
        let specs = parse_specs(entries);
        if specs.is_empty() {
            return ToolResult::error_with(
                "No valid agent specs \u{2014} each entry needs a non-empty 'task'.",
                ToolErrorCode::InvalidArguments,
            );
        }
        if specs.len() > 8 {
            return ToolResult::error_with(
                format!("Too many agents ({}); maximum is 8 per call.", specs.len()),
                ToolErrorCode::InvalidArguments,
            );
        }
        match manager.run_parallel_tool(&specs, &CancelToken::new()).await {
            Ok(digest) => ToolResult::success(digest),
            Err(e) => ToolResult::error(format!("Parallel spawn failed: {e}")),
        }
    }
}

/// The valid entries of an `agents` array (each needs a non-empty string task).
pub fn parse_specs(entries: &[Value]) -> Vec<ParallelAgentSpec> {
    entries
        .iter()
        .filter_map(|e| {
            let task = e.get("task")?.as_str().filter(|t| !t.is_empty())?;
            Some(ParallelAgentSpec {
                task: task.to_string(),
                agent_type: str_arg(e, "agent_type").unwrap_or("general").to_string(),
                label: str_arg(e, "label").map(str::to_string),
                size: None,
                after: None,
            })
        })
        .collect()
}

// ── report_progress ──────────────────────────────────────────────────────────

pub struct ReportProgressTool {
    handle: AgentToolsHandle,
}

pub const REPORT_PROGRESS_DESCRIPTION: &str = concat!(
    "Report how far along you are with the current task; the user sees it as a progress bar in the footer. ",
    "Call it at real milestones (not every step), ideally in the same response as another tool call. ",
    "percent is your honest estimate for the whole task (0-100); label is at most 4 words saying what you are on now."
);

#[async_trait::async_trait]
impl Tool for ReportProgressTool {
    fn name(&self) -> &str {
        "report_progress"
    }

    fn description(&self) -> &str {
        REPORT_PROGRESS_DESCRIPTION
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn affected_file_paths(&self, _args: &Value) -> Vec<String> {
        Vec::new()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4ca}",
            short_description: "Report progress",
            summary_arg_keys: &["percent", "label"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "percent": {
                "type": "integer",
                "description": "Estimated completion of the whole task, 0-100."
            },
            "label": {
                "type": "string",
                "description": "Optional: what you are doing now, at most 4 words (e.g. \"running the tests\")."
            }
        },
        "required": ["percent"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(manager) = self.handle.get() else {
            return ToolResult::error("Progress reporting is not configured.");
        };
        let Some(percent) = try_read_percent(args) else {
            return ToolResult::error_with(
                "'percent' must be a number from 0 to 100.",
                ToolErrorCode::InvalidArguments,
            );
        };
        let label = str_arg(args, "label");
        ToolResult::success(
            manager
                .orchestrator()
                .report_progress(percent.clamp(0, 100), label),
        )
    }
}

/// Accepts 40, 40.5, "40", "40%" and fractions 0..1 given as 0.4 (`TryReadPercent`).
pub fn try_read_percent(args: &Value) -> Option<i32> {
    let p = args.as_object()?.get("percent")?;
    let value = match p {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().trim_end_matches('%').trim().parse::<f64>().ok()?,
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
    // Math.Round: half to even.
    Some(value.min(100.0).round_ties_even() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn try_read_percent_accepts_numbers_strings_and_fractions() {
        assert_eq!(try_read_percent(&json!({"percent": 40})), Some(40));
        assert_eq!(try_read_percent(&json!({"percent": 40.5})), Some(40));
        assert_eq!(try_read_percent(&json!({"percent": 41.5})), Some(42));
        assert_eq!(try_read_percent(&json!({"percent": "40%"})), Some(40));
        assert_eq!(try_read_percent(&json!({"percent": " 40 "})), Some(40));
        assert_eq!(try_read_percent(&json!({"percent": 0.4})), Some(40));
        assert_eq!(try_read_percent(&json!({"percent": 250})), Some(100));
        assert_eq!(try_read_percent(&json!({"percent": 0})), Some(0));
        assert_eq!(try_read_percent(&json!({"percent": -1})), None);
        assert_eq!(try_read_percent(&json!({"percent": "lots"})), None);
        assert_eq!(try_read_percent(&json!({"percent": true})), None);
        assert_eq!(try_read_percent(&json!({})), None);
        assert_eq!(try_read_percent(&json!([1])), None);
    }

    #[test]
    fn parse_specs_skips_entries_without_a_task_and_defaults_the_type() {
        let specs = parse_specs(&[
            json!({"task": "a"}),
            json!({"task": ""}),
            json!({"agent_type": "explore"}),
            json!("nope"),
            json!({"task": "b", "agent_type": "explore", "label": "Scan"}),
        ]);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].agent_type, "general");
        assert_eq!(specs[1].agent_type, "explore");
        assert_eq!(specs[1].label.as_deref(), Some("Scan"));
    }
}
