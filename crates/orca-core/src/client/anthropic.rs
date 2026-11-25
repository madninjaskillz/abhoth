//! The Anthropic Messages API as a [`ChatBackend`] (`AnthropicChatClient` in C#, which wraps the
//! official SDK's `IChatClient` adapter). Written against the public HTTP API:
//! `POST {base}/v1/messages` with `x-api-key` and `anthropic-version: 2023-06-01`, SSE streaming
//! (`message_start`, `content_block_start`, `content_block_delta` with `text_delta` /
//! `input_json_delta` / `thinking_delta`, `content_block_stop`, `message_delta`,
//! `message_stop`).
//!
//! What OpenOrca's requests need on top, as in C#:
//! - prompt caching: every request sets top-level automatic `cache_control`, so tools + system +
//!   history are read from the cache on the next turn;
//! - effort: `reasoning_effort` (or `chat_template_kwargs.enable_thinking: false`, which becomes
//!   "low") goes as `output_config.effort`;
//! - hygiene: sampling parameters and the OpenAI / llama.cpp extra-body fields are dropped;
//! - reasoning goes back as a `thinking` block only with its signature
//!   ([`ChatMessage::reasoning_signature`]); the API rejects unsigned thinking, so reasoning
//!   without one (restored from a session, or from several thinking blocks) is dropped;
//! - images a tool returned go inside its `tool_result`.
//!
//! Usage: `prompt` = input + cache read + cache creation tokens, `cached` = cache read,
//! [`Usage::cache_creation_tokens`] = cache creation (the ledger's "cache write").

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use super::backend::ChatBackend;
use super::openai::{ChatError, SseParser};
use super::types::{
    ChatMessage, ChatOptions, ChatResponse, ContentPart, Role, StreamControl, StreamEvent,
    ToolCall, Usage,
};

/// Output cap when neither the request nor config sets one.
pub const DEFAULT_MAX_OUTPUT_TOKENS: i32 = 32000;
/// The API version header every request carries.
pub const API_VERSION: &str = "2023-06-01";
/// The public endpoint when no base URL is configured.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// One Anthropic endpoint and model.
#[derive(Debug, Clone)]
pub struct AnthropicSettings {
    /// e.g. `https://api.anthropic.com` (a trailing `/v1` is accepted and stripped).
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: i32,
    /// Time allowed until the response headers arrive.
    pub request_timeout: Duration,
    /// Longest gap between streamed events.
    pub idle_timeout: Duration,
}

impl AnthropicSettings {
    pub fn new(base_url: Option<&str>, api_key: &str, model: &str) -> Self {
        Self {
            base_url: normalize_base_url(base_url.unwrap_or(DEFAULT_BASE_URL)),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            request_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
        }
    }
}

/// `AnthropicChatClient.NormalizeBaseUrl`: the endpoint path is added here, so a configured
/// trailing "/v1" is stripped; blank means the public API.
pub fn normalize_base_url(base_url: &str) -> String {
    let url = base_url.trim().trim_end_matches('/');
    if url.is_empty() {
        return DEFAULT_BASE_URL.into();
    }
    match url.len().checked_sub(3) {
        Some(i) if url.is_char_boundary(i) && url[i..].eq_ignore_ascii_case("/v1") => {
            url[..i].to_string()
        }
        _ => url.to_string(),
    }
}

pub struct AnthropicClient {
    http: reqwest::Client,
    settings: AnthropicSettings,
}

impl AnthropicClient {
    pub fn new(settings: AnthropicSettings) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(settings.request_timeout.min(Duration::from_secs(30)))
            .build()
            .expect("reqwest client builds with static settings");
        Self { http, settings }
    }

    pub fn settings(&self) -> &AnthropicSettings {
        &self.settings
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.settings.base_url)
    }

    /// The JSON body of a request (public for tests).
    pub fn build_body(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> Value {
        // An OpenAI-style model id from the shared config (a failover list) is not ours.
        let mut model = options
            .model
            .clone()
            .unwrap_or_else(|| self.settings.model.clone());
        if !starts_with_ci(&model, "claude") && starts_with_ci(&self.settings.model, "claude") {
            model = self.settings.model.clone();
        }
        let max_tokens = options
            .max_output_tokens
            .filter(|m| *m > 0)
            .unwrap_or(self.settings.max_output_tokens);
        let (system, wire) = wire_messages(messages);
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert("max_tokens".into(), json!(max_tokens));
        body.insert("messages".into(), Value::Array(wire));
        if !system.is_empty() {
            body.insert("system".into(), Value::Array(system));
        }
        if !options.tools.is_empty() {
            let tools: Vec<Value> = options
                .tools
                .iter()
                .map(|t| {
                    json!({"name": t.name, "description": t.description,
                        "input_schema": input_schema(&t.parameters)})
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        // Automatic caching: the breakpoint goes on the last cacheable block of each request.
        body.insert("cache_control".into(), json!({"type": "ephemeral"}));
        if let Some(effort) = resolve_effort(&options.additional) {
            body.insert("output_config".into(), json!({"effort": effort}));
        }
        if stream {
            body.insert("stream".into(), json!(true));
        }
        Value::Object(body)
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, ChatError> {
        let url = self.endpoint();
        let req = self
            .http
            .post(&url)
            .header("x-api-key", &self.settings.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(body);
        let response = tokio::time::timeout(self.settings.request_timeout, req.send())
            .await
            .map_err(|_| ChatError::Timeout(self.settings.request_timeout))?
            .map_err(|source| ChatError::Transport {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ChatError::Http {
                status: status.as_u16(),
                url,
                body,
            });
        }
        Ok(response)
    }
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The tool's JSON schema as `input_schema` (an object schema; a missing type is "object").
fn input_schema(parameters: &Value) -> Value {
    let mut schema = match parameters {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".into()));
    Value::Object(schema)
}

/// `AnthropicChatClient.ResolveEffort`: the effort a request asks for ("low".."max"), or `None`
/// for the model's default. Thinking turned off, "off", "none" and "minimal" become "low".
pub fn resolve_effort(additional: &indexmap::IndexMap<String, Value>) -> Option<String> {
    if additional
        .get("chat_template_kwargs")
        .and_then(|k| k.get("enable_thinking"))
        == Some(&Value::Bool(false))
    {
        return Some("low".into());
    }
    let text = match additional.get("reasoning_effort")? {
        Value::String(s) => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    if matches!(text.as_str(), "off" | "none" | "minimal") {
        return Some("low".into());
    }
    EFFORTS.contains(&text.as_str()).then_some(text)
}

fn image_block(i: &super::types::ImagePart) -> Value {
    use base64::Engine;
    json!({"type": "image", "source": {"type": "base64", "media_type": i.media_type,
        "data": base64::engine::general_purpose::STANDARD.encode(&i.data)}})
}

/// The messages in the Messages API's shape: system text as top-level blocks; tool results as
/// `tool_result` blocks of a user turn (with the images the tool returned); consecutive turns
/// of the same role merged (tool results must all be in the user turn after the tool calls).
pub fn wire_messages(messages: &[ChatMessage]) -> (Vec<Value>, Vec<Value>) {
    let mut system = Vec::new();
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let push =
        |out: &mut Vec<(&'static str, Vec<Value>)>, role: &'static str, blocks: Vec<Value>| {
            if blocks.is_empty() {
                return;
            }
            match out.last_mut() {
                Some((r, b)) if *r == role => b.extend(blocks),
                _ => out.push((role, blocks)),
            }
        };
    for m in messages {
        match m.role {
            Role::System => {
                let text = m.text();
                if !text.is_empty() {
                    system.push(json!({"type": "text", "text": text}));
                }
            }
            Role::User => {
                let mut blocks = Vec::new();
                if let Some(t) = m.content.as_deref().filter(|t| !t.is_empty()) {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for p in &m.parts {
                    match p {
                        ContentPart::Text(t) if !t.is_empty() => {
                            blocks.push(json!({"type": "text", "text": t}))
                        }
                        ContentPart::Text(_) => {}
                        ContentPart::Image(i) => blocks.push(image_block(i)),
                    }
                }
                push(&mut out, "user", blocks);
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                // Only signed reasoning is sent back (see the module docs).
                if let (Some(r), Some(sig)) = (
                    m.reasoning.as_deref().filter(|r| !r.is_empty()),
                    m.reasoning_signature.as_deref().filter(|s| !s.is_empty()),
                ) {
                    blocks.push(json!({"type": "thinking", "thinking": r, "signature": sig}));
                }
                let text = m.text();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for c in &m.tool_calls {
                    let input = match serde_json::from_str::<Value>(&c.arguments) {
                        Ok(v @ Value::Object(_)) => v,
                        _ => json!({}),
                    };
                    blocks.push(
                        json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}),
                    );
                }
                push(&mut out, "assistant", blocks);
            }
            Role::Tool => {
                let text = m.content.clone().unwrap_or_default();
                let images: Vec<Value> = m.images().map(image_block).collect();
                let content = if images.is_empty() {
                    json!(text)
                } else {
                    let mut c = vec![json!({"type": "text", "text": text})];
                    c.extend(images);
                    Value::Array(c)
                };
                push(
                    &mut out,
                    "user",
                    vec![json!({"type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": content})],
                );
            }
        }
    }
    let wire = out
        .into_iter()
        .map(|(role, blocks)| json!({"role": role, "content": blocks}))
        .collect();
    (system, wire)
}

/// `stop_reason` as the OpenAI finish reasons the agent loop reads.
fn finish_reason(stop: &str) -> String {
    match stop {
        "end_turn" | "stop_sequence" | "pause_turn" => "stop",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        other => other,
    }
    .to_string()
}

/// Token counts from a `usage` object, merged into `usage` (a `message_delta` carries the final
/// output count and may repeat the input counts).
fn apply_usage(usage: &mut Usage, raw: &Value, input: &mut [i64; 3]) {
    let n = |k: &str| raw.get(k).and_then(Value::as_i64);
    if let Some(v) = n("input_tokens") {
        input[0] = v;
    }
    if let Some(v) = n("cache_read_input_tokens") {
        input[1] = v;
    }
    if let Some(v) = n("cache_creation_input_tokens") {
        input[2] = v;
    }
    usage.prompt_tokens = Some(input[0] + input[1] + input[2]);
    usage.cached_tokens = Some(input[1]);
    usage.cache_creation_tokens = Some(input[2]);
    if let Some(v) = n("output_tokens") {
        usage.completion_tokens = Some(v);
    }
}

/// An `error` event or body's message.
fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    let kind = e.get("type").and_then(Value::as_str).unwrap_or("error");
    let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
    Some(format!("{kind}: {msg}"))
}

#[derive(Default)]
struct Block {
    kind: String,
    id: String,
    name: String,
    json: String,
}

/// Builds the response from the stream's events.
#[derive(Default)]
struct Assembler {
    out: ChatResponse,
    blocks: BTreeMap<u64, Block>,
    /// The tool-call order (stream index -> call number).
    calls: Vec<u64>,
    input: [i64; 3],
    thinking_blocks: usize,
    signature: String,
}

impl Assembler {
    fn apply(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<StreamControl, ChatError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ChatError::Protocol(format!("bad event: {e}")))?;
        self.out.updates += 1;
        let mut tool_delta = false;
        let mut control = StreamControl::Continue;
        let mut emit = |e: StreamEvent, control: &mut StreamControl| {
            if on_event(e) == StreamControl::Stop {
                *control = StreamControl::Stop;
            }
        };
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let m = &v["message"];
                self.out.model = m.get("model").and_then(Value::as_str).map(str::to_string);
                if let Some(u) = m.get("usage") {
                    apply_usage(&mut self.out.usage, u, &mut self.input);
                    self.out.usage_reported = true;
                }
            }
            "content_block_start" => {
                let b = &v["content_block"];
                let kind = b
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut block = Block {
                    kind: kind.clone(),
                    ..Block::default()
                };
                match kind.as_str() {
                    "tool_use" => {
                        block.id = b.get("id").and_then(Value::as_str).unwrap_or("").into();
                        block.name = b.get("name").and_then(Value::as_str).unwrap_or("").into();
                        self.calls.push(index);
                        tool_delta = true;
                        emit(
                            StreamEvent::ToolCallStarted {
                                index: self.calls.len() - 1,
                                name: block.name.clone(),
                            },
                            &mut control,
                        );
                    }
                    "thinking" => {
                        self.thinking_blocks += 1;
                        if let Some(sig) = b.get("signature").and_then(Value::as_str) {
                            self.signature.push_str(sig);
                        }
                    }
                    "text" => {
                        if let Some(t) = b
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            self.text(t, &mut emit, &mut control);
                        }
                    }
                    _ => {}
                }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let t = d.get("text").and_then(Value::as_str).unwrap_or("");
                        self.text(t, &mut emit, &mut control);
                    }
                    "thinking_delta" => {
                        let t = d.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            self.out.reasoning.push_str(t);
                            self.out.content_items += 1;
                            emit(StreamEvent::Reasoning(t.to_string()), &mut control);
                        }
                    }
                    "signature_delta" => {
                        self.signature
                            .push_str(d.get("signature").and_then(Value::as_str).unwrap_or(""));
                    }
                    "input_json_delta" => {
                        tool_delta = true;
                        if let Some(b) = self.blocks.get_mut(&index) {
                            b.json.push_str(
                                d.get("partial_json").and_then(Value::as_str).unwrap_or(""),
                            );
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(stop) = v["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.out.finish_reason = Some(finish_reason(stop));
                }
                if let Some(u) = v.get("usage") {
                    apply_usage(&mut self.out.usage, u, &mut self.input);
                    self.out.usage_reported = true;
                    emit(StreamEvent::Usage(self.out.usage), &mut control);
                }
            }
            "error" => {
                return Err(ChatError::Server(
                    error_message(&v).unwrap_or_else(|| data.to_string()),
                ));
            }
            // content_block_stop, message_stop, ping: nothing to add.
            _ => {}
        }
        emit(
            StreamEvent::UpdateEnd {
                tool_call_delta: tool_delta,
            },
            &mut control,
        );
        Ok(control)
    }

    fn text(
        &mut self,
        t: &str,
        emit: &mut impl FnMut(StreamEvent, &mut StreamControl),
        control: &mut StreamControl,
    ) {
        self.out.text.push_str(t);
        self.out.content_items += 1;
        self.out.text_parts += 1;
        emit(StreamEvent::Text(t.to_string()), control);
    }

    fn finish(mut self, with_calls: bool) -> ChatResponse {
        // One signature per thinking block: several blocks can't share one reasoning string.
        if self.thinking_blocks == 1 && !self.signature.is_empty() {
            self.out.reasoning_signature = Some(std::mem::take(&mut self.signature));
        }
        if with_calls {
            for index in &self.calls {
                let Some(b) = self.blocks.remove(index) else {
                    continue;
                };
                if b.kind != "tool_use" {
                    continue;
                }
                self.out.tool_calls.push(ToolCall {
                    id: b.id,
                    name: b.name,
                    arguments: if b.json.trim().is_empty() {
                        "{}".into()
                    } else {
                        b.json
                    },
                });
                self.out.content_items += 1;
            }
        }
        if self.out.usage_reported {
            self.out.content_items += 1;
        }
        self.out
    }
}

/// A non-streamed answer's content blocks.
fn parse_message(v: &Value) -> ChatResponse {
    let mut out = ChatResponse {
        model: v.get("model").and_then(Value::as_str).map(str::to_string),
        updates: 1,
        ..ChatResponse::default()
    };
    let mut thinking_blocks = 0;
    for b in v
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match b.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                out.text
                    .push_str(b.get("text").and_then(Value::as_str).unwrap_or(""));
                out.text_parts += 1;
                out.content_items += 1;
            }
            "thinking" => {
                out.reasoning
                    .push_str(b.get("thinking").and_then(Value::as_str).unwrap_or(""));
                out.content_items += 1;
                thinking_blocks += 1;
                out.reasoning_signature = b
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty() && thinking_blocks == 1)
                    .map(str::to_string);
            }
            "tool_use" => {
                out.tool_calls.push(ToolCall {
                    id: b.get("id").and_then(Value::as_str).unwrap_or("").into(),
                    name: b.get("name").and_then(Value::as_str).unwrap_or("").into(),
                    arguments: b
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".into()),
                });
                out.content_items += 1;
            }
            _ => {}
        }
    }
    if let Some(stop) = v.get("stop_reason").and_then(Value::as_str) {
        out.finish_reason = Some(finish_reason(stop));
    }
    if let Some(u) = v.get("usage") {
        apply_usage(&mut out.usage, u, &mut [0; 3]);
        out.usage_reported = true;
        out.content_items += 1;
    }
    out
}

#[async_trait]
impl ChatBackend for AnthropicClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let body = self.build_body(messages, options, false);
        let response = self.send(&body).await?;
        let text = tokio::time::timeout(self.settings.request_timeout, response.text())
            .await
            .map_err(|_| ChatError::Timeout(self.settings.request_timeout))?
            .map_err(|e| ChatError::Protocol(e.without_url().to_string()))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ChatError::Protocol(format!("bad response body: {e}")))?;
        if let Some(e) = error_message(&v) {
            return Err(ChatError::Server(e));
        }
        Ok(parse_message(&v))
    }

    async fn stream_with_control(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let body = self.build_body(messages, options, true);
        let response = self.send(&body).await?;
        let idle = self.settings.idle_timeout;
        let mut bytes = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut acc = Assembler::default();
        let mut stopped = false;
        let mut failure: Option<String> = None;
        'outer: loop {
            let next = tokio::time::timeout(idle, bytes.next())
                .await
                .map_err(|_| ChatError::IdleTimeout(idle))?;
            let Some(chunk) = next else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    failure = Some(format!(
                        "The response ended prematurely: {}",
                        e.without_url()
                    ));
                    break;
                }
            };
            let mut events = parser.push(&chunk);
            if events.is_empty() {
                continue;
            }
            for data in events.drain(..) {
                match acc.apply(&data, on_event) {
                    Ok(StreamControl::Continue) => {}
                    Ok(StreamControl::Stop) => {
                        stopped = true;
                        break 'outer;
                    }
                    Err(e @ ChatError::Server(_)) if acc.out.updates <= 1 => return Err(e),
                    Err(e) => {
                        failure = Some(e.to_string());
                        break 'outer;
                    }
                }
            }
        }
        if !stopped && failure.is_none() {
            for data in parser.finish() {
                if let Err(e) = acc.apply(&data, on_event) {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
        std::mem::drop(bytes);
        if let Some(message) = failure {
            return Err(ChatError::Interrupted {
                message,
                partial: Box::new(acc.finish(false)),
            });
        }
        let mut out = acc.finish(!stopped);
        out.stopped = stopped;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_drops_a_trailing_v1() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com"
        );
        assert_eq!(normalize_base_url("http://h:1/"), "http://h:1");
        assert_eq!(normalize_base_url(" "), DEFAULT_BASE_URL);
    }

    #[test]
    fn effort_as_csharp_resolves_it() {
        let mut a = indexmap::IndexMap::new();
        assert_eq!(resolve_effort(&a), None);
        a.insert("reasoning_effort".into(), json!("High"));
        assert_eq!(resolve_effort(&a).as_deref(), Some("high"));
        a.insert("reasoning_effort".into(), json!("off"));
        assert_eq!(resolve_effort(&a).as_deref(), Some("low"));
        a.insert("reasoning_effort".into(), json!("turbo"));
        assert_eq!(resolve_effort(&a), None);
        a.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": false}),
        );
        assert_eq!(resolve_effort(&a).as_deref(), Some("low"));
    }

    #[test]
    fn finish_reasons_map_to_the_openai_names() {
        assert_eq!(finish_reason("end_turn"), "stop");
        assert_eq!(finish_reason("max_tokens"), "length");
        assert_eq!(finish_reason("tool_use"), "tool_calls");
    }
}
