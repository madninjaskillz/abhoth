//! Ollama's native chat API as a [`ChatBackend`] (C# uses OllamaSharp's `OllamaApiClient`).
//! Written against the public HTTP API: `POST {base}/api/chat`, NDJSON streaming (one JSON
//! object per line, the last with `done: true` and the token counts), native tools, and
//! `options` for the sampling settings (temperature, top_p, top_k, num_predict, num_ctx, ...).
//!
//! As OllamaSharp maps `ChatOptions`: temperature / top_p / top_k / presence_penalty and the
//! output cap (`num_predict`) go into `options`, and so do extra fields whose names are Ollama
//! options (`num_ctx`, `repeat_penalty`, `min_p`, ...); other extra fields are dropped. The
//! request's model wins, else the client's. Ollama gives tool calls no ids, so they are
//! numbered here.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use super::backend::ChatBackend;
use super::openai::ChatError;
use super::types::{
    ChatMessage, ChatOptions, ChatResponse, Role, StreamControl, StreamEvent, ToolCall, Usage,
};

/// Ollama's default address when no base URL is configured.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Extra-body fields that are Ollama model options (they go under `options`).
const OPTION_NAMES: [&str; 17] = [
    "num_ctx",
    "num_predict",
    "num_keep",
    "num_batch",
    "num_gpu",
    "num_thread",
    "repeat_penalty",
    "repeat_last_n",
    "frequency_penalty",
    "presence_penalty",
    "min_p",
    "typical_p",
    "seed",
    "stop",
    "mirostat",
    "mirostat_eta",
    "mirostat_tau",
];

#[derive(Debug, Clone)]
pub struct OllamaSettings {
    /// e.g. `http://localhost:11434` (a trailing `/api` or `/v1` is accepted and stripped).
    pub base_url: String,
    pub model: String,
    /// `num_ctx` sent with every request unless the request sets one (the provider's
    /// contextWindow): Ollama otherwise truncates prompts to its small default window.
    pub num_ctx: Option<i32>,
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
}

impl OllamaSettings {
    pub fn new(base_url: Option<&str>, model: &str) -> Self {
        let url = base_url
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = url
            .strip_suffix("/api")
            .or_else(|| url.strip_suffix("/v1"))
            .unwrap_or(url);
        Self {
            base_url: url.to_string(),
            model: model.to_string(),
            num_ctx: None,
            request_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
        }
    }
}

pub struct OllamaClient {
    http: reqwest::Client,
    settings: OllamaSettings,
}

impl OllamaClient {
    pub fn new(settings: OllamaSettings) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(settings.request_timeout.min(Duration::from_secs(30)))
            .build()
            .expect("reqwest client builds with static settings");
        Self { http, settings }
    }

    pub fn settings(&self) -> &OllamaSettings {
        &self.settings
    }

    /// The JSON body of a request (public for tests).
    pub fn build_body(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> Value {
        let mut body = Map::new();
        body.insert(
            "model".into(),
            json!(
                options
                    .model
                    .clone()
                    .unwrap_or_else(|| self.settings.model.clone())
            ),
        );
        body.insert(
            "messages".into(),
            Value::Array(messages.iter().map(wire_message).collect()),
        );
        body.insert("stream".into(), json!(stream));
        if !options.tools.is_empty() {
            let tools: Vec<Value> = options
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {"name": t.name,
                        "description": t.description, "parameters": t.parameters}})
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        let mut o = Map::new();
        if let Some(t) = options.temperature {
            o.insert("temperature".into(), super::f32_json(t));
        }
        if let Some(p) = options.top_p {
            o.insert("top_p".into(), super::f32_json(p));
        }
        if let Some(k) = options.top_k {
            o.insert("top_k".into(), json!(k));
        }
        if let Some(p) = options.presence_penalty {
            o.insert("presence_penalty".into(), super::f32_json(p));
        }
        if let Some(m) = options.max_output_tokens.filter(|m| *m > 0) {
            o.insert("num_predict".into(), json!(m));
        }
        for (k, v) in &options.additional {
            if OPTION_NAMES.contains(&k.as_str()) {
                o.insert(k.clone(), v.clone());
            }
        }
        if let Some(n) = self.settings.num_ctx.filter(|n| *n > 0) {
            o.entry("num_ctx").or_insert_with(|| json!(n));
        }
        if !o.is_empty() {
            body.insert("options".into(), Value::Object(o));
        }
        Value::Object(body)
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, ChatError> {
        let url = format!("{}/api/chat", self.settings.base_url);
        let req = self.http.post(&url).json(body);
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

fn wire_message(m: &ChatMessage) -> Value {
    use base64::Engine;
    let mut w = Map::new();
    w.insert("role".into(), json!(m.role.as_str()));
    w.insert("content".into(), json!(m.text()));
    let images: Vec<Value> = m
        .images()
        .map(|i| json!(base64::engine::general_purpose::STANDARD.encode(&i.data)))
        .collect();
    if !images.is_empty() {
        w.insert("images".into(), Value::Array(images));
    }
    if m.role == Role::Assistant && !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                let args = match serde_json::from_str::<Value>(&c.arguments) {
                    Ok(v @ Value::Object(_)) => v,
                    _ => json!({}),
                };
                json!({"function": {"name": c.name, "arguments": args}})
            })
            .collect();
        w.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(w)
}

#[derive(Default)]
struct Assembler {
    out: ChatResponse,
}

impl Assembler {
    fn apply(
        &mut self,
        line: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<StreamControl, ChatError> {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| ChatError::Protocol(format!("bad line: {e}")))?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            return Err(ChatError::Server(e.to_string()));
        }
        self.out.updates += 1;
        let mut control = StreamControl::Continue;
        let mut emit = |e: StreamEvent, control: &mut StreamControl| {
            if on_event(e) == StreamControl::Stop {
                *control = StreamControl::Stop;
            }
        };
        if self.out.model.is_none() {
            self.out.model = v.get("model").and_then(Value::as_str).map(str::to_string);
        }
        let msg = &v["message"];
        if let Some(t) = msg
            .get("thinking")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.out.reasoning.push_str(t);
            self.out.content_items += 1;
            emit(StreamEvent::Reasoning(t.to_string()), &mut control);
        }
        if let Some(t) = msg
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.out.text.push_str(t);
            self.out.content_items += 1;
            self.out.text_parts += 1;
            emit(StreamEvent::Text(t.to_string()), &mut control);
        }
        let mut tool_delta = false;
        for c in msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let f = &c["function"];
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = match f.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(v) if !v.is_null() => v.to_string(),
                _ => "{}".into(),
            };
            let index = self.out.tool_calls.len();
            let id = c
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{index}"));
            self.out.tool_calls.push(ToolCall {
                id,
                name: name.clone(),
                arguments,
            });
            self.out.content_items += 1;
            tool_delta = true;
            emit(StreamEvent::ToolCallStarted { index, name }, &mut control);
        }
        if v.get("done").and_then(Value::as_bool) == Some(true) {
            if let Some(r) = v.get("done_reason").and_then(Value::as_str) {
                self.out.finish_reason = Some(
                    match r {
                        "length" => "length",
                        "stop" if !self.out.tool_calls.is_empty() => "tool_calls",
                        other => other,
                    }
                    .to_string(),
                );
            }
            let n = |k: &str| v.get(k).and_then(Value::as_i64);
            if n("prompt_eval_count").is_some() || n("eval_count").is_some() {
                self.out.usage = Usage {
                    prompt_tokens: n("prompt_eval_count"),
                    completion_tokens: n("eval_count"),
                    ..Usage::default()
                };
                self.out.usage_reported = true;
                self.out.content_items += 1;
                emit(StreamEvent::Usage(self.out.usage), &mut control);
            }
        }
        emit(
            StreamEvent::UpdateEnd {
                tool_call_delta: tool_delta,
            },
            &mut control,
        );
        Ok(control)
    }
}

#[async_trait]
impl ChatBackend for OllamaClient {
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
        let mut acc = Assembler::default();
        acc.apply(text.trim(), &mut |_| StreamControl::Continue)?;
        Ok(acc.out)
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
        let mut buf: Vec<u8> = Vec::new();
        let mut acc = Assembler::default();
        let mut stopped = false;
        let mut failure: Option<String> = None;
        'outer: loop {
            let next = tokio::time::timeout(idle, bytes.next())
                .await
                .map_err(|_| ChatError::IdleTimeout(idle))?;
            let Some(chunk) = next else { break };
            match chunk {
                Ok(c) => buf.extend_from_slice(&c),
                Err(e) => {
                    failure = Some(format!(
                        "The response ended prematurely: {}",
                        e.without_url()
                    ));
                    break;
                }
            }
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                if line.trim().is_empty() {
                    continue;
                }
                match acc.apply(line.trim(), on_event) {
                    Ok(StreamControl::Continue) => {}
                    Ok(StreamControl::Stop) => {
                        stopped = true;
                        break 'outer;
                    }
                    Err(e @ ChatError::Server(_)) if acc.out.updates == 0 => return Err(e),
                    Err(e) => {
                        failure = Some(e.to_string());
                        break 'outer;
                    }
                }
            }
        }
        if !stopped && failure.is_none() {
            let rest = String::from_utf8_lossy(&buf).trim().to_string();
            if !rest.is_empty()
                && let Err(e) = acc.apply(&rest, on_event)
            {
                failure = Some(e.to_string());
            }
        }
        std::mem::drop(bytes);
        let mut out = acc.out;
        if let Some(message) = failure {
            out.tool_calls.clear();
            return Err(ChatError::Interrupted {
                message,
                partial: Box::new(out),
            });
        }
        if stopped {
            out.tool_calls.clear();
        }
        out.stopped = stopped;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_defaults_and_drops_api_suffixes() {
        assert_eq!(OllamaSettings::new(None, "m").base_url, DEFAULT_BASE_URL);
        assert_eq!(
            OllamaSettings::new(Some("http://h:11434/api/"), "m").base_url,
            "http://h:11434"
        );
        assert_eq!(
            OllamaSettings::new(Some("http://h:11434/v1"), "m").base_url,
            "http://h:11434"
        );
    }
}
