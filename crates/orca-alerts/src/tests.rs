use std::collections::BTreeMap;
use std::sync::Arc;

use crate::engine::{AlertState, Engine, NotifyKind, Signal, render};
use crate::rules::{AlertsConfig, RuleConfig, Severity};
use crate::sinks::{self, FakeSink, HaNotify, Sink};

fn vars(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn no_mute(_: &str, _: bool) -> Option<String> {
    None
}

fn hot(card: &str, active: bool, temp: &str, at: i64) -> Signal {
    Signal::Condition {
        rule: "hot_card".into(),
        vars: vars(&[("card", card), ("value", temp)]),
        active,
        severity: None,
        cause: None,
        at,
    }
}

fn engine() -> Engine {
    Engine::new(&AlertsConfig::default()).0
}

#[test]
fn a_condition_must_hold_for_its_for_before_it_opens() {
    let mut e = engine();
    assert!(e.observe(hot("A", true, "86", 1000), &no_mute).is_empty());
    assert!(e.observe(hot("A", true, "87", 1030), &no_mute).is_empty());
    let n = e.observe(hot("A", true, "88", 1060), &no_mute);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].identity, "hot_card/A");
    assert_eq!(n[0].message, "GPU A at 88°C");
    assert_eq!(n[0].kind, NotifyKind::Firing);
    // Going away before `for` never opens.
    assert!(e.observe(hot("B", true, "86", 1000), &no_mute).is_empty());
    assert!(e.observe(hot("B", false, "80", 1010), &no_mute).is_empty());
    assert!(e.alerts().all(|a| a.identity != "hot_card/B"));
}

#[test]
fn one_open_alert_per_identity_and_no_resend_before_repeat() {
    let mut e = engine();
    e.observe(hot("A", true, "86", 0), &no_mute);
    assert_eq!(e.observe(hot("A", true, "86", 60), &no_mute).len(), 1);
    // Still hot: nothing new until `repeat` (2h).
    for t in (120..7000).step_by(60) {
        assert!(
            e.observe(hot("A", true, "86", t), &no_mute).is_empty(),
            "at {t}"
        );
    }
    assert!(e.tick(7000, &no_mute).is_empty());
    let r = e.tick(60 + 7200, &no_mute);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, NotifyKind::Repeat);
    // A different card is a different identity.
    e.observe(hot("C", true, "90", 8000), &no_mute);
    assert_eq!(e.observe(hot("C", true, "90", 8060), &no_mute).len(), 1);
    assert_eq!(e.open().len(), 2);
}

#[test]
fn resolve_is_sent_and_flapping_within_cooldown_is_not_resent() {
    let mut e = engine();
    e.observe(hot("A", true, "86", 0), &no_mute);
    e.observe(hot("A", true, "86", 60), &no_mute);
    let r = e.observe(hot("A", false, "80", 120), &no_mute);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, NotifyKind::Resolved);
    // Hot again 5 minutes later (cooldown 30m): back open, counted, but no new message.
    assert!(e.observe(hot("A", true, "86", 420), &no_mute).is_empty());
    assert!(e.observe(hot("A", true, "86", 480), &no_mute).is_empty());
    let a = e.alerts().find(|a| a.identity == "hot_card/A").unwrap();
    assert_eq!(a.state, AlertState::Open);
    assert_eq!(a.count, 2);
    // After the cooldown, a new episode is sent again.
    e.observe(hot("A", false, "80", 600), &no_mute);
    e.observe(hot("A", true, "86", 600 + 1900), &no_mute);
    assert_eq!(
        e.observe(hot("A", true, "86", 600 + 1960), &no_mute).len(),
        1
    );
}

#[test]
fn event_alerts_dedupe_by_their_parts_and_close_after_cooldown() {
    let mut e = engine();
    let oom = |c: &str, at| Signal::Event {
        rule: "oom_kill".into(),
        vars: vars(&[("container", c), ("process", "python3")]),
        severity: None,
        cause: None,
        at,
    };
    assert_eq!(e.observe(oom("comfyui", 0), &no_mute).len(), 1);
    assert!(e.observe(oom("comfyui", 10), &no_mute).is_empty());
    assert_eq!(e.observe(oom("sa3", 20), &no_mute).len(), 1);
    let a = e
        .alerts()
        .find(|a| a.identity == "oom_kill/comfyui/python3")
        .unwrap();
    assert_eq!(a.count, 2);
    e.tick(10 + 1800, &no_mute);
    let a = e
        .alerts()
        .find(|a| a.identity == "oom_kill/comfyui/python3")
        .unwrap();
    assert_eq!(a.state, AlertState::Resolved);
    // A later OOM in the same container is a new episode.
    assert_eq!(e.observe(oom("comfyui", 5000), &no_mute).len(), 1);
}

#[test]
fn alerts_with_one_cause_inside_the_window_go_out_as_one() {
    let mut e = engine();
    let sig = |rule: &str, v: &[(&str, &str)], at| Signal::Event {
        rule: rule.into(),
        vars: vars(v),
        severity: None,
        cause: Some("card:B".into()),
        at,
    };
    assert!(
        e.observe(
            sig(
                "switch_failed",
                &[
                    ("server", "dt"),
                    ("layout", "coder"),
                    ("detail", "health timeout")
                ],
                100
            ),
            &no_mute
        )
        .is_empty()
    );
    assert!(
        e.observe(
            sig(
                "engine_crash",
                &[("engine", "llama"), ("card", "B"), ("detail", "exit 137")],
                110
            ),
            &no_mute
        )
        .is_empty()
    );
    assert!(e.tick(120, &no_mute).is_empty());
    let n = e.tick(130, &no_mute);
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].children.len(), 1);
    assert!(n[0].title.contains("+1 related"));
    // Both are critical; the earlier is the parent.
    assert_eq!(n[0].rule, "switch_failed");
    assert_eq!(n[0].children[0].rule, "engine_crash");
}

#[test]
fn calendar_mutes_silence_sinks_but_never_critical() {
    let mut e = engine();
    let mute =
        |rule: &str, critical: bool| (!critical && rule == "hot_card").then(|| "bench".to_string());
    e.observe(hot("A", true, "86", 0), &mute);
    assert!(e.observe(hot("A", true, "86", 60), &mute).is_empty());
    let a = e.alerts().find(|a| a.identity == "hot_card/A").unwrap();
    assert_eq!(a.state, AlertState::Open, "the UI still shows it");
    assert_eq!(a.muted_by.as_deref(), Some("bench"));
    // Critical (fan data stale) is never muted, even by a mute-everything selector.
    let all = |_: &str, critical: bool| (!critical).then(|| "maint".to_string());
    let n = e.observe(
        Signal::Condition {
            rule: "fan_stale".into(),
            vars: vars(&[("fan", "gpu"), ("detail", "no push for 25 s")]),
            active: true,
            severity: None,
            cause: None,
            at: 100,
        },
        &all,
    );
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].severity, Severity::Critical);
}

#[test]
fn rules_are_tunable_and_expression_rules_need_when() {
    let mut cfg = AlertsConfig::default();
    cfg.rules.insert(
        "hot_card".into(),
        RuleConfig {
            hold_for: Some("0s".into()),
            sinks: Some(vec!["ntfy".into()]),
            ..Default::default()
        },
    );
    cfg.rules.insert(
        "oom_kill".into(),
        RuleConfig {
            enabled: Some(false),
            ..Default::default()
        },
    );
    cfg.rules.insert(
        "nvme_hot".into(),
        RuleConfig {
            when: Some("host.nvme0 >= 70".into()),
            message: Some("NVMe at {value}".into()),
            ..Default::default()
        },
    );
    cfg.rules.insert("broken".into(), RuleConfig::default());
    let (mut e, issues) = Engine::new(&cfg);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let n = e.observe(hot("A", true, "86", 0), &no_mute);
    assert_eq!(n[0].sinks, vec!["ntfy".to_string()]);
    assert!(
        e.observe(
            Signal::Event {
                rule: "oom_kill".into(),
                vars: vars(&[]),
                severity: None,
                cause: None,
                at: 0
            },
            &no_mute
        )
        .is_empty()
    );
    let n = e.observe(
        Signal::Condition {
            rule: "nvme_hot".into(),
            vars: vars(&[("value", "71")]),
            active: true,
            severity: None,
            cause: None,
            at: 5,
        },
        &no_mute,
    );
    assert_eq!(n[0].message, "NVMe at 71");
}

#[test]
fn every_rule_the_brief_names_is_built_in() {
    let e = engine();
    for r in [
        "oom_kill",
        "engine_crash",
        "hot_card",
        "switch_failed",
        "fan_stale",
        "clock_mismatch",
    ] {
        assert!(e.rule(r).is_some_and(|x| x.enabled && x.builtin), "{r}");
    }
}

#[test]
fn templates_keep_unknown_names() {
    assert_eq!(
        render(
            "GPU {card} at {value}°C ({x})",
            &vars(&[("card", "A"), ("value", "90")])
        ),
        "GPU A at 90°C ({x})"
    );
}

#[test]
fn restored_alerts_are_not_resent() {
    let mut e = engine();
    e.observe(hot("A", true, "86", 0), &no_mute);
    e.observe(hot("A", true, "86", 60), &no_mute);
    let saved = e.take_dirty();
    let mut e2 = engine();
    e2.restore(saved);
    assert!(e2.observe(hot("A", true, "86", 120), &no_mute).is_empty());
}

#[test]
fn ha_payload_tags_and_clears() {
    let mut e = engine();
    e.observe(hot("A", true, "86", 0), &no_mute);
    let n = e.observe(hot("A", true, "86", 60), &no_mute).remove(0);
    let p = HaNotify::payload(&n);
    assert_eq!(p["data"]["tag"], "hot_card/A");
    assert!(p["title"].as_str().unwrap().contains("hot card"));
    let r = e.observe(hot("A", false, "80", 120), &no_mute).remove(0);
    assert_eq!(HaNotify::payload(&r)["message"], "clear_notification");
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_reaches_named_sinks_and_reports_missing_ones() {
    let fake = FakeSink::new("ha");
    let list: Vec<Arc<dyn Sink>> = vec![Arc::new(fake.clone())];
    let mut e = engine();
    e.observe(hot("A", true, "86", 0), &no_mute);
    let mut n = e.observe(hot("A", true, "86", 60), &no_mute).remove(0);
    n.sinks.push("ntfy".into());
    let r = sinks::dispatch(&n, &list).await;
    assert!(r[0].1.is_ok());
    assert!(r[1].1.as_ref().unwrap_err().contains("no sink"));
    assert_eq!(fake.sent().len(), 1);
}

#[test]
fn sinks_from_config_check_their_fields() {
    let mut cfg = indexmap::IndexMap::new();
    cfg.insert("ha".to_string(), serde_json::json!({"url": "http://ha:8123", "service": "mobile_app_x", "token_file": "/run/secrets/ha"}));
    cfg.insert(
        "ntfy".to_string(),
        serde_json::json!({"url": "https://ntfy.sh"}),
    );
    let (s, issues) = sinks::from_config(&cfg);
    assert_eq!(s.len(), 1);
    assert_eq!(issues.len(), 1);
}
