//! Utility tools (port of `OpenOrca.Tools.Utility`: env, print, task_list, think) and the
//! interactive `ask_user` (port of `OpenOrca.Tools.Interactive`).

use std::sync::{Arc, RwLock};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{TaskStore, append_line, get_i32_lenient, get_str, required_str};
use serde_json::Value;

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

/// The Utility tools (class names sorted, as C# registers them).
pub fn utility_tools() -> Vec<Arc<dyn Tool>> {
    utility_tools_with(TaskStore::shared())
}

/// [`utility_tools`] with its own task list (one per web UI session, D140).
pub fn utility_tools_with(tasks: Arc<TaskStore>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(EnvTool),
        Arc::new(PrintTool),
        Arc::new(TaskListTool { store: tasks }),
        Arc::new(ThinkTool),
    ]
}

/// The Interactive plugin: `ask_user`, answered by `prompter` (none: the tool reports that
/// prompting is not configured, as in C#).
pub fn interactive_tools(prompter: Option<Arc<dyn UserPrompter>>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AskUserTool::new(prompter))]
}

// ── env ──

pub struct EnvTool;

#[async_trait::async_trait]
impl Tool for EnvTool {
    fn name(&self) -> &str {
        "env"
    }

    fn description(&self) -> &str {
        "Inspect environment variables. Use action 'get' to get a specific variable, 'list' to list all (or filter by prefix)."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2022}",
            short_description: "Environment variable",
            summary_arg_keys: &["name"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["get", "list"],
                "description": "Action to perform: 'get' a specific variable or 'list' all variables"
            },
            "name": {
                "type": "string",
                "description": "Variable name (required for 'get')"
            },
            "prefix": {
                "type": "string",
                "description": "Filter variables by prefix (optional for 'list')"
            }
        },
        "required": ["action"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let action = match required_str(args, "action") {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e),
        };
        match action {
            "get" => {
                let Some(name) = get_str(args, "name") else {
                    return ToolResult::error("'name' is required for action 'get'.");
                };
                match std::env::var_os(name) {
                    Some(v) => ToolResult::success(format!("{name}={}", v.to_string_lossy())),
                    None => ToolResult::error(format!("Environment variable '{name}' is not set.")),
                }
            }
            "list" => {
                let prefix = get_str(args, "prefix");
                let mut sb = String::new();
                let mut count = 0;
                for (key, value) in std::env::vars_os() {
                    let key = key.to_string_lossy();
                    if let Some(p) = prefix
                        && !starts_with_ignore_case(&key, p)
                    {
                        continue;
                    }
                    append_line(&mut sb, &format!("{key}={}", value.to_string_lossy()));
                    count += 1;
                    if count >= 200 {
                        append_line(&mut sb, "... (truncated at 200 entries)");
                        break;
                    }
                }
                if count > 0 {
                    ToolResult::success(format!("Found {count} variables:\n{sb}"))
                } else {
                    ToolResult::success("No environment variables found matching the criteria.")
                }
            }
            other => ToolResult::error(format!("Unknown action: {other}. Use 'get' or 'list'.")),
        }
    }
}

/// `StartsWith(prefix, OrdinalIgnoreCase)`.
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    let mut it = s.chars();
    prefix.chars().all(|p| {
        it.next()
            .is_some_and(|c| c.to_uppercase().eq(p.to_uppercase()))
    })
}

// ── print ──

pub struct PrintTool;

#[async_trait::async_trait]
impl Tool for PrintTool {
    fn name(&self) -> &str {
        "print"
    }

    fn description(&self) -> &str {
        "Display a message to the user. Use this to show results, status updates, or explanations. The message content will always be visible to the user regardless of their verbosity setting."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::ALWAYS_VISIBLE
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2022}",
            short_description: "Print message",
            summary_arg_keys: &["message"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "The message to display to the user"
            }
        },
        "required": ["message"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success(get_str(args, "message").unwrap_or(""))
    }
}

// ── task_list ──

pub struct TaskListTool {
    pub store: Arc<TaskStore>,
}

impl Default for TaskListTool {
    /// The process-wide store, as in C# (sub-agents share it).
    fn default() -> Self {
        Self {
            store: TaskStore::shared(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        "Manage a session-scoped task list to track progress on complex work. Actions: add (create task), list (show all), complete (mark done), remove (delete). Helps organize multi-step tasks and show progress."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2022}",
            short_description: "Task list",
            summary_arg_keys: &[],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["add", "list", "complete", "remove"],
                "description": "Task action. Defaults to 'list'."
            },
            "task": {
                "type": "string",
                "description": "Task description (required for 'add')"
            },
            "id": {
                "type": "integer",
                "description": "Task ID (required for 'complete' and 'remove')"
            }
        }
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let store = &self.store;
        match get_str(args, "action").unwrap_or("list") {
            "add" => match get_str(args, "task").filter(|t| !t.trim().is_empty()) {
                None => ToolResult::error("Task description is required for 'add'."),
                Some(task) => {
                    let id = store.add(task);
                    ToolResult::success(format!("Added task #{id}: {task}\n\n{}", store.list()))
                }
            },
            action @ ("complete" | "remove") => {
                let Some(id) = args.get("id") else {
                    return ToolResult::error(format!("Task ID is required for '{action}'."));
                };
                let id = get_i32_lenient(Some(id), 0);
                let (found, done) = if action == "complete" {
                    (store.complete(id), "Completed")
                } else {
                    (store.remove(id), "Removed")
                };
                if !found {
                    return ToolResult::error(format!("Task #{id} not found."));
                }
                ToolResult::success(format!("{done} task #{id}.\n\n{}", store.list()))
            }
            _ => ToolResult::success(store.list()),
        }
    }
}

// ── think ──

pub struct ThinkTool;

#[async_trait::async_trait]
impl Tool for ThinkTool {
    fn name(&self) -> &str {
        "think"
    }

    fn description(&self) -> &str {
        "Use this tool to think through a problem step-by-step before acting. Returns the thought verbatim. No side effects. Especially useful for planning complex multi-step tasks, analyzing errors, or reasoning about code structure before making changes."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2022}",
            short_description: "Think",
            summary_arg_keys: &["thought"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "thought": {
                "type": "string",
                "description": "Your step-by-step reasoning, analysis, or plan"
            }
        },
        "required": ["thought"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        match required_str(args, "thought") {
            Ok(t) => ToolResult::success(t),
            Err(e) => ToolResult::error(e),
        }
    }
}

// ── ask_user ──

/// Puts a multiple-choice question to the user (the C# `UserPrompter` callback, which the
/// host wires: a console selection prompt, or the web UI's question picker).
#[async_trait::async_trait]
pub trait UserPrompter: Send + Sync {
    /// The chosen option (or a typed answer); `Err` holds the failure message.
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String>;
}

pub struct AskUserTool {
    prompter: RwLock<Option<Arc<dyn UserPrompter>>>,
}

impl AskUserTool {
    pub fn new(prompter: Option<Arc<dyn UserPrompter>>) -> Self {
        Self {
            prompter: RwLock::new(prompter),
        }
    }

    /// `IConfigurable.Configure`: install or replace the prompter.
    pub fn configure(&self, prompter: Arc<dyn UserPrompter>) {
        *self.prompter.write().unwrap() = Some(prompter);
    }
}

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a multiple-choice question. Use when you need clarification, want user confirmation before a risky action, or need them to choose between approaches. 2-10 options."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::INTERACTIVE
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{2022}",
            short_description: "Ask user",
            summary_arg_keys: &["question"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": "The question to ask the user"
            },
            "options": {
                "type": "array",
                "items": { "type": "string" },
                "description": "2-10 choices for the user to select from"
            }
        },
        "required": ["question", "options"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let question = match required_str(args, "question") {
            Ok(q) => q,
            Err(e) => return ToolResult::error(e),
        };
        let Some(options) = args.get("options") else {
            return ToolResult::error("Missing required argument: options");
        };
        const NOT_ARRAY: &str = "options must be a JSON array of strings.";
        // Local models sometimes send the array as a JSON string; parse it.
        let parsed;
        let options = match options {
            Value::String(raw) => match serde_json::from_str::<Value>(raw) {
                Ok(v) => {
                    parsed = v;
                    &parsed
                }
                Err(_) => return ToolResult::error(NOT_ARRAY),
            },
            other => other,
        };
        let Some(items) = options.as_array() else {
            return ToolResult::error(NOT_ARRAY);
        };
        let options: Vec<String> = items
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .collect();
        if options.len() < 2 {
            return ToolResult::error("At least 2 options are required.");
        }
        if options.len() > 10 {
            return ToolResult::error("A maximum of 10 options is allowed.");
        }
        let prompter = self.prompter.read().unwrap().clone();
        let Some(prompter) = prompter else {
            return ToolResult::error("User prompting not configured.");
        };
        match prompter.ask(question, &options).await {
            Ok(answer) => ToolResult::success(answer),
            Err(e) => ToolResult::error(format!("Failed to get user input: {e}")),
        }
    }
}
