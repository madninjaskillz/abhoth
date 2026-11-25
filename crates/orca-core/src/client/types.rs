//! Chat messages and request options (the parts of Microsoft.Extensions.AI the C# app uses).

use indexmap::IndexMap;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A tool call the model asked for (`FunctionCallContent`). `arguments` is the JSON text the
/// model produced (possibly invalid JSON; the agent loop repairs or reports it).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One conversation message.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    /// Text content (`None` for an assistant turn that only calls tools).
    pub content: Option<String>,
    /// The reasoning channel (`reasoning_content`), kept so it can be echoed back for
    /// preserve-thinking templates.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool`: the call this answers.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            ..Self::new(Role::Tool, content)
        }
    }

    /// The OpenAI wire form. `echo_reasoning` adds `reasoning_content` to assistant turns
    /// that have reasoning (`ReasoningEchoChatClient`).
    pub fn to_wire(&self, echo_reasoning: bool) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("role".into(), json!(self.role.as_str()));
        match self.role {
            Role::Tool => {
                m.insert(
                    "tool_call_id".into(),
                    json!(self.tool_call_id.clone().unwrap_or_default()),
                );
                m.insert(
                    "content".into(),
                    json!(self.content.clone().unwrap_or_default()),
                );
            }
            Role::Assistant => {
                // The C# client sends "" (not null) for a tool-call-only turn.
                m.insert(
                    "content".into(),
                    json!(self.content.clone().unwrap_or_default()),
                );
                if !self.tool_calls.is_empty() {
                    let calls: Vec<Value> = self
                        .tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                        .collect();
                    m.insert("tool_calls".into(), Value::Array(calls));
                }
                if echo_reasoning
                    && let Some(r) = self.reasoning.as_ref().filter(|r| !r.trim().is_empty())
                {
                    m.insert("reasoning_content".into(), json!(r));
                }
            }
            Role::System | Role::User => {
                m.insert(
                    "content".into(),
                    json!(self.content.clone().unwrap_or_default()),
                );
            }
        }
        Value::Object(m)
    }
}

/// A tool advertised to the model (native function calling).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    pub fn to_wire(&self) -> Value {
        json!({
            "type": "function",
            "function": {"description": self.description, "name": self.name, "parameters": self.parameters}
        })
    }
}

/// Per-request options (`ChatOptions`). Non-standard fields go into `additional`, in order,
/// and reach the wire as top-level body fields (what `ExtraBodyChatClient` does in C#).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatOptions {
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<i32>,
    pub presence_penalty: Option<f32>,
    pub max_output_tokens: Option<i32>,
    pub tools: Vec<ToolDefinition>,
    pub additional: IndexMap<String, Value>,
}

impl ChatOptions {
    pub fn set(&mut self, key: &str, value: Value) {
        self.additional.insert(key.to_string(), value);
    }
}

/// Token accounting reported by the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
}

/// A streamed piece of a response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// A tool call started (its name is known).
    ToolCallStarted {
        index: usize,
        name: String,
    },
    Usage(Usage),
}

/// The whole response, assembled from the stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    /// The model id the response body named.
    pub model: Option<String>,
    /// What served the request (header or body), see [`super::ServedModels`].
    pub served_model: Option<String>,
}

impl ChatResponse {
    /// The assistant message to append to the conversation.
    pub fn to_message(&self) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: if self.text.is_empty() && !self.tool_calls.is_empty() {
                None
            } else {
                Some(self.text.clone())
            },
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning.clone())
            },
            tool_calls: self.tool_calls.clone(),
            tool_call_id: None,
        }
    }
}
