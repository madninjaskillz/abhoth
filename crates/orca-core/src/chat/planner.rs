//! When to rewrite the history (`CompactionPlanner.cs`). Rewriting breaks the server's prefix
//! cache, so a planned compaction (past [`CompactionSettings::planned_trigger_tokens`]) only
//! runs when the requests after it, each carrying a shorter context, save more than the one
//! re-read of the rewritten history costs; and it runs as one big rewrite, never a trickle of
//! small ones. The hard limit ([`CompactionSettings::trigger_tokens`]) compacts regardless.
//! The system prompt and the first task message are never touched.

use std::fmt;

use super::compaction::{self, CompactionSettings, effective_keep_first_tokens};
use super::compactor::{CompactionReport, ConversationCompactor};
use super::conversation::Conversation;
use super::cpt_or_default;
use crate::client::Role;
use crate::config::ContextConfig;

/// A light compaction must get the history at least this far under the planned point, or the
/// summary runs too.
pub const LIGHT_TARGET_SHARE: f64 = 0.8;

/// A compaction must shrink the (calibrated) prompt by at least this share, or it is skipped.
pub const MIN_REDUCTION_SHARE: f64 = 0.2;

/// What a compaction check decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Leave the history alone (the prompt cache stays warm).
    None,
    /// Thinking-aware compaction only ([`Conversation::compact_light`]): no model call.
    Light,
    /// Thinking-aware compaction, then the model summary ([`ConversationCompactor`]).
    Full,
}

impl fmt::Display for CompactionAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CompactionAction::None => "None",
            CompactionAction::Light => "Light",
            CompactionAction::Full => "Full",
        })
    }
}

/// A compaction decision with the numbers behind it (for the log and the bench).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    pub action: CompactionAction,
    pub tokens_before: i32,
    pub tokens_after: i32,
    pub light_cut_index: usize,
    pub savings_seconds: f64,
    pub cost_seconds: f64,
    pub remaining_requests: i32,
    pub reason: String,
    /// End (exclusive) of the kept head (`context.keepFirstTokens`): the thinking-aware pass
    /// starts here.
    pub head_end: usize,
}

impl CompactionPlan {
    #[allow(clippy::too_many_arguments)]
    fn new(
        action: CompactionAction,
        tokens_before: i32,
        tokens_after: i32,
        light_cut_index: usize,
        savings_seconds: f64,
        cost_seconds: f64,
        remaining_requests: i32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            action,
            tokens_before,
            tokens_after,
            light_cut_index,
            savings_seconds,
            cost_seconds,
            remaining_requests,
            reason: reason.into(),
            head_end: 0,
        }
    }

    pub fn nothing(tokens: i32, reason: impl Into<String>) -> Self {
        Self::new(
            CompactionAction::None,
            tokens,
            tokens,
            0,
            0.0,
            0.0,
            0,
            reason,
        )
    }

    fn with_head(mut self, head_end: usize) -> Self {
        self.head_end = head_end;
        self
    }
}

impl fmt::Display for CompactionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: ~{} -> ~{} tokens, saves ~{:.0} s over ~{} requests, costs ~{:.0} s ({})",
            self.action,
            self.tokens_before,
            self.tokens_after,
            self.savings_seconds,
            self.remaining_requests,
            self.cost_seconds,
            self.reason
        )
    }
}

/// Request-time model of a local server, fitted on the Strata copies: a cache miss reads
/// ~1000 prompt tokens/s; a request costs ~1.4 s more per 100k tokens of context; about 2% of
/// requests miss the cache for reasons other than a rewrite and re-read most of the history.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionCostModel {
    pub prefill_tokens_per_second: f64,
    pub seconds_per_request_per_100k_tokens: f64,
    pub cache_miss_rate: f64,
    pub summary_tokens_per_second: f64,
    pub summary_output_tokens: i32,
    pub margin: f64,
}

impl Default for CompactionCostModel {
    fn default() -> Self {
        Self {
            prefill_tokens_per_second: 1000.0,
            seconds_per_request_per_100k_tokens: 1.4,
            cache_miss_rate: 0.02,
            summary_tokens_per_second: 60.0,
            summary_output_tokens: 2500,
            margin: 1.2,
        }
    }
}

impl CompactionCostModel {
    /// The summarizer reads the old span serialized with tool output cut.
    pub const SUMMARY_INPUT_SHARE: f64 = 0.46;

    pub fn from_config(config: &ContextConfig) -> Self {
        Self {
            prefill_tokens_per_second: if config.prefill_tokens_per_second > 0 {
                f64::from(config.prefill_tokens_per_second)
            } else {
                1000.0
            },
            ..Self::default()
        }
    }

    /// Seconds a request spends per token of context it carries (slower generation plus
    /// cache-miss re-reads).
    pub fn seconds_per_context_token_per_request(&self) -> f64 {
        self.seconds_per_request_per_100k_tokens / 100_000.0
            + self.cache_miss_rate / self.prefill_tokens_per_second
    }

    /// One re-read of the rewritten history.
    pub fn reread_seconds(&self, tokens_after: i32) -> f64 {
        f64::from(tokens_after) / self.prefill_tokens_per_second
    }

    /// The model summary: reading the old span, writing the checkpoint.
    pub fn summary_seconds(&self, summarized_tokens: i32) -> f64 {
        f64::from(summarized_tokens) * Self::SUMMARY_INPUT_SHARE / self.prefill_tokens_per_second
            + f64::from(self.summary_output_tokens) / self.summary_tokens_per_second
    }
}

/// Decide for `conversation`. `remaining_requests`: the caller's estimate of the requests
/// still to come (a worker's step budget). The estimate used is at least half the requests so
/// far and at least 10.
pub fn plan(
    conversation: &Conversation,
    settings: &CompactionSettings,
    config: &ContextConfig,
    remaining_requests: Option<i32>,
    model: Option<&CompactionCostModel>,
) -> CompactionPlan {
    let model = model
        .cloned()
        .unwrap_or_else(|| CompactionCostModel::from_config(config));
    let cpt = cpt_or_default(config.chars_per_token);
    let tokens = conversation.estimate_context_tokens(cpt);
    let hard = settings.should_compact(tokens);
    if !config.thinking_aware_compaction {
        return if hard {
            CompactionPlan::new(
                CompactionAction::Full,
                tokens,
                tokens,
                0,
                0.0,
                0.0,
                0,
                "hard limit (summary only)",
            )
        } else {
            CompactionPlan::nothing(tokens, "thinking-aware compaction off")
        };
    }
    if !hard && !settings.should_plan_compaction(tokens) {
        return CompactionPlan::nothing(tokens, "under the planned point");
    }

    let messages = conversation.messages();
    // The head (task + first turns) and any checkpoint after it stay: only the middle,
    // between the head and the kept recent tail, is rewritten.
    let head = compaction::head_end(messages, effective_keep_first_tokens(config), cpt);
    let start = if messages
        .get(head)
        .is_some_and(|m| compaction::try_get_summary(m).is_some())
    {
        head + 1
    } else {
        head
    };
    // Never rewrite the task: the first user turn (and anything before it) stays verbatim.
    let mut first_user: isize = (start..messages.len())
        .find(|&i| compaction::is_turn_start(&messages[i]))
        .map_or(-1, |i| i as isize);
    if head > 0 {
        first_user = -1; // the task is inside the head
    }
    let cut = compaction::find_cut_point(
        messages,
        start,
        messages.len(),
        settings.keep_recent_tokens,
        cpt,
    )
    .first_kept_index;
    // Estimates are scaled to the server's real count (Conversation::estimate_calibration).
    let calibration = conversation.estimate_calibration();
    let reclaim = if cut as isize > (first_user + 1).max(start as isize) {
        (f64::from(conversation.compact_light_estimate(cut, cpt, start)) * calibration) as i32
    } else {
        0
    };
    let after_light = (tokens - reclaim).max(0);
    // A rewrite keeps the system prompt + head as a cached prefix; only what follows is re-read.
    let kept_prefix = if head > 0 {
        conversation.estimate_prefix_tokens_calibrated(head, cpt)
    } else {
        0
    };

    let turns = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count() as i32;
    let remaining = 10.max(remaining_requests.unwrap_or(0).max(turns / 2));
    let growth = if turns > 0 {
        f64::from(tokens) / f64::from(turns)
    } else {
        0.0
    };

    let light_enough =
        f64::from(after_light) <= f64::from(settings.planned_trigger_tokens) * LIGHT_TARGET_SHARE;
    let action = if light_enough {
        CompactionAction::Light
    } else {
        CompactionAction::Full
    };
    // A summary keeps the tail from the cut on verbatim: its real size, not the nominal one.
    let after_full = if cut > start {
        conversation.estimate_tail_tokens(cut, cpt)
            + model.summary_output_tokens
            + (kept_prefix - conversation.estimate_prefix_tokens_calibrated(0, cpt))
    } else {
        tokens
    };
    let after = if light_enough {
        after_light
    } else {
        after_light.min(after_full)
    };

    // Not worth a cache break (or, at the hard limit, a summary request) when the real prompt
    // would shrink by less than MIN_REDUCTION_SHARE: the next check would only plan it again.
    if !hard && f64::from(tokens - after) < f64::from(tokens) * MIN_REDUCTION_SHARE {
        return CompactionPlan::new(
            CompactionAction::None,
            tokens,
            after,
            cut,
            0.0,
            0.0,
            0,
            "would cut the real prompt by under 20%",
        )
        .with_head(head);
    }
    let cost = model.reread_seconds((after - kept_prefix).max(0))
        + if action == CompactionAction::Full {
            model.summary_seconds((after_light - settings.keep_recent_tokens).max(0))
        } else {
            0.0
        };
    let mut savings =
        f64::from(remaining * (tokens - after)) * model.seconds_per_context_token_per_request();

    if hard {
        return CompactionPlan::new(
            action,
            tokens,
            after,
            cut,
            savings,
            cost,
            remaining,
            "hard limit",
        )
        .with_head(head);
    }

    // Left alone, will the run reach the hard limit anyway? Then that compaction (summary
    // included) is paid later regardless, so doing the work now costs little extra.
    let projected = f64::from(tokens) + f64::from(remaining) * growth;
    if projected > f64::from(settings.trigger_tokens()) {
        savings += model.reread_seconds(settings.keep_recent_tokens + model.summary_output_tokens)
            + model.summary_seconds(settings.trigger_tokens() - settings.keep_recent_tokens);
    }

    if reclaim <= 0 && action == CompactionAction::Light {
        return CompactionPlan::nothing(tokens, "nothing to drop before the kept tail");
    }
    if config.compact_cost_gate && savings <= cost * model.margin {
        return CompactionPlan::new(
            CompactionAction::None,
            tokens,
            after,
            cut,
            savings,
            cost,
            remaining,
            "not worth the cache break yet",
        )
        .with_head(head);
    }
    CompactionPlan::new(
        action,
        tokens,
        after,
        cut,
        savings,
        cost,
        remaining,
        if config.compact_cost_gate {
            "pays for its re-read"
        } else {
            "planned point (cost gate off)"
        },
    )
    .with_head(head)
}

/// Carry out `plan`: the thinking-aware pass, then (Full) the model summary through
/// `compactor`, or deterministic pruning when there is none. Returns whether the history was
/// rewritten, and what the compactor reported.
pub async fn apply(
    conversation: &mut Conversation,
    plan: &CompactionPlan,
    config: &ContextConfig,
    compactor: Option<&dyn ConversationCompactor>,
) -> (bool, CompactionReport) {
    let mut report = CompactionReport::default();
    if plan.action == CompactionAction::None {
        return (false, report);
    }
    let cpt = cpt_or_default(config.chars_per_token);
    let rewrites = conversation.rewrite_count();
    if plan.light_cut_index > plan.head_end {
        conversation.compact_light(plan.light_cut_index, cpt, false, plan.head_end);
    }
    if plan.action == CompactionAction::Full {
        match compactor {
            Some(c) => {
                report = c
                    .compact_conversation(conversation, None, Some(config))
                    .await;
            }
            None => {
                let end = conversation.messages().len().saturating_sub(2);
                conversation.prune_tool_results_before(end, 200);
            }
        }
    }
    (conversation.rewrite_count() != rewrites, report)
}

/// Before a follow-up goes to an agent whose turn ended (its server cache is probably gone):
/// drop the old reasoning and bulky output first so the re-read is short. No model call.
/// Returns the estimated tokens reclaimed.
pub fn compact_for_resume(
    conversation: &mut Conversation,
    settings: &CompactionSettings,
    config: &ContextConfig,
) -> i32 {
    let cpt = cpt_or_default(config.chars_per_token);
    let messages = conversation.messages();
    let head = compaction::head_end(messages, effective_keep_first_tokens(config), cpt);
    let start = if messages
        .get(head)
        .is_some_and(|m| compaction::try_get_summary(m).is_some())
    {
        head + 1
    } else {
        head
    };
    let cut = compaction::find_cut_point(
        messages,
        start,
        messages.len(),
        settings.keep_recent_tokens,
        cpt,
    )
    .first_kept_index;
    if cut > start {
        conversation.compact_light(cut, cpt, false, start)
    } else {
        0
    }
}
