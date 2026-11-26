use std::sync::Arc;

use orca_calendar::keys::{KeyAction, KeyDecision};
use orca_calendar::now::BlockAction;
use orca_calendar::{EntryConfig, Safety};
use serde_json::{Value, json};

use super::*;
use crate::guards::{Action, Caller, GuardBoard};

const FILE: &str = r#"# The pipelines (hand-written comment: must survive UI edits)
[calendar]
enabled = true

[calendar.entries.ai-news]
title = "AI News"
rrule = "FREQ=DAILY;BYHOUR=7;BYMINUTE=0"
duration = "90m"
lead = "15m"
detect = { process = { argv_any = "run_daily.sh", label_arg = "--channel", label = "ai-news", label_default = "ai-news" }, mode = "both", grace = "10m" }
effects = [ { block_switch = "server:gpu" }, { block_on_demand = "server:gpu" } ]
"#;

fn svc(text: &str) -> (tempfile::TempDir, Arc<CalendarService>) {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("calendar.toml");
    std::fs::write(&file, text).unwrap();
    let store = Arc::new(Store::memory().unwrap());
    let s = CalendarService::open(
        "gpu",
        "Europe/London",
        &file,
        d.path(),
        store,
        Safety {
            excluded_cards: vec!["C".into()],
            ..Default::default()
        },
    )
    .unwrap();
    (d, s)
}

fn local_now(s: &CalendarService, plus: i64) -> String {
    orca_calendar::recur::to_local(s.tz(), now_epoch() + plus)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

fn entry(v: Value) -> EntryConfig {
    serde_json::from_value(v).unwrap()
}

#[test]
fn a_manual_pipeline_run_blocks_switches_through_the_guard_board_with_its_reason() {
    let (_d, s) = svc(FILE);
    let board = GuardBoard::default();
    board.add_guard(s.clone());
    let t = now_epoch();
    let anyone = Caller::default();
    // A run outside the planned window (detected by the /proc scan) starts an unplanned
    // occurrence that blocks switches and on-demand.
    let procs = vec![(
        4121u32,
        vec![
            "/bin/bash".to_string(),
            "/srv/videoPipeline/run_daily.sh".to_string(),
        ],
    )];
    let planned_now = s.now_state(t).active.iter().any(|o| o.entry == "ai-news");
    s.report_processes(&procs, t);
    let b = board.check(Action::Switch, &[], &anyone, None, t as f64 + 1.0);
    assert!(b.iter().any(|x| x.source == "calendar:ai-news"), "{b:?}");
    assert!(b.iter().all(|x| x.retry_seconds >= 60));
    if !planned_now {
        assert!(s.now_state(t + 1).active.iter().any(|o| o.unplanned));
    }
    // Another label is not this entry's run.
    let (_d2, s2) = svc(FILE);
    s2.report_processes(
        &[(
            1,
            vec![
                "bash".into(),
                "run_daily.sh".into(),
                "--channel".into(),
                "car-news".into(),
            ],
        )],
        t,
    );
    assert!(s2.now_state(t + 1).active.iter().all(|o| !o.unplanned));
    // `calendar check`.
    assert!(!s.check(BlockAction::Switch, Some("B"), t + 1).is_empty());
    assert!(!s.check(BlockAction::OnDemand, None, t + 1).is_empty());
}

#[test]
fn edits_keep_comments_back_up_audit_and_refuse_stale_revs_and_errors() {
    let (d, s) = svc(FILE);
    let rev = s.rev();
    let e = entry(
        json!({"title": "Maintenance", "at": "2026-12-05T20:00", "duration": "2h", "priority": "critical",
        "effects": [{"block_switch": "all"}, {"block_ensure": "card:B"}]}),
    );
    let r = s
        .save_entry("maint", Some(&e), Some(&rev), "admin", "kernel update")
        .unwrap();
    let text = std::fs::read_to_string(d.path().join("calendar.toml")).unwrap();
    assert!(
        text.starts_with("# The pipelines (hand-written comment"),
        "{text}"
    );
    assert!(text.contains("[calendar.entries.maint]"));
    assert!(text.contains("[calendar.entries.ai-news]"));
    assert_ne!(r["rev"], rev);
    assert_eq!(
        std::fs::read_dir(d.path().join("backups")).unwrap().count(),
        1
    );
    assert!(
        s.store
            .audit_rows(5)
            .unwrap()
            .iter()
            .any(|a| a.action == "calendar.save" && a.reason == "kernel update")
    );
    // The old rev is refused.
    assert_eq!(
        s.save_entry("maint", Some(&e), Some(&rev), "admin", "")
            .unwrap_err()
            .0,
        412
    );
    // Errors refuse the save and leave the file alone.
    let bad = entry(
        json!({"title": "x", "at": "2026-12-05T20:00", "effects": [{"reserve_card": "B", "on_reserve": "preempt"}], "duration": "1h"}),
    );
    let (st, msg) = s
        .save_entry("x", Some(&bad), None, "admin", "")
        .unwrap_err();
    assert_eq!(st, 422);
    assert!(msg.contains("priority high"), "{msg}");
    let excluded = entry(
        json!({"title": "x", "at": "2026-12-05T20:00", "effects": [{"reserve_card": "C"}], "duration": "1h"}),
    );
    assert_eq!(
        s.save_entry("x", Some(&excluded), None, "admin", "")
            .unwrap_err()
            .0,
        422
    );
    // Delete.
    s.save_entry("maint", None, None, "admin", "").unwrap();
    assert!(
        !std::fs::read_to_string(d.path().join("calendar.toml"))
            .unwrap()
            .contains("maint")
    );
}

#[test]
fn event_keys_are_shown_once_hashed_audited_and_only_good_inside_their_event() {
    let (_d, s) = svc(FILE);
    // A one-off running now, reserving B, with a key that may lease and switch to one layout.
    let e = entry(
        json!({"title": "pipeline test", "at": local_now(&s, -60), "duration": "30m",
        "effects": [{"block_switch": "all"}, {"reserve_card": "B"}],
        "key": {"scopes": ["chat", "use_reserved", "switch"], "layouts": ["coder-b"]}}),
    );
    s.save_entry("test-run", Some(&e), None, "admin", "")
        .unwrap();
    let (rec, secret) = s.issue_key("test-run", "admin").unwrap();
    assert!(secret.starts_with("abe_"));
    assert!(!rec.prefix.is_empty() && secret.starts_with(&rec.prefix));
    // Only the hash is stored.
    let raw: Vec<String> = {
        let c = s.store.conn();
        let mut st = c.prepare("SELECT key_hash FROM event_keys").unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    assert!(raw.iter().all(|h| !h.contains(&secret[4..])));
    // Use of B is allowed; switching to another layout is not; a probe never switches.
    assert!(matches!(
        s.authorize(&secret, &KeyAction::UseCard { card: "B".into() }, true),
        KeyDecision::Allow { .. }
    ));
    assert!(matches!(
        s.authorize(
            &secret,
            &KeyAction::Switch {
                layout: "flash".into(),
                probe: false
            },
            true
        ),
        KeyDecision::Deny { status: 403, .. }
    ));
    assert!(matches!(
        s.authorize(
            &secret,
            &KeyAction::Switch {
                layout: "coder-b".into(),
                probe: true
            },
            true
        ),
        KeyDecision::Deny { status: 403, .. }
    ));
    assert!(matches!(
        s.authorize(
            &secret,
            &KeyAction::Switch {
                layout: "coder-b".into(),
                probe: false
            },
            true
        ),
        KeyDecision::Allow { .. }
    ));
    assert!(matches!(
        s.authorize(
            "abe_notarealkeynotarealkeynotarealk",
            &KeyAction::UseCard { card: "B".into() },
            true
        ),
        KeyDecision::Deny { status: 401, .. }
    ));
    // Every use is in the key's audit; control actions also in the main audit.
    let uses = s.key_uses("test-run", 10);
    assert_eq!(uses.len(), 4);
    assert!(
        uses.iter()
            .any(|u| u.decision == "deny" && u.action.contains("(probe)"))
    );
    assert!(
        s.store
            .audit_rows(20)
            .unwrap()
            .iter()
            .any(|a| a.action == "calendar.key.switch")
    );
    // The guard board lets the key's caller through this event's blocks and nobody else.
    let board = GuardBoard::default();
    board.add_guard(s.clone());
    let t = now_epoch() as f64;
    let caller = s.caller(&secret).expect("inside the window");
    assert!(
        board
            .check(Action::Use, &["B".into()], &caller, None, t)
            .is_empty()
    );
    assert!(
        !board
            .check(Action::Use, &["B".into()], &Caller::default(), None, t)
            .is_empty()
    );
    assert!(
        board
            .check(Action::Switch, &[], &caller, Some("coder-b"), t)
            .is_empty()
    );
    assert!(
        !board
            .check(Action::Switch, &[], &caller, Some("flash"), t)
            .is_empty()
    );
    // Rotate: a new key; the old one retires at the end of this occurrence (+ grace).
    let (_, new_secret, retire) = s.rotate_key("test-run", "admin").unwrap();
    assert_ne!(new_secret, secret);
    assert!(retire <= now_epoch() + 86_400 && retire > now_epoch());
    assert!(matches!(
        s.authorize(&secret, &KeyAction::UseCard { card: "B".into() }, true),
        KeyDecision::Allow { .. }
    ));
    // Revoke.
    s.revoke_key(rec.id, "admin").unwrap();
    assert!(matches!(
        s.authorize(&secret, &KeyAction::UseCard { card: "B".into() }, true),
        KeyDecision::Deny { status: 401, .. }
    ));
    assert!(s.caller(&secret).is_none());
}

#[test]
fn a_key_outside_its_window_is_refused_with_the_next_time_and_counted() {
    let (_d, s) = svc(FILE);
    let e = entry(
        json!({"title": "later", "at": local_now(&s, 6 * 3600), "duration": "30m", "effects": [{"reserve_card": "B"}]}),
    );
    s.save_entry("later", Some(&e), None, "admin", "").unwrap();
    let (rec, secret) = s.issue_key("later", "admin").unwrap();
    for _ in 0..4 {
        match s.authorize(&secret, &KeyAction::UseCard { card: "B".into() }, true) {
            KeyDecision::Deny {
                status,
                reason,
                next,
            } => {
                assert_eq!(status, 403);
                assert!(
                    reason.starts_with("event key outside its window (next: "),
                    "{reason}"
                );
                assert!(next.is_some());
            }
            x => panic!("{x:?}"),
        }
    }
    assert_eq!(s.outside_window_recent(rec.id), 4);
    assert!(s.caller(&secret).is_none());
}

#[test]
fn the_bench_queue_gets_a_protected_one_off_with_its_own_key() {
    let (_d, s) = svc(FILE);
    let (id, key) = s
        .bench_begin(
            "kv-ab night 1",
            &["B".into()],
            &["bench-kv".into()],
            1800,
            "bench-queue",
        )
        .unwrap();
    assert_eq!(id, "bench-kv-ab-night-1");
    let st = s.now_state(now_epoch());
    assert_eq!(st.holders["B"].entry, id);
    assert!(st.holds.iter().any(|(_, e, _)| e == &id));
    assert!(
        st.blocks
            .iter()
            .any(|b| b.entry == id && b.action == BlockAction::OnDemand)
    );
    assert!(matches!(
        s.authorize(&key, &KeyAction::Lease { card: "B".into() }, true),
        KeyDecision::Allow { .. }
    ));
    // It shows in the calendar as a runtime entry and cannot be edited as a file entry.
    assert!(
        s.entries()
            .iter()
            .any(|e| e.id == id && e.origin == "bench")
    );
    assert_eq!(
        s.save_entry(&id, None, None, "admin", "").unwrap_err().0,
        409
    );
    s.bench_end(&id, "bench-queue").unwrap();
    assert!(!s.now_state(now_epoch()).holders.contains_key("B"));
    assert!(matches!(
        s.authorize(&key, &KeyAction::Lease { card: "B".into() }, true),
        KeyDecision::Deny { .. }
    ));
}

#[test]
fn the_cron_import_writes_the_pipelines_once() {
    let (d, s) = svc("");
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../orca-calendar/tests/fixtures/truenas-cronjobs.json"
    ))
    .unwrap();
    let jobs: Value = serde_json::from_str(&raw).unwrap();
    let dry = s.import_cron(&jobs, "admin", false).unwrap();
    assert_eq!(dry["written"], 0);
    assert_eq!(dry["found"].as_array().unwrap().len(), 12);
    let r = s.import_cron(&jobs, "admin", true).unwrap();
    assert_eq!(r["written"], 12);
    let again = s.import_cron(&jobs, "admin", true).unwrap();
    assert_eq!(again["written"], 0);
    let text = std::fs::read_to_string(d.path().join("calendar.toml")).unwrap();
    assert!(text.contains("[calendar.entries.game-news]") && text.contains("after = \"ai-news\""));
    assert!(s.issues().iter().all(|i| !i.is_error()), "{:?}", s.issues());
}

#[test]
fn the_api_lists_previews_and_acts_on_occurrences() {
    let (_d, s) = svc(FILE);
    let (st, v) = api::handle(&s, "GET", &["calendar"], &[], &Value::Null, "admin");
    assert_eq!(st, 200);
    assert_eq!(v["entries"][0]["id"], "ai-news");
    assert_eq!(v["entries"][0]["schedule"], "daily 07:00");
    let body = json!({"id": "maint", "entry": {"title": "m", "rrule": "FREQ=WEEKLY;BYDAY=SU;BYHOUR=7", "duration": "1h", "effects": [{"reserve_card": "B"}]}});
    let (st, v) = api::handle(&s, "POST", &["calendar", "preview"], &[], &body, "admin");
    assert_eq!(st, 200);
    assert_eq!(v["next"].as_array().unwrap().len(), 10);
    let t = now_epoch();
    let (st, v) = api::handle(
        &s,
        "GET",
        &["calendar", "occurrences"],
        &[
            ("from".into(), t.to_string()),
            ("to".into(), (t + 3 * 86_400).to_string()),
        ],
        &Value::Null,
        "admin",
    );
    assert_eq!(st, 200);
    let occ = v["occurrences"][0]["id"].as_str().unwrap().to_string();
    let enc = occ.replace('@', "%40");
    let (st, _) = api::handle(
        &s,
        "POST",
        &["calendar", "occurrences", &enc, "skip"],
        &[],
        &json!({"reason": "holiday"}),
        "admin",
    );
    assert_eq!(st, 200);
    let (_, v) = api::handle(
        &s,
        "GET",
        &["calendar", "occurrences"],
        &[
            ("from".into(), t.to_string()),
            ("to".into(), (t + 3 * 86_400).to_string()),
        ],
        &Value::Null,
        "admin",
    );
    assert_eq!(v["occurrences"][0]["skipped"], true);
    let (st, v) = api::handle(
        &s,
        "GET",
        &["calendar", "check"],
        &[("effect".into(), "nope".into())],
        &Value::Null,
        "admin",
    );
    assert_eq!(st, 400, "{v}");
    let (st, _) = api::handle(&s, "GET", &["calendar", "nope"], &[], &Value::Null, "admin");
    assert_eq!(st, 404);
}

#[test]
fn edit_entry_replaces_only_its_table() {
    let text = "# top\n[calendar]\nenabled = true\n\n# about a\n[calendar.entries.a]\ntitle = \"a\"\nat = \"2026-12-01T10:00\"\n\n[calendar.entries.b]\ntitle = \"b\"\nat = \"2026-12-01T10:00\"\n";
    let e = entry(json!({"title": "a2", "at": "2026-12-02T10:00"}));
    let out = edit_entry(text, "a", Some(&e)).unwrap();
    assert!(
        out.contains("# about a\n[calendar.entries.a]\ntitle = \"a2\""),
        "{out}"
    );
    assert!(out.contains("[calendar.entries.b]\ntitle = \"b\""));
    let out = edit_entry("", "new", Some(&e)).unwrap();
    assert_eq!(
        out.trim_start(),
        "[calendar.entries.new]\ntitle = \"a2\"\nat = \"2026-12-02T10:00\"\n"
    );
}
