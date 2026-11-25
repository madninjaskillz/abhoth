//! Ported from `OpenOrca.Tools.Tests/UtilityToolTests.cs` and `AskUserToolTests.cs`.

use std::sync::Arc;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_common::{NEWLINE, TaskStore};
use orca_tools_utility::*;
use serde_json::{Value, json};

fn ctx() -> ToolContext {
    ToolContext::new(std::env::temp_dir())
}

async fn run(tool: &dyn Tool, args: Value) -> ToolResult {
    tool.execute(&args, &ctx()).await
}

/// Each test gets its own store (C# resets the static one around every test).
fn task_tool() -> TaskListTool {
    TaskListTool {
        store: Arc::new(TaskStore::default()),
    }
}

// ── ThinkTool ──

#[tokio::test]
async fn think_tool_returns_thought_verbatim() {
    let thought = "I need to: 1) read the file 2) find the bug 3) fix it";
    let r = run(&ThinkTool, json!({"thought": thought})).await;
    assert!(!r.is_error);
    assert_eq!(r.content, thought);
}

#[tokio::test]
async fn think_tool_handles_multiline_thoughts() {
    let r = run(&ThinkTool, json!({"thought": "Line1\nLine2\nLine3"})).await;
    assert!(!r.is_error);
    assert!(r.content.contains("Line1") && r.content.contains("Line3"));
}

// ── TaskListTool ──

#[tokio::test]
async fn task_list_tool_add_and_list() {
    let tool = task_tool();
    let add = run(&tool, json!({"action": "add", "task": "Write unit tests"})).await;
    assert!(!add.is_error);
    assert!(add.content.contains("#1") && add.content.contains("Write unit tests"));
    let list = run(&tool, json!({"action": "list"})).await;
    assert!(!list.is_error);
    assert!(list.content.contains("Write unit tests"));
    assert!(list.content.contains("0/1 completed"));
}

#[tokio::test]
async fn task_list_tool_complete_task() {
    let tool = task_tool();
    run(&tool, json!({"action": "add", "task": "Task A"})).await;
    run(&tool, json!({"action": "add", "task": "Task B"})).await;
    let r = run(&tool, json!({"action": "complete", "id": 1})).await;
    assert!(!r.is_error);
    assert!(r.content.contains("1/2 completed"));
    assert!(r.content.contains("[x] #1"));
    assert!(r.content.contains("[ ] #2"));
}

#[tokio::test]
async fn task_list_tool_remove_task() {
    let tool = task_tool();
    run(&tool, json!({"action": "add", "task": "Temporary"})).await;
    let r = run(&tool, json!({"action": "remove", "id": 1})).await;
    assert!(!r.is_error);
    assert!(r.content.contains("No tasks"));
}

#[tokio::test]
async fn task_list_tool_complete_invalid_id_returns_error() {
    let r = run(&task_tool(), json!({"action": "complete", "id": 999})).await;
    assert!(r.is_error);
    assert!(r.content.contains("not found"));
}

#[tokio::test]
async fn task_list_tool_add_without_description_returns_error() {
    let r = run(&task_tool(), json!({"action": "add"})).await;
    assert!(r.is_error);
    assert!(r.content.to_lowercase().contains("required"));
}

#[tokio::test]
async fn task_list_tool_empty_list() {
    let r = run(&task_tool(), json!({"action": "list"})).await;
    assert!(!r.is_error);
    assert!(r.content.contains("No tasks"));
}

#[tokio::test]
async fn task_list_exact_output() {
    let tool = task_tool();
    let r = run(&tool, json!({"action": "add", "task": "A"})).await;
    let nl = NEWLINE;
    assert_eq!(
        r.content,
        format!("Added task #1: A\n\nTasks: 0/1 completed{nl}{nl}  [ ] #1: A{nl}")
    );
    let r = run(&tool, json!({"action": "complete", "id": "1"})).await;
    assert!(r.content.starts_with("Completed task #1.\n\n"));
    let r = run(&tool, json!({"action": "remove"})).await;
    assert_eq!(r.content, "Task ID is required for 'remove'.");
}

// ── print / env ──

#[tokio::test]
async fn print_returns_message() {
    let r = run(&PrintTool, json!({"message": "hi there"})).await;
    assert_eq!(r.content, "hi there");
}

#[tokio::test]
async fn env_get_list_and_errors() {
    let r = run(
        &EnvTool,
        json!({"action": "get", "name": "ORCA_SURELY_UNSET_VAR"}),
    )
    .await;
    assert!(r.is_error);
    assert_eq!(
        r.content,
        "Environment variable 'ORCA_SURELY_UNSET_VAR' is not set."
    );
    let r = run(&EnvTool, json!({"action": "get", "name": "PATH"})).await;
    assert!(!r.is_error && r.content.to_uppercase().starts_with("PATH="));
    let r = run(&EnvTool, json!({"action": "list", "prefix": "pAtH"})).await;
    assert!(r.content.starts_with("Found "), "{}", r.content);
    let r = run(
        &EnvTool,
        json!({"action": "list", "prefix": "ORCA_SURELY_UNSET_"}),
    )
    .await;
    assert_eq!(
        r.content,
        "No environment variables found matching the criteria."
    );
    let r = run(&EnvTool, json!({"action": "get"})).await;
    assert_eq!(r.content, "'name' is required for action 'get'.");
    let r = run(&EnvTool, json!({"action": "x"})).await;
    assert_eq!(r.content, "Unknown action: x. Use 'get' or 'list'.");
}

// ── AskUserTool ──

struct FirstOption;

#[async_trait::async_trait]
impl UserPrompter for FirstOption {
    async fn ask(&self, _question: &str, options: &[String]) -> Result<String, String> {
        Ok(options[0].clone())
    }
}

fn ask_tool() -> AskUserTool {
    AskUserTool::new(Some(Arc::new(FirstOption)))
}

#[tokio::test]
async fn ask_user_normal_array_works() {
    let r = run(
        &ask_tool(),
        json!({"question": "Pick one", "options": ["A", "B"]}),
    )
    .await;
    assert!(!r.is_error);
    assert_eq!(r.content, "A");
}

#[tokio::test]
async fn ask_user_stringified_array_parsed_successfully() {
    let r = run(
        &ask_tool(),
        json!({"question": "Pick one", "options": "[\"Option A\", \"Option B\", \"Option C\"]"}),
    )
    .await;
    assert!(!r.is_error);
    assert_eq!(r.content, "Option A");
}

#[tokio::test]
async fn ask_user_invalid_string_options_returns_error() {
    let r = run(
        &ask_tool(),
        json!({"question": "Pick one", "options": "not valid json"}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("array"));
}

#[tokio::test]
async fn ask_user_too_few_options_returns_error() {
    let r = run(
        &ask_tool(),
        json!({"question": "Pick one", "options": ["Only one"]}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("2 options"));
}

#[tokio::test]
async fn ask_user_without_prompter_and_with_failure() {
    let r = run(
        &AskUserTool::new(None),
        json!({"question": "Q", "options": ["A", "B"]}),
    )
    .await;
    assert_eq!(r.content, "User prompting not configured.");

    struct Fails;
    #[async_trait::async_trait]
    impl UserPrompter for Fails {
        async fn ask(&self, _q: &str, _o: &[String]) -> Result<String, String> {
            Err("no terminal".into())
        }
    }
    let tool = AskUserTool::new(None);
    tool.configure(Arc::new(Fails));
    let r = run(&tool, json!({"question": "Q", "options": ["A", "B"]})).await;
    assert!(r.is_error);
    assert_eq!(r.content, "Failed to get user input: no terminal");
}
