//! OpenAI-compatible chat client with SSE streaming (what `OpenAIClient` + `ExtraBodyChatClient`
//! + `ServedModelChatClient` + `ReasoningEchoChatClient` do together in C#).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::f32_json;
use super::served_models::{self, ServedModels};
use super::tool_images;
use super::types::{
    ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent, ToolCall, ToolCallDelta,
    Usage,
};

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
    /// An error message the server gave some other way (the server error probe).
    #[error("{0}")]
    Server(String),
    /// The stream broke after it had started (the connection dropped, a chunk could not be
    /// read). `partial` holds what arrived before, without tool calls: the C# client only
    /// surfaces those once the stream has ended.
    #[error("{message}")]
    Interrupted {
        message: String,
        partial: Box<ChatResponse>,
    },
}

impl ChatError {
    /// The HTTP status of a rejected request, if that is what this is.
    pub fn status(&self) -> Option<u16> {
        match self {
            ChatError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
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
    /// Whether the server takes images (`lmStudio.vision`); `None` = ask the server once
    /// ([`super::tool_images::supports_vision`]).
    pub vision: Option<bool>,
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
            vision: None,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    settings: ClientSettings,
    served: Arc<ServedModels>,
    /// Requests go here instead (sticky slots: the Lead's slot-pinned client).
    router: Option<Arc<dyn super::backend::ChatBackend>>,
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
            router: None,
        }
    }

    /// Route every request through `router` (e.g. the slot runtime's Lead client); the
    /// settings stay this client's (the model names the session sends).
    pub fn with_router(mut self, router: Arc<dyn super::backend::ChatBackend>) -> Self {
        self.router = Some(router);
        self
    }

    /// This client without its router: what the router itself falls back to.
    pub fn without_router(&self) -> Self {
        Self {
            router: None,
            ..self.clone()
        }
    }

    pub fn router(&self) -> Option<&Arc<dyn super::backend::ChatBackend>> {
        self.router.as_ref()
    }

    pub fn settings(&self) -> &ClientSettings {
        &self.settings
    }

    /// For switches the REPL changes between requests (`/uncensored`).
    pub fn settings_mut(&mut self) -> &mut ClientSettings {
        &mut self.settings
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
        // Key order as the OpenAI .NET SDK writes it (checked by the cross-check harness):
        // temperature, top_p, messages, max_completion_tokens, presence_penalty, stream,
        // stream_options, tools, tool_choice; the ExtraBodyChatClient fields; model.
        let mut body = serde_json::Map::new();
        if let Some(t) = options.temperature {
            body.insert("temperature".into(), f32_json(t));
        }
        if let Some(p) = options.top_p {
            body.insert("top_p".into(), f32_json(p));
        }
        let wire: Vec<Value> = messages
            .iter()
            .map(|m| m.to_wire(self.settings.echo_reasoning))
            .collect();
        body.insert("messages".into(), Value::Array(wire));
        // A request that names no model of its own (a sub-agent's: the C# orchestrator leaves
        // `ChatOptions.ModelId` unset) gets the client's model right after the messages.
        let early_model = options.model_from_client && options.model.is_none();
        if early_model {
            body.insert("model".into(), json!(model));
        }
        if let Some(n) = options.max_output_tokens {
            // The OpenAI .NET SDK (2.x) sends MaxOutputTokenCount as max_completion_tokens.
            body.insert("max_completion_tokens".into(), json!(n));
        }
        if let Some(p) = options.presence_penalty {
            body.insert("presence_penalty".into(), f32_json(p));
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
        // The SDK writes `model` after the patched extra-body fields.
        if !early_model {
            body.insert("model".into(), json!(model));
        }
        Ok(Value::Object(body))
    }

    /// Whether this server takes images now (`ToolImagesChatClient.SupportsVisionAsync`).
    async fn supports_vision(&self) -> bool {
        tool_images::supports_vision(
            &self.settings.base_url,
            Some(&self.settings.api_key),
            self.settings.vision,
        )
        .await
    }

    /// Non-streaming request. Images in the messages go as `ToolImagesChatClient` sends them
    /// ([`tool_images::rewrite`]); a server that refuses them gets the request once more
    /// without.
    pub async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        if let Some(router) = &self.router {
            return router.complete(actor, messages, options).await;
        }
        if !tool_images::has_images(messages) {
            return self.complete_once(actor, messages, options).await;
        }
        let with_images = self.supports_vision().await;
        match self
            .complete_once(actor, &tool_images::rewrite(messages, with_images), options)
            .await
        {
            Err(e) if with_images && tool_images::is_vision_refusal(&e) => {
                tool_images::mark_refused(&self.settings.base_url, self.settings.vision, &e);
            }
            other => return other,
        }
        self.complete_once(actor, &tool_images::rewrite(messages, false), options)
            .await
    }

    async fn complete_once(
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
        out.usage_reported = v["usage"].is_object();
        out.timings = super::usage::ServerTimings::from_value(&v["timings"]);
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
        let mut control = |e: StreamEvent| {
            on_event(e);
            StreamControl::Continue
        };
        self.stream_with_control(actor, messages, options, &mut control)
            .await
    }

    /// [`Self::stream`] with a stop hook: when `on_event` returns [`StreamControl::Stop`] the
    /// stream is dropped (ending the request) and the response so far comes back with
    /// `stopped` set. An error after the stream started is [`ChatError::Interrupted`] with the
    /// partial response; an idle gap stays [`ChatError::IdleTimeout`].
    pub async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.stream_observed(actor, messages, options, on_event, None)
            .await
    }

    /// [`Self::stream_with_control`] that also hands every raw tool-call fragment to
    /// `on_tool_delta` as it arrives (the early read-only tool runs: the C# client's raw
    /// `StreamingChatCompletionUpdate.ToolCallUpdates`), before the update's other events.
    /// A routed client (slots) passes no fragments: its calls then start at the stream's end.
    pub async fn stream_observed(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
        mut on_tool_delta: Option<&mut (dyn FnMut(&ToolCallDelta) + Send)>,
    ) -> Result<ChatResponse, ChatError> {
        if let Some(router) = &self.router {
            return router
                .stream_with_control(actor, messages, options, on_event)
                .await;
        }
        if !tool_images::has_images(messages) {
            return self
                .stream_once(actor, messages, options, on_event, on_tool_delta)
                .await;
        }
        let with_images = self.supports_vision().await;
        match self
            .stream_once(
                actor,
                &tool_images::rewrite(messages, with_images),
                options,
                on_event,
                on_tool_delta
                    .as_mut()
                    .map(|f| &mut **f as &mut (dyn FnMut(&ToolCallDelta) + Send)),
            )
            .await
        {
            // The refusal is the answer to the request itself: nothing has streamed yet.
            Err(e) if with_images && tool_images::is_vision_refusal(&e) => {
                tool_images::mark_refused(&self.settings.base_url, self.settings.vision, &e);
            }
            other => return other,
        }
        self.stream_once(
            actor,
            &tool_images::rewrite(messages, false),
            options,
            on_event,
            on_tool_delta,
        )
        .await
    }

    async fn stream_once(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
        mut on_tool_delta: Option<&mut (dyn FnMut(&ToolCallDelta) + Send)>,
    ) -> Result<ChatResponse, ChatError> {
        let body = self.build_body(messages, options, true)?;
        let requested = body["model"].as_str().map(str::to_string);
        self.served.begin(actor);
        let response = self.send(&body).await?;
        self.note_header(actor, &response);

        let idle = self.settings.idle_timeout;
        let mut bytes = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut acc = Accumulator::default();
        let mut served_done = false;
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
                    // No URL in the message: C# shows the transport's own words, and a port
                    // number must not look like a status code to the retry classifier.
                    failure = Some(format!(
                        "The response ended prematurely: {}",
                        error_chain(&e.without_url())
                    ));
                    break;
                }
            };
            for data in parser.push(&chunk) {
                if data.trim() == "[DONE]" {
                    break 'outer;
                }
                match acc.apply_observed(&data, on_event, on_tool_delta.as_deref_mut()) {
                    Ok(StreamControl::Continue) => {}
                    Ok(StreamControl::Stop) => {
                        stopped = true;
                        break 'outer;
                    }
                    Err(e) => {
                        failure = Some(e.to_string());
                        break 'outer;
                    }
                }
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
        if !stopped && failure.is_none() {
            for data in parser.finish() {
                if data.trim() == "[DONE]" {
                    continue;
                }
                match acc.apply_observed(&data, on_event, on_tool_delta.as_deref_mut()) {
                    Ok(StreamControl::Continue) => {}
                    Ok(StreamControl::Stop) => {
                        stopped = true;
                        break;
                    }
                    Err(e) => {
                        failure = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        // Dropping the byte stream closes the connection, which ends a stopped request.
        drop(bytes);
        let served = if served_done {
            self.served.for_actor(actor).and_then(|s| s.served)
        } else {
            self.served
                .complete(actor, requested.as_deref(), None, self.settings.direct)
                .served
        };
        if let Some(message) = failure {
            let mut partial = acc.finish(false);
            partial.served_model = served;
            return Err(ChatError::Interrupted {
                message,
                partial: Box::new(partial),
            });
        }
        // A stopped stream never ended: like the C# client, its tool calls are not surfaced.
        let mut out = acc.finish(!stopped);
        out.stopped = stopped;
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

/// An error with its sources, ": "-joined.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cur = s.source();
    }
    out
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u["prompt_tokens"].as_i64(),
        completion_tokens: u["completion_tokens"].as_i64(),
        cached_tokens: u["prompt_tokens_details"]["cached_tokens"].as_i64(),
        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_i64(),
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
    usage_reported: bool,
    timings: Option<super::usage::ServerTimings>,
    model: Option<String>,
    leading_empty_part: bool,
    content_items: usize,
    text_parts: usize,
    updates: usize,
}

impl Accumulator {
    /// Apply one SSE data chunk; the events go to `on_event`, ending with
    /// [`StreamEvent::UpdateEnd`]. Stops early when `on_event` says so.
    #[cfg(test)]
    fn apply(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<StreamControl, ChatError> {
        self.apply_observed(data, on_event, None)
    }

    /// [`Self::apply`] that also reports each raw tool-call fragment to `on_tool_delta`.
    fn apply_observed<'d>(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
        on_tool_delta: Option<&mut (dyn FnMut(&ToolCallDelta) + Send + 'd)>,
    ) -> Result<StreamControl, ChatError> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| ChatError::Protocol(format!("{e}: {data}")))?;
        if let Some(err) = v.get("error") {
            return Err(ChatError::Protocol(format!("error in stream: {err}")));
        }
        self.updates += 1;
        let mut stop = false;
        let mut emit = |e: StreamEvent| {
            if on_event(e) == StreamControl::Stop {
                stop = true;
            }
        };
        if self.model.is_none()
            && let Some(m) = v["model"].as_str().filter(|m| !m.is_empty())
        {
            self.model = Some(m.to_string());
        }
        // RequestStats: the timings ride next to the usage (any chunk's win over earlier ones).
        if let Some(t) = super::usage::ServerTimings::from_value(&v["timings"]) {
            self.timings = Some(t);
        }
        // Microsoft.Extensions.AI reports an update's text and reasoning before its usage.
        let mut tool_call_delta = false;
        if let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) {
            if let Some(observe) = on_tool_delta
                && let Some(calls) = choice["delta"]["tool_calls"].as_array()
            {
                for (pos, c) in calls.iter().enumerate() {
                    observe(&ToolCallDelta {
                        index: c["index"].as_u64().map(|i| i as usize).unwrap_or(pos),
                        id: c["id"].as_str().map(str::to_string),
                        name: c["function"]["name"].as_str().map(str::to_string),
                        arguments: c["function"]["arguments"].as_str().map(str::to_string),
                    });
                }
            }
            tool_call_delta = self.apply_choice(choice, &mut emit);
        }
        if v.get("usage").is_some_and(|u| u.is_object()) {
            let usage = parse_usage(&v["usage"]);
            self.usage = usage;
            self.usage_reported = true;
            self.content_items += 1;
            emit(StreamEvent::Usage(usage));
        }
        emit(StreamEvent::UpdateEnd { tool_call_delta });
        Ok(if stop {
            StreamControl::Stop
        } else {
            StreamControl::Continue
        })
    }

    /// One choice's delta; true when it carried a tool-call fragment.
    fn apply_choice(&mut self, choice: &Value, emit: &mut dyn FnMut(StreamEvent)) -> bool {
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        let reasoning = delta["reasoning_content"]
            .as_str()
            .or(delta["reasoning"].as_str());
        if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
            self.reasoning.push_str(r);
            self.content_items += 1;
            emit(StreamEvent::Reasoning(r.to_string()));
        }
        if delta["content"].as_str() == Some("") && self.text.is_empty() {
            self.leading_empty_part = true;
        }
        // Every content string is a text item in the C# client, an empty one too.
        if let Some(t) = delta["content"].as_str() {
            self.text.push_str(t);
            self.content_items += 1;
            self.text_parts += 1;
            emit(StreamEvent::Text(t.to_string()));
        }
        let mut tool_call_delta = false;
        if let Some(calls) = delta["tool_calls"].as_array() {
            tool_call_delta = !calls.is_empty();
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
                        emit(StreamEvent::ToolCallStarted {
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
        tool_call_delta
    }

    /// The assembled response. Tool calls only when the stream `completed` (the C# client
    /// surfaces them once it has seen the end of the stream).
    fn finish(self, completed: bool) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .filter(|(_, c)| completed && !c.name.is_empty())
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
            content_items: self.content_items + tool_calls.len(),
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            usage_reported: self.usage_reported,
            timings: self.timings,
            model: self.model,
            served_model: None,
            leading_empty_part: self.leading_empty_part,
            empty_first_part: false,
            text_parts: self.text_parts,
            updates: self.updates,
            stopped: false,
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
        let mut sink = |e: StreamEvent| {
            events.push(e);
            StreamControl::Continue
        };
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
        let r = acc.finish(true);
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
