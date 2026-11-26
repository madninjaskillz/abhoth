use chrono::NaiveDate;
use chrono_tz::Europe::London;
use serde_json::json;

use crate::keys::{self, KeyAction, KeyDecision, KeyRecord};
use crate::now::BlockAction;
use crate::recur::{Recurrence, Rrule, to_local, to_utc};
use crate::*;

/// Epoch of a London wall-clock time.
fn ldn(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
    to_utc(
        London,
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap(),
    )
}

fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, mi, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

fn cal(toml_text: &str) -> (Calendar, Vec<Issue>) {
    let cfg = CalendarConfig::from_toml(toml_text).expect("toml");
    Calendar::compile(&cfg, "Europe/London", &Safety::default())
}

fn cal_ok(toml_text: &str) -> Calendar {
    let (c, issues) = cal(toml_text);
    assert!(issues.iter().all(|i| !i.is_error()), "{issues:?}");
    c
}

fn starts(c: &Calendar, entry: &str, from: i64, to: i64) -> Vec<i64> {
    c.occurrences(from, to, &Live::default(), &Overrides::default(), from)
        .into_iter()
        .filter(|o| o.entry == entry)
        .map(|o| o.start)
        .collect()
}

// ── recurrence and DST ─────────────────────────────────────────────────────────────────────────

const PIPELINES: &str = r#"
[calendar]
enabled = true

[calendar.entries.ai-news]
title = "AI News"
rrule = "FREQ=DAILY;BYHOUR=7;BYMINUTE=0"
duration = "90m"
lead = "15m"
detect = { process = { argv_any = "run_daily.sh", label_arg = "--channel", label = "ai-news", label_default = "ai-news" }, mode = "both", grace = "10m" }
effects = [ { block_switch = "server:deepthought" }, { block_on_demand = "server:deepthought" }, { reserve_card = "B", on_reserve = "wait" } ]

[calendar.entries.ai-news.key]
scopes = ["chat", "use_reserved"]
routes = ["qwen3.8-27b-coder"]

[calendar.entries.game-news]
title = "Game News"
cron = "0 7 * * *"
after = "ai-news"
duration = "60m"
detect = { process = { argv_any = "run_daily.sh", label_arg = "--channel", label = "game-news" }, grace = "10m" }
effects = [ { block_switch = "server:deepthought" }, { reserve_card = "B" } ]

[calendar.entries.car-news]
title = "Car News"
cron = "0 7 * * 0"
after = "game-news"
duration = "60m"
effects = [ { block_switch = "server:deepthought" } ]
"#;

#[test]
fn daily_seven_stays_seven_across_both_dst_changes() {
    let c = cal_ok(PIPELINES);
    // BST starts Sunday 29 March 2026; ends Sunday 25 October 2026.
    let s = starts(
        &c,
        "ai-news",
        utc(2026, 3, 27, 0, 0),
        utc(2026, 3, 31, 0, 0),
    );
    assert_eq!(
        s,
        vec![
            utc(2026, 3, 27, 7, 0),
            utc(2026, 3, 28, 7, 0),
            utc(2026, 3, 29, 6, 0),
            utc(2026, 3, 30, 6, 0)
        ]
    );
    let s = starts(
        &c,
        "ai-news",
        utc(2026, 10, 24, 0, 0),
        utc(2026, 10, 27, 0, 0),
    );
    assert_eq!(
        s,
        vec![
            utc(2026, 10, 24, 6, 0),
            utc(2026, 10, 25, 7, 0),
            utc(2026, 10, 26, 7, 0)
        ]
    );
    for t in s {
        assert_eq!(to_local(London, t).format("%H:%M").to_string(), "07:00");
    }
}

#[test]
fn gap_and_overlap_times_follow_rfc5545() {
    // 01:30 does not exist on 29 March (01:00 GMT -> 02:00 BST): it is read with the offset
    // from before the gap, i.e. 01:30 UTC = 02:30 BST.
    let gap = NaiveDate::from_ymd_opt(2026, 3, 29)
        .unwrap()
        .and_hms_opt(1, 30, 0)
        .unwrap();
    assert_eq!(to_utc(London, gap), utc(2026, 3, 29, 1, 30));
    // 01:30 happens twice on 25 October: the first (BST) one counts.
    let twice = NaiveDate::from_ymd_opt(2026, 10, 25)
        .unwrap()
        .and_hms_opt(1, 30, 0)
        .unwrap();
    assert_eq!(to_utc(London, twice), utc(2026, 10, 25, 0, 30));
    // A cron job at 01:30 runs once on both days.
    let c = cal_ok(
        r#"[calendar.entries.night]
title = "night"
cron = "30 1 * * *"
"#,
    );
    assert_eq!(
        starts(&c, "night", utc(2026, 3, 29, 0, 0), utc(2026, 3, 29, 23, 0)).len(),
        1
    );
    assert_eq!(
        starts(
            &c,
            "night",
            utc(2026, 10, 25, 0, 0),
            utc(2026, 10, 25, 23, 0)
        ),
        vec![utc(2026, 10, 25, 0, 30)]
    );
}

#[test]
fn time_zones_other_than_the_default_expand_in_their_own_local_time() {
    let c = cal_ok(
        r#"[calendar.entries.ny]
title = "NY standup"
rrule = "FREQ=WEEKLY;BYDAY=MO,WE;BYHOUR=9;BYMINUTE=30"
tz = "America/New_York"
duration = "15m"
"#,
    );
    // 2026-03-09 is the Monday after US DST began (8 March): 09:30 EDT = 13:30 UTC, and the
    // week before 09:30 EST = 14:30 UTC.
    let s = starts(&c, "ny", utc(2026, 3, 2, 0, 0), utc(2026, 3, 12, 0, 0));
    assert_eq!(
        s,
        vec![
            utc(2026, 3, 2, 14, 30),
            utc(2026, 3, 4, 14, 30),
            utc(2026, 3, 9, 13, 30),
            utc(2026, 3, 11, 13, 30)
        ]
    );
}

#[test]
fn rrule_parts_count_until_interval_ordinals_and_exdates() {
    let anchor = NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(20, 0, 0)
        .unwrap();
    let r = |s: &str| Recurrence::Rrule {
        rule: Rrule::parse(s).unwrap(),
        anchor,
    };
    let from = anchor;
    let to = NaiveDate::from_ymd_opt(2027, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    // Last Sunday of each month (the DST days in March and October among them).
    let v = r("FREQ=MONTHLY;BYDAY=-1SU").starts(London, from, to);
    assert_eq!(v.len(), 12);
    assert_eq!(v[2].date(), NaiveDate::from_ymd_opt(2026, 3, 29).unwrap());
    assert_eq!(v[9].date(), NaiveDate::from_ymd_opt(2026, 10, 25).unwrap());
    // First Monday.
    let v = r("FREQ=MONTHLY;BYDAY=1MO;COUNT=3").starts(London, from, to);
    assert_eq!(
        v.iter().map(|d| d.date().to_string()).collect::<Vec<_>>(),
        ["2026-01-05", "2026-02-02", "2026-03-02"]
    );
    // Every other week on Fridays, until a date.
    let v = r("FREQ=WEEKLY;INTERVAL=2;BYDAY=FR;UNTIL=20260215T000000").starts(London, from, to);
    assert_eq!(
        v.iter().map(|d| d.date().to_string()).collect::<Vec<_>>(),
        ["2026-01-02", "2026-01-16", "2026-01-30", "2026-02-13"]
    );
    // COUNT counts from the anchor even when the window starts later.
    let later = NaiveDate::from_ymd_opt(2026, 1, 3)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert_eq!(r("FREQ=DAILY;COUNT=3").starts(London, later, to).len(), 1);
    // Unsupported parts are refused, not ignored.
    assert!(Rrule::parse("FREQ=YEARLY;BYWEEKNO=20").is_err());
    assert!(Rrule::parse("BYHOUR=7").is_err());
    assert!(Rrule::parse("FREQ=DAILY;COUNT=2;UNTIL=20260101").is_err());
    // EXDATE and until in config.
    let c = cal_ok(
        r#"[calendar.entries.x]
title = "x"
rrule = "FREQ=DAILY;BYHOUR=7"
duration = "10m"
effects = [ { block_switch = "all" } ]
exdates = ["2026-12-02T07:00"]
until = "2026-12-04T00:00"
"#,
    );
    let s = starts(&c, "x", ldn(2026, 12, 1, 0, 0), ldn(2026, 12, 10, 0, 0));
    assert_eq!(s, vec![ldn(2026, 12, 1, 7, 0), ldn(2026, 12, 3, 7, 0)]);
}

#[test]
fn cron_follows_vixie_day_rules() {
    let c = cal_ok(
        r#"[calendar.entries.a]
title = "a"
cron = "0 12 1 * 1"

[calendar.entries.b]
title = "b"
cron = "0 12 */10 * *"

[calendar.entries.c]
title = "c"
cron = "15 3 * jan-mar sun"
"#,
    );
    // Day-of-month 1 OR Monday (both restricted).
    let a = starts(&c, "a", ldn(2026, 6, 1, 0, 0), ldn(2026, 6, 16, 0, 0));
    assert_eq!(a.len(), 3, "1 June (a Monday), 8 June, 15 June");
    // `*/10` counts as '*' for the OR rule: days 1, 11, 21, 31.
    assert_eq!(
        starts(&c, "b", ldn(2026, 1, 1, 0, 0), ldn(2026, 2, 1, 0, 0)).len(),
        4
    );
    assert_eq!(
        starts(&c, "c", ldn(2026, 3, 1, 0, 0), ldn(2026, 5, 1, 0, 0)).len(),
        5
    );
}

// ── after, lead, detection ────────────────────────────────────────────────────────────────────

#[test]
fn after_sequences_game_news_behind_ai_news_and_car_news_on_sundays() {
    let c = cal_ok(PIPELINES);
    let day = |d| ldn(2026, 11, d, 0, 0);
    let occ = c.occurrences(
        day(29),
        day(30),
        &Live::default(),
        &Overrides::default(),
        day(29),
    );
    let get = |e: &str| occ.iter().find(|o| o.entry == e).cloned().unwrap();
    let ai = get("ai-news");
    assert_eq!(
        ai.block_start,
        ldn(2026, 11, 29, 6, 45),
        "the lead replaces the quiet window"
    );
    assert_eq!(ai.end, ldn(2026, 11, 29, 8, 30));
    let game = get("game-news");
    assert_eq!(game.start, ai.end);
    let car = get("car-news");
    assert_eq!(car.start, game.end, "29 Nov 2026 is a Sunday");
    // Monday: no car news.
    let occ = c.occurrences(
        day(30),
        day(30) + 86_400,
        &Live::default(),
        &Overrides::default(),
        day(30),
    );
    assert!(occ.iter().all(|o| o.entry != "car-news"));
}

#[test]
fn a_long_run_extends_its_occurrence_and_pushes_the_next_pipeline() {
    let c = cal_ok(PIPELINES);
    let mut live = Live::default();
    // AI News runs from 07:00 until 12:15 (a bad day), scanned every minute.
    let mut t = ldn(2026, 11, 26, 7, 0);
    while t <= ldn(2026, 11, 26, 12, 15) {
        live.report("ai-news", t, true, Some("pid 4121".into()));
        live.report("game-news", t, true, None); // waiting on the run lock
        t += 60;
    }
    let noon = ldn(2026, 11, 26, 12, 0);
    let ov = Overrides::default();
    let st = c.now(noon, &live, &ov);
    let ai = st.active.iter().find(|o| o.entry == "ai-news").unwrap();
    assert!(ai.extended);
    assert_eq!(ai.phase(noon), Phase::Extended);
    assert!(
        st.blocks
            .iter()
            .any(|b| b.entry == "ai-news" && b.action == BlockAction::Switch)
    );
    // AI News holds card B; Game News (waiting, same priority, started later) does not.
    assert_eq!(st.holders["B"].entry, "ai-news");
    assert_eq!(st.holders["B"].contenders, vec!["game-news".to_string()]);
    // It ends at 12:15; the block lasts until 12:25 (grace 10m).
    live.report("ai-news", ldn(2026, 11, 26, 12, 15) + 30, false, None);
    let after = ldn(2026, 11, 26, 12, 20);
    let st = c.now(after, &live, &ov);
    let ai = st.active.iter().find(|o| o.entry == "ai-news").unwrap();
    assert_eq!(ai.block_end, ldn(2026, 11, 26, 12, 25));
    // Game News' planned start moved behind AI News' real end.
    let game = c
        .occurrences(after, after + 1, &live, &ov, after)
        .into_iter()
        .find(|o| o.entry == "game-news")
        .unwrap();
    assert_eq!(game.start, ldn(2026, 11, 26, 12, 15));
    assert_eq!(game.end, ldn(2026, 11, 26, 13, 15));
    // After 12:25, B is Game News'.
    let st = c.now(ldn(2026, 11, 26, 12, 30), &live, &ov);
    assert_eq!(st.holders["B"].entry, "game-news");
}

#[test]
fn a_manual_run_starts_an_unplanned_occurrence_and_stale_detection_keeps_blocking() {
    let c = cal_ok(PIPELINES);
    let mut live = Live::default();
    let t0 = ldn(2026, 11, 26, 15, 0);
    live.report("ai-news", t0, true, None);
    live.report("ai-news", t0 + 60, true, None);
    let st = c.now(t0 + 90, &live, &Overrides::default());
    let o = st
        .active
        .iter()
        .find(|o| o.entry == "ai-news")
        .expect("unplanned occurrence");
    assert!(o.unplanned);
    assert_eq!(o.id, format!("ai-news@run{t0}"));
    // The probe goes silent: still running as far as the calendar knows.
    let later = t0 + 3600;
    let st = c.now(later, &live, &Overrides::default());
    assert!(
        st.active
            .iter()
            .any(|o| o.entry == "ai-news" && o.block_end > later)
    );
    // It reports again: not running. 10 minutes of grace, then free.
    live.report("ai-news", later, false, None);
    assert!(
        c.now(later + 30, &live, &Overrides::default())
            .active
            .iter()
            .any(|o| o.entry == "ai-news")
    );
    let free = c.now(later + 3600, &live, &Overrides::default());
    assert!(free.active.iter().all(|o| o.entry != "ai-news"));
}

#[test]
fn a_planned_occurrence_without_its_signal_ends_on_time_and_skips_are_honoured() {
    let c = cal_ok(PIPELINES);
    let live = Live::default();
    let mut ov = Overrides::default();
    let at = ldn(2026, 11, 27, 8, 40);
    let st = c.now(at, &live, &ov);
    assert!(
        st.active.iter().all(|o| o.entry != "ai-news"),
        "ended 08:30"
    );
    let at = ldn(2026, 11, 27, 7, 10);
    ov.skipped
        .insert(format!("ai-news@{}", ldn(2026, 11, 27, 7, 0)));
    let st = c.now(at, &live, &ov);
    assert!(st.active.iter().all(|o| o.entry != "ai-news"));
    // Ending one early.
    let mut ov = Overrides::default();
    ov.ended.insert(
        format!("ai-news@{}", ldn(2026, 11, 27, 7, 0)),
        ldn(2026, 11, 27, 7, 5),
    );
    assert!(
        c.now(at, &live, &ov)
            .active
            .iter()
            .all(|o| o.entry != "ai-news")
    );
}

// ── overlaps and priority ─────────────────────────────────────────────────────────────────────

const BENCH: &str = r#"
[calendar.entries.bench]
title = "Bench run"
at = "2026-11-29T06:30"
duration = "3h"
effects = [ { block_on_demand = "all" }, { hold_clocks = "B" }, { reserve_card = "B" }, { reserve_slots = "card:B", n = 2 } ]
"#;

#[test]
fn equal_priority_first_started_keeps_the_card_and_the_save_warns() {
    let c = cal_ok(&format!("{PIPELINES}{BENCH}"));
    let st = c.now(
        ldn(2026, 11, 29, 7, 15),
        &Live::default(),
        &Overrides::default(),
    );
    assert_eq!(
        st.holders["B"].entry, "bench",
        "bench began at 06:30, ai-news' lead at 06:45"
    );
    // Blocks combine: the switch block of ai-news and bench's on-demand block.
    assert!(
        st.blocks
            .iter()
            .any(|b| b.entry == "ai-news" && b.action == BlockAction::Switch)
    );
    assert!(
        st.blocks
            .iter()
            .any(|b| b.entry == "bench" && b.action == BlockAction::OnDemand)
    );
    // Only the holder's reservation blocks use of B.
    let uses: Vec<_> = st
        .blocks
        .iter()
        .filter(|b| b.action == BlockAction::Use)
        .collect();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].entry, "bench");
    assert_eq!(st.slot_reserves.len(), 1);
    assert_eq!(st.holds.len(), 1);
    let w = c.overlaps(ldn(2026, 11, 28, 0, 0), ldn(2026, 11, 30, 0, 0));
    let m: Vec<_> = w.iter().map(|o| o.message.clone()).collect();
    assert!(
        m.iter()
            .any(|x| x.starts_with("ai-news and bench both reserve B on Sun 2026-11-29 06:45")),
        "{m:?}"
    );
    assert!(w.iter().all(|o| o.equal_priority));
}

#[test]
fn higher_priority_holds_the_card_whatever_started_first() {
    let c = cal_ok(&format!(
        "{PIPELINES}{}",
        BENCH
            .replace(
                "duration = \"3h\"",
                "duration = \"3h\"\npriority = \"high\""
            )
            .replace("06:30", "07:20")
    ));
    let st = c.now(
        ldn(2026, 11, 29, 7, 30),
        &Live::default(),
        &Overrides::default(),
    );
    assert_eq!(st.holders["B"].entry, "bench");
    assert_eq!(st.holders["B"].contenders, vec!["ai-news".to_string()]);
}

#[test]
fn profiles_resolve_to_the_more_conservative_one_and_mutes_spare_critical() {
    let c = cal_ok(
        r#"[calendar.entries.quiet]
title = "quiet night"
rrule = "FREQ=DAILY;BYHOUR=22"
duration = "8h"
effects = [ { fan_profile = "quiet" }, { mute_alerts = "hot_*" } ]

[calendar.entries.bench]
title = "bench"
at = "2026-11-26T23:00"
duration = "1h"
effects = [ { fan_profile = "performance" } ]
"#,
    );
    let st = c.now(
        ldn(2026, 11, 26, 23, 30),
        &Live::default(),
        &Overrides::default(),
    );
    let duty = |p: &str| match p {
        "quiet" => Some(30),
        "performance" => Some(80),
        _ => None,
    };
    assert_eq!(
        now::conservative(&st.fan_profiles["*"], duty)
            .unwrap()
            .profile,
        "performance"
    );
    // An unknown profile ranks first: nothing unknown can lower cooling.
    let st2 = now::NowState {
        fan_profiles: [(
            "*".into(),
            vec![
                now::ProfileClaim {
                    entry: "x".into(),
                    profile: "mystery".into(),
                    until: 0,
                },
                now::ProfileClaim {
                    entry: "y".into(),
                    profile: "quiet".into(),
                    until: 0,
                },
            ],
        )]
        .into(),
        ..Default::default()
    };
    assert_eq!(
        now::conservative(&st2.fan_profiles["*"], duty)
            .unwrap()
            .profile,
        "mystery"
    );
    assert_eq!(c.muted(&st, "hot_card", false).as_deref(), Some("quiet"));
    assert_eq!(c.muted(&st, "hot_card", true), None);
    assert_eq!(c.muted(&st, "oom_kill", false), None);
}

#[test]
fn validation_refuses_preempt_below_high_and_excluded_cards() {
    let (_, issues) = cal(r#"[calendar.entries.p]
title = "p"
at = "2026-12-01T10:00"
duration = "1h"
effects = [ { reserve_card = "B", on_reserve = "preempt" } ]
"#);
    assert!(
        issues
            .iter()
            .any(|i| i.is_error() && i.message.contains("needs priority high"))
    );
    let (_, issues) = cal(r#"[calendar.entries.p]
title = "p"
at = "2026-12-01T10:00"
duration = "1h"
priority = "high"
effects = [ { reserve_card = "B", on_reserve = "preempt", preempt_after = "10s" } ]
"#);
    assert!(issues.iter().all(|i| !i.is_error()));
    assert!(
        issues
            .iter()
            .any(|i| i.message.contains("cancels other users' streams"))
    );
    let cfg = CalendarConfig::from_toml(
        r#"[calendar.entries.p]
title = "p"
at = "2026-12-01T10:00"
duration = "1h"
effects = [ { reserve_card = "D" } ]
[calendar.entries.p.key]
scopes = ["switch"]
layouts = ["dense-pi-everywhere"]
"#,
    )
    .unwrap();
    let safety = Safety {
        excluded_cards: vec!["D".into()],
        excluded_models: vec!["dense-pi".into()],
        unsafe_layouts: vec!["dense-pi-everywhere".into()],
    };
    let (_, issues) = Calendar::compile(&cfg, "Europe/London", &safety);
    assert_eq!(
        issues.iter().filter(|i| i.is_error()).count(),
        2,
        "{issues:?}"
    );
    // preempt_after has a floor of 30 s.
    let e = Effect::parse(
        &json!({"reserve_card": "B", "on_reserve": "preempt", "preempt_after": "10s"}),
    )
    .unwrap();
    assert!(matches!(
        e,
        Effect::ReserveCard {
            preempt_after: 30,
            ..
        }
    ));
}

#[test]
fn bad_entries_are_reported_and_left_out() {
    let (c, issues) = cal(r#"[calendar.entries.a]
title = "a"
rrule = "FREQ=FORTNIGHTLY"
duration = "1h"
[calendar.entries.b]
title = "b"
cron = "0 7 * *"
[calendar.entries.c]
title = "c"
at = "2026-12-01T10:00"
effects = [ { block_switch = "all" } ]
[calendar.entries.d]
title = "d"
after = "e"
[calendar.entries.e]
title = "e"
after = "d"
[calendar.entries.f]
title = "f"
at = "2026-12-01T10:00"
tz = "Mars/Olympus"
"#);
    assert!(
        c.entries.is_empty(),
        "{:?}",
        c.entries.keys().collect::<Vec<_>>()
    );
    assert!(issues.len() >= 6, "{issues:?}");
}

// ── event keys ────────────────────────────────────────────────────────────────────────────────

fn record(event: &str) -> KeyRecord {
    KeyRecord {
        id: 1,
        event: event.into(),
        prefix: "abe_xxxxxxxx".into(),
        owner: "admin".into(),
        created: 0,
        revoked: None,
        retire_at: None,
        last_used: None,
    }
}

fn decide(c: &Calendar, live: &Live, key: &KeyRecord, action: KeyAction, now: i64) -> KeyDecision {
    let ov = Overrides::default();
    let st = c.now(now, live, &ov);
    keys::authorize(
        c,
        &st,
        live,
        &ov,
        key,
        true,
        &action,
        &Safety {
            excluded_cards: vec!["D".into()],
            ..Default::default()
        },
        now,
    )
}

fn use_b() -> KeyAction {
    KeyAction::UseCard { card: "B".into() }
}

#[test]
fn keys_work_only_inside_their_window_with_grace() {
    let c = cal_ok(PIPELINES);
    let live = Live::default();
    let k = record("ai-news");
    // Lead starts 06:45, grace_before 5m: from 06:40.
    let d = decide(&c, &live, &k, use_b(), ldn(2026, 11, 27, 6, 39));
    match d {
        KeyDecision::Deny {
            status,
            reason,
            next,
        } => {
            assert_eq!(status, 403);
            assert_eq!(
                reason,
                "event key outside its window (next: 2026-11-27 06:40)"
            );
            assert_eq!(next, Some(ldn(2026, 11, 27, 6, 40)));
        }
        x => panic!("{x:?}"),
    }
    assert!(matches!(
        decide(&c, &live, &k, use_b(), ldn(2026, 11, 27, 6, 41)),
        KeyDecision::Allow { .. }
    ));
    // Planned end 08:30, grace_after 15m: until 08:45.
    // (Card B is Game News' from 08:30, so ask for chat here.)
    let chat = || KeyAction::Chat {
        route: "qwen3.8-27b-coder".into(),
        cards: vec![],
        model: None,
    };
    assert!(matches!(
        decide(&c, &live, &k, chat(), ldn(2026, 11, 27, 8, 44)),
        KeyDecision::Allow { .. }
    ));
    assert!(matches!(
        decide(&c, &live, &k, chat(), ldn(2026, 11, 27, 8, 46)),
        KeyDecision::Deny { status: 403, .. }
    ));
    assert!(matches!(
        decide(&c, &live, &k, use_b(), ldn(2026, 11, 27, 8, 40)),
        KeyDecision::Deny { status: 409, .. }
    ));
    // An extended run keeps the key valid.
    let mut live = Live::default();
    let mut t = ldn(2026, 11, 27, 7, 0);
    while t <= ldn(2026, 11, 27, 13, 10) {
        live.report("ai-news", t, true, None);
        t += 60;
    }
    assert!(matches!(
        decide(&c, &live, &k, use_b(), ldn(2026, 11, 27, 13, 10)),
        KeyDecision::Allow { .. }
    ));
}

#[test]
fn scopes_are_least_privilege_and_structural_limits_come_first() {
    let c = cal_ok(PIPELINES);
    let live = Live::default();
    let k = record("ai-news");
    let at = ldn(2026, 11, 27, 7, 10);
    let deny_reason = |a: KeyAction| match decide(&c, &live, &k, a, at) {
        KeyDecision::Deny { reason, .. } => reason,
        x => panic!("{x:?}"),
    };
    // Default scopes: chat + use_reserved. Nothing else.
    assert!(matches!(
        decide(
            &c,
            &live,
            &k,
            KeyAction::Chat {
                route: "qwen3.8-27b-coder".into(),
                cards: vec![],
                model: None
            },
            at
        ),
        KeyDecision::Allow { .. }
    ));
    assert!(
        deny_reason(KeyAction::Chat {
            route: "other".into(),
            cards: vec![],
            model: None
        })
        .contains("not one of the event's routes")
    );
    assert!(deny_reason(KeyAction::Lease { card: "B".into() }).contains("lease scope"));
    assert!(
        deny_reason(KeyAction::Switch {
            layout: "coder".into(),
            probe: false
        })
        .contains("switch scope")
    );
    assert!(
        deny_reason(KeyAction::CardState {
            card: "B".into(),
            online: true
        })
        .contains("card_state scope")
    );
    assert!(
        deny_reason(KeyAction::UseCard { card: "A".into() }).contains("not reserved by ai-news")
    );
    // Probes never switch, whoever's key.
    assert!(
        deny_reason(KeyAction::Switch {
            layout: "coder".into(),
            probe: true
        })
        .contains("probe")
    );
    assert!(
        deny_reason(KeyAction::OnDemand {
            route: "r".into(),
            card: "B".into(),
            probe: true
        })
        .contains("probe")
    );
    // Excluded cards are outside every key.
    assert!(deny_reason(KeyAction::UseCard { card: "D".into() }).contains("excluded"));
    // Revoked, disabled owner.
    let mut revoked = k.clone();
    revoked.revoked = Some(1);
    assert!(matches!(
        decide(&c, &live, &revoked, use_b(), at),
        KeyDecision::Deny { status: 401, .. }
    ));
    let ov = Overrides::default();
    let st = c.now(at, &live, &ov);
    assert!(matches!(
        keys::authorize(
            &c,
            &st,
            &live,
            &ov,
            &k,
            false,
            &use_b(),
            &Safety::default(),
            at
        ),
        KeyDecision::Deny { status: 403, .. }
    ));
    // Usage is charged to the owner.
    match decide(&c, &live, &k, use_b(), at) {
        KeyDecision::Allow {
            charge_to, lifts, ..
        } => {
            assert_eq!(charge_to, "admin");
            assert_eq!(lifts, vec!["ai-news".to_string()]);
        }
        x => panic!("{x:?}"),
    }
}

#[test]
fn switch_scope_is_limited_to_its_layouts_and_holds_are_clipped() {
    let c = cal_ok(
        r#"[calendar.entries.bench]
title = "bench"
at = "2026-12-01T10:00"
duration = "1h"
effects = [ { block_switch = "all" }, { reserve_card = "B" } ]
[calendar.entries.bench.key]
scopes = ["switch", "hold_clocks", "use_reserved"]
layouts = ["bench-kv"]
grace_after = "0m"
"#,
    );
    let live = Live::default();
    let k = record("bench");
    let at = ldn(2026, 12, 1, 10, 50);
    assert!(matches!(
        decide(
            &c,
            &live,
            &k,
            KeyAction::Switch {
                layout: "bench-kv".into(),
                probe: false
            },
            at
        ),
        KeyDecision::Allow { .. }
    ));
    assert!(matches!(
        decide(
            &c,
            &live,
            &k,
            KeyAction::Switch {
                layout: "coder".into(),
                probe: false
            },
            at
        ),
        KeyDecision::Deny { .. }
    ));
    match decide(
        &c,
        &live,
        &k,
        KeyAction::HoldClocks {
            card: "B".into(),
            ttl: 3600,
        },
        at,
    ) {
        KeyDecision::Allow {
            ttl,
            hostd_validates,
            ..
        } => {
            assert_eq!(ttl, Some(600), "clipped to the occurrence's end");
            assert!(hostd_validates);
        }
        x => panic!("{x:?}"),
    }
}

#[test]
fn a_key_gets_409_on_a_card_another_event_holds_unless_it_overrides_lower() {
    let text = format!("{PIPELINES}{BENCH}");
    let c = cal_ok(&text);
    let live = Live::default();
    let at = ldn(2026, 11, 29, 7, 15);
    match decide(&c, &live, &record("ai-news"), use_b(), at) {
        KeyDecision::Deny { status, reason, .. } => {
            assert_eq!(status, 409);
            assert!(
                reason.starts_with("card B reserved by bench until ~09:30"),
                "{reason}"
            );
        }
        x => panic!("{x:?}"),
    }
    // ai-news at high priority with overrides_lower: it holds B anyway (8.4) and lifts bench.
    let c = cal_ok(
        &text
            .replace(
                "routes = [\"qwen3.8-27b-coder\"]",
                "routes = [\"qwen3.8-27b-coder\"]\noverrides_lower = true",
            )
            .replace("lead = \"15m\"", "lead = \"15m\"\npriority = \"high\""),
    );
    match decide(&c, &live, &record("ai-news"), use_b(), at) {
        KeyDecision::Allow { lifts, .. } => assert_eq!(
            lifts,
            vec!["ai-news".to_string(), "bench".to_string()],
            "{:#?}",
            c.now(at, &live, &Overrides::default()).active
        ),
        x => panic!("{x:?}"),
    }
    // Never equal or higher: bench (normal) cannot lift ai-news (high).
    let st = c.now(at, &live, &Overrides::default());
    assert_eq!(c.liftable("bench", &st), vec!["bench".to_string()]);
}

#[test]
fn keys_are_random_hashed_and_rotation_keeps_the_old_one_until_the_run_ends() {
    let a = keys::generate().unwrap();
    let b = keys::generate().unwrap();
    assert!(keys::looks_like_event_key(&a.secret));
    assert_ne!(a.secret, b.secret);
    assert_eq!(a.hash, keys::hash(&a.secret));
    assert_eq!(a.hash.len(), 64);
    assert!(a.secret.starts_with(&a.prefix));
    assert!(!a.hash.contains(&a.secret[4..]));
    let c = cal_ok(PIPELINES);
    // Rotated at 07:10 during a run: the old key lasts to the end of this occurrence + grace.
    let at = ldn(2026, 11, 27, 7, 10);
    let t = keys::retire_time(&c, "ai-news", at, &Live::default(), &Overrides::default());
    assert_eq!(t, ldn(2026, 11, 27, 8, 45));
    // Rotated at noon: tomorrow's run (ends 08:45 with grace) comes before 24 h are up.
    let at = ldn(2026, 11, 27, 12, 0);
    assert_eq!(
        keys::retire_time(&c, "ai-news", at, &Live::default(), &Overrides::default()),
        ldn(2026, 11, 28, 8, 45)
    );
    // An event that next happens in a week: 24 h.
    let weekly = cal_ok(&PIPELINES.replace("FREQ=DAILY;BYHOUR=7", "FREQ=WEEKLY;BYDAY=SU;BYHOUR=7"));
    let fri = ldn(2026, 11, 27, 12, 0);
    assert_eq!(
        keys::retire_time(
            &weekly,
            "ai-news",
            fri,
            &Live::default(),
            &Overrides::default()
        ),
        fri + 86_400
    );
    let mut k = record("ai-news");
    k.retire_at = Some(at + 10);
    assert!(k.live(at) && !k.live(at + 10));
}

// ── import ────────────────────────────────────────────────────────────────────────────────────

#[test]
fn truenas_cron_import_makes_the_pipeline_entries_and_info_for_the_rest() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/truenas-cronjobs.json"
    ))
    .unwrap();
    let jobs: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let imp = cron_import::import(&jobs, "deepthought", "admin");
    assert_eq!(imp.len(), 12);
    let p: Vec<_> = imp
        .iter()
        .filter(|i| i.kind == "pipeline")
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(p, ["ai-news", "game-news", "car-news"]);
    let by = |id: &str| imp.iter().find(|i| i.id == id).unwrap();
    assert_eq!(by("game-news").entry.after.as_deref(), Some("ai-news"));
    assert_eq!(by("car-news").entry.after.as_deref(), Some("game-news"));
    assert_eq!(by("car-news").entry.cron.as_deref(), Some("0 7 * * 0"));
    assert_eq!(by("ai-news").entry.lead.as_deref(), Some("15m"));
    // No command text is copied (it can hold secrets).
    let text = emit::entries("imported", cron_import::as_entries(&imp).iter());
    assert!(
        !text.contains("2>&1") && !text.contains("--wait-lock") && !text.contains(".venv"),
        "{text}"
    );
    // It compiles, and round-trips through calendar.toml.
    let c = cal_ok(&format!("[calendar]\nenabled = true\n\n{text}"));
    assert_eq!(c.entries.len(), 12);
    let disabled = imp.iter().filter(|i| !i.entry.enabled).count();
    assert_eq!(disabled, 3);
    // Every-minute collectors and the 10-minute retry are listed, not expanded.
    assert!(c.entries.values().filter(|e| e.frequent_info()).count() >= 5);
    let occ = c.occurrences(
        ldn(2026, 11, 29, 0, 0),
        ldn(2026, 11, 30, 0, 0),
        &Live::default(),
        &Overrides::default(),
        0,
    );
    assert!(occ.len() < 20, "{}", occ.len());
    // The Sunday chain: AI News 07:00 (lead 06:45), Game News at 08:30, Car News at 09:30.
    let s = |e: &str| occ.iter().find(|o| o.entry == e).unwrap().start;
    assert_eq!(s("game-news"), ldn(2026, 11, 29, 8, 30));
    assert_eq!(s("car-news"), ldn(2026, 11, 29, 9, 30));
}

#[test]
fn ics_import_keeps_rrules_exdates_and_zones_and_skips_cancelled() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc-1@example\r\nSUMMARY:Maintenance\\, kernel update\r\nDTSTART;TZID=Europe/London:20261205T200000\r\nDTEND;TZID=Europe/London:20261205T220000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:abc-2@example\r\nSUMMARY:Weekly\r\n  review\r\nDTSTART:20261201T090000Z\r\nDURATION:PT30M\r\nRRULE:FREQ=WEEKLY;BYDAY=TU\r\nEXDATE:20261208T090000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:abc-3@example\r\nSUMMARY:Gone\r\nSTATUS:CANCELLED\r\nDTSTART:20261201T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let (ev, issues) = ics::parse(text);
    assert!(issues.is_empty());
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].summary, "Maintenance, kernel update");
    assert_eq!(ev[0].duration, 7200);
    assert_eq!(ev[1].summary, "Weekly review");
    let mut issues = vec![];
    let entries = ics::to_entries(
        &ev,
        "ics",
        "ics:test",
        &[json!({"block_switch": "all"})],
        Priority::High,
        London,
        &mut issues,
    );
    let cfg = CalendarConfig {
        entries,
        ..Default::default()
    };
    let (c, issues) = Calendar::compile(&cfg, "Europe/London", &Safety::default());
    assert!(issues.iter().all(|i| !i.is_error()), "{issues:?}");
    let weekly = c
        .entries
        .values()
        .find(|e| e.cfg.title == "Weekly review")
        .unwrap();
    let s = starts(
        &c,
        &weekly.id,
        utc(2026, 12, 1, 0, 0),
        utc(2026, 12, 23, 0, 0),
    );
    assert_eq!(
        s,
        vec![
            utc(2026, 12, 1, 9, 0),
            utc(2026, 12, 15, 9, 0),
            utc(2026, 12, 22, 9, 0)
        ]
    );
    let m = c
        .entries
        .values()
        .find(|e| e.cfg.title.starts_with("Maintenance"))
        .unwrap();
    assert_eq!(
        starts(&c, &m.id, utc(2026, 12, 5, 0, 0), utc(2026, 12, 6, 0, 0)),
        vec![utc(2026, 12, 5, 20, 0)]
    );
    assert_eq!(m.priority(), Priority::High);
}

#[test]
fn emitted_toml_round_trips() {
    let cfg = CalendarConfig::from_toml(PIPELINES).unwrap();
    let text = emit::entries("x", cfg.entries.iter());
    let back = CalendarConfig::from_toml(&text).unwrap();
    assert_eq!(back.entries, cfg.entries);
}
