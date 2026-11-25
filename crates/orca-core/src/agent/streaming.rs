//! One streamed model response (`StreamingResponseProcessor.ProcessStreamAsync`): thinking
//! accounting (the API reasoning channel and `<think>` tags in the text), the thinking budget,
//! the runaway-thinking guard and the per-step reasoning cap (both stop the stream), the
//! native-tools retry of an empty stream, and partial output kept when the stream dies after
//! text arrived.

use super::session::{AgentObserver, AgentSession};
use super::think_tag_filter::ThinkTagFilter;
use crate::chat::ReasoningCap;
use crate::client::{ChatError, ChatOptions, ChatResponse, StreamControl, StreamEvent, Usage};

/// `StreamingResult`.
#[derive(Debug, Default)]
pub(super) struct StreamOutcome {
    /// What streamed (on a stream error: the partial output, without tool calls).
    pub response: ChatResponse,
    /// Text content items (the chunk-count fallback for output tokens).
    pub token_count: i64,
    /// Reasoning chunks: API reasoning pieces plus text chunks inside think tags.
    pub thinking_token_count: i64,
    /// `thinking.budgetTokens` was passed while thinking.
    pub budget_exceeded: bool,
    /// No content at all.
    pub empty: bool,
    /// The empty first stream was retried without tools: switch to text tool calling.
    pub native_tools_downgraded: bool,
    /// The runaway guard stopped a reply that was all reasoning.
    pub thinking_runaway: bool,
    /// The per-step reasoning cap stopped the request.
    pub reasoning_capped: bool,
    /// The stream died after text arrived; the partial output is in `response`.
    pub stream_error: Option<ChatError>,
    /// The usage to account (none after a native-tools retry, as in C#).
    pub usage: Usage,
}

/// Per-stream counters shared by the event callback.
#[derive(Default)]
struct Counters {
    token_count: i64,
    text_parts: usize,
    thinking: i64,
    budget_exceeded: bool,
    runaway: bool,
    capped: bool,
    /// The current update's reasoning and answer text (for the reasoning cap).
    update_reasoning: String,
    update_text: String,
}

impl AgentSession {
    /// Stream one response with `options`. `cap` is the turn's reasoning cap (its
    /// `begin_request` already called); `runaway_cap` the runaway guard's chunk limit (0 = off).
    pub(super) async fn process_stream(
        &mut self,
        options: &ChatOptions,
        observer: &mut dyn AgentObserver,
        mut cap: Option<&mut ReasoningCap>,
        runaway_cap: i32,
    ) -> Result<StreamOutcome, ChatError> {
        let budget = i64::from(self.config.thinking.budget_tokens);
        let runaway_cap = i64::from(runaway_cap);
        let mut think = ThinkTagFilter::new();
        let mut c = Counters::default();
        let result = {
            let mut on_event = |e: StreamEvent| -> StreamControl {
                match e {
                    StreamEvent::Text(t) => {
                        c.token_count += 1;
                        c.text_parts += 1;
                        let (think_text, _) = think.process(&t);
                        if !think_text.is_empty() {
                            c.thinking += 1;
                            if budget > 0 && c.thinking > budget && !think.in_response_phase() {
                                c.budget_exceeded = true;
                            }
                            // (A native tool call only surfaces at the end of the stream, so
                            // "no function call yet" always holds while it runs.)
                            if runaway_cap > 0
                                && c.thinking >= runaway_cap
                                && !think.in_response_phase()
                            {
                                c.runaway = true;
                            }
                        }
                        c.update_text.push_str(&t);
                        if !t.is_empty() {
                            observer.on_text(&t);
                        }
                    }
                    StreamEvent::Reasoning(r) => {
                        c.thinking += 1;
                        if budget > 0 && c.thinking > budget {
                            c.budget_exceeded = true;
                        }
                        think.append_external_thinking(&r);
                        observer.on_reasoning(&r);
                        if runaway_cap > 0 && c.thinking >= runaway_cap && c.text_parts == 0 {
                            c.runaway = true;
                        }
                        c.update_reasoning.push_str(&r);
                    }
                    StreamEvent::ToolCallStarted { .. } | StreamEvent::Usage(_) => {}
                    StreamEvent::UpdateEnd { tool_call_delta } => {
                        let reasoning = std::mem::take(&mut c.update_reasoning);
                        let text = std::mem::take(&mut c.update_text);
                        if let Some(cap) = cap.as_deref_mut()
                            && cap.observe_update(
                                (!reasoning.is_empty()).then_some(reasoning.as_str()),
                                Some(text.as_str()),
                                tool_call_delta,
                            )
                        {
                            c.capped = true;
                            return StreamControl::Stop;
                        }
                        if c.runaway {
                            return StreamControl::Stop;
                        }
                    }
                }
                StreamControl::Continue
            };
            // Read-only calls may start while the rest of the response is still streaming.
            let mut on_tool_delta = self
                .early_runs
                .as_mut()
                .map(|early| move |d: &crate::client::ToolCallDelta| early.observe(d));
            self.client
                .stream_observed(
                    &self.actor,
                    &self.messages,
                    options,
                    &mut on_event,
                    on_tool_delta
                        .as_mut()
                        .map(|f| f as &mut (dyn FnMut(&crate::client::ToolCallDelta) + Send)),
                )
                .await
        };

        let mut stream_error = None;
        let response = match result {
            Ok(r) => r,
            // The stream died mid-response but text already arrived: keep it and let the
            // loop decide whether it holds a complete tool call.
            Err(ChatError::Interrupted { message, partial }) if partial.text_parts > 0 => {
                tracing::warn!(
                    "stream died mid-response after {} text parts; keeping the partial output",
                    partial.text_parts
                );
                stream_error = Some(ChatError::Interrupted {
                    message,
                    partial: Box::default(),
                });
                *partial
            }
            Err(e) => {
                self.last_chunk_count = c.token_count;
                return Err(e);
            }
        };

        let mut out = StreamOutcome {
            token_count: c.token_count,
            thinking_token_count: c.thinking,
            budget_exceeded: c.budget_exceeded,
            usage: response.usage,
            ..Default::default()
        };
        if c.capped {
            out.reasoning_capped = true;
            out.response = response;
            return Ok(out);
        }
        if c.runaway {
            tracing::warn!(
                "thinking runaway: {} reasoning chunks with no text or tool call; generation cancelled",
                c.thinking
            );
            out.thinking_runaway = true;
            out.response = response;
            return Ok(out);
        }

        // Native tools retry: the stream had updates but no content (the server could not
        // stream a call). Same request without tools; the session switches to text calling
        // after it (the retry still carries the system prompt it was built with).
        if stream_error.is_none()
            && response.content_items == 0
            && response.updates > 0
            && self.native_tools
        {
            tracing::debug!(
                "stream had {} updates but no content with native tools; retrying without tools",
                response.updates
            );
            let mut retry_options = options.clone();
            retry_options.tools.clear();
            let mut retry_tokens = 0i64;
            let retry = {
                let mut on_event = |e: StreamEvent| {
                    // The C# retry loop only looks at text.
                    if let StreamEvent::Text(t) = e {
                        retry_tokens += 1;
                        if !t.is_empty() {
                            observer.on_text(&t);
                        }
                    }
                };
                self.client
                    .stream(&self.actor, &self.messages, &retry_options, &mut on_event)
                    .await
            };
            self.native_tools = false;
            self.remember_native_tools_support(false);
            let retry = retry?;
            out.native_tools_downgraded = true;
            out.token_count += retry_tokens;
            out.empty = retry.content_items == 0;
            // C# keeps the first stream's (empty) usage: the retry's is not counted.
            out.usage = Usage::default();
            out.response = retry;
            self.last_chunk_count = out.token_count;
            return Ok(out);
        }

        out.empty = stream_error.is_none() && response.content_items == 0;
        out.stream_error = stream_error;
        out.response = response;
        self.last_chunk_count = out.token_count;
        Ok(out)
    }
}
