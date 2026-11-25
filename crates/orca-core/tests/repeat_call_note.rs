//! Port of `tests/OpenOrca.Core.Tests/RepeatCallNoteTests.cs`.

use orca_core::chat::repeat_call_note::note_for;
use orca_core::client::{ChatMessage, ToolCall};
use serde_json::{Value, json};

fn tool(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn bash(id: &str, command: &str) -> ToolCall {
    tool(id, "bash", json!({ "command": command }))
}

fn assistant(calls: Vec<ToolCall>) -> ChatMessage {
    let mut m = ChatMessage::assistant("");
    m.content = None;
    m.tool_calls = calls;
    m
}

fn step(h: &mut Vec<ChatMessage>, call: &ToolCall, result: Option<&str>) {
    h.push(assistant(vec![call.clone()]));
    if let Some(r) = result {
        h.push(ChatMessage::tool_result(call.id.clone(), r));
    }
}

fn history() -> Vec<ChatMessage> {
    vec![ChatMessage::user("build it")]
}

#[test]
fn same_command_same_output_no_edit_between_is_noted() {
    let mut h = history();
    step(
        &mut h,
        &bash("a", "dotnet build"),
        Some("Build succeeded. Time Elapsed 00:00:04.21"),
    );
    step(&mut h, &bash("b", "grep -n Foo src/A.cs"), Some("12: Foo"));
    let current = bash("c", "dotnet build");
    step(&mut h, &current, None);

    let note = note_for(&h, &current, "Build succeeded. Time Elapsed 00:00:05.87").unwrap();

    assert!(note.contains("2 steps ago"));
    assert!(note.contains("no write or edit since"));
}

#[test]
fn an_edit_in_between_is_a_real_recheck() {
    let mut h = history();
    step(&mut h, &bash("a", "dotnet build"), Some("Build succeeded."));
    step(
        &mut h,
        &tool("e", "edit", json!({"path": "a.cs"})),
        Some("Edited"),
    );
    let current = bash("c", "dotnet build");
    step(&mut h, &current, None);

    assert!(note_for(&h, &current, "Build succeeded.").is_none());
}

#[test]
fn an_edit_earlier_in_the_same_message_is_a_real_recheck() {
    let mut h = history();
    step(&mut h, &bash("a", "dotnet build"), Some("Build succeeded."));
    let edit = tool("e", "write", json!({"path": "a.cs"}));
    let current = bash("c", "dotnet build");
    h.push(assistant(vec![edit, current.clone()]));
    h.push(ChatMessage::tool_result("e", "Wrote"));

    assert!(note_for(&h, &current, "Build succeeded.").is_none());
}

#[test]
fn different_output_is_not_noted() {
    let mut h = history();
    step(
        &mut h,
        &bash("a", "tasklist | findstr App"),
        Some("App.exe 1234"),
    );
    let current = bash("c", "tasklist | findstr App");
    step(&mut h, &current, None);

    assert!(note_for(&h, &current, "(no output)").is_none());
}

#[test]
fn different_arguments_or_unwatched_tools_are_not_noted() {
    let mut h = history();
    step(&mut h, &bash("a", "dotnet build"), Some("ok"));
    let other = bash("c", "dotnet test");
    step(&mut h, &other, None);
    assert!(note_for(&h, &other, "ok").is_none());

    let job = tool("j1", "job", json!({"action": "screenshot"}));
    step(&mut h, &job, Some("Screenshot 1"));
    let job2 = tool("j2", "job", json!({"action": "screenshot"}));
    step(&mut h, &job2, None);
    assert!(note_for(&h, &job2, "Screenshot 1").is_none());
}

#[test]
fn repeated_read_of_an_unchanged_file_is_noted() {
    let mut h = history();
    let read1 = tool("r1", "read", json!({"path": "src/A.cs"}));
    step(&mut h, &read1, Some("class A {}"));
    let read2 = tool("r2", "read", json!({"path": "src/A.cs"}));
    step(&mut h, &read2, None);

    assert!(
        note_for(&h, &read2, "class A {}")
            .unwrap()
            .contains("1 step ago")
    );
}

#[test]
fn a_third_repeat_compares_against_the_noted_result() {
    let mut h = history();
    step(&mut h, &bash("a", "make"), Some("ok"));
    let b = bash("b", "make");
    let mut probe = h.clone();
    probe.push(assistant(vec![b.clone()]));
    let noted = format!("ok{}", note_for(&probe, &b, "ok").unwrap_or_default());
    step(&mut h, &b, Some(&noted));
    let c = bash("c", "make");
    step(&mut h, &c, None);

    assert!(note_for(&h, &c, "ok").unwrap().contains("1 step ago"));
}
