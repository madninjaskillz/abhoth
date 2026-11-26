use std::sync::Arc;

use serde_json::json;

use super::*;

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn board() -> GuardBoard {
    let b = GuardBoard::default();
    b.declare("video", vec![Action::OnDemand], Scope::All, 300);
    b.declare("bench", vec![Action::OnDemand], Scope::All, 600);
    b.declare(
        "music",
        vec![Action::Switch, Action::OnDemand, Action::Ensure],
        Scope::All,
        600,
    );
    b
}

const NOW: f64 = 1_800_000_000.0;

#[test]
fn video_scan_matches_runs_and_lock_waiters() {
    let m = ProcessMatch::video();
    let procs = vec![
        (10, argv(&["/bin/bash", "/srv/videoPipeline/run_daily.sh"])),
        // cron job 55: waits for AI News' run lock; it is part of the day's run
        (
            11,
            argv(&[
                "bash",
                "run_daily.sh",
                "--channel",
                "game-news",
                "--wait-lock",
            ]),
        ),
        (12, argv(&["sh", "-c", "run_daily.sh"])), // script in argv[:3] under sh
        (13, argv(&["python3", "run_daily.sh"])),  // not a shell: no
        (14, argv(&["vim", "run_daily.sh"])),
        (15, argv(&["bash", "a", "b", "run_daily.sh"])), // past argv[:3]: no
        (16, argv(&["bash", "run_daily.sh", "--channel"])), // no value: default label
        (17, vec![]),
    ];
    let runs = video_runs(&procs, &m);
    assert_eq!(
        runs,
        vec![
            (10, "ai-news".to_string()),
            (11, "game-news".to_string()),
            (12, "ai-news".to_string()),
            (16, "ai-news".to_string())
        ]
    );
    assert_eq!(
        video_reading(&procs, &m).as_deref(),
        Some("ai-news, game-news")
    );
    assert_eq!(video_reading(&procs[3..5], &m), None);
    assert_eq!(
        parse_cmdline(b"bash\0run_daily.sh\0--channel\0car-news\0"),
        argv(&["bash", "run_daily.sh", "--channel", "car-news"])
    );
}

#[test]
fn bench_queue_file() {
    assert_eq!(bench_running(None), None);
    assert_eq!(
        bench_running(Some(&json!({"running": false, "current": null}))),
        None
    );
    assert_eq!(
        bench_running(Some(&json!({"running": true}))).as_deref(),
        Some("queue running")
    );
    assert_eq!(
        bench_running(Some(&json!({"current": {"id": "lc2-7"}}))).as_deref(),
        Some("lc2-7")
    );
    assert_eq!(
        bench_running(Some(&json!({"current": "gap-3"}))).as_deref(),
        Some("gap-3")
    );
}

#[test]
fn stale_detectors_mean_wait_for_switches_never_for_ensure() {
    let b = board();
    // nothing reported yet: every detector is stale
    let blocks = b.blocks(Action::OnDemand, &[], NOW);
    assert_eq!(blocks.len(), 3);
    assert!(blocks.iter().all(|x| x.stale));
    assert!(b.blocks(Action::Ensure, &[], NOW).is_empty());
    for id in ["video", "bench", "music"] {
        b.report(id, NOW, None);
    }
    assert!(b.blocks(Action::OnDemand, &[], NOW + 45.0).is_empty());
    // 45 s is fresh, 46 s is stale
    let late = b.blocks(Action::OnDemand, &[], NOW + 46.0);
    assert_eq!(late.len(), 3);
    assert!(late.iter().all(|x| x.stale));
    assert!(b.blocks(Action::Ensure, &[], NOW + 46.0).is_empty());
}

#[test]
fn active_detectors_block_their_actions_only() {
    let b = board();
    for id in ["video", "bench", "music"] {
        b.report(id, NOW, None);
    }
    b.report(
        "video",
        NOW + 1.0,
        Some("a video pipeline is running (ai-news)".into()),
    );
    let od = b.blocks(Action::OnDemand, &[], NOW + 2.0);
    assert_eq!(od.len(), 1);
    assert_eq!(od[0].source, "video");
    assert_eq!(od[0].retry_seconds, 300);
    // video blocks on-demand, not admin switches (layoutctl: the video guard is on-demand's)
    assert!(b.blocks(Action::Switch, &[], NOW + 2.0).is_empty());
    // an older reading never replaces a newer one
    b.report("video", NOW, None);
    assert_eq!(b.blocks(Action::OnDemand, &[], NOW + 2.0).len(), 1);
    b.report("music", NOW + 3.0, Some("music mode is on".into()));
    assert_eq!(b.blocks(Action::Switch, &[], NOW + 4.0).len(), 1);
    assert_eq!(b.blocks(Action::Ensure, &[], NOW + 4.0).len(), 1);
}

#[test]
fn calendar_entries_and_card_scope() {
    let b = GuardBoard::default();
    let cal = Arc::new(Calendar::default());
    cal.entries.lock().unwrap().push(CalendarEntry {
        id: "comfy-night".into(),
        reason: "card B reserved for ComfyUI".into(),
        blocks: vec![Action::Switch],
        scope: Scope::Cards(vec!["B".into()]),
        when: When::Once {
            from: NOW,
            until: NOW + 3600.0,
        },
    });
    b.add_guard(cal.clone());
    let a = vec!["A".to_string()];
    let bb = vec!["B".to_string()];
    assert!(b.blocks(Action::Switch, &a, NOW + 10.0).is_empty());
    let x = b.blocks(Action::Switch, &bb, NOW + 10.0);
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].retry_seconds, 3590);
    // a Switch reservation also blocks on-demand
    assert_eq!(b.blocks(Action::OnDemand, &bb, NOW + 10.0).len(), 1);
    assert!(b.blocks(Action::Switch, &bb, NOW + 3600.0).is_empty());
    assert!(b.blocks(Action::Switch, &bb, NOW - 1.0).is_empty());
    assert_eq!(b.active(NOW + 1.0).len(), 1);
}

#[test]
fn quiet_window_in_london_time_across_dst() {
    let q = CalendarEntry::quiet_window();
    // 2026-07-01 06:50 BST = 05:50 UTC
    let summer = chrono::DateTime::parse_from_rfc3339("2026-07-01T05:50:00Z")
        .unwrap()
        .timestamp() as f64;
    assert_eq!(q.on_until(summer), Some(summer + 40.0 * 60.0));
    assert_eq!(q.on_until(summer + 3600.0), None);
    // 2026-12-01 06:50 GMT = 06:50 UTC
    let winter = chrono::DateTime::parse_from_rfc3339("2026-12-01T06:50:00Z")
        .unwrap()
        .timestamp() as f64;
    assert!(q.on_until(winter).is_some());
    assert!(q.on_until(winter - 3600.0).is_none());
    // the day after the October change: 06:45 GMT is in
    let after = chrono::DateTime::parse_from_rfc3339("2026-10-26T06:45:00Z")
        .unwrap()
        .timestamp() as f64;
    assert!(q.on_until(after).is_some());
}

#[test]
fn guard_leases_block_switches() {
    let g = crate::leases::GuardLease {
        key: "bench-1".into(),
        reason: "bench".into(),
        owner: "bench".into(),
        since: NOW,
        renewed: NOW,
        principal: None,
    };
    let r = lease_reservations(&[g]);
    assert!(r[0].blocks_action(Action::Switch));
    assert!(r[0].blocks_action(Action::OnDemand));
    assert!(!r[0].blocks_action(Action::Ensure));
}
