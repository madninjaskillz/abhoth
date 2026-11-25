//! Port of `tests/OpenOrca.Core.Tests/ConversationTests.cs`.

use orca_core::chat::Conversation;
use orca_core::client::{ChatMessage, Role, ToolCall};

fn text(m: &ChatMessage) -> &str {
    m.content.as_deref().unwrap_or("")
}

fn call_message(id: &str, name: &str) -> ChatMessage {
    let mut m = ChatMessage::assistant("");
    m.tool_calls.push(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: String::new(),
    });
    m
}

#[test]
fn add_user_message_adds_to_messages() {
    let mut c = Conversation::new();
    c.add_user_message("Hello");
    assert_eq!(c.messages().len(), 1);
    assert_eq!(text(&c.messages()[0]), "Hello");
}

#[test]
fn add_assistant_message_adds_to_messages() {
    let mut c = Conversation::new();
    c.add_assistant_message("Hi there");
    assert_eq!(c.messages().len(), 1);
    assert_eq!(text(&c.messages()[0]), "Hi there");
}

#[test]
fn add_system_message_sets_system_prompt() {
    let mut c = Conversation::new();
    c.add_system_message("You are a helpful assistant.");
    assert_eq!(c.system_prompt(), Some("You are a helpful assistant."));
}

#[test]
fn get_messages_for_api_includes_system_prompt_first() {
    let mut c = Conversation::new();
    c.add_system_message("System prompt");
    c.add_user_message("Hello");
    let messages = c.get_messages_for_api();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role.as_str(), "system");
    assert_eq!(messages[1].role.as_str(), "user");
}

#[test]
fn clear_removes_all_messages() {
    let mut c = Conversation::new();
    c.add_system_message("System");
    c.add_user_message("Hello");
    c.add_assistant_message("Hi");
    c.clear();
    assert!(c.messages().is_empty());
    assert_eq!(c.system_prompt(), None);
}

#[test]
fn estimate_token_count_returns_reasonable_estimate() {
    let mut c = Conversation::new();
    c.add_user_message("Hello world"); // 11 chars, ~3 tokens
    let estimate = c.estimate_token_count(3.5);
    assert!(estimate > 0);
    assert!(estimate < 10);
}

#[test]
fn truncate_to_fit_removes_old_messages() {
    let mut c = Conversation::new();
    for i in 0..100 {
        c.add_user_message(format!(
            "Message {i} with some longer content to increase token count substantially more"
        ));
    }
    let before = c.messages().len();
    c.truncate_to_fit(50);
    assert!(c.messages().len() < before);
    assert!(c.messages().len() >= 2); // minimum retained
}

// ── CompactWithSummary ──

#[test]
fn compact_with_summary_replaces_old_messages_with_summary() {
    let mut c = Conversation::new();
    for i in 0..3 {
        c.add_user_message(format!("User message {i}"));
        c.add_assistant_message(format!("Assistant message {i}"));
    }
    assert_eq!(c.messages().len(), 6);
    let removed = c.compact_with_summary("This is a summary of the conversation.", 1);
    assert!(removed > 0);
    assert!(c.messages().len() < 6);
    assert!(text(&c.messages()[0]).contains("[Conversation summary]"));
}

#[test]
fn compact_with_summary_preserves_last_n_turns() {
    let mut c = Conversation::new();
    c.add_user_message("Old question");
    c.add_assistant_message("Old answer");
    c.add_user_message("Recent question");
    c.add_assistant_message("Recent answer");
    c.compact_with_summary("Summary of old conversation.", 1);
    assert!(
        c.messages()
            .iter()
            .any(|m| text(m).contains("Recent question"))
    );
    assert!(
        c.messages()
            .iter()
            .any(|m| text(m).contains("Recent answer"))
    );
}

#[test]
fn compact_with_summary_returns_zero_when_not_enough_messages() {
    let mut c = Conversation::new();
    c.add_user_message("Hello");
    assert_eq!(c.compact_with_summary("Summary", 5), 0);
}

// ── GetMessagesForCompaction ──

#[test]
fn get_messages_for_compaction_returns_older_messages() {
    let mut c = Conversation::new();
    c.add_user_message("Old 1");
    c.add_assistant_message("Old response 1");
    c.add_user_message("Recent 1");
    c.add_assistant_message("Recent response 1");
    let to_compact = c.get_messages_for_compaction(1);
    assert_eq!(to_compact.len(), 2);
    assert_eq!(text(&to_compact[0]), "Old 1");
}

#[test]
fn get_messages_for_compaction_returns_empty_when_preserve_exceeds_count() {
    let mut c = Conversation::new();
    c.add_user_message("Hello");
    assert!(c.get_messages_for_compaction(5).is_empty());
}

// ── RemoveLastTurns ──

#[test]
fn remove_last_turns_removes_one_turn() {
    let mut c = Conversation::new();
    c.add_user_message("First question");
    c.add_assistant_message("First answer");
    c.add_user_message("Second question");
    c.add_assistant_message("Second answer");
    assert_eq!(c.remove_last_turns(1), 2);
    assert_eq!(c.messages().len(), 2);
    assert_eq!(text(&c.messages()[0]), "First question");
}

#[test]
fn remove_last_turns_removes_multiple_turns() {
    let mut c = Conversation::new();
    for (q, a) in [("Q1", "A1"), ("Q2", "A2"), ("Q3", "A3")] {
        c.add_user_message(q);
        c.add_assistant_message(a);
    }
    assert_eq!(c.remove_last_turns(2), 4);
    assert_eq!(c.messages().len(), 2);
    assert_eq!(text(&c.messages()[0]), "Q1");
}

#[test]
fn remove_last_turns_handles_tool_messages() {
    let mut c = Conversation::new();
    c.add_user_message("Do something");
    c.add_assistant_message("Using a tool");
    c.add_message(ChatMessage::tool_result("c1", "tool result"));
    c.add_assistant_message("Done!");
    assert_eq!(c.remove_last_turns(1), 4);
    assert!(c.messages().is_empty());
}

#[test]
fn remove_last_turns_returns_zero_when_empty() {
    let mut c = Conversation::new();
    assert_eq!(c.remove_last_turns(1), 0);
}

// ── GetMessageCountByRole ──

#[test]
fn get_message_count_by_role_counts_correctly() {
    let mut c = Conversation::new();
    c.add_user_message("Q1");
    c.add_assistant_message("A1");
    c.add_user_message("Q2");
    c.add_assistant_message("A2");
    c.add_message(ChatMessage::tool_result("c1", "result"));
    let counts = c.get_message_count_by_role();
    assert_eq!(counts["user"], 2);
    assert_eq!(counts["assistant"], 2);
    assert_eq!(counts["tool"], 1);
}

#[test]
fn get_message_count_by_role_returns_empty_when_no_messages() {
    assert!(Conversation::new().get_message_count_by_role().is_empty());
}

// ── PruneOldToolResults ──

#[test]
fn prune_old_tool_results_stubs_old_results_preserves_recent_turns() {
    let mut c = Conversation::new();
    let big = "x".repeat(5000);
    c.add_user_message("first request");
    c.add_message(call_message("c1", "read_file"));
    c.add_message(ChatMessage::tool_result("c1", big.clone()));
    c.add_user_message("second request");
    c.add_assistant_message("answer two");
    c.add_user_message("third request");
    c.add_message(ChatMessage::tool_result("c2", big.clone()));

    let reclaimed = c.prune_old_tool_results(2, 200);

    assert!(reclaimed > 4000);
    assert_eq!(c.messages()[2].role, Role::Tool);
    assert!(text(&c.messages()[2]).contains("pruned"));
    assert_eq!(text(c.messages().last().unwrap()), big);
}

#[test]
fn prune_old_tool_results_text_mode_results_also_stubbed() {
    let mut c = Conversation::new();
    c.add_user_message("do something");
    c.add_assistant_message("<tool_call>...</tool_call>");
    c.add_user_message(format!(
        "Here are the results of the tool calls you made:\n\n{}",
        "y".repeat(5000)
    ));
    c.add_user_message("next request");
    c.add_assistant_message("working");
    c.add_user_message("another request");

    let reclaimed = c.prune_old_tool_results(2, 200);

    assert!(reclaimed > 4000);
    assert!(text(&c.messages()[2]).contains("pruned"));
}

#[test]
fn prune_old_tool_results_small_results_untouched() {
    let mut c = Conversation::new();
    c.add_user_message("first");
    c.add_message(ChatMessage::tool_result("c1", "small"));
    c.add_user_message("second");
    c.add_user_message("third");
    assert_eq!(c.prune_old_tool_results(2, 200), 0);
    assert_eq!(text(&c.messages()[1]), "small");
}

// ── Rust-side bookkeeping (no C# counterpart: the session carries usage separately) ──

#[test]
fn api_messages_round_trip_and_in_place_changes_drop_the_usage_base() {
    let mut c = Conversation::new();
    c.add_system_message("sys");
    c.add_user_message("hello");
    c.record_prompt_usage(5000, 1, None);
    let (messages, usage) = c.into_api_messages();
    assert_eq!(messages[0].role, Role::System);

    let same = Conversation::from_api_messages(messages.clone(), usage.clone());
    assert!(same.has_usage_base());
    assert_eq!(same.system_prompt(), Some("sys"));

    let mut changed = messages.clone();
    changed[1].content = Some("edited in place".into());
    assert!(!Conversation::from_api_messages(changed, usage.clone()).has_usage_base());

    let mut new_system = messages;
    new_system[0].content = Some("another system prompt".into());
    assert!(!Conversation::from_api_messages(new_system, usage).has_usage_base());
}
