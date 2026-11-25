//! Ported from tests/OpenOrca.Core.Tests/SessionManagerTests.cs and SessionForkTests.cs, plus
//! compatibility checks against session files written by the C# build
//! (`fixtures/csharp/sessions`, made by `SessionManager.SaveAsync`/`ForkAsync` in a small C#
//! program; only the working directory was replaced with `C:\work\project`).
//!
//! The C# SessionManagerTests save into the real `~/.openorca/sessions` (cleaning up `test_*`
//! files); these use a temp directory instead. A C# `Conversation` keeps the system prompt
//! apart from its messages; here it is the first message, as in the agent loop.

use orca_core::client::{ChatMessage, Role, ToolCall};
use orca_core::config::dotnet_json::to_string_dotnet_with_newline;
use orca_core::session::{
    SessionData, SessionManager, messages_to_session, new_session_id, session_to_messages,
};

fn manager() -> (tempfile::TempDir, SessionManager) {
    let dir = tempfile::tempdir().unwrap();
    let m = SessionManager::with_dir(dir.path());
    (dir, m)
}

fn test_id() -> String {
    format!("test_{}", new_session_id())
}

fn assistant_with_calls(text: Option<&str>, calls: &[(&str, &str, &str)]) -> ChatMessage {
    ChatMessage {
        content: text.map(str::to_string),
        tool_calls: calls
            .iter()
            .map(|(id, name, args)| ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
            })
            .collect(),
        ..ChatMessage::assistant("")
    }
}

// ── SessionManagerTests ──

#[test]
fn save_and_load_text_messages_only_round_trips() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::system("You are a helpful assistant."),
        ChatMessage::user("Hello"),
        ChatMessage::assistant("Hi there!"),
    ];
    let id = m
        .save(&conversation, Some("Test session"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();

    assert_eq!(loaded.title, "Test session");
    assert_eq!(
        loaded.system_prompt.as_deref(),
        Some("You are a helpful assistant.")
    );
    assert_eq!(loaded.messages.len(), 2); // user + assistant (system is separate)
    assert_eq!(loaded.messages[0].role, "user");
    assert_eq!(loaded.messages[0].text.as_deref(), Some("Hello"));
    assert_eq!(loaded.messages[1].role, "assistant");
    assert_eq!(loaded.messages[1].text.as_deref(), Some("Hi there!"));
    m.delete(&id).unwrap();
}

#[test]
fn save_and_load_with_tool_calls_round_trips() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::user("Read test.txt"),
        assistant_with_calls(
            Some("I'll read that file."),
            &[("call_123", "read_file", r#"{"path":"test.txt"}"#)],
        ),
    ];
    let id = m
        .save(&conversation, Some("Tool test"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();

    assert_eq!(loaded.messages.len(), 2);
    let saved = &loaded.messages[1];
    assert_eq!(saved.role, "assistant");
    let calls = saved.tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id.as_deref(), Some("call_123"));
    assert_eq!(calls[0].name, "read_file");
    assert!(calls[0].arguments.as_deref().unwrap().contains("test.txt"));
    m.delete(&id).unwrap();
}

#[test]
fn save_and_load_with_tool_results_round_trips() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::user("Read it"),
        ChatMessage::tool_result("call_456", "file contents here"),
    ];
    let id = m
        .save(&conversation, Some("Result test"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();

    assert_eq!(loaded.messages.len(), 2);
    let saved = &loaded.messages[1];
    assert_eq!(saved.role, "tool");
    let results = saved.tool_results.as_ref().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call_id.as_deref(), Some("call_456"));
    assert_eq!(results[0].result, "file contents here");
    m.delete(&id).unwrap();
}

#[test]
fn save_and_load_preserves_message_ordering() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::user("First"),
        ChatMessage::assistant("Reply 1"),
        ChatMessage::user("Second"),
        ChatMessage::assistant("Reply 2"),
        ChatMessage::user("Third"),
    ];
    let id = m
        .save(&conversation, Some("Order test"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    let texts: Vec<_> = loaded
        .messages
        .iter()
        .map(|m| m.text.clone().unwrap())
        .collect();
    assert_eq!(texts, ["First", "Reply 1", "Second", "Reply 2", "Third"]);
    m.delete(&id).unwrap();
}

#[test]
fn session_to_conversation_restores_conversation_correctly() {
    let (_d, m) = manager();
    let original = vec![
        ChatMessage::system("System prompt"),
        ChatMessage::user("Hello"),
        ChatMessage::assistant("Hi!"),
    ];
    let id = m
        .save(&original, Some("Restore test"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    let restored = session_to_messages(&loaded);

    assert_eq!(restored[0].role, Role::System);
    assert_eq!(restored[0].content.as_deref(), Some("System prompt"));
    // Two messages besides the system prompt.
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[1].role, Role::User);
    assert_eq!(restored[2].role, Role::Assistant);
    m.delete(&id).unwrap();
}

#[test]
fn session_to_conversation_restores_tool_calls() {
    let (_d, m) = manager();
    let original = vec![
        ChatMessage::user("Do it"),
        assistant_with_calls(None, &[("c1", "bash", r#"{"command":"ls"}"#)]),
        ChatMessage::tool_result("c1", "file1.txt\nfile2.txt"),
    ];
    let id = m
        .save(&original, Some("TC restore"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    let restored = session_to_messages(&loaded);

    assert_eq!(restored.len(), 3);
    let fc = restored[1].tool_calls.first().unwrap();
    assert_eq!(fc.name, "bash");
    assert_eq!(restored[2].role, Role::Tool);
    assert_eq!(restored[2].tool_call_id.as_deref(), Some("c1"));
    assert!(
        restored[2]
            .content
            .as_deref()
            .unwrap()
            .contains("file1.txt")
    );
    m.delete(&id).unwrap();
}

#[test]
fn load_non_existent_id_returns_null() {
    let (_d, m) = manager();
    assert!(m.load("nonexistent_id_12345").unwrap().is_none());
}

#[test]
fn delete_non_existent_session_returns_false() {
    let (_d, m) = manager();
    assert!(!m.delete("nonexistent_delete_test").unwrap());
}

#[test]
fn delete_existing_session_returns_true() {
    let (_d, m) = manager();
    let id = m
        .save(
            &[ChatMessage::user("Temp")],
            Some("Delete test"),
            Some(&test_id()),
        )
        .unwrap();
    assert!(m.delete(&id).unwrap());
    assert!(m.load(&id).unwrap().is_none());
}

#[test]
fn list_returns_saved_sessions() {
    let (_d, m) = manager();
    let id = m
        .save(
            &[ChatMessage::user("List test")],
            Some("List test session"),
            Some(&test_id()),
        )
        .unwrap();
    assert!(m.list().iter().any(|s| s.id == id));
    m.delete(&id).unwrap();
}

#[test]
fn save_without_title_uses_first_user_message() {
    let (_d, m) = manager();
    let id = m
        .save(
            &[ChatMessage::user("Help me write a function")],
            None,
            Some(&test_id()),
        )
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    assert_eq!(loaded.title, "Help me write a function");
    m.delete(&id).unwrap();
}

#[test]
fn save_without_title_long_message_truncates() {
    let (_d, m) = manager();
    let id = m
        .save(
            &[ChatMessage::user("A".repeat(100))],
            None,
            Some(&test_id()),
        )
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    assert!(loaded.title.len() <= 60);
    assert!(loaded.title.ends_with("..."));
    m.delete(&id).unwrap();
}

/// C# passes `null` arguments; the Rust call's argument text is then empty (not an object).
#[test]
fn save_and_load_tool_call_null_arguments_no_error() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::user("Test"),
        assistant_with_calls(None, &[("c_null", "some_tool", "")]),
    ];
    let id = m
        .save(&conversation, Some("Null args"), Some(&test_id()))
        .unwrap();
    let loaded = m.load(&id).unwrap().unwrap();
    let tc = loaded.messages[1].tool_calls.as_ref().unwrap();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].name, "some_tool");
    assert!(tc[0].arguments.is_none());
    m.delete(&id).unwrap();
}

// ── SessionForkTests ──

#[test]
fn fork_async_creates_new_session_with_parent_id() {
    let (_d, m) = manager();
    let conversation = vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")];
    let parent_id = m
        .save(&conversation, Some("Parent"), Some(&test_id()))
        .unwrap();
    let fork_id = m
        .fork(&conversation, Some("My Fork"), &parent_id, 2)
        .unwrap();
    assert_ne!(parent_id, fork_id);

    let forked = m.load(&fork_id).unwrap().unwrap();
    assert_eq!(forked.title, "My Fork");
    assert_eq!(
        forked.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );
    assert_eq!(forked.fork_point_message_index, Some(2));
}

#[test]
fn fork_async_preserves_messages() {
    let (_d, m) = manager();
    let conversation = vec![
        ChatMessage::user("First"),
        ChatMessage::assistant("Reply 1"),
        ChatMessage::user("Second"),
    ];
    let parent_id = m
        .save(&conversation, Some("Parent"), Some(&test_id()))
        .unwrap();
    let fork_id = m.fork(&conversation, None, &parent_id, 3).unwrap();
    let forked = m.load(&fork_id).unwrap().unwrap();
    assert_eq!(forked.messages.len(), 3);
    assert_eq!(forked.messages[0].text.as_deref(), Some("First"));
}

#[test]
fn fork_async_default_title_contains_parent_id() {
    let (_d, m) = manager();
    let conversation = vec![ChatMessage::user("Test")];
    let parent_id = m
        .save(&conversation, Some("Original"), Some(&test_id()))
        .unwrap();
    let fork_id = m.fork(&conversation, None, &parent_id, 1).unwrap();
    let forked = m.load(&fork_id).unwrap().unwrap();
    assert!(forked.title.contains(&parent_id));
}

#[test]
fn get_session_tree_no_sessions_returns_empty() {
    let (_d, m) = manager();
    assert!(m.session_tree().is_empty());
}

#[test]
fn get_session_tree_with_fork_shows_parent_child() {
    let (_d, m) = manager();
    let conversation = vec![ChatMessage::user("Root")];
    let root_id = m
        .save(&conversation, Some("Root Session"), Some(&test_id()))
        .unwrap();
    let fork_id = m.fork(&conversation, Some("Fork 1"), &root_id, 1).unwrap();
    let forked = m.load(&fork_id).unwrap().unwrap();
    assert_eq!(forked.parent_session_id.as_deref(), Some(root_id.as_str()));

    let tree = m.session_tree();
    assert!(!tree.is_empty());
    let fork_in_tree: Vec<_> = tree.iter().filter(|(s, _)| s.id == fork_id).collect();
    assert_eq!(fork_in_tree.len(), 1);
    assert_eq!(fork_in_tree[0].1, 1);
    let root_in_tree: Vec<_> = tree.iter().filter(|(s, _)| s.id == root_id).collect();
    assert_eq!(root_in_tree.len(), 1);
    assert_eq!(root_in_tree[0].1, 0);
}

#[test]
fn session_data_parent_fields_default_null() {
    let data = SessionData::default();
    assert!(data.parent_session_id.is_none());
    assert!(data.fork_point_message_index.is_none());
}

// ── Beyond the C# tests ──

#[test]
fn new_ids_are_eight_hex_digits_and_differ() {
    let a = new_session_id();
    let b = new_session_id();
    assert_eq!(a.len(), 8);
    assert!(
        a.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_ne!(a, b);
}

#[test]
fn list_is_most_recent_first_and_skips_unreadable_files() {
    let (d, m) = manager();
    let mut old = messages_to_session(&[ChatMessage::user("old")]);
    old.id = "old".into();
    old.updated_at = orca_core::session::DotNetDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    m.write(&old).unwrap();
    let new_id = m.save(&[ChatMessage::user("new")], None, None).unwrap();
    std::fs::write(d.path().join("broken.json"), "{ not json").unwrap();
    let ids: Vec<_> = m.list().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [new_id, "old".to_string()]);
}

#[test]
fn latest_for_prefers_the_working_directory() {
    let (_d, m) = manager();
    let mut here = messages_to_session(&[ChatMessage::user("here")]);
    here.id = "here".into();
    here.working_directory = Some(r"C:\Work\Project".into());
    here.updated_at = orca_core::session::DotNetDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    m.write(&here).unwrap();
    let mut elsewhere = messages_to_session(&[ChatMessage::user("elsewhere")]);
    elsewhere.id = "elsewhere".into();
    elsewhere.working_directory = Some(r"C:\other".into());
    m.write(&elsewhere).unwrap();

    let (s, other) = m
        .latest_for(std::path::Path::new(r"c:\work\project"))
        .unwrap();
    assert_eq!((s.id.as_str(), other), ("here", false));
    let (s, other) = m.latest_for(std::path::Path::new(r"C:\nowhere")).unwrap();
    assert_eq!((s.id.as_str(), other), ("elsewhere", true));
}

#[test]
fn auto_save_keeps_the_id_and_honours_the_setting() {
    let (_d, m) = manager();
    let mut config = orca_core::config::OrcaConfig::default();
    let mut id = None;
    m.auto_save(&config, &[ChatMessage::user("one")], &mut id);
    let first = id.clone().unwrap();
    m.auto_save(
        &config,
        &[ChatMessage::user("one"), ChatMessage::assistant("two")],
        &mut id,
    );
    assert_eq!(id.as_deref(), Some(first.as_str()));
    assert_eq!(m.load(&first).unwrap().unwrap().messages.len(), 2);

    config.session.auto_save = false;
    let mut none = None;
    m.auto_save(&config, &[ChatMessage::user("x")], &mut none);
    assert!(none.is_none());
    assert_eq!(m.list().len(), 1);
}

// ── Compatibility with the C# build ──

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/csharp")
}

fn fixture_text(rel: &str) -> String {
    // Git may check the fixture out with either line break; compare with "\n".
    std::fs::read_to_string(fixture_dir().join(rel))
        .unwrap()
        .replace("\r\n", "\n")
}

#[test]
fn csharp_session_loads_and_saves_back_byte_identical() {
    for rel in ["sessions/csharp01.json", "sessions/ed269504.json"] {
        let text = fixture_text(rel);
        let data: SessionData = serde_json::from_str(&text).unwrap();
        assert_eq!(
            to_string_dotnet_with_newline(&data, "\n").unwrap(),
            text,
            "{rel}"
        );
    }
}

#[test]
fn csharp_session_maps_to_messages() {
    let m = SessionManager::new(&fixture_dir());
    let data = m.load("csharp01").unwrap().unwrap();
    assert_eq!(data.working_directory.as_deref(), Some(r"C:\work\project"));
    let msgs = session_to_messages(&data);
    let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
    use Role::*;
    assert_eq!(
        roles,
        [
            System, User, Assistant, Tool, Assistant, Tool, Tool, Assistant
        ]
    );
    assert_eq!(
        msgs[0].content.as_deref(),
        Some("You are Orca.\nBe brief & exact <ok>.")
    );
    assert_eq!(
        msgs[1].content.as_deref(),
        Some("Fix the 'bug' in a+b <now> \u{2014} caf\u{e9} \u{1F600}\r\nplease")
    );
    assert_eq!(
        msgs[2].reasoning.as_deref(),
        Some("Need to look at main.py.")
    );
    assert_eq!(msgs[2].content.as_deref(), Some("I'll read it first."));
    let args: serde_json::Value = serde_json::from_str(&msgs[2].tool_calls[0].arguments).unwrap();
    assert_eq!(
        args,
        serde_json::json!({"path": "main.py", "offset": 1, "limit": 2.5, "flags": [true, null], "opt": {"x": "<y>"}})
    );
    assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_1"));
    // A tool-call-only turn has no text; a call without arguments has empty argument text.
    assert_eq!(msgs[4].content, None);
    assert_eq!(msgs[4].tool_calls.len(), 2);
    assert_eq!(msgs[4].tool_calls[1].arguments, "");
    assert_eq!(msgs[5].content.as_deref(), Some("a&b"));
    assert_eq!(msgs[7].content.as_deref(), Some("Done: fixed a+b."));

    let fork = m.load("ed269504").unwrap().unwrap();
    assert_eq!(fork.parent_session_id.as_deref(), Some("csharp01"));
    assert_eq!(fork.fork_point_message_index, Some(3));
    assert_eq!(fork.title, "Fork \"one\"");
    assert_eq!(
        m.session_tree().iter().map(|(_, d)| *d).collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn csharp_session_round_trips_through_messages() {
    // What Rust writes for a conversation it loaded is what C# wrote (ids, times, title and
    // directory are per save).
    let data: SessionData = serde_json::from_str(&fixture_text("sessions/csharp01.json")).unwrap();
    let again = messages_to_session(&session_to_messages(&data));
    assert_eq!(again.system_prompt, data.system_prompt);
    assert_eq!(again.messages, data.messages);
    assert_eq!(
        to_string_dotnet_with_newline(&again.messages, "\n").unwrap(),
        to_string_dotnet_with_newline(&data.messages, "\n").unwrap()
    );
}

#[test]
fn raw_arguments_that_are_not_an_object_are_kept() {
    let mut data = SessionData::default();
    data.messages.push(orca_core::session::SessionMessage {
        role: "assistant".into(),
        text: Some(String::new()),
        tool_calls: Some(vec![orca_core::session::SessionToolCall {
            call_id: Some("c".into()),
            name: "bash".into(),
            arguments: Some("not json".into()),
        }]),
        ..Default::default()
    });
    let msgs = session_to_messages(&data);
    assert_eq!(
        msgs[0].tool_calls[0].arguments,
        r#"{"_raw_json":"not json"}"#
    );
}
