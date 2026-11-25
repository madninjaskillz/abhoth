//! The session file (`SessionData`), and the mapping between it and the agent's messages
//! (`SessionManager.ConversationToSession` / `SessionToConversation`).
//!
//! On disk (`<config dir>/sessions/<id>.json`) the file is System.Text.Json output with
//! `JsonDefaults` (camelCase, nulls left out, indented, platform line breaks, default
//! `JavaScriptEncoder` escaping), written by [`crate::config::dotnet_json`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::datetime::DotNetDateTime;
use crate::client::{ChatMessage, Role, ToolCall};

/// `SessionData`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionData {
    pub id: String,
    pub title: String,
    pub created_at: DotNetDateTime,
    pub updated_at: DotNetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<SessionMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_point_message_index: Option<i32>,
    /// The bridge engine that ran the session ("claude", "codex"); `None` = native.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The bridge CLI's session id (resumed with the session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_session_id: Option<String>,
}

impl Default for SessionData {
    fn default() -> Self {
        let now = DotNetDateTime::now();
        Self {
            id: new_session_id(),
            title: "Untitled".into(),
            created_at: now,
            updated_at: now,
            working_directory: None,
            model: None,
            system_prompt: None,
            messages: Vec::new(),
            parent_session_id: None,
            fork_point_message_index: None,
            engine: None,
            engine_session_id: None,
        }
    }
}

/// `SessionMessage`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// An assistant turn's reasoning (sent back so the prompt cache keeps hitting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<SessionToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_results: Option<Vec<SessionToolResult>>,
}

/// `SessionToolCall`. `arguments` is the argument object as indented .NET JSON text.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// `SessionToolResult`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub result: String,
}

/// `Guid.NewGuid().ToString("N")[..8]`: eight lower-case hex digits.
pub fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_i64(DotNetDateTime::now().ticks);
    format!("{:08x}", h.finish() as u32)
}

/// `ConversationToSession`: the agent's messages (system prompt first, if any) as a session.
/// `CreatedAt`/`UpdatedAt` are now (C# builds a fresh `SessionData` on every save).
pub fn messages_to_session(messages: &[ChatMessage]) -> SessionData {
    let mut session = SessionData::default();
    let mut rest = messages;
    if let Some((first, tail)) = messages.split_first()
        && first.role == Role::System
    {
        session.system_prompt = first.content.clone();
        rest = tail;
    }
    session.messages = rest.iter().map(message_to_session).collect();
    session
}

fn message_to_session(m: &ChatMessage) -> SessionMessage {
    let mut out = SessionMessage {
        role: m.role.as_str().into(),
        ..Default::default()
    };
    if m.role == Role::Tool {
        // The text lives in the FunctionResultContent in C#; the message text is empty.
        out.text = Some(String::new());
        out.tool_results = Some(vec![SessionToolResult {
            call_id: m.tool_call_id.clone(),
            result: m.content.clone().unwrap_or_default(),
        }]);
        return out;
    }
    out.text = Some(m.content.clone().unwrap_or_default());
    out.reasoning = m.reasoning.clone().filter(|r| !r.is_empty());
    if !m.tool_calls.is_empty() {
        out.tool_calls = Some(
            m.tool_calls
                .iter()
                .map(|c| SessionToolCall {
                    call_id: Some(c.id.clone()),
                    name: c.name.clone(),
                    arguments: session_arguments(&c.arguments),
                })
                .collect(),
        );
    }
    out
}

/// The stored form of a call's arguments: the argument object re-serialized with
/// `JsonDefaults` (as C# serializes `FunctionCallContent.Arguments`); `None` when the model's
/// text was not a JSON object (C# has no arguments dictionary then).
fn session_arguments(raw: &str) -> Option<String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => crate::config::dotnet_json::to_string_dotnet(&v).ok(),
        _ => None,
    }
}

/// `SessionToConversation`: the session as agent messages, system prompt first. A message
/// with several tool results becomes one tool message per result (the wire form).
pub fn session_to_messages(session: &SessionData) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(session.messages.len() + 1);
    if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        out.push(ChatMessage::system(sp));
    }
    for m in &session.messages {
        let role = match m.role.to_lowercase().as_str() {
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            "system" => Role::System,
            _ => Role::User,
        };
        let text = m.text.clone().unwrap_or_default();
        if role == Role::Tool
            && let Some(results) = m.tool_results.as_ref().filter(|r| !r.is_empty())
        {
            for r in results {
                out.push(ChatMessage::tool_result(
                    r.call_id.clone().unwrap_or_default(),
                    r.result.clone(),
                ));
            }
            continue;
        }
        let tool_calls: Vec<ToolCall> = m
            .tool_calls
            .iter()
            .flatten()
            .map(|c| ToolCall {
                id: c.call_id.clone().unwrap_or_default(),
                name: c.name.clone(),
                arguments: restore_arguments(c.arguments.as_deref()),
            })
            .collect();
        let mut msg = ChatMessage {
            role,
            content: if text.is_empty() && !tool_calls.is_empty() {
                None
            } else {
                Some(text)
            },
            reasoning: m.reasoning.clone().filter(|r| !r.is_empty()),
            reasoning_signature: None,
            tool_calls,
            tool_call_id: None,
            leading_empty_part: false,
            empty_first_part: false,
            parts: Vec::new(),
        };
        // Tool results on a non-tool message (not written by C#, but readable): keep them
        // as tool messages after it so no call is left unanswered.
        let results: Vec<ChatMessage> = m
            .tool_results
            .iter()
            .flatten()
            .map(|r| ChatMessage::tool_result(r.call_id.clone().unwrap_or_default(), &r.result))
            .collect();
        if role == Role::Tool {
            msg.tool_call_id = Some(String::new());
        }
        out.push(msg);
        out.extend(results);
    }
    out
}

/// Stored arguments back to the call's argument text. Text that is not a JSON object is kept
/// under `_raw_json`, as C# does when it cannot deserialize the arguments.
fn restore_arguments(stored: Option<&str>) -> String {
    let Some(s) = stored else {
        return String::new();
    };
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(_)) => s.to_string(),
        // `null` deserializes to a null dictionary: no arguments.
        Ok(Value::Null) => String::new(),
        _ => serde_json::json!({ "_raw_json": s }).to_string(),
    }
}
