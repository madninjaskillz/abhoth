//! The turn loop (`AgentLoopRunner.RunAgentLoopAsync` with `ToolCallDispatcher`): streaming
//! requests, native and text tool calls, nudges, the doom-loop and retry-loop guards, the
//! failing-test gate, same-model retries and model failover, the verification / summary
//! wind-down and the iteration-limit handoff.
//!
//! Also the stream guards (`streaming.rs`: the thinking-runaway cancel and the per-step
//! reasoning cap stop a stream; partial output survives a stream that dies mid-response and
//! its complete text tool call runs), adaptive reasoning, the server error probe after an
//! empty reply, and the startup probes (`MaxTokensNegotiator`, the context-window detection).
//!
//! Not ported yet (see STATUS.md): pair voting, early read-only runs, the parameter
//! auto-correction and the non-streaming fallback of the loop's catch-all error handler.

use std::time::Duration;

use serde_json::Value;

use orca_abstractions::{ToolResult, ToolRiskLevel};

use super::long_task::LongTaskMonitor;
use super::loop_messages as msg;
use super::prompts::{self, MAX_SUMMARY_CONTINUATIONS, SUMMARY_CONTINUE_MESSAGE};
use super::run_evidence;
use super::session::{AgentError, AgentObserver, AgentSession, TurnEnd};
use super::tool_call_parser::{self, ParsedToolCall};
use crate::chat::ReasoningCap;
use crate::chat::adaptive_reasoning::AdaptiveReasoning;
use crate::client::{ChatError, ChatMessage, ChatResponse, Role, ToolCall};

/// What the model is told after the runaway guard cut a reply that was all reasoning.
pub const RUNAWAY_REDIRECT_MESSAGE: &str = "Your last response was cut off because it was entirely internal reasoning with no action. Do NOT deliberate further. Immediately output the tool call (or the final answer) for your next step.";

/// `ToolCallExecutor.DoomLoopThreshold`.
const DOOM_LOOP_THRESHOLD: usize = 3;

/// Per-turn state (`AgentLoopState`).
#[derive(Default)]
pub(super) struct LoopState {
    pub nudge_attempts: u32,
    pub tools_executed: bool,
    pub verification_requested: bool,
    pub verification_tools_used: bool,
    pub summary_requested: bool,
    pub summary_continuations: u32,
    pub doom_loop_strikes: u32,
    pub test_fail_retries: u32,
    pub same_model_retry_attempts: u32,
    pub total_retry_delay: Duration,
    pub budget_advisory_injected: bool,
    pub task_brief: Option<String>,
    pub iterations_since_reminder: i32,
    pub iteration_budget: i32,
    /// Why the budget was not extended (named in the handoff request).
    pub exhaust_reason: Option<String>,
    /// History length at the start of each iteration (progress windows).
    pub history_marks: Vec<usize>,
    pub stuck_strikes: i32,
    pub stuck_anchor: Option<usize>,
    pub stuck_fires_in_streak: i32,
    pub stuck_last_fire_count: Option<usize>,
    pub pre_verification_wrap_up: Option<String>,
    pub pre_verification_epoch: u64,
    /// Runaway-thinking cancels this turn.
    pub runaway_trips: i32,
    /// The cold (temperature 0.2) recovery attempt after the third runaway.
    pub runaway_recovery_active: bool,
    /// The runaway cap for this turn's next requests (halved per trip); None = configured.
    pub runaway_cap_override: Option<i32>,
}

impl LoopState {
    /// `ResetForModelSwitch`: the next model must not inherit the old one's strikes.
    fn reset_for_model_switch(&mut self) {
        self.nudge_attempts = 0;
        self.verification_requested = false;
        self.verification_tools_used = false;
        self.summary_requested = false;
        self.summary_continuations = 0;
        self.same_model_retry_attempts = 0;
    }
}

/// What to do after a failed request.
enum Recovery {
    Continue,
    Break,
}

impl AgentSession {
    /// Run one user turn to the end.
    pub async fn run_turn(
        &mut self,
        user_message: &str,
        observer: &mut dyn AgentObserver,
    ) -> Result<TurnEnd, AgentError> {
        self.messages.push(ChatMessage::user(user_message));
        self.begin_turn();
        // A new request: no Lead mode chosen yet, and the Lead's bar starts at 0%.
        if let Some(o) = &self.orchestrator {
            o.reset_lead_progress();
        }
        self.overflow_recovery_used = false;
        let mut s = LoopState {
            iteration_budget: self.config.agent.main_loop_max_iterations.max(1),
            task_brief: self.snapshot_task_brief(),
            ..Default::default()
        };
        // Per-step reasoning cap (agent.reasoning.maxStepTokens) and adaptive reasoning, fresh
        // every turn; None = off.
        let mut reasoning_cap = ReasoningCap::for_config(&self.config, true);
        let mut adaptive = AdaptiveReasoning::for_config(&self.config.agent.reasoning, true);
        // Set when the cap stops a request: the re-ask sends that request's thinking setting.
        let mut cap_reask_skips_thinking: Option<bool> = None;
        // A previous turn's failover must not pin the session on a backup model.
        self.model_index = 0;
        self.prepare_for_generation(observer).await;
        self.native_tools = self.resolve_native_tools();
        self.sync_system_prompt_tool_mode();

        let mut iteration: i32 = 0;
        let mut budget_exhausted = false;
        loop {
            // Soft limit reached: keep going only while the turn shows measurable progress.
            if iteration >= s.iteration_budget
                && !self.try_extend_budget(&mut s, iteration, observer)
            {
                budget_exhausted = true;
                break;
            }
            s.history_marks.push(self.tool_call_history.len());
            // Long tool-heavy turns overflow the window mid-loop.
            let compacted = self.maybe_compact().await;
            self.inject_task_reminder(&mut s, iteration, compacted);
            self.inject_budget_advisory(&mut s, iteration);
            let closed = self.close_dangling_tool_calls();
            if closed > 0 {
                tracing::warn!(
                    "conversation ended on {closed} tool call(s) without results; recorded them as not run"
                );
            }

            let context_mark = self.context_mark();
            // A reasoning-cap re-ask is the same step asked again: it keeps the thinking
            // setting of the request it replaces and is not a new adaptive decision.
            let skip_thinking = match cap_reask_skips_thinking.take() {
                Some(skip) => skip,
                None => adaptive
                    .as_mut()
                    .is_some_and(|a| a.should_skip(&self.messages)),
            };
            let options = self.build_options_for(s.runaway_recovery_active, skip_thinking);
            // The cap watches this request only (not the wind-down requests made elsewhere).
            if let Some(cap) = reasoning_cap.as_mut() {
                cap.begin_request();
            }
            let runaway_cap = s
                .runaway_cap_override
                .unwrap_or(self.config.thinking.runaway_cap_chunks);
            let outcome = match self
                .process_stream(&options, observer, reasoning_cap.as_mut(), runaway_cap)
                .await
            {
                Ok(o) => o,
                Err(ChatError::IdleTimeout(d)) => {
                    observer.on_notice(&format!(
                        "Streaming timeout: no tokens received for {}s",
                        d.as_secs()
                    ));
                    match self.retry_or_failover(&mut s, None, observer).await {
                        Recovery::Continue => {
                            iteration += 1;
                            continue;
                        }
                        Recovery::Break => break,
                    }
                }
                Err(e) => {
                    // The server rejected the prompt as too long: compact and retry once.
                    if self.recover_from_overflow(&e).await {
                        iteration += 1;
                        continue;
                    }
                    return Err(e.into());
                }
            };

            if outcome.reasoning_capped
                && let Some(cap) = reasoning_cap.as_mut()
            {
                // Ask again, appending only (the prompt cache holds).
                tracing::info!(
                    "reasoning cap: lead step {} cut at {} reasoning tokens (limit {}, fire {}/{})",
                    iteration + 1,
                    cap.reasoning_tokens(),
                    cap.max_step_tokens(),
                    cap.fires_this_step(),
                    cap.retries()
                );
                observer.on_notice(&format!("Lead {}.", cap.describe()));
                self.total_output_tokens += i64::from(cap.reasoning_tokens());
                cap.reask(&mut self.messages);
                cap_reask_skips_thinking = Some(skip_thinking);
                iteration += 1;
                continue;
            }

            if outcome.thinking_runaway {
                // Escalate: shrink the cap each trip; after the third, one cold recovery
                // attempt, and a spiral during recovery ends the turn.
                s.runaway_trips += 1;
                let base = self.config.thinking.runaway_cap_chunks;
                s.runaway_cap_override = Some(base.min(500).max(base >> (s.runaway_trips & 31)));
                observer.on_notice(&format!(
                    "Thinking runaway cancelled after {} reasoning chunks with no action (strike {}).",
                    crate::chat::n0(outcome.thinking_token_count),
                    s.runaway_trips
                ));
                if s.runaway_trips >= 3 && s.runaway_recovery_active {
                    observer.on_notice("Model keeps disappearing into reasoning spirals \u{2014} ending this turn. A fresh prompt (or a smaller step) usually breaks the pattern.");
                    break;
                }
                if s.runaway_trips >= 3 {
                    s.runaway_recovery_active = true;
                    observer.on_notice(
                        "Third strike \u{2014} one recovery attempt at low temperature.",
                    );
                }
                self.messages
                    .push(ChatMessage::user(RUNAWAY_REDIRECT_MESSAGE));
                iteration += 1;
                continue;
            }

            if outcome.empty {
                tracing::warn!("no tokens received from the LLM: empty response");
                let server_error = crate::client::max_tokens_negotiator::probe_for_server_error(
                    &self.probe_http,
                    &self.config,
                    &self.messages,
                    &options,
                )
                .await;
                if let Some(e) = &server_error {
                    observer.on_notice(&format!("LLM server error: {e}"));
                }
                let recovered = match server_error {
                    Some(e) => self.recover_from_overflow(&ChatError::Server(e)).await,
                    None => false,
                };
                if recovered {
                    iteration += 1;
                    continue;
                }
                observer.on_notice("LLM returned an empty response.");
                match self.retry_or_failover(&mut s, None, observer).await {
                    Recovery::Continue => {
                        iteration += 1;
                        continue;
                    }
                    Recovery::Break => break,
                }
            }

            let outcome_note = (
                outcome.budget_exceeded,
                outcome.thinking_token_count,
                self.config.thinking.budget_tokens,
            );
            let response = outcome.response;
            if let Some(err) = &outcome.stream_error {
                // Partial output survived a mid-stream death: run it if it holds a complete
                // tool call (the generation wasn't wasted); otherwise retry.
                let salvageable = !response.tool_calls.is_empty()
                    || !tool_call_parser::parse_tool_calls_from_text(&response.text).is_empty();
                if !salvageable {
                    observer.on_notice(&format!(
                        "Stream interrupted: {err} \u{2014} partial text had no complete tool call."
                    ));
                    match self.retry_or_failover(&mut s, Some(err), observer).await {
                        Recovery::Continue => {
                            iteration += 1;
                            continue;
                        }
                        Recovery::Break => break,
                    }
                }
                observer.on_notice(
                    "Stream interrupted mid-response \u{2014} executing the completed tool call from partial output.",
                );
            } else {
                // A healthy stream proves the connection is good again.
                s.same_model_retry_attempts = 0;
            }

            // The server's real prompt size becomes the base for the next estimate.
            self.record_prompt_usage(context_mark, &outcome.usage);

            self.last_assistant_response = Some(prompts::strip_next_prompt_tag(&response.text));

            // Quality-based failover (refusals, too-short replies) when backups exist.
            if self.model_index + 1 < self.config.lm_studio.models.len() {
                let verdict = super::response_quality::check(&self.config, &response.text);
                if verdict.should_switch_to_backup {
                    observer.on_notice(&format!(
                        "{} \u{2014} retrying with next model...",
                        verdict.reason.as_deref().unwrap_or("Low-quality response")
                    ));
                    if self.switch_model(&mut s, observer).await {
                        iteration += 1;
                        continue;
                    }
                }
            }

            // Prefer the server's real output count over stream-chunk counting.
            self.total_output_tokens += outcome
                .usage
                .completion_tokens
                .unwrap_or(outcome.token_count);

            if !response.tool_calls.is_empty() {
                // A native call without arguments for a tool that needs some: the server lost
                // the payload. Drop the response and fall back to text tool calling.
                if self.native_tools && self.has_missing_required_args(&response.tool_calls) {
                    tracing::warn!(
                        "native tool call(s) have empty args; downgrading to text-based calling"
                    );
                    self.native_tools = false;
                    self.remember_native_tools_support(false);
                    iteration += 1;
                    continue;
                }
                let assistant = response.to_message();
                if let Some(stop) = self.handle_doom_loop(
                    &mut s,
                    &response.tool_calls,
                    assistant.clone(),
                    true,
                    observer,
                ) {
                    if stop {
                        break;
                    }
                    iteration += 1;
                    continue;
                }
                self.messages.push(assistant);
                self.inject_thinking_budget_note(&outcome_note);
                let history_before = self.tool_call_history.len();
                self.execute_native_calls(&response.tool_calls, observer)
                    .await;
                self.note_round_for_reasoning(
                    adaptive.as_mut(),
                    history_before,
                    response.tool_calls.len(),
                );
                s.tools_executed = true;
                if s.verification_requested {
                    s.verification_tools_used = true;
                }
                self.check_stuck_probing(&mut s, observer);
            } else if let parsed @ [_, ..] =
                tool_call_parser::parse_tool_calls_from_text(&response.text).as_slice()
            {
                let calls: Vec<ToolCall> = parsed.iter().map(parsed_to_call).collect();
                let assistant = response.to_message();
                if let Some(stop) =
                    self.handle_doom_loop(&mut s, &calls, assistant.clone(), false, observer)
                {
                    if stop {
                        break;
                    }
                    iteration += 1;
                    continue;
                }
                self.messages.push(assistant);
                self.inject_thinking_budget_note(&outcome_note);
                self.execute_text_calls(&calls, observer).await;
                s.tools_executed = true;
                if s.verification_requested {
                    s.verification_tools_used = true;
                }
                self.check_stuck_probing(&mut s, observer);
            } else {
                self.messages.push(response.to_message());

                // Helpers started in the background and never collected: wait for them and
                // hand their reports back, so the turn (or a -p run) can't end under them.
                if let Some(collect) = self.collect_background.clone()
                    && let Some(background) = collect().await.filter(|b| !b.is_empty())
                {
                    observer.on_notice("Background helpers finished; handing their reports over.");
                    self.messages.push(ChatMessage::user(background));
                    iteration += 1;
                    continue;
                }

                // Refuse to wind down over a failing test run (at most twice per turn).
                if !s.summary_requested
                    && s.test_fail_retries < 2
                    && let Some(tail) = self.recent_test_failure()
                {
                    s.test_fail_retries += 1;
                    observer.on_notice(&format!(
                        "Tests are failing \u{2014} sending the model back to fix them (retry {}/2).",
                        s.test_fail_retries
                    ));
                    self.messages
                        .push(ChatMessage::user(msg::test_failure_message(&tail)));
                    iteration += 1;
                    continue;
                }

                if !self.handle_no_tool_call_response(&response, &mut s, observer) {
                    break;
                }
            }

            if self.check_retry_loop(observer) {
                break;
            }
            iteration += 1;
        }

        if budget_exhausted {
            self.handle_iteration_exhaustion(&s, observer).await;
            return Ok(TurnEnd::IterationLimit);
        }
        Ok(TurnEnd::Done)
    }

    /// `ToolCallExecutor.BeginTurn`: evidence scans and loop guards start fresh.
    fn begin_turn(&mut self) {
        self.turn_history_start = self.tool_call_history.len();
        self.final_prose = None;
        self.recent_tool_errors.clear();
        self.recent_call_history.clear();
    }

    /// `SnapshotTaskBrief`: the latest user message, at most 500 characters.
    fn snapshot_task_brief(&self) -> Option<String> {
        let text = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)?
            .content
            .as_deref()?
            .trim();
        if text.is_empty() {
            return None;
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        Some(if units.len() > 500 {
            format!("{}\u{2026}", String::from_utf16_lossy(&units[..500]))
        } else {
            text.to_string()
        })
    }

    /// `ResolveNativeToolsForActiveModel`: native iff config wants it, there are tools, and
    /// this model hasn't already proven it can't do native calls (sticky).
    pub fn resolve_native_tools(&self) -> bool {
        if !self.config.lm_studio.native_tool_calling || self.registry.all().is_empty() {
            return false;
        }
        match self.active_model() {
            Some(m) => self.native_tools_support.get(&m).copied().unwrap_or(true),
            None => true,
        }
    }

    /// Remember (sticky) whether the active model can do native calls, and keep the system
    /// prompt's tool protocol in step with the mode now in use.
    pub(super) fn remember_native_tools_support(&mut self, supported: bool) {
        if let Some(m) = self.active_model() {
            self.prompt_builder
                .remember_native_tools_support(&m, supported);
            self.native_tools_support.insert(m, supported);
        }
        self.sync_system_prompt_tool_mode();
    }

    /// The task brief, recited on a cadence once the context is large (never during the
    /// verification / summary wind-down).
    fn inject_task_reminder(&mut self, s: &mut LoopState, iteration: i32, compacted: bool) {
        s.iterations_since_reminder += 1;
        let interval = self.config.agent.task_reminder_interval;
        let Some(brief) = s.task_brief.clone() else {
            return;
        };
        if interval <= 0 || !s.tools_executed || s.verification_requested || s.summary_requested {
            return;
        }
        // Right after compaction, or on the cadence once the context is large.
        if !compacted
            && (s.iterations_since_reminder < interval
                || self.estimate_context_tokens()
                    < self.config.agent.task_reminder_min_context_tokens)
        {
            return;
        }
        s.iterations_since_reminder = 0;
        let files = if self.files_modified.is_empty() {
            "(none yet)".to_string()
        } else {
            self.files_modified
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let note = msg::task_reminder(&brief, &files, iteration + 1, s.iteration_budget);
        self.add_harness_note(&note);
    }

    /// "Wrap it up" near the end of the budget, deferred while the turn is still measurably
    /// progressing and the budget can still be extended.
    fn inject_budget_advisory(&mut self, s: &mut LoopState, iteration: i32) {
        if s.budget_advisory_injected
            || !s.tools_executed
            || iteration < (s.iteration_budget as f64 * 0.8) as i32
        {
            return;
        }
        let hard_cap = s
            .iteration_budget
            .max(self.config.agent.max_iterations_hard_cap);
        let extendable = s.iteration_budget < hard_cap;
        if extendable {
            let start = self.progress_window_start(s, iteration);
            if self
                .long_task_monitor()
                .assess_progress(&self.tool_call_history, start)
                .progress
            {
                return;
            }
        }
        s.budget_advisory_injected = true;
        let note = msg::budget_advisory(s.iteration_budget - iteration, extendable);
        self.add_harness_note(&note);
    }

    /// `AddHarnessNote`: after a tool round the note rides on the last tool result (keeps the
    /// server's prefix cache); otherwise it is a user message.
    pub fn add_harness_note(&mut self, note: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == Role::Tool
        {
            let content = last.content.get_or_insert_with(String::new);
            content.push_str("\n\n");
            content.push_str(note);
            return;
        }
        self.messages.push(ChatMessage::user(note));
    }

    /// `CloseDanglingToolCalls`: the conversation must not end on an assistant message with
    /// tool calls (llama.cpp rejects the prefill). Returns how many calls were closed.
    pub fn close_dangling_tool_calls(&mut self) -> usize {
        let Some(last) = self.messages.last() else {
            return 0;
        };
        if last.role != Role::Assistant {
            return 0;
        }
        let ids: Vec<String> = last.tool_calls.iter().map(|c| c.id.clone()).collect();
        for id in &ids {
            self.messages.push(ChatMessage::tool_result(
                id.clone(),
                msg::DANGLING_CALL_RESULT,
            ));
        }
        ids.len()
    }

    /// `TryRetrySameModelAsync`, then `TrySwitchModelAsync`: two delayed retries (at most 60 s
    /// of waiting per turn), then the next model in `lmStudio.models`.
    async fn retry_or_failover(
        &mut self,
        s: &mut LoopState,
        err: Option<&ChatError>,
        observer: &mut dyn AgentObserver,
    ) -> Recovery {
        if s.same_model_retry_attempts < 2
            && s.total_retry_delay < Duration::from_secs(60)
            && err.is_none_or(is_retryable_error)
        {
            let delay = self.retry_base_delay * 2u32.pow(s.same_model_retry_attempts);
            let delay = delay.min(Duration::from_secs(30));
            s.same_model_retry_attempts += 1;
            s.total_retry_delay += delay;
            observer.on_notice(&format!(
                "Transient failure \u{2014} retrying same model in {:.0}s (attempt {}/2)...",
                delay.as_secs_f64(),
                s.same_model_retry_attempts
            ));
            tokio::time::sleep(delay).await;
            return Recovery::Continue;
        }
        if self.switch_model(s, observer).await {
            Recovery::Continue
        } else {
            Recovery::Break
        }
    }

    /// `TrySwitchModelAsync`: the next model in `lmStudio.models`, with per-turn state reset
    /// and a short breather.
    async fn switch_model(&mut self, s: &mut LoopState, observer: &mut dyn AgentObserver) -> bool {
        let models = &self.config.lm_studio.models;
        let next = self.model_index + 1;
        if next >= models.len() {
            return false;
        }
        self.model_index = next;
        observer.on_notice(&format!(
            "Model failed \u{2014} retrying with: {} (priority {}/{})",
            models[next],
            next + 1,
            models.len()
        ));
        s.reset_for_model_switch();
        self.recent_tool_errors.clear();
        self.recent_call_history.clear();
        self.native_tools = self.resolve_native_tools();
        self.sync_system_prompt_tool_mode();
        tokio::time::sleep(self.failover_delay).await;
        true
    }

    /// `ToolCallExecutor.DetectDoomLoop`: the same call (tool + arguments) three times in a
    /// row for read-only tools, or three times within the last nine calls for the others,
    /// with no file change in between. Records every call.
    fn detect_doom_loop(&mut self, calls: &[ToolCall]) -> Option<String> {
        for call in calls {
            let args = normalized_args(&call.arguments);
            let epoch = self.mutation_epoch;
            self.recent_call_history
                .push((call.name.clone(), args.clone(), epoch));
            let read_only = self
                .registry
                .resolve(&call.name)
                .is_some_and(|t| t.risk_level() == ToolRiskLevel::ReadOnly);
            let same = |t: &(String, String, u64)| t.0 == call.name && t.1 == args && t.2 == epoch;
            let h = &self.recent_call_history;
            let doomed = if read_only {
                h.len() >= DOOM_LOOP_THRESHOLD
                    && h[h.len() - DOOM_LOOP_THRESHOLD..].iter().all(same)
            } else {
                let window = DOOM_LOOP_THRESHOLD * 3;
                let from = h.len().saturating_sub(window);
                h[from..].iter().filter(|t| same(t)).count() >= DOOM_LOOP_THRESHOLD
            };
            if doomed {
                tracing::warn!(
                    "doom loop detected: {} called {DOOM_LOOP_THRESHOLD}+ times with identical arguments",
                    call.name
                );
                return Some(call.name.clone());
            }
        }
        None
    }

    /// `HandleDoomLoop` / `CheckDoomLoop`: on detection the calls are not run; the round is
    /// committed (calls, "not run" results, redirect). `Some(true)` ends the turn (second
    /// detection), `Some(false)` continues, `None` means no doom loop.
    fn handle_doom_loop(
        &mut self,
        s: &mut LoopState,
        calls: &[ToolCall],
        assistant: ChatMessage,
        native: bool,
        observer: &mut dyn AgentObserver,
    ) -> Option<bool> {
        let tool = self.detect_doom_loop(calls)?;
        observer.on_notice(&format!(
            "Doom loop detected: {tool} called repeatedly with identical args \u{2014} redirecting."
        ));
        let redirect = msg::doom_loop_redirect_message(&tool);
        self.messages.push(assistant);
        if native {
            for c in calls {
                self.messages.push(ChatMessage::tool_result(
                    c.id.clone(),
                    msg::DOOM_LOOP_SKIPPED_RESULT,
                ));
            }
            self.add_harness_note(&redirect);
        } else {
            let skipped: Vec<String> = calls
                .iter()
                .map(|c| {
                    format!(
                        "[Tool result for {}]\n{}",
                        c.name,
                        msg::DOOM_LOOP_SKIPPED_RESULT
                    )
                })
                .collect();
            self.messages.push(ChatMessage::user(format!(
                "{}\n\n{redirect}",
                skipped.join("\n\n")
            )));
        }
        s.doom_loop_strikes += 1;
        if s.doom_loop_strikes >= 2 {
            observer.on_notice("Doom loop persisted after redirect \u{2014} stopping this turn.");
            return Some(true);
        }
        Some(false)
    }

    /// Run native calls in order; each result becomes a tool message.
    async fn execute_native_calls(&mut self, calls: &[ToolCall], observer: &mut dyn AgentObserver) {
        let several = calls.len() > 1;
        for (i, call) in calls.iter().enumerate() {
            self.record_raw_arguments = several && i == 0;
            let mut result = self.run_one_call(call, true, observer).await;
            if self.config.agent.repeat_call_notes
                && let Some(note) =
                    crate::chat::repeat_call_note::note_for(&self.messages, call, &result)
            {
                result.push_str(&note);
            }
            self.messages
                .push(ChatMessage::tool_result(call.id.clone(), result));
        }
        self.record_raw_arguments = false;
    }

    /// Text-mode calls: all results go back as one user message.
    async fn execute_text_calls(&mut self, calls: &[ToolCall], observer: &mut dyn AgentObserver) {
        let mut parts = Vec::new();
        for call in calls {
            let result = self.run_one_call(call, false, observer).await;
            parts.push((call.name.clone(), result));
        }
        self.messages
            .push(ChatMessage::user(msg::text_tool_results_message(&parts)));
    }

    /// Policies, then the tool pipeline and retry detection. Returns the model-visible result.
    async fn run_one_call(
        &mut self,
        call: &ToolCall,
        native: bool,
        observer: &mut dyn AgentObserver,
    ) -> String {
        let types = self
            .orchestrator
            .as_ref()
            .map(|o| o.type_registry().clone());
        let mut block = super::policies::check_blocked(
            &self.config,
            &self.registry,
            self.plan_mode,
            native,
            &call.name,
            &call.arguments,
            types.as_ref(),
        );
        // agent.leadMode "auto": the per-request choice (only with the lead_mode tool).
        if block.is_none()
            && let Some(orchestrator) = &self.orchestrator
        {
            let advertised = self
                .registry
                .resolve(super::policies::LEAD_MODE_TOOL_NAME)
                .is_some();
            match super::policies::check_auto_lead(
                &self.config,
                orchestrator.lead_request_mode().as_deref(),
                advertised,
                &call.name,
            ) {
                super::policies::AutoLeadVerdict::Run => {}
                super::policies::AutoLeadVerdict::ChooseHandsOn(reason) => {
                    tracing::info!(
                        "Lead mode for this request: {} \u{2014} {reason}",
                        super::policies::LEAD_MODE_HANDS_ON
                    );
                    orchestrator.set_lead_request_mode(Some(super::policies::LEAD_MODE_HANDS_ON));
                }
                super::policies::AutoLeadVerdict::Blocked(b) => block = Some(b),
            }
        }
        if let Some(block) = block {
            tracing::info!("blocked tool {}: {}", call.name, block.message);
            observer.on_notice(&block.message);
            return block.message;
        }
        let call = ToolCall {
            arguments: strip_reason(&call.arguments),
            ..call.clone()
        };
        observer.on_tool_start(&call);
        let result = self.execute_tool_call(&call, observer).await;
        observer.on_tool_end(&call, &result);
        self.apply_retry_detection(&call, &result)
    }

    /// `ApplyRetryDetection`: count identical failures; the 2nd gets a warning, the 3rd and
    /// later are replaced by a STOP notice (verbatim-output tools keep their text).
    fn apply_retry_detection(&mut self, call: &ToolCall, result: &ToolResult) -> String {
        let key = format!("{}::{}", call.name, normalized_args(&call.arguments));
        if !result.is_error {
            self.recent_tool_errors.remove(&key);
            return result.content.clone();
        }
        let epoch = self.mutation_epoch;
        let entry = self
            .recent_tool_errors
            .entry(key)
            .and_modify(|prev| {
                if prev.0 == result.content && prev.2 == epoch {
                    prev.1 += 1;
                } else {
                    *prev = (result.content.clone(), 1, epoch);
                }
            })
            .or_insert_with(|| (result.content.clone(), 1, epoch));
        let count = entry.1;
        let verbatim = self.registry.resolve(&call.name).is_some_and(|t| {
            t.capabilities()
                .contains(orca_abstractions::ToolCapabilities::VERBATIM_OUTPUT)
        });
        if verbatim {
            return result.content.clone();
        }
        if count >= 3 {
            return msg::retry_stop_message(count, &result.content);
        }
        if count >= 2 {
            return format!("{}{}", result.content, msg::RETRY_WARNING_SUFFIX);
        }
        result.content.clone()
    }

    /// `CheckRetryLoopDetection`: a redirect at 3 identical failures, the end of the turn at 4.
    fn check_retry_loop(&mut self, observer: &mut dyn AgentObserver) -> bool {
        let max = self
            .recent_tool_errors
            .values()
            .map(|v| v.1)
            .max()
            .unwrap_or(0);
        if max >= 4 {
            observer.on_notice(&format!(
                "Tool stuck in retry loop ({max} identical failures) \u{2014} stopping."
            ));
            return true;
        }
        if max >= 3 {
            self.messages
                .push(ChatMessage::user(msg::RETRY_LOOP_REDIRECT_MESSAGE));
        }
        false
    }

    /// `TryGetRecentTestFailure`: the latest test/build-style run this turn failed (and no
    /// later one passed): a head+tail excerpt of its output.
    fn recent_test_failure(&self) -> Option<String> {
        for r in self.turn_history().iter().rev() {
            if !is_command_runner(&r.name) {
                continue;
            }
            match run_evidence::classify(r.result.as_deref(), r.is_error, r.arguments.as_deref()) {
                run_evidence::RunOutcome::Pass => return None,
                run_evidence::RunOutcome::Fail => {
                    return Some(run_evidence::excerpt(
                        r.result.as_deref().unwrap_or(""),
                        run_evidence::EXCERPT_HEAD_CHARS,
                        run_evidence::EXCERPT_TAIL_CHARS,
                    ));
                }
                _ => {}
            }
        }
        None
    }

    /// `ToolCallDispatcher.HandleNoToolCallResponse`: nudges, verification and the summary
    /// wind-down. True when a follow-up message was added and the loop goes on.
    fn handle_no_tool_call_response(
        &mut self,
        response: &ChatResponse,
        s: &mut LoopState,
        observer: &mut dyn AgentObserver,
    ) -> bool {
        let text = &response.text;
        let lower = text.to_lowercase();
        let has_open = lower.contains("<tool_call>");
        let has_close = lower.contains("</tool_call>");
        if !s.summary_requested && has_open && !has_close && s.nudge_attempts < 2 {
            s.nudge_attempts += 1;
            observer.on_notice("Tool call was truncated \u{2014} asking model to continue...");
            self.messages
                .push(ChatMessage::user(prompts::platform_lines(
                    msg::TRUNCATED_TOOL_CALL_MESSAGE,
                )));
            return true;
        }

        if !s.summary_requested
            && s.nudge_attempts < 1
            && !text.is_empty()
            && tool_call_parser::should_nudge_for_tool_calls(text)
            && !(s.tools_executed && self.has_recent_test_pass_evidence())
        {
            s.nudge_attempts += 1;
            observer.on_notice("Nudging model to use tool calls...");
            self.messages
                .push(ChatMessage::user(prompts::platform_lines(
                    msg::nudge_message(self.native_tools),
                )));
            return true;
        }

        // Delegated work and no passing check since the agents finished: ask the Lead once to
        // check the result itself before it answers.
        if s.tools_executed
            && !s.verification_requested
            && !self.is_verification_off()
            && self.delegated_without_check()
        {
            s.verification_requested = true;
            // One request: whatever the Lead does next, its reply ends the turn.
            s.verification_tools_used = true;
            let wrap_up = prompts::extract_final_prose(text);
            s.pre_verification_wrap_up =
                prompts::is_substantive_final_message(&wrap_up).then_some(wrap_up);
            s.pre_verification_epoch = self.mutation_epoch;
            tracing::info!(
                "Delegated work with no check since the agents finished \u{2014} asking the Lead for the final check"
            );
            self.messages
                .push(ChatMessage::user(prompts::platform_lines(
                    super::prompt_constants::get_delegated_check_message(
                        self.config.tool_profile.eq_ignore_ascii_case("pi"),
                    ),
                )));
            return true;
        }

        if s.tools_executed && !s.verification_requested {
            s.verification_requested = true;
            if self.should_request_verification() {
                let wrap_up = prompts::extract_final_prose(text);
                s.pre_verification_wrap_up =
                    prompts::is_substantive_final_message(&wrap_up).then_some(wrap_up);
                s.pre_verification_epoch = self.mutation_epoch;
                self.messages
                    .push(ChatMessage::user(prompts::platform_lines(
                        prompts::VERIFICATION_MESSAGE,
                    )));
                return true;
            }
            s.verification_tools_used = true;
        }
        if s.tools_executed && s.verification_requested && !s.summary_requested {
            if !s.verification_tools_used {
                s.verification_tools_used = true;
                self.messages
                    .push(ChatMessage::user(prompts::platform_lines(
                        prompts::VERIFICATION_RETRY_MESSAGE,
                    )));
                return true;
            }
            let mut prose = prompts::extract_final_prose(text);
            if !prompts::is_substantive_final_message(&prose)
                && self.mutation_epoch == s.pre_verification_epoch
                && let Some(wrap_up) = s.pre_verification_wrap_up.take()
            {
                prose = wrap_up;
            }
            s.pre_verification_wrap_up = None;
            if prompts::is_substantive_final_message(&prose) {
                self.final_prose = Some(prose);
                return false;
            }
            s.summary_requested = true;
            self.messages
                .push(ChatMessage::user(prompts::summary_request_message()));
            return true;
        }
        if s.summary_requested
            && response.finish_reason.as_deref() == Some("length")
            && s.summary_continuations < MAX_SUMMARY_CONTINUATIONS
        {
            s.summary_continuations += 1;
            self.messages
                .push(ChatMessage::user(SUMMARY_CONTINUE_MESSAGE));
            return true;
        }
        false
    }

    /// `HandleIterationExhaustionAsync`: ask for a structured handoff (with the tools still
    /// advertised so the prompt cache holds), or leave the recorded facts if that fails.
    async fn handle_iteration_exhaustion(
        &mut self,
        s: &LoopState,
        observer: &mut dyn AgentObserver,
    ) {
        observer.on_notice(&format!(
            "Iteration limit ({}) reached{}. Requesting a progress summary.",
            s.iteration_budget,
            s.exhaust_reason
                .as_deref()
                .map(|r| format!(" \u{2014} {r}"))
                .unwrap_or_default()
        ));
        let facts = self
            .long_task_monitor()
            .build_handoff_facts(&self.tool_call_history, self.files_modified.iter());
        let reason = s
            .exhaust_reason
            .clone()
            .unwrap_or_else(|| format!("{} iterations", s.iteration_budget));
        self.messages
            .push(ChatMessage::user(msg::iteration_handoff_message(
                &reason, &facts,
            )));
        let options = self.build_options();
        let text = match self
            .client
            .complete(&self.actor, &self.messages, &options)
            .await
        {
            Ok(r) => prompts::extract_final_prose(r.text.trim()),
            Err(e) => {
                tracing::warn!("iteration-exhaustion summary request failed: {e}");
                String::new()
            }
        };
        let text = if text.is_empty() {
            format!("## Handoff (automatic \u{2014} the model returned no summary)\n{facts}")
        } else {
            text
        };
        self.messages.push(ChatMessage::assistant(text.clone()));
        self.last_assistant_response = Some(text);
    }

    /// `CheckStuckProbing`: the model keeps probing (no file change) while the latest test run
    /// keeps failing the same way. Steer once per `agent.stuckProbeThreshold` calls, at most
    /// twice per streak; it only steers (the budget ends the turn).
    fn check_stuck_probing(&mut self, s: &mut LoopState, observer: &mut dyn AgentObserver) {
        let threshold = self.config.agent.stuck_probe_threshold;
        if threshold <= 0 || s.summary_requested {
            return;
        }
        let Some(streak) = self
            .long_task_monitor()
            .current_stuck_streak(&self.tool_call_history)
        else {
            return;
        };
        if s.stuck_anchor != Some(streak.anchor_index) {
            s.stuck_anchor = Some(streak.anchor_index);
            s.stuck_fires_in_streak = 0;
            s.stuck_last_fire_count = None;
        }
        if s.stuck_fires_in_streak >= 2 {
            return;
        }
        let count = self.tool_call_history.len();
        let since = match s.stuck_last_fire_count {
            Some(last) => count.saturating_sub(last),
            None => streak.calls,
        };
        if (since as i64) < threshold as i64 {
            return;
        }
        s.stuck_strikes += 1;
        s.stuck_fires_in_streak += 1;
        s.stuck_last_fire_count = Some(count);
        let failure = streak.failure.describe();
        observer.on_notice(&format!(
            "Probing in circles ({} calls, no file change, same failing tests) \u{2014} steering toward a decisive fix (strike {}).",
            streak.calls, s.stuck_strikes
        ));
        let note = super::prompt_constants::get_stuck_probe_message(
            s.stuck_strikes.min(2),
            streak.calls as i32,
            &streak.failure.key,
            &failure,
            &streak.recent_probes,
            false,
        );
        self.add_harness_note(&note);
    }

    /// `ToolCallDispatcher.InjectBudgetAdvisory`: the reply's thinking passed
    /// `thinking.budgetTokens`.
    fn inject_thinking_budget_note(&mut self, note: &(bool, i64, i32)) {
        let (exceeded, used, budget) = *note;
        if exceeded {
            self.messages.push(ChatMessage::user(format!(
                "Note: Your thinking exceeded the budget of {budget} tokens ({used} used). Keep reasoning concise for simple tasks."
            )));
        }
    }

    /// `NoteRoundForReasoning`: record the round just run for adaptive reasoning. A round the
    /// history does not fully cover (a call refused before it ran) counts as not obvious.
    fn note_round_for_reasoning(
        &self,
        adaptive: Option<&mut AdaptiveReasoning>,
        history_before: usize,
        call_count: usize,
    ) {
        let Some(adaptive) = adaptive else {
            return;
        };
        let history = &self.tool_call_history;
        let calls: Vec<(String, bool)> = if history.len() - history_before == call_count {
            history[history_before..]
                .iter()
                .map(|r| (r.name.clone(), r.is_error))
                .collect()
        } else {
            Vec::new()
        };
        adaptive.note_round(&calls, &self.messages);
    }

    /// `PrepareForGenerationAsync`: once per session, negotiate `max_tokens` (when not
    /// configured) and detect the context window (LM Studio / Ollama report it; Strata, ninfer
    /// and vLLM-style servers only lower it). Skipped in demo mode, for Anthropic, and when
    /// [`AgentSession::startup_probes`] is off.
    async fn prepare_for_generation(&mut self, observer: &mut dyn AgentObserver) {
        if !self.startup_probes
            || self.max_tokens_negotiated
            || self.config.demo_mode
            || self.config.lm_studio.is_anthropic()
        {
            return;
        }
        self.max_tokens_negotiated = true;
        if let Some(n) =
            crate::client::max_tokens_negotiator::negotiate(&self.config, &self.probe_http).await
        {
            self.config.lm_studio.max_tokens = Some(n);
            observer.on_notice(&format!("Negotiated max output tokens: {n}"));
        }
        if !self.config.context.auto_detect_context_window {
            return;
        }
        let detected = crate::client::context_window_detector::detect(
            &self.config.lm_studio.base_url,
            self.config.lm_studio.primary_model(),
            &self.probe_http,
        )
        .await;
        // slots.lead.contextWindow wins: the probe asks the balancer, not the Lead's endpoint.
        let lead_window = self
            .config
            .slots
            .as_ref()
            .and_then(|s| s.compute_lead_context_window());
        if lead_window.is_none()
            && let Some(d) = detected.filter(|d| *d > 0)
            && d != self.config.context.context_window_size
        {
            self.config.context.context_window_size = d;
            observer.on_notice(&format!(
                "Detected context window: {} tokens",
                crate::chat::n0(i64::from(d))
            ));
        }
        // Strata (/health max_context) and vLLM-style servers (/v1/models max_model_len) say
        // their window too; it only lowers the configured one.
        if let Some(served) = crate::client::server_identity::smallest_context_window(
            &self.config,
            &self.server_identity,
        )
        .await
            && served < self.config.context.context_window_size
        {
            observer.on_notice(&format!(
                "Server context window is {} tokens (configured {}) \u{2014} using the server's.",
                crate::chat::n0(i64::from(served)),
                crate::chat::n0(i64::from(self.config.context.context_window_size))
            ));
            self.config.context.context_window_size = served;
        }
    }

    /// A long-task monitor over this turn's tool history.
    pub(super) fn long_task_monitor(&self) -> LongTaskMonitor<'_> {
        let mutating: std::collections::HashSet<String> = self
            .registry
            .all()
            .iter()
            .filter(|t| {
                t.capabilities()
                    .contains(orca_abstractions::ToolCapabilities::MODIFIES_FILES)
            })
            .map(|t| t.name().to_string())
            .collect();
        LongTaskMonitor::new(
            self.turn_history_start,
            move |r: &super::session::ToolCallRecord| !r.is_error && mutating.contains(&r.name),
            Some(self.ctx.cwd.as_path()),
        )
    }

    /// `ProgressWindowStart`: history index where the window ending at `iteration` begins.
    fn progress_window_start(&self, s: &LoopState, iteration: i32) -> usize {
        let first =
            (iteration.max(0) as usize).saturating_sub(LongTaskMonitor::EXTENSION_ITERATIONS);
        s.history_marks
            .get(first)
            .copied()
            .unwrap_or(self.tool_call_history.len())
    }

    /// `TryExtendBudget`: more iterations (up to `agent.maxIterationsHardCap`) while the last
    /// window shows measurable progress.
    fn try_extend_budget(
        &mut self,
        s: &mut LoopState,
        iteration: i32,
        observer: &mut dyn AgentObserver,
    ) -> bool {
        let hard_cap = s
            .iteration_budget
            .max(self.config.agent.max_iterations_hard_cap);
        if s.iteration_budget >= hard_cap {
            s.exhaust_reason = (hard_cap > self.config.agent.main_loop_max_iterations.max(1))
                .then(|| format!("hard cap agent.maxIterationsHardCap = {hard_cap}"));
            return false;
        }
        let start = self.progress_window_start(s, iteration);
        let verdict = self
            .long_task_monitor()
            .assess_progress(&self.tool_call_history, start);
        if !verdict.progress {
            s.exhaust_reason = Some(format!(
                "no measurable progress in the last {} iterations ({})",
                LongTaskMonitor::EXTENSION_ITERATIONS,
                verdict.reason
            ));
            return false;
        }
        let previous = s.iteration_budget;
        s.iteration_budget = hard_cap.min(previous + LongTaskMonitor::EXTENSION_ITERATIONS as i32);
        s.budget_advisory_injected = false;
        observer.on_notice(&format!(
            "Iteration budget {previous} reached, still progressing ({}) \u{2014} extended to {} (cap {hard_cap}).",
            verdict.reason, s.iteration_budget
        ));
        if !s.summary_requested {
            self.messages.push(ChatMessage::user(format!(
                "[Budget] You are making progress ({}), so the iteration budget was extended from {previous} to {}. Continue with the original request. Further extensions are granted only while your changes measurably move the tests forward.",
                verdict.reason, s.iteration_budget
            )));
        }
        true
    }
}

/// `ErrorRecoveryHandler.IsRetryableError`: rate limits, overload and timeouts are worth a
/// delayed retry of the same model; anything else is not.
pub fn is_retryable_error(err: &ChatError) -> bool {
    let msg = err.to_string().to_lowercase();
    if msg.contains("rate limit") || msg.contains("429") || msg.contains("too many requests") {
        return true;
    }
    if msg.contains("503") || msg.contains("overloaded") || msg.contains("service unavailable") {
        return true;
    }
    if msg.contains("timeout") || msg.contains("timed out") {
        return true;
    }
    matches!(err.status(), Some(429 | 503 | 504))
}

fn parsed_to_call(p: &ParsedToolCall) -> ToolCall {
    ToolCall {
        id: p.id.clone(),
        name: p.name.clone(),
        arguments: Value::Object(p.arguments.clone()).to_string(),
    }
}

impl AgentSession {
    fn is_verification_off(&self) -> bool {
        matches!(
            self.config
                .agent
                .verification_mode
                .trim()
                .to_lowercase()
                .as_str(),
            "off" | "never" | "false"
        )
    }

    /// `DelegatedWithoutCheck`: this turn's last successful delegation of code work has no
    /// passing command run after it.
    pub(super) fn delegated_without_check(&self) -> bool {
        let history = self.turn_history();
        let Some(last) = history.iter().rposition(|r| {
            matches!(r.name.as_str(), "subagent" | "spawn_agent" | "spawn_agents")
                && !r.is_error
                && delegates_code_work(r.arguments.as_deref())
        }) else {
            return false;
        };
        !history[last + 1..].iter().any(|r| {
            is_command_runner(&r.name)
                && run_evidence::classify(r.result.as_deref(), r.is_error, r.arguments.as_deref())
                    == run_evidence::RunOutcome::Pass
        })
    }
}

/// Agent types that only read: delegating to them changes no code.
const READ_ONLY_AGENT_TYPES: &[&str] =
    &["scout", "planner", "reviewer", "explore", "plan", "review"];

/// `DelegatesCodeWork`: whether a delegation call's arguments send work to an agent that can
/// change code: any named agent (agent / agent_type, in single, tasks, chain or agents form)
/// that is not read-only, a task with no agent, or a follow-up ("to"). Unparsable = yes.
pub fn delegates_code_work(args_json: Option<&str>) -> bool {
    let Some(text) = args_json.filter(|a| !a.trim().is_empty()) else {
        return true;
    };
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(text) else {
        return true;
    };
    if root
        .get("to")
        .and_then(Value::as_str)
        .is_some_and(|t| !t.trim().is_empty())
    {
        return true;
    }
    let root = Value::Object(root);
    let mut items = vec![&root];
    for key in ["tasks", "chain", "agents"] {
        if let Some(Value::Array(arr)) = root.get(key) {
            items.extend(arr.iter().filter(|e| e.is_object()));
        }
    }
    let mut saw_task = false;
    for item in items {
        if !item.get("task").is_some_and(Value::is_string) {
            continue;
        }
        saw_task = true;
        let agent = item
            .get("agent")
            .and_then(Value::as_str)
            .or_else(|| item.get("agent_type").and_then(Value::as_str))
            .map(str::trim);
        match agent {
            Some(a)
                if !a.is_empty()
                    && READ_ONLY_AGENT_TYPES
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(a)) => {}
            _ => return true,
        }
    }
    !saw_task
}

fn is_command_runner(name: &str) -> bool {
    matches!(
        name,
        "bash" | "shell" | "run_command" | "execute_command" | "get_process_output" | "run_tests"
    )
}

/// Arguments in one canonical form for loop detection (compact JSON, or the raw text).
fn normalized_args(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => v.to_string(),
        Err(_) => raw.to_string(),
    }
}

/// `StripReason`: drop a `_reason` argument before the tool sees it.
fn strip_reason(raw: &str) -> String {
    if !raw.contains("_reason") {
        return raw.to_string();
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(mut o)) if o.contains_key("_reason") => {
            o.shift_remove("_reason");
            Value::Object(o).to_string()
        }
        _ => raw.to_string(),
    }
}
