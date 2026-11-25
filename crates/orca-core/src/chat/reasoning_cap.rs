//! The per-step reasoning cap of one actor (`ReasoningCap.cs`, `agent.reasoning.maxStepTokens`,
//! 0 = off): when a request's reasoning passes the limit before any answer text or tool call
//! has started, the caller stops that request and asks again ([`ReasoningCap::reask`]), so the
//! model gets on with acting. It never fires once the answer or a tool call has begun. After
//! `agent.reasoning.capRetries` fires on one step the next request of that step runs uncapped.
//!
//! The re-ask only appends to the conversation, so the server's prompt-prefix cache holds.
//! Where the actor's requests send reasoning back (`slots.preserveThinking`), the cut reasoning
//! stays as a partial assistant turn followed by a short user message. Elsewhere the note
//! carries the end of the reasoning instead and, after a tool round, rides on the last tool
//! result. Only the API reasoning channel is watched, not `<think>` tags in the text.
//!
//! This is the pure part; stopping the stream when [`ReasoningCap::observe`] fires is the
//! agent loop's job.

use super::{cpt_or_default, len16, n0, tail16};
use crate::client::{ChatMessage, Role, StreamEvent};
use crate::config::OrcaConfig;

/// What the model is told after its thinking was cut.
pub const NUDGE: &str =
    "[Thinking limit] You have thought enough. Act now: make the tool call or give the answer.";

/// The visible text of the partial assistant turn that keeps the cut reasoning. Never empty:
/// a server template may leave out an assistant turn with neither text nor a tool call.
pub const CUT_MARKER: &str = "(thinking stopped at the limit)";

/// Starts the end of the cut reasoning when it is carried in the note.
pub const TAIL_INTRO: &str = "Your thinking so far ended with:";

/// Most characters of the cut reasoning carried in the note.
pub const TAIL_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct ReasoningCap {
    max_step_tokens: i32,
    retries: i32,
    chars_per_token: f32,
    keeps_reasoning: bool,
    reasoning: String,
    reasoning_len: usize,
    chunks: i32,
    active: bool,
    answer_started: bool,
    fires_this_step: i32,
    fired: bool,
    events: i32,
}

impl ReasoningCap {
    pub fn new(
        max_step_tokens: i32,
        retries: i32,
        chars_per_token: f32,
        keeps_reasoning: bool,
    ) -> Self {
        Self {
            max_step_tokens: max_step_tokens.max(1),
            retries: retries.max(1),
            chars_per_token: cpt_or_default(chars_per_token),
            keeps_reasoning,
            reasoning: String::new(),
            reasoning_len: 0,
            chunks: 0,
            active: false,
            answer_started: false,
            fires_this_step: 0,
            fired: false,
            events: 0,
        }
    }

    /// The cap for this role, or None when `agent.reasoning.maxStepTokens` is off. The Lead
    /// and the sub-agents share the setting.
    pub fn for_config(config: &OrcaConfig, lead: bool) -> Option<Self> {
        let reasoning = &config.agent.reasoning;
        if reasoning.max_step_tokens <= 0 {
            return None;
        }
        let keeps = config.slots.as_ref().is_some_and(|s| {
            s.enabled
                && if lead {
                    s.lead_sends_reasoning_back()
                } else {
                    s.sends_reasoning_back()
                }
        });
        Some(Self::new(
            reasoning.max_step_tokens,
            reasoning.cap_retries,
            config.context.chars_per_token,
            keeps,
        ))
    }

    /// Reasoning tokens one request may spend before it has started to answer.
    pub fn max_step_tokens(&self) -> i32 {
        self.max_step_tokens
    }

    /// Times the cap may fire on one step; the request after that runs uncapped.
    pub fn retries(&self) -> i32 {
        self.retries
    }

    /// Whether the cut reasoning stays in the conversation as the partial assistant turn.
    pub fn keeps_reasoning(&self) -> bool {
        self.keeps_reasoning
    }

    /// The cap stopped the current request.
    pub fn fired(&self) -> bool {
        self.fired
    }

    /// Times the cap has fired on the current step (1 = first).
    pub fn fires_this_step(&self) -> i32 {
        self.fires_this_step
    }

    /// Times the cap has fired for this actor.
    pub fn events(&self) -> i32 {
        self.events
    }

    /// Reasoning tokens of the current request so far (at the cut, once fired).
    pub fn reasoning_tokens(&self) -> i32 {
        Self::estimate_tokens(self.chunks, self.reasoning_len as i32, self.chars_per_token)
    }

    /// The current request's reasoning so far.
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// Reasoning tokens from what streamed: one token per chunk (llama.cpp, Strata), unless
    /// chunks carry over two tokens' worth of characters on average, then from characters.
    pub fn estimate_tokens(chunks: i32, chars: i32, chars_per_token: f32) -> i32 {
        let cpt = cpt_or_default(chars_per_token);
        if chunks <= 0 || chars <= 0 {
            return 0;
        }
        if chars as f32 > chunks as f32 * 2.0 * cpt {
            (f64::from(chars as f32 / cpt)).ceil() as i32
        } else {
            chunks
        }
    }

    /// Call before each request. A request that follows one the cap did not stop starts a new
    /// step; one that follows a fire is the same step asked again, capped only while fires
    /// are left.
    pub fn begin_request(&mut self) {
        if !self.fired {
            self.fires_this_step = 0;
        }
        self.fired = false;
        self.active = self.fires_this_step < self.retries;
        self.answer_started = false;
        self.chunks = 0;
        self.reasoning.clear();
        self.reasoning_len = 0;
    }

    /// Feed one streamed event. True when the cap fires on it: stop the request and call
    /// [`Self::reask`].
    pub fn observe(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::Text(t) => self.observe_update(None, Some(t), false),
            StreamEvent::Reasoning(r) => self.observe_update(Some(r), None, false),
            StreamEvent::ToolCallStarted { .. } => self.observe_update(None, None, true),
            StreamEvent::Usage(_) => self.fired,
        }
    }

    /// Feed one streamed update with its parts. An update that carries answer text (non-empty)
    /// or a tool call never fires, nor does anything after it.
    pub fn observe_update(
        &mut self,
        reasoning: Option<&str>,
        text: Option<&str>,
        tool_call: bool,
    ) -> bool {
        if self.fired {
            return true;
        }
        if tool_call || text.is_some_and(|t| !t.is_empty()) {
            self.answer_started = true;
        }
        let Some(reasoning) = reasoning.filter(|r| !r.is_empty()) else {
            return false;
        };
        if self.answer_started {
            return false;
        }
        self.chunks += 1;
        self.reasoning.push_str(reasoning);
        self.reasoning_len += len16(reasoning);
        if !self.active || self.reasoning_tokens() < self.max_step_tokens {
            return false;
        }
        self.fired = true;
        self.fires_this_step += 1;
        self.events += 1;
        true
    }

    /// After a fire: append the re-ask to `messages`. Nothing already there is removed or
    /// reordered (without kept reasoning, the note may ride on the last tool result).
    pub fn reask(&self, messages: &mut Vec<ChatMessage>) {
        if self.keeps_reasoning {
            let mut partial = ChatMessage::assistant(CUT_MARKER);
            partial.reasoning = Some(self.reasoning.clone());
            messages.push(partial);
            messages.push(ChatMessage::user(NUDGE));
            return;
        }
        let note = Self::note_with_tail(&self.reasoning);
        match messages.last_mut() {
            Some(last) if last.role == Role::Tool => {
                let content = last.content.take().unwrap_or_default();
                last.content = Some(format!("{content}\n\n{note}"));
            }
            _ => messages.push(ChatMessage::user(note)),
        }
    }

    /// The nudge plus the last [`TAIL_CHARS`] characters of the cut reasoning.
    pub fn note_with_tail(reasoning: &str) -> String {
        let tail = reasoning.trim();
        if tail.is_empty() {
            return NUDGE.to_string();
        }
        let tail = if len16(tail) > TAIL_CHARS {
            format!("...{}", tail16(tail, TAIL_CHARS))
        } else {
            tail.to_string()
        };
        format!("{NUDGE}\n{TAIL_INTRO}\n{tail}")
    }

    /// The status line of a fire, e.g. "thinking capped at ~1,204 tokens (limit 1,200); asked
    /// to act".
    pub fn describe(&self) -> String {
        format!(
            "thinking capped at ~{} tokens (limit {}); asked to act",
            n0(i64::from(self.reasoning_tokens())),
            n0(i64::from(self.max_step_tokens))
        )
    }
}
