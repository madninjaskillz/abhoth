//! Port of `tests/OpenOrca.Cli.Tests/ConversationCompactorTests.cs` against the fake OpenAI
//! server (the C# tests use a scripted `IChatClient`; console output is the report's notices).

use std::sync::Arc;

use orca_core::chat::compaction::{self, CompactionSettings};
use orca_core::chat::compaction_prompts::SUMMARIZATION_SYSTEM_PROMPT;
use orca_core::chat::{CompactionReport, Conversation, ConversationCompactor, LlmCompactor};
use orca_core::client::{ChatMessage, ClientSettings, OpenAiClient, Role, ServedModels, ToolCall};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

const SUMMARY: &str = "## Goal\nTHE SUMMARY";

/// Keep ~`keep_recent_tokens` verbatim; big window so nothing else interferes. No kept head
/// (keepFirstTokens 0) unless asked: these conversations are a few tokens long.
fn create_config(keep_recent_tokens: i32, window: i32, keep_first_tokens: i32) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.context.context_window_size = window;
    config.context.keep_recent_tokens = keep_recent_tokens;
    config.context.keep_first_tokens = keep_first_tokens;
    config.lm_studio.models = vec!["scripted-model".into()];
    config
}

fn default_config() -> OrcaConfig {
    create_config(9, 100_000, 0)
}

/// N user/assistant turn pairs: u1, a1, u2, a2, ...
fn build_conversation(turns: usize) -> Conversation {
    let mut c = Conversation::new();
    for i in 1..=turns {
        c.add_user_message(format!("user message {i}"));
        c.add_assistant_message(format!("assistant reply {i}"));
    }
    c
}

fn call(id: &str, tool: &str, path: &str) -> ChatMessage {
    let mut m = ChatMessage::assistant("");
    m.tool_calls.push(ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments: json!({ "path": path }).to_string(),
    });
    m
}

fn result(id: &str, text: &str) -> ChatMessage {
    ChatMessage::tool_result(id, text)
}

fn text(m: &ChatMessage) -> &str {
    m.content.as_deref().unwrap_or("")
}

fn client_for(server: &FakeOpenAi) -> OpenAiClient {
    OpenAiClient::new(
        ClientSettings::new(server.base_url(), "k", None),
        Arc::new(ServedModels::default()),
    )
}

async fn compact(
    server: &FakeOpenAi,
    config: &OrcaConfig,
    conversation: &mut Conversation,
    extra: Option<&str>,
) -> CompactionReport {
    let client = client_for(server);
    LlmCompactor::new(&client, config, "lead")
        .compact_conversation(conversation, extra, None)
        .await
}

/// The user prompt of each summary request.
fn prompts(server: &FakeOpenAi) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|r| {
            r.body["messages"][1]["content"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn summary_of(m: &ChatMessage) -> String {
    compaction::try_get_summary(m).expect("a checkpoint")
}

#[tokio::test]
async fn compact_summarizes_older_messages_keeps_recent_tail() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(4);

    let report = compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(summary_of(&c.messages()[0]), SUMMARY);
    assert_eq!(c.messages()[0].role, Role::User);
    assert_eq!(
        text(&c.messages()[c.messages().len() - 2]),
        "user message 4"
    );
    assert!(!c.messages().iter().any(|m| text(m) == "user message 1"));
    assert!(report.mentions("Compacted"));
}

#[tokio::test]
async fn compact_keeps_the_first_turns_verbatim_summary_goes_after_them() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(6);

    // ~6 tokens per message: the task and its reply fit the head, the next turn does not.
    compact(&server, &create_config(9, 100_000, 12), &mut c, None).await;

    assert_eq!(text(&c.messages()[0]), "user message 1");
    assert_eq!(text(&c.messages()[1]), "assistant reply 1");
    assert!(compaction::try_get_summary(&c.messages()[2]).is_some());
    assert_eq!(
        text(&c.messages()[c.messages().len() - 2]),
        "user message 6"
    );
    assert!(!c.messages().iter().any(|m| text(m) == "user message 3"));
    let p = prompts(&server);
    assert_eq!(p.len(), 1);
    assert!(!p[0].contains("user message 1")); // the head is kept, not summarized
}

#[tokio::test]
async fn compact_request_uses_pi_system_prompt_serialized_transcript_and_template() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(4);

    compact(&server, &default_config(), &mut c, Some("the API design")).await;

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let messages = requests[0].body["messages"].as_array().unwrap().clone();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], SUMMARIZATION_SYSTEM_PROMPT);
    let prompt = messages[1]["content"].as_str().unwrap();
    assert!(
        prompt.starts_with(
            "<conversation>\n[User]: user message 1\n\n[Assistant]: assistant reply 1"
        )
    );
    assert!(
        prompt.contains("</conversation>\n\nThe messages above are a conversation to summarize.")
    );
    assert!(prompt.contains("## Constraints & Preferences"));
    assert!(prompt.ends_with("\n\nAdditional focus: the API design"));
    assert!(!prompt.contains("<previous-summary>"));
    // Request shape: low temperature, a summary cap, the configured model, not streamed.
    let body = &requests[0].body;
    assert_eq!(body["temperature"], json!(0.3));
    assert_eq!(body["model"], "scripted-model");
    assert!(body["max_completion_tokens"].as_i64().unwrap() >= 512);
    assert_eq!(body.get("stream"), None);
}

#[tokio::test]
async fn compact_with_previous_checkpoint_uses_update_prompt_and_carries_file_lists() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = Conversation::new();
    c.add_message(compaction::create_summary_message(
        "## Goal\nOLD\n\n<read-files>\nold-read.txt\n</read-files>\n\n<modified-files>\nold-mod.txt\n</modified-files>",
    ));
    c.add_user_message("user message 2");
    c.add_message(call("c1", "read", "new-read.txt"));
    c.add_message(result("c1", "contents"));
    c.add_assistant_message("assistant reply 2");
    c.add_user_message("user message 3");
    c.add_assistant_message("assistant reply 3");

    compact(&server, &default_config(), &mut c, None).await;

    let p = prompts(&server);
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("<previous-summary>\n## Goal\nOLD\n</previous-summary>"));
    assert!(p[0].contains("The messages above are NEW conversation messages"));
    assert!(!p[0].contains("[User]: The conversation history before this point")); // old checkpoint not re-serialized
    assert!(p[0].contains("[Assistant tool calls]: read(path=\"new-read.txt\")"));

    let summary = summary_of(&c.messages()[0]);
    assert!(summary.ends_with(
        "\n\n<read-files>\nnew-read.txt\nold-read.txt\n</read-files>\n\n<modified-files>\nold-mod.txt\n</modified-files>"
    ));
    assert_eq!(
        c.messages()
            .iter()
            .filter(|m| text(m).contains("<summary>"))
            .count(),
        1
    );
}

#[tokio::test]
async fn compact_appends_file_lists_from_pi_and_open_orca_tools_and_strips_echoed_lists() {
    let server = FakeOpenAi::start(vec![Reply::text(format!(
        "{SUMMARY}\n\n<read-files>\nhallucinated.txt\n</read-files>"
    ))])
    .await;
    let mut c = Conversation::new();
    c.add_user_message("do it");
    c.add_message(call("c1", "read_file", "a.txt"));
    c.add_message(result("c1", "A"));
    c.add_message(call("c2", "edit", "b.txt"));
    c.add_message(result("c2", "ok"));
    c.add_message(call("c3", "write_file", "c.txt"));
    c.add_message(result("c3", "ok"));
    c.add_message(call("c4", "read", "b.txt"));
    c.add_message(result("c4", "B"));
    c.add_assistant_message("done");
    c.add_user_message("next request");
    c.add_assistant_message("next reply");

    compact(&server, &create_config(7, 100_000, 0), &mut c, None).await;

    assert_eq!(
        summary_of(&c.messages()[0]),
        format!(
            "{SUMMARY}\n\n<read-files>\na.txt\n</read-files>\n\n<modified-files>\nb.txt\nc.txt\n</modified-files>"
        )
    );
}

#[tokio::test]
async fn compact_never_separates_tool_call_from_result() {
    // One long turn of tool calls with big results: whatever the budget, the kept tail must
    // start at a user or assistant message, never at an orphaned tool result.
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    for keep in (5..4000).step_by(97) {
        let mut c = Conversation::new();
        c.add_user_message("refactor everything");
        for i in 0..12 {
            c.add_message(call(&format!("c{i}"), "read", &format!("f{i}.py")));
            c.add_message(result(&format!("c{i}"), &"x".repeat(300 + i * 50)));
        }

        compact(&server, &create_config(keep, 100_000, 0), &mut c, None).await;

        let msgs = c.messages();
        assert_ne!(msgs[1].role, Role::Tool, "keep {keep}");
        let call_ids: Vec<&str> = msgs
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
            .collect();
        for r in msgs.iter().filter(|m| m.role == Role::Tool) {
            assert!(
                call_ids.contains(&r.tool_call_id.as_deref().unwrap()),
                "keep {keep}"
            );
        }
    }
}

#[tokio::test]
async fn compact_split_turn_adds_turn_prefix_summary() {
    let server = FakeOpenAi::start(vec![Reply::text("## Original Request\nPREFIX")]).await;
    let mut c = Conversation::new();
    c.add_user_message("refactor everything");
    for i in 0..6 {
        c.add_message(call(&format!("c{i}"), "read", &format!("f{i}.py")));
        c.add_message(result(&format!("c{i}"), &"x".repeat(400)));
    }

    compact(&server, &create_config(200, 100_000, 0), &mut c, None).await;

    // Nothing before the turn start: only the turn-prefix request is made.
    let p = prompts(&server);
    assert_eq!(p.len(), 1);
    assert!(p[0].starts_with("# Conversation\n[User]: refactor everything"));
    assert!(p[0].contains(
        "# Instructions\nThe messages above are earlier context from an ongoing conversation."
    ));

    let summary = summary_of(&c.messages()[0]);
    assert!(summary.starts_with(
        "No prior history.\n\n---\n\n**Turn Context (split turn):**\n\n## Original Request\nPREFIX"
    ));
    assert_eq!(c.messages()[1].role, Role::Assistant); // kept tail starts at a tool call
    assert!(summary.contains("<read-files>\nf0.py"));
}

#[tokio::test]
async fn compact_system_prompt_survives() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(4);
    c.add_system_message("You are a helpful test agent.");

    compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(c.system_prompt(), Some("You are a helpful test agent."));
    assert!(!c.messages().iter().any(|m| m.role == Role::System));
}

#[tokio::test]
async fn compact_short_conversation_no_ops() {
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(1);

    let report = compact(&server, &create_config(10_000, 100_000, 0), &mut c, None).await;

    assert_eq!(c.messages().len(), 2);
    assert!(server.requests().is_empty()); // no LLM call made
    assert!(report.mentions("Not enough messages"));
}

#[tokio::test]
async fn compact_empty_summary_response_leaves_conversation_unchanged() {
    let server = FakeOpenAi::start(vec![Reply::text("")]).await;
    let mut c = build_conversation(4);

    let report = compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(c.messages().len(), 8);
    assert!(report.mentions("Failed to generate summary"));
}

#[tokio::test]
async fn compact_length_stop_is_a_failure_not_a_checkpoint() {
    let server = FakeOpenAi::start(vec![
        Reply::text("## Goal\npartial").with_finish_reason("length"),
    ])
    .await;
    let mut c = build_conversation(4);

    let report = compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(c.messages().len(), 8);
    assert!(report.mentions("token cap"));
}

#[tokio::test]
async fn compact_llm_error_leaves_conversation_unchanged_when_not_over_budget() {
    let server = FakeOpenAi::start(vec![Reply::error(500, "server exploded")]).await;
    let mut c = build_conversation(4);
    let rewrites = c.rewrite_count();

    let report = compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(c.messages().len(), 8);
    assert_eq!(c.rewrite_count(), rewrites);
    assert!(report.mentions("Failed to compact"));
}

#[tokio::test]
async fn compact_llm_error_over_budget_falls_back_to_pruning_old_tool_results() {
    let server = FakeOpenAi::start(vec![Reply::error(500, "server exploded")]).await;
    let mut c = Conversation::new();
    c.add_user_message("go");
    for i in 0..8 {
        c.add_message(call(&format!("c{i}"), "read", &format!("f{i}.py")));
        c.add_message(result(&format!("c{i}"), &"x".repeat(3000)));
    }

    let config = create_config(1500, 4096, 0);
    compact(&server, &config, &mut c, None).await;

    let settings = CompactionSettings::from_config(&config.context);
    assert!(!settings.should_compact(c.estimate_context_tokens(config.context.chars_per_token)));
    assert!(text(&c.messages()[2]).contains("[tool result pruned"));
}

#[tokio::test]
async fn compact_a_tool_call_reply_is_a_failure() {
    // Pi getSummarizationFailure: a summary request that calls a tool is no checkpoint.
    let server = FakeOpenAi::start(vec![Reply::tool("read_file", json!({"path": "x"}))]).await;
    let mut c = build_conversation(4);

    let report = compact(&server, &default_config(), &mut c, None).await;

    assert_eq!(c.messages().len(), 8);
    assert!(report.mentions("attempted to call a tool"));
    let _: Value = server.requests()[0].body.clone();
}

#[tokio::test]
async fn session_compactor_summarizes_like_the_llm_compactor() {
    // The owned compactor sub-agents use (wire_agents sets it on the orchestrator).
    let server = FakeOpenAi::start(vec![Reply::text(SUMMARY)]).await;
    let mut c = build_conversation(4);
    let compactor = orca_core::agent::SessionCompactor::new(client_for(&server), default_config());

    let report = compactor.compact_conversation(&mut c, None, None).await;

    assert_eq!(summary_of(&c.messages()[0]), SUMMARY);
    assert!(!c.messages().iter().any(|m| text(m) == "user message 1"));
    assert!(report.mentions("Compacted"));
    assert_eq!(server.requests().len(), 1);
}
