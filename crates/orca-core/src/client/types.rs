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

/// An image in a message (`DataContent` with an image media type): attached to a prompt, or
/// returned by a tool. `name` is the file it came from (a prompt attachment's path), if any.
#[derive(Debug, Clone, PartialEq)]
pub struct ImagePart {
    pub data: Vec<u8>,
    pub media_type: String,
    pub name: Option<String>,
}

impl ImagePart {
    pub fn new(data: Vec<u8>, media_type: impl Into<String>) -> Self {
        Self {
            data,
            media_type: media_type.into(),
            name: None,
        }
    }

    /// The `data:` URI the OpenAI client sends (`data:image/png;base64,...`).
    pub fn data_uri(&self) -> String {
        use base64::Engine;
        format!(
            "data:{};base64,{}",
            self.media_type,
            base64::engine::general_purpose::STANDARD.encode(&self.data)
        )
    }
}

/// A content part after a message's text ([`ChatMessage::parts`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Text(String),
    Image(ImagePart),
}

impl ContentPart {
    /// The OpenAI wire form of the part, as the .NET SDK writes it.
    pub fn to_wire(&self) -> Value {
        match self {
            ContentPart::Text(t) => json!({"type": "text", "text": t}),
            ContentPart::Image(i) => {
                json!({"type": "image_url", "image_url": {"url": i.data_uri()}})
            }
        }
    }
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
    /// The signature Anthropic gave the reasoning (a thinking block's `signature`, C#
    /// `TextReasoningContent.ProtectedData`): only signed reasoning is sent back to Anthropic.
    /// Not saved in sessions; dropped with the reasoning.
    pub reasoning_signature: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool`: the call this answers.
    pub tool_call_id: Option<String>,
    /// The stream opened with an empty content delta before the text. The C# client keeps
    /// that as a separate empty text part and so sends the content as a parts array.
    pub leading_empty_part: bool,
    /// The C# message was built as `new ChatMessage(Assistant, "")` plus the text whatever its
    /// tool calls (a pair-vote winner): non-empty text goes as a parts array with an empty
    /// first part, tool calls or not.
    pub empty_first_part: bool,
    /// Content parts after `content` (C# `Contents` beyond the text): images attached to a
    /// prompt or returned by a tool, and the notes that replace them for a server without
    /// vision. A user or system message with parts goes on the wire as a parts array, text
    /// first (`[{"type":"text",...}, {"type":"image_url",...}]`). Sessions keep only the text.
    pub parts: Vec<ContentPart>,
}

impl ChatMessage {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            reasoning: None,
            reasoning_signature: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            leading_empty_part: false,
            empty_first_part: false,
            parts: Vec::new(),
        }
    }

    /// The message with `images` appended as parts.
    pub fn with_images(mut self, images: impl IntoIterator<Item = ImagePart>) -> Self {
        self.parts
            .extend(images.into_iter().map(ContentPart::Image));
        self
    }

    /// The image parts.
    pub fn images(&self) -> impl Iterator<Item = &ImagePart> {
        self.parts.iter().filter_map(|p| match p {
            ContentPart::Image(i) => Some(i),
            ContentPart::Text(_) => None,
        })
    }

    /// True when the message carries an image part.
    pub fn has_images(&self) -> bool {
        self.images().next().is_some()
    }

    /// All text (content plus text parts), as `ChatMessage.Text` concatenates it in C#.
    pub fn text(&self) -> String {
        let mut out = self.content.clone().unwrap_or_default();
        for p in &self.parts {
            if let ContentPart::Text(t) = p {
                out.push_str(t);
            }
        }
        out
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
                let text = self.content.clone().unwrap_or_default();
                let parts = (self.leading_empty_part && self.tool_calls.is_empty())
                    || self.empty_first_part;
                let content = if parts && !text.is_empty() {
                    json!([{"type": "text", "text": ""}, {"type": "text", "text": text}])
                } else {
                    json!(text)
                };
                m.insert("content".into(), content);
                if !self.tool_calls.is_empty() {
                    let calls: Vec<Value> = self
                        .tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": echo_arguments(&c.arguments)}}))
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
                let text = self.content.clone().unwrap_or_default();
                let content = if self.parts.is_empty() {
                    json!(text)
                } else {
                    let mut parts = vec![json!({"type": "text", "text": text})];
                    parts.extend(self.parts.iter().map(ContentPart::to_wire));
                    Value::Array(parts)
                };
                m.insert("content".into(), content);
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
            "function": {"description": self.description, "name": self.name, "parameters": wire_parameters(&self.parameters)}
        })
    }
}

/// The parameter schema as the Microsoft.Extensions.AI OpenAI adapter sends it: re-read into
/// its `ToolJson` shape, so the top level is exactly `type` (default "object"), `required`
/// (default `[]`), `properties` (default `{}`) and `additionalProperties` (default false), in
/// that order; other top-level keys are dropped and nested schemas pass through unchanged.
pub fn wire_parameters(schema: &Value) -> Value {
    let type_ = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("object");
    let required = schema
        .get("required")
        .filter(|r| r.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]));
    let properties = schema
        .get("properties")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let additional = schema
        .get("additionalProperties")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json!({
        "type": type_,
        "required": required,
        "properties": properties,
        "additionalProperties": additional
    })
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
    /// No `ModelId` in C#: the SDK writes the client's model right after the messages instead
    /// of last (sub-agent requests).
    pub model_from_client: bool,
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
    /// `completion_tokens_details.reasoning_tokens` (`UsageDetails.ReasoningTokenCount`).
    pub reasoning_tokens: Option<i64>,
    /// Tokens written to the prompt cache (Anthropic `cache_creation_input_tokens`, C#
    /// `AdditionalCounts["CacheCreationInputTokens"]`); part of `prompt_tokens`.
    pub cache_creation_tokens: Option<i64>,
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
    /// One streamed update (an SSE chunk) has been fully reported. `tool_call_delta`: the chunk
    /// carried a tool-call fragment (the C# client only surfaces tool calls at the end of the
    /// stream, but its reasoning cap counts the first fragment as the answer starting).
    UpdateEnd {
        tool_call_delta: bool,
    },
}

/// One raw tool-call fragment of a streamed update, as the server sent it (the C# client's
/// `StreamingChatToolCallUpdate`): the call's index, and whichever of its id, name and argument
/// text this chunk carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// What a streaming caller wants after an event: go on, or stop the request now (the stream
/// is dropped, which ends the request; the response so far is returned with `stopped` set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamControl {
    Continue,
    Stop,
}

/// The whole response, assembled from the stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub reasoning: String,
    /// See [`ChatMessage::reasoning_signature`].
    pub reasoning_signature: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    /// The server reported usage (a `usage` object arrived); `usage` is all `None` otherwise.
    pub usage_reported: bool,
    /// llama.cpp / Strata `timings` (the last ones the stream carried).
    pub timings: Option<super::usage::ServerTimings>,
    /// The model id the response body named.
    pub model: Option<String>,
    /// What served the request (header or body), see [`super::ServedModels`].
    pub served_model: Option<String>,
    /// See [`ChatMessage::leading_empty_part`].
    pub leading_empty_part: bool,
    /// See [`ChatMessage::empty_first_part`].
    pub empty_first_part: bool,
    /// Content items as Microsoft.Extensions.AI reports them: each `content` string (an empty
    /// one too), each reasoning piece, each usage report and each finished tool call. A stream
    /// with none is the C# client's "empty response".
    pub content_items: usize,
    /// Text content items (each `content` string, empty ones included).
    pub text_parts: usize,
    /// Streamed updates (SSE chunks with data) received.
    pub updates: usize,
    /// The caller stopped the stream ([`StreamControl::Stop`]).
    pub stopped: bool,
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
            reasoning_signature: self
                .reasoning_signature
                .clone()
                .filter(|_| !self.reasoning.is_empty()),
            tool_calls: self.tool_calls.clone(),
            tool_call_id: None,
            leading_empty_part: self.leading_empty_part,
            empty_first_part: self.empty_first_part,
            parts: Vec::new(),
        }
    }
}

/// Tool-call arguments as the C# client sends them back in history: parsed and re-serialized
/// indented (platform line breaks), or `null` when they were not valid JSON.
pub fn echo_arguments(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => {
            let s = serde_json::to_string_pretty(&v).unwrap_or_default();
            let s = if cfg!(windows) {
                s.replace('\n', "\r\n")
            } else {
                s
            };
            escape_non_bmp(&s)
        }
        Err(_) => "null".into(),
    }
}

/// The relaxed encoder the C# client echoes arguments with escapes characters outside the
/// Basic Multilingual Plane as UTF-16 surrogate pairs (uppercase hex).
fn escape_non_bmp(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) > 0xFFFF {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push('\\');
                out.push('u');
                out.push_str(&format!("{u:04X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}
