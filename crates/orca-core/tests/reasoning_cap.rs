//! Port of `tests/OpenOrca.Core.Tests/ReasoningCapTests.cs` (the pure parts).
//!
//! Not ported yet: the `Worker_*` tests drive `AgentOrchestrator` sub-agents (phase 3) with a
//! scripted stream that the cap stops mid-way; the Rust loop does not stop a stream on a fire
//! yet (the agent loop's integration, see `reasoning_cap.rs`).

use orca_core::chat::ReasoningCap;
use orca_core::chat::reasoning_cap::{CUT_MARKER, NUDGE, TAIL_CHARS, TAIL_INTRO};
use orca_core::client::{ChatMessage, Role, StreamEvent, ToolCall};
use orca_core::config::{OrcaConfig, SlotLeadConfig, SlotsConfig};

fn thinking(text: &str) -> StreamEvent {
    StreamEvent::Reasoning(text.into())
}

/// Feeds one-token reasoning chunks until the cap fires; how many it took (0 = never).
fn feed_until_fire(cap: &mut ReasoningCap, chunks: usize) -> usize {
    for i in 1..=chunks {
        if cap.observe(&thinking(" tok")) {
            return i;
        }
    }
    0
}

#[test]
fn config_is_off_by_default_and_round_trips_through_config_json() {
    let defaults = OrcaConfig::default();
    assert_eq!(defaults.agent.reasoning.max_step_tokens, 0);
    assert_eq!(defaults.agent.reasoning.cap_retries, 1);
    assert!(ReasoningCap::for_config(&defaults, true).is_none());
    assert!(ReasoningCap::for_config(&defaults, false).is_none());

    let json = r#"{"agent":{"reasoning":{"lead":"medium","maxStepTokens":1200,"capRetries":2}}}"#;
    let config: OrcaConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.agent.reasoning.max_step_tokens, 1200);
    assert_eq!(config.agent.reasoning.cap_retries, 2);
    assert_eq!(config.agent.reasoning.lead.as_deref(), Some("medium"));

    let cap = ReasoningCap::for_config(&config, false).unwrap();
    assert_eq!(cap.max_step_tokens(), 1200);
    assert_eq!(cap.retries(), 2);
}

#[test]
fn for_keeps_the_reasoning_only_where_requests_send_it_back() {
    let mut config = OrcaConfig::default();
    config.agent.reasoning.max_step_tokens = 100;
    // No slots: nothing echoes reasoning_content, so the note has to carry it.
    assert!(
        !ReasoningCap::for_config(&config, true)
            .unwrap()
            .keeps_reasoning()
    );

    config.slots = Some(SlotsConfig::default());
    assert!(
        ReasoningCap::for_config(&config, true)
            .unwrap()
            .keeps_reasoning()
    );
    assert!(
        ReasoningCap::for_config(&config, false)
            .unwrap()
            .keeps_reasoning()
    );

    config.slots.as_mut().unwrap().preserve_thinking = false;
    assert!(
        !ReasoningCap::for_config(&config, false)
            .unwrap()
            .keeps_reasoning()
    );
    config.slots.as_mut().unwrap().lead = Some(SlotLeadConfig {
        preserve_thinking: Some(true),
        ..SlotLeadConfig::default()
    });
    assert!(
        ReasoningCap::for_config(&config, true)
            .unwrap()
            .keeps_reasoning()
    );
    assert!(
        !ReasoningCap::for_config(&config, false)
            .unwrap()
            .keeps_reasoning()
    );
}

#[test]
fn estimate_tokens_counts_chunks_unless_chunks_carry_more_than_a_token() {
    for (chunks, chars, expected) in [
        (0, 0, 0),
        (1200, 4800, 1200),
        (1200, 8000, 1200),
        (10, 700, 200),
    ] {
        assert_eq!(
            ReasoningCap::estimate_tokens(chunks, chars, 3.5),
            expected,
            "{chunks}/{chars}"
        );
    }
}

#[test]
fn fires_when_reasoning_reaches_the_limit_before_any_answer() {
    let mut cap = ReasoningCap::new(50, 1, 3.5, true);
    cap.begin_request();

    assert_eq!(feed_until_fire(&mut cap, 500), 50);
    assert!(cap.fired());
    assert_eq!(cap.reasoning_tokens(), 50);
    assert_eq!(cap.fires_this_step(), 1);
    assert_eq!(cap.events(), 1);
    assert_eq!(cap.reasoning(), " tok".repeat(50));
    assert_eq!(
        cap.describe(),
        "thinking capped at ~50 tokens (limit 50); asked to act"
    );
}

#[test]
fn never_fires_once_answer_text_or_a_tool_call_has_started() {
    for start in [
        StreamEvent::Text("Writing the file now.".into()),
        StreamEvent::ToolCallStarted {
            index: 0,
            name: "write".into(),
        },
    ] {
        let mut cap = ReasoningCap::new(50, 1, 3.5, true);
        cap.begin_request();
        assert_eq!(feed_until_fire(&mut cap, 40), 0);
        assert!(!cap.observe(&start));
        assert_eq!(feed_until_fire(&mut cap, 500), 0);
        assert!(!cap.fired());
    }

    // The update that crosses the limit also carries the start of the answer: no fire.
    let mut mixed = ReasoningCap::new(50, 1, 3.5, true);
    mixed.begin_request();
    assert_eq!(feed_until_fire(&mut mixed, 49), 0);
    assert!(!mixed.observe_update(Some(" tok"), Some("Done."), false));
    assert!(!mixed.fired());
}

#[test]
fn empty_text_chunks_do_not_count_as_an_answer() {
    let mut cap = ReasoningCap::new(50, 1, 3.5, true);
    cap.begin_request();
    assert!(!cap.observe(&StreamEvent::Text(String::new())));
    assert_eq!(feed_until_fire(&mut cap, 500), 50);
}

#[test]
fn after_cap_retries_fires_the_step_runs_uncapped_and_the_next_step_is_capped_again() {
    let mut cap = ReasoningCap::new(50, 2, 3.5, true);

    cap.begin_request();
    assert_eq!(feed_until_fire(&mut cap, 500), 50);
    cap.begin_request(); // same step asked again: a fresh count, capped once more
    assert_eq!(feed_until_fire(&mut cap, 500), 50);
    assert_eq!(cap.fires_this_step(), 2);
    cap.begin_request(); // out of fires: the step gets its long think
    assert_eq!(feed_until_fire(&mut cap, 5000), 0);
    assert!(!cap.fired());

    cap.begin_request(); // the request before was not stopped, so this is a new step
    assert_eq!(feed_until_fire(&mut cap, 500), 50);
    assert_eq!(cap.fires_this_step(), 1);
    assert_eq!(cap.events(), 3);
}

fn tool_loop_messages() -> Vec<ChatMessage> {
    let mut assistant = ChatMessage::assistant("");
    assistant.reasoning = Some("read first".into());
    assistant.tool_calls.push(ToolCall {
        id: "c1".into(),
        name: "read".into(),
        arguments: r#"{"path":"spec.md"}"#.into(),
    });
    vec![
        ChatMessage::system("sys"),
        ChatMessage::user("build it"),
        assistant,
        ChatMessage::tool_result("c1", "the spec"),
    ]
}

#[test]
fn reask_with_reasoning_sent_back_appends_the_partial_turn_and_the_nudge_and_leaves_the_prefix_alone()
 {
    let mut messages = tool_loop_messages();
    let before = messages.clone();
    let mut cap = ReasoningCap::new(5, 1, 3.5, true);
    cap.begin_request();
    assert_eq!(feed_until_fire(&mut cap, 50), 5);

    cap.reask(&mut messages);

    assert_eq!(messages.len(), before.len() + 2);
    assert_eq!(&messages[..before.len()], before.as_slice());
    let partial = &messages[messages.len() - 2];
    assert_eq!(partial.role, Role::Assistant);
    assert_eq!(partial.reasoning.as_deref(), Some(cap.reasoning()));
    // Never empty: a server template may leave out an assistant turn with no text and no call.
    assert_eq!(partial.content.as_deref(), Some(CUT_MARKER));
    assert!(!CUT_MARKER.trim().is_empty());
    assert!(partial.tool_calls.is_empty());
    let last = messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.as_deref(), Some(NUDGE));
}

#[test]
fn reask_partial_turn_goes_over_the_wire_as_reasoning_content() {
    // C# sends the request through ReasoningEchoChatClient; here the wire form of each
    // message with reasoning echo on is checked directly.
    let mut messages = tool_loop_messages();
    let mut cap = ReasoningCap::new(5, 1, 3.5, true);
    cap.begin_request();
    feed_until_fire(&mut cap, 50);
    cap.reask(&mut messages);

    let wire: Vec<_> = messages.iter().map(|m| m.to_wire(true)).collect();
    let roles: Vec<_> = wire.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "assistant", "user"]
    );
    assert_eq!(wire[4]["reasoning_content"].as_str(), Some(cap.reasoning()));
    assert!(wire[4].get("tool_calls").is_none());
    assert!(wire[4]["content"].to_string().contains(CUT_MARKER));
    assert!(wire[5]["content"].to_string().contains(NUDGE));
    assert!(
        !serde_json::to_string(&wire)
            .unwrap()
            .contains("enable_thinking")
    );
}

#[test]
fn reask_without_reasoning_sent_back_carries_the_end_of_the_thinking_in_the_note() {
    // After a tool round the note rides on the last tool result, like other harness notes.
    let mut messages = tool_loop_messages();
    let count = messages.len();
    let mut cap = ReasoningCap::new(5, 1, 3.5, false);
    cap.begin_request();
    cap.observe(&thinking(
        "Plan: write parser.py with a tokenizer, then tests.",
    ));
    feed_until_fire(&mut cap, 50);

    cap.reask(&mut messages);

    assert_eq!(messages.len(), count);
    let result = messages.last().unwrap().content.as_deref().unwrap();
    assert!(result.starts_with(&format!("the spec\n\n{NUDGE}")));
    assert!(result.contains(TAIL_INTRO));
    assert!(result.contains("Plan: write parser.py with a tokenizer, then tests."));

    // No tool result to ride on: a user message.
    let mut fresh = vec![ChatMessage::user("build it")];
    cap.begin_request();
    feed_until_fire(&mut cap, 50);
    cap.reask(&mut fresh);
    assert_eq!(fresh.len(), 2);
    assert_eq!(fresh[1].role, Role::User);
    assert!(fresh[1].content.as_deref().unwrap().starts_with(NUDGE));
}

#[test]
fn note_with_tail_keeps_only_the_end_of_long_thinking() {
    let note = ReasoningCap::note_with_tail(&format!("{}THE PLAN", "a".repeat(5000)));
    assert!(note.ends_with("THE PLAN"));
    assert!(note.len() < TAIL_CHARS + NUDGE.len() + TAIL_INTRO.len() + 10);
    assert_eq!(ReasoningCap::note_with_tail("  \n"), NUDGE);
}
