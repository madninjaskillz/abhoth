//! `DemoChatClient` (`--demo`): canned answers without a server. The first request reads the
//! README, the second lists the folder, then a summary; words stream 8 ms apart.

use std::time::Duration;

use async_trait::async_trait;

use super::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, Role, StreamControl,
    StreamEvent,
};

const TOKEN_DELAY: Duration = Duration::from_millis(8);

const TOOL_CALL_READ_FILE: &str = "Let me start by reading the README to understand this project.\n\n<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"README.md\"}}</tool_call>";

const TOOL_CALL_LIST_DIR: &str = "Good, I can see the README. Let me also check the project structure.\n\n<tool_call>{\"name\":\"list_directory\",\"arguments\":{\"path\":\".\"}}</tool_call>";

const FINAL_SUMMARY: &str = concat!(
    "Here's a summary of the **Abhoth** project:\n\n",
    "**Abhoth** is an autonomous AI coding agent that runs in your terminal. ",
    "It connects to local LLM servers like LM Studio or Ollama via an OpenAI-compatible API ",
    "and uses **31 built-in tools** to read, write, and execute code autonomously.\n\n",
    "**Key highlights:**\n\n",
    "- **Autonomous agent loop** — plans, acts, observes, and iterates up to 25 turns per request\n",
    "- **31 tools** — file I/O, shell execution, git operations, web search, GitHub integration, and sub-agent spawning\n",
    "- **Works with any local model** — Mistral, Llama, DeepSeek, Qwen, and more\n",
    "- **Smart tool calling** — auto-detects native function calling support, falls back to text-based `<tool_call>` tags\n",
    "- **Rich CLI** — streaming output, thinking indicator, slash commands, session management, context compaction\n",
    "- **Privacy-first** — everything runs locally, no data leaves your machine\n\n",
    "The project is structured as a .NET 9 solution with three main assemblies: ",
    "`OpenOrca.Cli` (console UI), `OpenOrca.Core` (domain logic), and `OpenOrca.Tools` (tool implementations). ",
    "It's MIT licensed and designed as a local, open-source alternative to cloud-based AI coding assistants."
);

pub struct DemoChatClient;

/// `PickResponse`: by the number of tool results so far (text-mode `[Tool result for ` user
/// messages, or tool messages).
pub fn pick_response(messages: &[ChatMessage]) -> &'static str {
    let results = messages
        .iter()
        .filter(|m| {
            m.role == Role::Tool
                || (m.role == Role::User
                    && m.content
                        .as_deref()
                        .is_some_and(|t| t.contains("[Tool result for ")))
        })
        .count();
    match results {
        0 => TOOL_CALL_READ_FILE,
        1 => TOOL_CALL_LIST_DIR,
        _ => FINAL_SUMMARY,
    }
}

fn response(text: &str, parts: usize) -> ChatResponse {
    ChatResponse {
        text: text.to_string(),
        finish_reason: Some("stop".into()),
        model: Some("demo-model".into()),
        content_items: parts,
        text_parts: parts,
        ..Default::default()
    }
}

#[async_trait]
impl ChatBackend for DemoChatClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        _options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(response(pick_response(messages), 1))
    }

    async fn stream_with_control(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        _options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let text = pick_response(messages);
        let mut sent = String::new();
        let mut parts = 0;
        for (i, word) in text.split(' ').enumerate() {
            let token = if i == 0 {
                word.to_string()
            } else {
                format!(" {word}")
            };
            sent.push_str(&token);
            parts += 1;
            let stop = on_event(StreamEvent::Text(token)) == StreamControl::Stop;
            on_event(StreamEvent::UpdateEnd {
                tool_call_delta: false,
            });
            if stop {
                break;
            }
            tokio::time::sleep(TOKEN_DELAY).await;
        }
        Ok(response(&sent, parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_tool_results_so_far() {
        let mut m = vec![ChatMessage::user("hi")];
        assert!(pick_response(&m).contains("read_file"));
        m.push(ChatMessage::user("[Tool result for read_file] ..."));
        assert!(pick_response(&m).contains("list_directory"));
        m.push(ChatMessage::tool_result("c1", "x"));
        assert!(pick_response(&m).starts_with("Here's a summary"));
    }
}
