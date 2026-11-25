//! Port of `tests/OpenOrca.Core.Tests/ConversationCompactionTests.cs`.

use orca_core::chat::Conversation;
use orca_core::chat::compaction::{self, CompactionFileOps, CompactionSettings};
use orca_core::chat::conversation::TEXT_TOOL_RESULTS_PREFIX;
use orca_core::client::{ChatMessage, Role, ToolCall};
use orca_core::config::ContextConfig;
use serde_json::{Value, json};

fn call(id: &str, tool: &str, args: Value) -> ChatMessage {
    let mut m = ChatMessage::assistant("");
    m.tool_calls.push(ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments: args.to_string(),
    });
    m
}

fn result(id: &str, text: &str) -> ChatMessage {
    ChatMessage::tool_result(id, text)
}

fn context(window: i32) -> ContextConfig {
    ContextConfig {
        context_window_size: window,
        ..ContextConfig::default()
    }
}

// ── Settings ──

#[test]
fn settings_large_window_uses_pi_defaults() {
    let s = CompactionSettings::from_config(&context(262144));
    assert_eq!(s.reserve_tokens, 16384);
    assert_eq!(s.keep_recent_tokens, 20000);
    assert_eq!(s.trigger_tokens(), 262144 - 16384);
    assert!(!s.should_compact(262144 - 16384));
    assert!(s.should_compact(262144 - 16383));
}

#[test]
fn settings_small_window_scale_down_from_threshold() {
    let s = CompactionSettings::from_config(&ContextConfig {
        auto_compact_threshold: 0.8,
        ..context(16384)
    });
    assert_eq!(s.reserve_tokens, (16384.0f32 * 0.2f32) as i32);
    assert_eq!(s.keep_recent_tokens, (16384 - s.reserve_tokens) / 4);
}

#[test]
fn settings_explicit_values_are_clamped_so_compaction_can_make_room() {
    let s = CompactionSettings::from_config(&ContextConfig {
        reserve_tokens: 16384,
        keep_recent_tokens: 20000,
        ..context(16384)
    });
    assert_eq!(s.reserve_tokens, 8192);
    assert_eq!(s.keep_recent_tokens, 4096);
}

// ── Serialization (Pi serializeConversation) ──

#[test]
fn serialize_matches_pi_format() {
    let mut assistant = ChatMessage::assistant("Reading it.");
    assistant.reasoning = Some("hmm".into());
    assistant.tool_calls = vec![
        ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: json!({"path": "src/a.py", "limit": 5}).to_string(),
        },
        ToolCall {
            id: "c2".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls \"x\""}).to_string(),
        },
    ];
    let text = compaction::serialize_conversation(&[
        ChatMessage::user("fix it"),
        assistant,
        result("c1", &"y".repeat(2500)),
    ]);
    assert_eq!(
        text,
        format!(
            "[User]: fix it\n\n[Assistant thinking]: hmm\n\n[Assistant]: Reading it.\n\n[Assistant tool calls]: read(path=\"src/a.py\", limit=5); bash(command=\"ls \\\"x\\\"\")\n\n[Tool result]: {}\n\n[... 500 more characters truncated]",
            "y".repeat(2000)
        )
    );
}

#[test]
fn serialize_empty_assistant_text_is_omitted() {
    let text = compaction::serialize_conversation(&[call("c1", "read", json!({"path": "a"}))]);
    assert_eq!(text, "[Assistant tool calls]: read(path=\"a\")");
}

#[test]
fn serialize_text_mode_tool_results_are_tool_results() {
    let text = compaction::serialize_conversation(&[ChatMessage::user(format!(
        "{TEXT_TOOL_RESULTS_PREFIX} you made:\n\nok"
    ))]);
    assert!(text.starts_with("[Tool result]: Here are the results"));
}

// ── Cut point ──

#[test]
fn cut_point_never_lands_on_tool_result() {
    let mut msgs = vec![ChatMessage::user("go")];
    for i in 0..5 {
        msgs.push(call(
            &format!("c{i}"),
            "read",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(result(&format!("c{i}"), &"x".repeat(1000)));
    }
    for keep in (1..2500).step_by(13) {
        let cut = compaction::find_cut_point(&msgs, 0, msgs.len(), keep, 3.5);
        assert_ne!(msgs[cut.first_kept_index].role, Role::Tool);
    }
}

#[test]
fn cut_point_trailing_results_over_budget_keeps_their_tool_call() {
    let msgs = vec![
        ChatMessage::user("go"),
        call("c1", "read", json!({"path": "f"})),
        result("c1", &"x".repeat(10_000)),
    ];
    let cut = compaction::find_cut_point(&msgs, 0, msgs.len(), 100, 3.5);
    assert_eq!(cut.first_kept_index, 1);
    assert!(cut.is_split_turn);
    assert_eq!(cut.turn_start_index, Some(0));
}

#[test]
fn cut_point_at_user_message_is_not_split_turn() {
    let msgs = vec![
        ChatMessage::user("a".repeat(400)),
        ChatMessage::assistant("b".repeat(400)),
        ChatMessage::user("c".repeat(400)),
        ChatMessage::assistant("short"),
    ];
    let cut = compaction::find_cut_point(&msgs, 0, msgs.len(), 110, 3.5);
    assert_eq!(cut.first_kept_index, 2);
    assert!(!cut.is_split_turn);
}

#[test]
fn cut_point_never_cuts_at_text_mode_tool_results() {
    let msgs = vec![
        ChatMessage::user("go"),
        ChatMessage::assistant("<tool_call>{\"name\":\"read\"}</tool_call>"),
        ChatMessage::user(format!(
            "{TEXT_TOOL_RESULTS_PREFIX} you made:\n\n{}",
            "x".repeat(3000)
        )),
    ];
    let cut = compaction::find_cut_point(&msgs, 0, msgs.len(), 50, 3.5);
    assert_eq!(cut.first_kept_index, 1);
}

// ── Prepare ──

#[test]
fn prepare_nothing_to_summarize_returns_null() {
    let msgs = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
    assert!(compaction::prepare(&msgs, 10_000, 3.5, None, 0).is_none());
}

#[test]
fn prepare_previous_checkpoint_is_not_resummarized() {
    let msgs = vec![
        compaction::create_summary_message("OLD\n\n<modified-files>\nm.txt\n</modified-files>"),
        ChatMessage::user("a".repeat(400)),
        ChatMessage::assistant("b".repeat(400)),
        ChatMessage::user("c".repeat(400)),
        ChatMessage::assistant("short"),
    ];
    let prep = compaction::prepare(&msgs, 110, 3.5, None, 0).unwrap();
    assert_eq!(prep.previous_summary.as_deref(), Some("OLD"));
    assert_eq!(prep.messages_to_summarize.len(), 2);
    assert_eq!(prep.messages_to_summarize[0], msgs[1]);
    assert!(prep.file_ops.edited.contains("m.txt"));
}

#[test]
fn prepare_recognises_legacy_checkpoint() {
    let s = compaction::try_get_summary(&ChatMessage::user("[Conversation summary]\nold style"));
    assert_eq!(s.as_deref(), Some("old style"));
}

// ── File ops ──

#[test]
fn file_ops_cover_pi_and_open_orca_tools_read_only_excludes_modified() {
    let mut ops = CompactionFileOps::default();
    ops.add_tool_call("read", &json!({"path": "a"}));
    ops.add_tool_call("read_file", &json!({"path": "b"}));
    ops.add_tool_call("edit", &json!({"path": "b"}));
    ops.add_tool_call("write", &json!({"path": "c"}));
    ops.add_tool_call("edit_file", &json!({"path": "d"}));
    ops.add_tool_call("write_file", &json!({"path": "e"}));
    ops.add_tool_call("bash", &json!({"command": "cat z"}));

    let (read, modified) = ops.compute_file_lists();
    assert_eq!(read, ["a"]);
    assert_eq!(modified, ["b", "c", "d", "e"]);
    assert_eq!(
        CompactionFileOps::format(&read, &modified),
        "\n\n<read-files>\na\n</read-files>\n\n<modified-files>\nb\nc\nd\ne\n</modified-files>"
    );
}

// ── Conversation integration ──

#[test]
fn apply_compaction_replaces_prefix_with_pi_checkpoint_message() {
    let mut c = Conversation::new();
    c.add_user_message("u1");
    c.add_assistant_message("a1");
    c.add_user_message("u2");
    let removed = c.apply_compaction("SUM", 2, 0);
    assert_eq!(removed, 2);
    assert_eq!(c.messages().len(), 2);
    assert_eq!(
        c.messages()[0].content.as_deref(),
        Some(
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nSUM\n</summary>"
        )
    );
    assert_eq!(c.messages()[1].content.as_deref(), Some("u2"));
}

#[test]
fn estimate_context_tokens_uses_server_usage_plus_trailing_estimate() {
    let mut c = Conversation::new();
    c.set_system_prompt(Some("s".repeat(7000)));
    c.add_user_message("hello");
    c.record_prompt_usage(5000, c.messages().len(), None);
    assert_eq!(c.estimate_context_tokens(3.5), 5000);

    c.add_assistant_message("a".repeat(350)); // 100 tokens at 3.5 chars/token
    assert_eq!(c.estimate_context_tokens(3.5), 5100);
    assert!(c.has_usage_base());
}

#[test]
fn estimate_context_tokens_rewrite_invalidates_usage() {
    let mut c = Conversation::new();
    c.add_user_message("hello");
    c.add_assistant_message("hi");
    c.record_prompt_usage(50_000, c.messages().len(), None);

    c.apply_compaction("s", 1, 0);
    assert!(!c.has_usage_base());
    assert_eq!(c.estimate_context_tokens(3.5), c.estimate_token_count(3.5));
}

#[test]
fn record_prompt_usage_stale_rewrite_count_is_ignored() {
    let mut c = Conversation::new();
    c.add_user_message("hello");
    let rc = c.rewrite_count();
    c.set_system_prompt(Some("changed after the request was built".into()));
    c.record_prompt_usage(50_000, 1, Some(rc));
    assert!(!c.has_usage_base());
}

#[test]
fn appends_do_not_bump_rewrite_count_so_prefix_stays_stable() {
    let mut c = Conversation::new();
    c.set_system_prompt(Some("sys".into()));
    let rc = c.rewrite_count();
    c.add_user_message("u");
    c.add_assistant_message("a");
    c.add_message(result("c", "r"));
    c.set_system_prompt(Some("sys".into())); // same value
    assert_eq!(c.rewrite_count(), rc);
}
