//! Port of `tests/OpenOrca.Core.Tests/CompactionPlannerTests.cs`: thinking-aware compaction
//! (`Conversation::compact_light`), the planned compaction point (context.compactAtRatio) and
//! the cost gate (`planner`).

use orca_core::chat::compaction::{self, CompactionSettings};
use orca_core::chat::conversation::{LIGHT_RESULT_MAX_CHARS, estimate_message_tokens};
use orca_core::chat::planner::{self, LIGHT_TARGET_SHARE};
use orca_core::chat::{CompactionAction, CompactionCostModel, Conversation};
use orca_core::client::{ChatMessage, Role, ToolCall};
use orca_core::config::ContextConfig;
use serde_json::{Value, json};

const WINDOW: i32 = 131072;

fn config(ratio: f32, gate: bool) -> ContextConfig {
    ContextConfig {
        context_window_size: WINDOW,
        compact_at_ratio: ratio,
        compact_cost_gate: gate,
        chars_per_token: 3.5,
        ..ContextConfig::default()
    }
}

fn default_config() -> ContextConfig {
    config(0.5, true)
}

fn assistant_call(id: &str, name: &str, args: Value, reasoning: Option<String>) -> ChatMessage {
    let mut a = ChatMessage::assistant("");
    a.content = None;
    a.reasoning = reasoning;
    a.tool_calls.push(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    });
    a
}

/// A worker-like history: the task, then `steps` steps of (reasoning + a bash call, a
/// result), with every 3rd step a write of a big file.
fn worker_with(
    steps: usize,
    reasoning_chars: usize,
    result_chars: usize,
    write_chars: usize,
) -> Conversation {
    let mut c = Conversation::new();
    c.add_system_message("You are a worker.");
    c.add_user_message("Build the UI. Done when: dotnet build passes.");
    for i in 0..steps {
        let write = i % 3 == 2;
        let (name, args) = if write {
            (
                "write",
                json!({"path": format!("src/F{i}.cs"), "content": "w".repeat(write_chars)}),
            )
        } else {
            ("bash", json!({"command": format!("dotnet build #{i}")}))
        };
        // C# adds an empty reasoning part when reasoningChars is 0; it is never dropped.
        c.add_message(assistant_call(
            &format!("c{i}"),
            name,
            args,
            Some("r".repeat(reasoning_chars)),
        ));
        c.add_message(ChatMessage::tool_result(
            format!("c{i}"),
            if write {
                "Wrote file".to_string()
            } else {
                "o".repeat(result_chars)
            },
        ));
    }
    c
}

fn worker(steps: usize) -> Conversation {
    worker_with(steps, 3000, 2500, 6000)
}

fn has_reasoning(m: &ChatMessage) -> bool {
    m.reasoning.as_deref().is_some_and(|r| !r.is_empty())
}

// ── Settings ──

#[test]
fn planned_point_is_half_the_window_by_default() {
    let s = CompactionSettings::from_config(&ContextConfig {
        context_window_size: WINDOW,
        ..ContextConfig::default()
    });
    assert_eq!(s.planned_trigger_tokens, WINDOW / 2);
    assert_eq!(s.trigger_tokens(), WINDOW - 16384); // the hard limit is unchanged
    assert!(!s.should_plan_compaction(WINDOW / 2));
    assert!(s.should_plan_compaction(WINDOW / 2 + 1));
}

#[test]
fn planned_point_off_or_above_the_hard_limit_is_the_hard_limit() {
    for ratio in [0.0f32, 1.0, 0.95] {
        let s = CompactionSettings::from_config(&ContextConfig {
            context_window_size: WINDOW,
            compact_at_ratio: ratio,
            ..ContextConfig::default()
        });
        assert_eq!(
            s.trigger_tokens(),
            s.planned_trigger_tokens,
            "ratio {ratio}"
        );
    }
}

#[test]
fn planned_point_stays_above_twice_the_kept_tail() {
    let s = CompactionSettings::from_config(&ContextConfig {
        context_window_size: WINDOW,
        compact_at_ratio: 0.1,
        ..ContextConfig::default()
    });
    assert_eq!(s.keep_recent_tokens * 2, s.planned_trigger_tokens);
}

// ── Thinking-aware compaction ──

#[test]
fn compact_light_drops_reasoning_cuts_output_and_write_text_before_the_index_only() {
    let mut c = worker(10);
    let rewrites = c.rewrite_count();
    let cut = 1 + 2 * 6; // the first 6 steps

    let reclaimed = c.compact_light(cut, 3.5, false, 0);

    assert!(reclaimed > 0);
    assert_eq!(c.rewrite_count(), rewrites + 1);
    let msgs = c.messages();
    assert_eq!(
        msgs[0].content.as_deref(),
        Some("Build the UI. Done when: dotnet build passes.")
    ); // the task is untouched
    for (i, m) in msgs.iter().enumerate().take(cut).skip(1) {
        assert!(!has_reasoning(m));
        if m.role == Role::Tool {
            assert!(m.content.as_deref().unwrap().encode_utf16().count() <= LIGHT_RESULT_MAX_CHARS);
        }
        for fc in m.tool_calls.iter().filter(|f| f.name == "write") {
            let args: Value = serde_json::from_str(&fc.arguments).unwrap();
            assert_eq!(args["path"], format!("src/F{}.cs", (i - 1) / 2)); // what was written where stays
            assert!(
                args["content"]
                    .as_str()
                    .unwrap()
                    .contains("dropped at compaction")
            );
        }
    }
    // The kept tail is verbatim, reasoning included.
    for m in &msgs[cut..] {
        if m.role == Role::Assistant {
            assert!(has_reasoning(m));
        }
    }
    assert_eq!(msgs.last().unwrap().content.as_deref().unwrap().len(), 2500);
}

#[test]
fn compact_light_dry_run_estimates_without_changing() {
    let mut c = worker(6);
    let before = c.estimate_token_count(3.5);
    let rewrites = c.rewrite_count();
    let n = c.messages().len();

    let estimate = c.compact_light(n, 3.5, true, 0);

    assert_eq!(c.estimate_token_count(3.5), before);
    assert_eq!(c.rewrite_count(), rewrites);
    let real = c.compact_light(n, 3.5, false, 0);
    assert_eq!(estimate, real);
    let dropped = before - c.estimate_token_count(3.5);
    assert!(
        (real - 5..=real + 5).contains(&dropped),
        "{dropped} vs {real}"
    );
}

// CompactLight_RemovesOldImages_AndSaysSo: not portable yet; the Rust message model has no
// image parts (tool results are text only), so there is nothing to drop.

#[test]
fn compact_light_nothing_to_drop_does_not_rewrite() {
    let mut c = worker_with(3, 0, 100, 100);
    let rewrites = c.rewrite_count();
    let n = c.messages().len();
    assert_eq!(c.compact_light(n, 3.5, false, 0), 0);
    assert_eq!(c.rewrite_count(), rewrites);
}

// ── Planner ──

#[test]
fn plan_under_the_planned_point_does_nothing() {
    let c = worker(5);
    let cfg = default_config();
    let plan = planner::plan(&c, &CompactionSettings::from_config(&cfg), &cfg, None, None);
    assert_eq!(plan.action, CompactionAction::None);
}

#[test]
fn plan_past_the_planned_point_light_compaction_pays_over_a_long_run() {
    // ~72k tokens, most of it old reasoning and output.
    let c = worker(32);
    let cfg = default_config();
    let settings = CompactionSettings::from_config(&cfg);
    assert!(c.estimate_context_tokens(3.5) > settings.planned_trigger_tokens);
    assert!(c.estimate_context_tokens(3.5) < settings.trigger_tokens());

    let plan = planner::plan(&c, &settings, &cfg, Some(40), None);

    assert_eq!(plan.action, CompactionAction::Light);
    assert!(plan.savings_seconds > plan.cost_seconds);
    assert!(
        f64::from(plan.tokens_after)
            <= f64::from(settings.planned_trigger_tokens) * LIGHT_TARGET_SHARE
    );
}

fn fast_server() -> CompactionCostModel {
    // A server where context costs nothing per request and caches never miss.
    CompactionCostModel {
        seconds_per_request_per_100k_tokens: 0.0,
        cache_miss_rate: 0.0,
        ..CompactionCostModel::default()
    }
}

#[test]
fn plan_gate_says_no_when_the_run_is_about_to_end_and_the_server_is_fast_at_long_context() {
    let c = worker(32);
    let cfg = default_config();
    let settings = CompactionSettings::from_config(&cfg);

    let plan = planner::plan(&c, &settings, &cfg, Some(1), Some(&fast_server()));

    assert_eq!(plan.action, CompactionAction::None);
    assert!(plan.reason.contains("not worth"));
}

#[test]
fn plan_gate_off_compacts_at_the_planned_point() {
    let c = worker(32);
    let cfg = config(0.5, false);

    let plan = planner::plan(
        &c,
        &CompactionSettings::from_config(&cfg),
        &cfg,
        Some(1),
        Some(&fast_server()),
    );

    assert_eq!(plan.action, CompactionAction::Light);
}

#[test]
fn plan_hard_limit_always_compacts_and_summarizes_when_light_is_not_enough() {
    // Big tool output in the kept tail and little reasoning: light compaction can't get far enough.
    let c = worker_with(110, 200, 4000, 200);
    let cfg = config(0.0, true);
    let settings = CompactionSettings::from_config(&cfg);
    assert!(settings.should_compact(c.estimate_context_tokens(3.5)));

    let plan = planner::plan(&c, &settings, &cfg, Some(1), None);

    assert_ne!(plan.action, CompactionAction::None);
    assert_eq!(plan.reason, "hard limit");
}

#[test]
fn plan_full_when_light_cannot_get_under_the_planned_point() {
    // Output that is all short (nothing to cut) but many steps of it.
    let mut c = Conversation::new();
    c.add_user_message("task");
    for i in 0..400 {
        c.add_message(assistant_call(
            &format!("c{i}"),
            "bash",
            json!({"command": "x"}),
            None,
        ));
        c.add_message(ChatMessage::tool_result(format!("c{i}"), "o".repeat(1400)));
    }
    let cfg = config(0.5, false);
    let plan = planner::plan(&c, &CompactionSettings::from_config(&cfg), &cfg, None, None);
    assert_eq!(plan.action, CompactionAction::Full);
}

#[tokio::test]
async fn apply_light_rewrites_once_and_shrinks_the_history() {
    let mut c = worker(32);
    let cfg = default_config();
    let settings = CompactionSettings::from_config(&cfg);
    let plan = planner::plan(&c, &settings, &cfg, Some(40), None);
    let rewrites = c.rewrite_count();

    let (rewritten, _) = planner::apply(&mut c, &plan, &cfg, None).await;
    assert!(rewritten);

    assert_eq!(c.rewrite_count(), rewrites + 1);
    assert!(c.estimate_context_tokens(3.5) < settings.planned_trigger_tokens);
    // Afterwards it is under the planned point: no second rewrite on the next check.
    assert_eq!(
        planner::plan(&c, &settings, &cfg, Some(40), None).action,
        CompactionAction::None
    );
}

#[test]
fn compact_for_resume_drops_old_reasoning_keeps_the_tail() {
    let mut c = worker(30);
    let before = c.estimate_token_count(3.5);
    let cfg = default_config();
    let settings = CompactionSettings::from_config(&cfg);

    let reclaimed = planner::compact_for_resume(&mut c, &settings, &cfg);

    assert!(reclaimed > 0);
    assert!(c.estimate_token_count(3.5) < before);
    let msgs = c.messages();
    assert!(has_reasoning(&msgs[msgs.len() - 2]));
}

#[test]
fn cost_model_matches_the_measured_server() {
    let m = CompactionCostModel::default();
    // 30k-token re-read at ~1000 tok/s.
    assert!((m.reread_seconds(30_000) - 30.0).abs() < 0.05);
    // A request carrying 50k fewer tokens: ~0.7 s faster + ~1 s of expected miss re-read.
    let s = 50_000.0 * m.seconds_per_context_token_per_request();
    assert!((1.5..=2.0).contains(&s), "{s}");
}

// ── The kept head (context.keepFirstTokens) ──

#[test]
fn head_end_keeps_the_task_and_first_turns_on_a_cut_point() {
    let c = worker(20);
    let head = compaction::head_end(c.messages(), 9000, 3.5);
    assert!(head > 1, "more than the task");
    assert!(
        compaction::is_cut_point(&c.messages()[head]),
        "the head never ends between a call and its result"
    );
    let tokens: i32 = c.messages()[..head]
        .iter()
        .map(|m| estimate_message_tokens(m, 3.5))
        .sum();
    assert!((1..=9000).contains(&tokens));
    assert_eq!(compaction::head_end(c.messages(), 0, 3.5), 0); // off
}

#[tokio::test]
async fn plan_trims_only_the_middle_and_the_head_lowers_the_reread_cost() {
    let with_head = config(0.5, false);
    let without_head = ContextConfig {
        keep_first_tokens: 0,
        ..config(0.5, false)
    };
    let settings = CompactionSettings::from_config(&with_head);
    let mut c = worker(60);
    assert!(c.estimate_token_count(3.5) > settings.planned_trigger_tokens);

    let plan = planner::plan(&c, &settings, &with_head, Some(30), None);
    let plan_no_head = planner::plan(&c, &settings, &without_head, Some(30), None);
    assert_ne!(plan.action, CompactionAction::None);
    assert!(plan.head_end > 1);
    assert!(
        plan.cost_seconds < plan_no_head.cost_seconds,
        "{plan} vs {plan_no_head}"
    );

    let head_before: Vec<ChatMessage> = c.messages()[..plan.head_end].to_vec();
    let light = planner_plan_light(&plan);
    planner::apply(&mut c, &light, &with_head, None).await;
    // The head is verbatim (its reasoning included); the middle lost its reasoning.
    assert_eq!(&c.messages()[..plan.head_end], head_before.as_slice());
    assert!(has_reasoning(&c.messages()[1]));
    assert!(!has_reasoning(&c.messages()[plan.head_end]));
}

fn planner_plan_light(plan: &orca_core::chat::CompactionPlan) -> orca_core::chat::CompactionPlan {
    orca_core::chat::CompactionPlan {
        action: CompactionAction::Light,
        ..plan.clone()
    }
}

#[test]
fn summary_goes_after_the_head_and_replaces_itself_next_time() {
    let mut c = worker(40);
    let prep = compaction::prepare(c.messages(), 8000, 3.5, None, 9000).unwrap();
    assert!(prep.head_end > 1);
    let head: Vec<ChatMessage> = c.messages()[..prep.head_end].to_vec();
    assert!(!prep.messages_to_summarize.iter().any(|m| head.contains(m)));

    c.apply_compaction("first summary", prep.first_kept_index, prep.head_end);
    assert_eq!(&c.messages()[..prep.head_end], head.as_slice());
    assert_eq!(
        compaction::try_get_summary(&c.messages()[prep.head_end]).as_deref(),
        Some("first summary")
    );

    for _ in 0..20 {
        c.add_message(ChatMessage::assistant("t".repeat(3000)));
    }
    let prep2 = compaction::prepare(c.messages(), 8000, 3.5, None, 9000).unwrap();
    assert_eq!(prep2.head_end, prep.head_end); // fixed at the first compaction
    assert_eq!(
        prep2.previous_summary.as_deref().map(str::trim),
        Some("first summary")
    );
    c.apply_compaction("second summary", prep2.first_kept_index, prep2.head_end);
    assert_eq!(&c.messages()[..prep.head_end], head.as_slice());
    assert_eq!(
        c.messages()
            .iter()
            .filter(|m| compaction::try_get_summary(m).is_some())
            .count(),
        1
    );
}

// ── Calibration against the server's real prompt count ──

#[test]
fn calibration_scales_estimates_of_a_rewritten_history() {
    let mut c = worker(20);
    let estimate = c.estimate_token_count(3.5);
    assert_eq!(c.estimate_calibration(), 1.0);

    let real = (f64::from(estimate) * 2.2) as i32;
    let n = c.messages().len();
    c.record_prompt_usage(real, n, None);
    assert!((2.15..=2.25).contains(&c.estimate_calibration()));
    assert_eq!(c.estimate_context_tokens(3.5), real); // usage base: the real count

    c.compact_light(5, 3.5, false, 0); // a rewrite: no usage base, so the estimate is used, scaled
    assert!(!c.has_usage_base());
    let base = f64::from(c.estimate_token_count(3.5));
    let ctx = c.estimate_context_tokens(3.5);
    assert!(((base * 2.15) as i32..=(base * 2.25) as i32).contains(&ctx));
}

#[test]
fn calibration_ignores_small_prompts_and_is_clamped() {
    let mut c = worker(1);
    let n = c.messages().len();
    c.record_prompt_usage(c.estimate_token_count(3.5) * 3, n, None); // mostly tool schemas
    assert_eq!(c.estimate_calibration(), 1.0);

    let mut big = worker(20);
    let n = big.messages().len();
    big.record_prompt_usage(big.estimate_token_count(3.5) * 10, n, None);
    assert_eq!(big.estimate_calibration(), 4.0);
}

/// orca-haiku4: a history estimated at ~23k was ~51k on the server, most of it in the recent
/// tail a summary keeps. Calibrated, the plan sees the real tail and skips.
#[test]
fn plan_skips_a_compaction_that_would_not_cut_the_real_prompt() {
    let cfg = ContextConfig {
        context_window_size: 100_000,
        compact_at_ratio: 0.5,
        compact_cost_gate: false,
        chars_per_token: 3.5,
        ..ContextConfig::default()
    };
    let settings = CompactionSettings::from_config(&cfg);
    let mut c = worker_with(18, 0, 2500, 500);
    let estimate = c.estimate_token_count(3.5);
    assert!((12_000..=30_000).contains(&estimate), "{estimate}");
    let n = c.messages().len();
    c.record_prompt_usage(51_000, n, None);

    let plan = planner::plan(&c, &settings, &cfg, Some(30), None);

    assert_eq!(plan.tokens_before, 51_000);
    assert_eq!(plan.action, CompactionAction::None);
    assert!(plan.reason.contains("under 20%"));
    assert!(f64::from(plan.tokens_after) > 51_000.0 * 0.8, "{plan}");
}

#[test]
fn plan_still_compacts_when_the_estimate_matches_the_server() {
    let cfg = ContextConfig {
        context_window_size: 100_000,
        compact_at_ratio: 0.5,
        compact_cost_gate: false,
        chars_per_token: 3.5,
        ..ContextConfig::default()
    };
    let settings = CompactionSettings::from_config(&cfg);
    let mut c = worker(40);
    let estimate = c.estimate_token_count(3.5);
    assert!(estimate > settings.planned_trigger_tokens);
    let n = c.messages().len();
    c.record_prompt_usage(estimate, n, None);

    let plan = planner::plan(&c, &settings, &cfg, Some(30), None);

    assert_ne!(plan.action, CompactionAction::None);
    assert!(
        f64::from(plan.tokens_after) < f64::from(estimate) * 0.8,
        "{plan}"
    );
}
