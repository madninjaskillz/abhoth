//! OpenAI-compatible chat client with SSE streaming (what `OpenAIClient` + `ExtraBodyChatClient`
//! + `ServedModelChatClient` + `ReasoningEchoChatClient` do together in C#).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::f32_json;
use super::served_models::{self, ServedModels};
use super::types::{ChatMessage, ChatOptions, ChatResponse, StreamEvent, ToolCall, Usage};

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("HTTP {status} from {url}: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },
    #[error("request to {url} failed: {source}")]
    Transport { url: String, source: reqwest::Error },
    #[error("no response from the server within {0:?}")]
    Timeout(Duration),
    #[error("the server sent no data for {0:?} while streaming")]
    IdleTimeout(Duration),
    #[error("invalid response from the server: {0}")]
    Protocol(String),
    #[error("no model configured: set lmStudio.models or pass a model")]
    NoModel,
}

/// Settings for one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct ClientSettings {
    /// e.g. `http://localhost:1234/v1` (requests go to `{base_url}/chat/completions`).
    pub base_url: String,
    pub api_key: String,
    /// The model used when a request names none.
    pub default_model: Option<String>,
    /// Time allowed until the response headers arrive (`lmStudio.timeoutSeconds`).
    pub request_timeout: Duration,
    /// Longest gap between streamed chunks (`lmStudio.streamingTimeoutSeconds`).
    pub idle_timeout: Duration,
    /// Send each assistant turn's reasoning back as `reasoning_content` (preserve-thinking).
    pub echo_reasoning: bool,
    /// One more top-level body field (the `/uncensored` switch), added last and only when the
    /// request doesn't set it itself.
    pub session_field: Option<(String, Value)>,
    /// The request went straight to a model server (a slot copy), not through a gateway.
    pub direct: bool,
}

impl ClientSettings {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model,
            request_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
            echo_reasoning: false,
            session_field: None,
            direct: false,
        }
    }
}

pub struct OpenAiClient {
    http: reqwest::Client,
    settings: ClientSettings,
    served: Arc<ServedModels>,
}

impl OpenAiClient {
    pub fn new(settings: ClientSettings, served: Arc<ServedModels>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(settings.request_timeout.min(Duration::from_secs(30)))
            .user_agent(concat!("openorca-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builds with static settings");
        Self {
            http,
            settings,
            served,
        }
    }

    pub fn settings(&self) -> &ClientSettings {
        &self.settings
    }

    pub fn served_models(&self) -> &Arc<ServedModels> {
        &self.served
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        )
    }

    /// The JSON body for a request (public for tests and the cross-check harness).
    pub fn build_body(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> Result<Value, ChatError> {
        let model = options
            .model
            .clone()
            .or_else(|| self.settings.default_model.clone())
            .ok_or(ChatError::NoModel)?;
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(model));
        let wire: Vec<Value> = messages
            .iter()
            .map(|m| m.to_wire(self.settings.echo_reasoning))
            .collect();
        body.insert("messages".into(), Value::Array(wire));
        if let Some(t) = options.temperature {
            body.insert("temperature".into(), f32_json(t));
        }
        if let Some(p) = options.top_p {
            body.insert("top_p".into(), f32_json(p));
        }
        if let Some(p) = options.presence_penalty {
            body.insert("presence_penalty".into(), f32_json(p));
        }
        if let Some(n) = options.max_output_tokens {
            // The OpenAI .NET SDK (2.x) sends MaxOutputTokenCount as max_completion_tokens.
            body.insert("max_completion_tokens".into(), json!(n));
        }
        if stream {
            body.insert("stream".into(), json!(true));
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        if !options.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(options.tools.iter().map(|t| t.to_wire()).collect()),
            );
            body.insert("tool_choice".into(), json!("auto"));
        }
        // ExtraBodyChatClient: top_k first, then the additional properties in order, then the
        // session field last (only when the request doesn't set that key itself).
        if let Some(k) = options.top_k {
            body.insert("top_k".into(), json!(k));
        }
        for (k, v) in &options.additional {
            body.insert(k.clone(), v.clone());
        }
        if let Some((k, v)) = &self.settings.session_field
            && !options.additional.contains_key(k)
        {
            body.insert(k.clone(), v.clone());
        }
        Ok(Value::Object(body))
    }

    /// Non-streaming request.
    pub async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let body = self.build_body(messages, options, false)?;
        let requested = body["model"].as_str().map(str::to_string);
        self.served.begin(actor);
        let response = self.send(&body).await?;
        self.note_header(actor, &response);
        let url = self.endpoint();
        let text = tokio::time::timeout(self.settings.request_timeout, response.text())
            .await
            .map_err(|_| ChatError::Timeout(self.settings.request_timeout))?
            .map_err(|source| ChatError::Transport { url, source })?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ChatError::Protocol(format!("{e}: {text}")))?;
        let mut out = ChatResponse {
            model: v["model"].as_str().map(str::to_string),
            ..ChatResponse::default()
        };
        let choice = &v["choices"][0];
        let msg = &choice["message"];
        out.text = msg["content"].as_str().unwrap_or_default().to_string();
        out.reasoning = msg["reasoning_content"]
            .as_str()
            .or(msg["reasoning"].as_str())
            .unwrap_or_default()
            .to_string();
        out.finish_reason = choice["finish_reason"].as_str().map(str::to_string);
        if let Some(calls) = msg["tool_calls"].as_array() {
            for c in calls {
                out.tool_calls.push(ToolCall {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    name: c["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    arguments: c["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
        out.usage = parse_usage(&v["usage"]);
        let served = self.served.complete(
            actor,
            requested.as_deref(),
            out.model.as_deref(),
            self.settings.direct,
        );
        out.served_model = served.served;
        Ok(out)
    }

    /// Streaming request: `on_event` sees text, reasoning and tool-call starts as they arrive;
    /// the assembled response is returned at the end.
    pub async fn stream(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ChatResponse, ChatError> {
        let body = self.build_body(messages, options, true)?;
        let requested = body["model"].as_str().map(str::to_string);
        self.served.begin(actor);
        let response = self.send(&body).await?;
        self.note_header(actor, &response);

        let url = self.endpoint();
        let idle = self.settings.idle_timeout;
        let mut bytes = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut acc = Accumulator::default();
        let mut served_done = false;

        'outer: loop {
            let next = tokio::time::timeout(idle, bytes.next())
                .await
                .map_err(|_| ChatError::IdleTimeout(idle))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|source| ChatError::Transport {
                url: url.clone(),
                source,
            })?;
            for data in parser.push(&chunk) {
                if data.trim() == "[DONE]" {
                    break 'outer;
                }
                acc.apply(&data, on_event)?;
                if !served_done && acc.model.is_some() {
                    served_done = true;
                    self.served.complete(
                        actor,
                        requested.as_deref(),
                        acc.model.as_deref(),
                        self.settings.direct,
                    );
                }
            }
        }
        for data in parser.finish() {
            if data.trim() != "[DONE]" {
                acc.apply(&data, on_event)?;
            }
        }
        let served = if served_done {
            self.served.for_actor(actor).and_then(|s| s.served)
        } else {
            self.served
                .complete(actor, requested.as_deref(), None, self.settings.direct)
                .served
        };
        let mut out = acc.finish();
        out.served_model = served;
        Ok(out)
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, ChatError> {
        let url = self.endpoint();
        let req = self
            .http
            .post(&url)
            .bearer_auth(&self.settings.api_key)
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

    fn note_header(&self, actor: &str, response: &reqwest::Response) {
        for name in served_models::HEADERS {
            if let Some(v) = response.headers().get(name).and_then(|v| v.to_str().ok())
                && let Some(served) = served_models::clean(Some(v))
            {
                self.served.note_header(actor, &served);
                return;
            }
        }
    }
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u["prompt_tokens"].as_i64(),
        completion_tokens: u["completion_tokens"].as_i64(),
        cached_tokens: u["prompt_tokens_details"]["cached_tokens"].as_i64(),
    }
}

/// Server-sent events: collects `data:` lines and yields each event's data at a blank line.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            self.line(&line, &mut events);
        }
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            self.data
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // Comments (":"), "event:", "id:" and "retry:" lines carry nothing we use.
    }

    /// The stream ended: whatever is left (a last event without its blank line).
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.line(rest.trim_end_matches('\r'), &mut events);
        }
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
        }
        events
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles streamed chunks into a [`ChatResponse`]; tool calls are keyed by their index.
#[derive(Default)]
struct Accumulator {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialCall>,
    finish_reason: Option<String>,
    usage: Usage,
    model: Option<String>,
}

impl Accumulator {
    fn apply(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<(), ChatError> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| ChatError::Protocol(format!("{e}: {data}")))?;
        if let Some(err) = v.get("error") {
            return Err(ChatError::Protocol(format!("error in stream: {err}")));
        }
        if self.model.is_none()
            && let Some(m) = v["model"].as_str().filter(|m| !m.is_empty())
        {
            self.model = Some(m.to_string());
        }
        if v.get("usage").is_some_and(|u| u.is_object()) {
            let usage = parse_usage(&v["usage"]);
            self.usage = usage;
            on_event(StreamEvent::Usage(usage));
        }
        let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
            return Ok(());
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        let reasoning = delta["reasoning_content"]
            .as_str()
            .or(delta["reasoning"].as_str());
        if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
            self.reasoning.push_str(r);
            on_event(StreamEvent::Reasoning(r.to_string()));
        }
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            on_event(StreamEvent::Text(t.to_string()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (pos, c) in calls.iter().enumerate() {
                let index = c["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = c["id"].as_str().filter(|s| !s.is_empty()) {
                    entry.id = id.to_string();
                }
                if let Some(name) = c["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                    let first = entry.name.is_empty();
                    entry.name.push_str(name);
                    if first {
                        on_event(StreamEvent::ToolCallStarted {
                            index,
                            name: name.to_string(),
                        });
                    }
                }
                if let Some(args) = c["function"]["arguments"].as_str() {
                    entry.arguments.push_str(args);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> ChatResponse {
        let tool_calls = self
            .calls
            .into_iter()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{i}")
                } else {
                    c.id
                },
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        ChatResponse {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            model: self.model,
            served_model: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_split_chunks_and_crlf() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\"").is_empty());
        let ev = p.push(b":1}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(ev, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
        assert!(p.push(b": keep-alive\n\n").is_empty());
        p.push(b"data: tail");
        assert_eq!(p.finish(), vec!["tail".to_string()]);
    }

    #[test]
    fn accumulator_joins_tool_call_fragments_by_index() {
        let mut acc = Accumulator::default();
        let mut events = Vec::new();
        let mut sink = |e: StreamEvent| events.push(e);
        for d in [
            r#"{"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"glob","arguments":"{\"pat"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"tern\":\"*\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":8}}}"#,
        ] {
            acc.apply(d, &mut sink).unwrap();
        }
        let r = acc.finish();
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].name, "read_file");
        assert_eq!(r.tool_calls[1].arguments, r#"{"pattern":"*"}"#);
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.usage.cached_tokens, Some(8));
        assert_eq!(r.model.as_deref(), Some("m"));
        assert!(events.contains(&StreamEvent::ToolCallStarted {
            index: 1,
            name: "glob".into()
        }));
    }
}
