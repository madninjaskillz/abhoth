//! State-machine tests against the recording fake driver: idle → wake → serving → idle, engine
//! restarts re-asserting the policy, drift detection, caps, holds and the failure paths.

use std::time::Duration;

use orca_gpuctl::controller::{AlertKind, AssertReason, ClockEvent, Controller, Mode, Phase};
use orca_gpuctl::limits::Limits;
use orca_gpuctl::nvml::{FakeNvml, GpuCommand, GpuControl, reasons};
use orca_gpuctl::{CardPowerConfig, WakeSource};

const A: &str = "GPU-aaaa";
const B: &str = "GPU-bbbb";
const C: &str = "GPU-cccc";

fn t(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn cfg(s: &str) -> CardPowerConfig {
    toml::from_str(s).expect("config")
}

/// Card A as decided: unlocked boost (no pin), PL 350, no cap offered.
fn card_a() -> CardPowerConfig {
    cfg(r#"
        persistence = true
        power_limit_w = 350
        serving = "unlocked"
        expect_max_sm_mhz = 1597
        idle = { after = "60s", lock_mhz = 135, wake = ["router", "sniff", "util"] }
        reassert = ["boot", "engine_ready", "every:5m"]
        on_mismatch = "fix"
        allow_cap = false
    "#)
}

fn card_b() -> CardPowerConfig {
    cfg(r#"
        persistence = true
        power_limit_w = 250
        app_clocks = [877, 1380]
        serving = "unlocked"
        idle = { after = "60s", lock_mhz = 135 }
    "#)
}

fn card_c() -> CardPowerConfig {
    cfg(r#"
        manage_power = true
        idle = { after = "60s", lock_mhz = 135, wake = ["sniff", "util"], sniff_ports = [8188] }
    "#)
}

fn setup() -> (Controller, FakeNvml) {
    let mut fake = FakeNvml::default()
        .with_card(A, 1597)
        .with_card(B, 1380)
        .with_card(C, 1530);
    for (u, pl) in [(A, 350.0), (B, 250.0), (C, 300.0)] {
        fake.card(u).reading.power_limit_w = Some(pl);
    }
    fake.card(B).reading.app_mem_mhz = Some(877);
    fake.card(B).reading.app_sm_mhz = Some(1380);
    let ctl = Controller::new(
        [
            ("A".to_string(), A.to_string(), card_a()),
            ("B".to_string(), B.to_string(), card_b()),
            ("C".to_string(), C.to_string(), card_c()),
        ],
        Mode::On,
        t(0),
    );
    (ctl, fake)
}

/// Polls every `step` seconds from `from` to `to` inclusive, collecting events.
fn run(
    ctl: &mut Controller,
    fake: &mut FakeNvml,
    from: u64,
    to: u64,
    step: u64,
) -> Vec<ClockEvent> {
    let mut ev = Vec::new();
    let mut s = from;
    while s <= to {
        ev.extend(ctl.poll(fake, t(s)));
        s += step;
    }
    ev
}

fn lowered(ev: &[ClockEvent], card: &str) -> bool {
    ev.iter()
        .any(|e| matches!(e, ClockEvent::Lowered { card: c, .. } if c == card))
}

fn alerts(ev: &[ClockEvent], kind: AlertKind) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            ClockEvent::Alert(a) if a.kind == kind => Some(format!("{}: {}", a.card, a.message)),
            _ => None,
        })
        .collect()
}

const LOW: GpuCommand = GpuCommand::LockGpuClocks {
    min_mhz: 135,
    max_mhz: 135,
};

#[test]
fn idle_wake_serving_idle() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    // Start applies the policy: persistence, PL, and A unlocked (-rgc), never a 1597 pin.
    assert_eq!(
        fake.sent(A),
        [
            GpuCommand::SetPersistence { on: true },
            GpuCommand::SetPowerLimit { watts: 350 },
            GpuCommand::ResetGpuClocks
        ]
    );
    fake.clear();

    // 59 s of nothing: still serving.
    let ev = run(&mut ctl, &mut fake, 1, 59, 1);
    assert!(!lowered(&ev, "A"));
    assert!(fake.log.is_empty());

    // 60 s: every idle card is locked at 135.
    let ev = run(&mut ctl, &mut fake, 60, 60, 1);
    assert!(lowered(&ev, "A") && lowered(&ev, "B") && lowered(&ev, "C"));
    assert_eq!(fake.sent(A), [LOW]);
    assert_eq!(ctl.card("A").unwrap().phase, Phase::Low);
    assert_eq!(ctl.lowered_uuids().len(), 3);
    fake.clear();

    // The router admits a request for A: wake before the proxy, back to unlocked.
    let ev = ctl.wake(
        &mut fake,
        "A",
        WakeSource::Router,
        "POST /v1/chat/completions",
        t(100),
    );
    assert!(ev.iter().any(|e| matches!(e, ClockEvent::Woke { card, source: WakeSource::Router, dry_run: false, .. } if card == "A")));
    assert_eq!(fake.sent(A), [GpuCommand::ResetGpuClocks]);
    assert_eq!(ctl.card("A").unwrap().phase, Phase::Serving);
    fake.clear();

    // Serving: work keeps it up.
    fake.load(A, 95, 1597, 55);
    let ev = run(&mut ctl, &mut fake, 101, 200, 5);
    assert!(!lowered(&ev, "A"));
    // Work stops; 60 s later it idles again.
    fake.load(A, 0, 135, 35);
    let ev = run(&mut ctl, &mut fake, 205, 270, 5);
    assert!(lowered(&ev, "A"));
    assert_eq!(fake.sent(A).last(), Some(&LOW));
    assert_eq!(ctl.card("A").unwrap().low_count, 2);
}

#[test]
fn util_wakes_and_wake_sources_are_per_card() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.clear();

    // C does not wake on router (it is not behind the router), only sniff / util.
    ctl.wake(&mut fake, "C", WakeSource::Router, "", t(61));
    assert_eq!(ctl.card("C").unwrap().phase, Phase::Low);
    ctl.wake(
        &mut fake,
        "C",
        WakeSource::Sniff,
        "POST /prompt :8188",
        t(62),
    );
    assert_eq!(ctl.card("C").unwrap().phase, Phase::Serving);

    // NVML utilisation wakes B on the next sample.
    fake.load(B, 30, 1380, 40);
    let ev = run(&mut ctl, &mut fake, 63, 63, 1);
    assert!(ev.iter().any(
        |e| matches!(e, ClockEvent::Woke { card, source: WakeSource::Util, .. } if card == "B")
    ));
    assert_eq!(fake.sent(B), [GpuCommand::ResetGpuClocks]);

    // The sniffer watches C's extra port and engine ports of cards that wake on sniff.
    ctl.set_engine_ports("A", vec![8096]);
    let m = ctl.sniff_map();
    assert_eq!(m[&8188], ["C"]);
    assert_eq!(m[&8096], ["A"]);
}

#[test]
fn pinned_card_is_re_pinned_on_wake() {
    let mut fake = FakeNvml::default().with_card(A, 1597);
    let mut ctl = Controller::new(
        [(
            "A".into(),
            A.into(),
            cfg("serving = { pin = 1597 }\nidle = { after = \"60s\" }"),
        )],
        Mode::On,
        t(0),
    );
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.clear();
    ctl.wake(&mut fake, "A", WakeSource::Router, "", t(70));
    assert_eq!(
        fake.sent(A),
        [GpuCommand::LockGpuClocks {
            min_mhz: 1597,
            max_mhz: 1597
        }]
    );
}

#[test]
fn engine_restart_reasserts_policy() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    // A switch restarts A's engine; meanwhile something reset its power limit and capped it.
    fake.card(A).reading.power_limit_w = Some(300.0);
    fake.apply(
        A,
        GpuCommand::LockGpuClocks {
            min_mhz: 135,
            max_mhz: 1380,
        },
    )
    .unwrap();
    fake.clear();

    let (o, ev) = ctl.engine_ready(&mut fake, "A", t(90));
    let o = o.expect("A re-asserts on engine_ready");
    assert_eq!(o.reason, AssertReason::EngineReady);
    // Woken from low, PL fixed, clocks unlocked again (the stray 1380 cap is gone).
    assert!(
        fake.sent(A)
            .contains(&GpuCommand::SetPowerLimit { watts: 350 })
    );
    assert_eq!(fake.sent(A).last(), Some(&GpuCommand::ResetGpuClocks));
    assert_eq!(fake.cards[A].lock, None);
    assert!(o.ok, "{o:?}");
    assert_eq!(o.summary, "A unlocked (boost to 1597), PL 350 W ✓");
    assert_eq!(
        alerts(&ev, AlertKind::ClockMismatch),
        ["A: power limit is 300 W, expected 350 W (fixed)"]
    );
    assert_eq!(ctl.card("A").unwrap().phase, Phase::Serving);
}

#[test]
fn periodic_reassert_every_five_minutes() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    // Keep A busy so it stays serving.
    fake.load(A, 90, 1597, 50);
    run(&mut ctl, &mut fake, 1, 290, 10);
    fake.card(A).reading.persistence = Some(false);
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 300, 300, 1);
    let asserted: Vec<_> = ev
        .iter()
        .filter_map(|e| {
            if let ClockEvent::Asserted(o) = e {
                Some(o)
            } else {
                None
            }
        })
        .collect();
    // A, B and C all default to every:5m; A (on_mismatch = fix) fixes persistence and re-unlocks.
    let a = asserted.iter().find(|o| o.card == "A").expect("A asserted");
    assert_eq!(a.reason, AssertReason::Periodic);
    assert_eq!(
        fake.sent(A),
        [
            GpuCommand::SetPersistence { on: true },
            GpuCommand::ResetGpuClocks
        ]
    );
    // Next one is five minutes later, not before.
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 310, 590, 10);
    assert!(
        !ev.iter()
            .any(|e| matches!(e, ClockEvent::Asserted(o) if o.card == "A"))
    );
    let ev = run(&mut ctl, &mut fake, 600, 600, 1);
    assert!(
        ev.iter()
            .any(|e| matches!(e, ClockEvent::Asserted(o) if o.card == "A"))
    );
}

#[test]
fn periodic_warn_reports_without_touching() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    fake.load(B, 90, 1380, 50);
    run(&mut ctl, &mut fake, 1, 290, 10);
    // Someone reset B's app clocks. B is unlocked with the default on_mismatch = warn.
    fake.card(B).reading.app_sm_mhz = Some(1230);
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 300, 300, 1);
    assert!(
        fake.sent(B).is_empty(),
        "warn never touches the card: {:?}",
        fake.sent(B)
    );
    assert_eq!(
        alerts(&ev, AlertKind::ClockMismatch),
        ["B: app clocks are 877,1230, expected 877,1380"]
    );
    // But an engine start always fixes it.
    let (o, _) = ctl.engine_ready(&mut fake, "B", t(310));
    assert!(o.unwrap().ok);
    assert!(fake.sent(B).contains(&GpuCommand::SetAppClocks {
        mem_mhz: 877,
        sm_mhz: 1380
    }));
}

#[test]
fn drift_busy_and_cool_but_below_max_alerts_and_fixes() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    // A stray cap (the old HA select through clockctl) holds A at 1380.
    fake.apply(
        A,
        GpuCommand::LockGpuClocks {
            min_mhz: 135,
            max_mhz: 1380,
        },
    )
    .unwrap();
    fake.load(A, 100, 1597, 50);
    fake.clear();
    // 29 s below max: not yet.
    let ev = run(&mut ctl, &mut fake, 1, 30, 1);
    assert!(alerts(&ev, AlertKind::ClockDrift).is_empty());
    let ev = run(&mut ctl, &mut fake, 31, 31, 1);
    let d = alerts(&ev, AlertKind::ClockDrift);
    assert_eq!(d.len(), 1, "{ev:?}");
    assert!(d[0].contains("at 1380 MHz, expected 1597"), "{d:?}");
    // on_mismatch = fix: the serving policy is re-applied, which removes the stray cap.
    assert_eq!(fake.sent(A), [GpuCommand::ResetGpuClocks]);
    fake.load(A, 100, 1597, 50);
    let ev = run(&mut ctl, &mut fake, 32, 32, 1);
    assert_eq!(alerts(&ev, AlertKind::ClockDriftCleared).len(), 1);
    // Only one alert per episode.
    assert!(!ctl.card("A").unwrap().drift_alerted);
}

#[test]
fn drift_ignores_hot_power_capped_and_idle_cards() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    fake.apply(
        A,
        GpuCommand::LockGpuClocks {
            min_mhz: 135,
            max_mhz: 1380,
        },
    )
    .unwrap();
    // Hot: thermal behaviour is the hardware's business.
    fake.load(A, 100, 1597, 80);
    assert!(alerts(&run(&mut ctl, &mut fake, 1, 60, 1), AlertKind::ClockDrift).is_empty());
    // Cool but at its power limit.
    fake.load(A, 100, 1597, 50);
    fake.card(A).reading.event_reasons = Some(reasons::SW_POWER_CAP);
    assert!(alerts(&run(&mut ctl, &mut fake, 61, 120, 1), AlertKind::ClockDrift).is_empty());
    // Light load is not "busy".
    fake.load(A, 20, 1597, 50);
    assert!(
        alerts(
            &run(&mut ctl, &mut fake, 121, 180, 1),
            AlertKind::ClockDrift
        )
        .is_empty()
    );
}

#[test]
fn cap_override_with_ttl_not_offered_for_card_a() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    fake.clear();
    let err = ctl
        .override_cap(&mut fake, "A", Some(1400), t(600), None, t(1))
        .unwrap_err();
    assert!(err.contains("not offered for card A"), "{err}");
    assert!(fake.sent(A).is_empty());
    assert!(
        !ctl.status(t(1))
            .iter()
            .find(|s| s.card == "A")
            .unwrap()
            .cap_offered
    );

    // B: allowed, with a TTL of at most an hour.
    assert!(
        ctl.override_cap(&mut fake, "B", Some(1200), t(7200), None, t(1))
            .is_err()
    );
    ctl.override_cap(&mut fake, "B", Some(1200), t(600), None, t(1))
        .unwrap();
    assert_eq!(
        fake.sent(B),
        [GpuCommand::LockGpuClocks {
            min_mhz: 135,
            max_mhz: 1200
        }]
    );
    // B idles and wakes: the cap is re-applied (gpu-power's -rgc used to wipe clockctl's cap).
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.clear();
    ctl.wake(&mut fake, "B", WakeSource::Router, "", t(100));
    assert_eq!(
        fake.sent(B),
        [GpuCommand::LockGpuClocks {
            min_mhz: 135,
            max_mhz: 1200
        }]
    );
    // Expiry restores the policy (unlocked).
    fake.load(B, 50, 1200, 40);
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 600, 602, 1);
    assert!(
        ev.iter()
            .any(|e| matches!(e, ClockEvent::CapExpired { card } if card == "B"))
    );
    assert_eq!(fake.sent(B), [GpuCommand::ResetGpuClocks]);
}

#[test]
fn cap_respects_limits_file() {
    let (mut ctl, mut fake) = setup();
    let limits = Limits::parse(
        r#"max_ttl = "30m"
        [gpus.GPU-bbbb]
        actions = ["lock_clocks"]
        clock_mhz = [300, 1380]"#,
    )
    .unwrap();
    // Floor 300 in the limits: a 135,1200 window is outside it.
    let e = ctl
        .override_cap(&mut fake, "B", Some(1200), t(600), Some(&limits), t(0))
        .unwrap_err();
    assert!(e.contains("outside 300-1380"), "{e}");
    let e = ctl
        .override_cap(&mut fake, "B", Some(1200), t(3000), Some(&limits), t(0))
        .unwrap_err();
    assert!(e.contains("ttl"), "{e}");
}

#[test]
fn holds_block_idle_and_wake_low_cards() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.clear();
    // A switch holds every card (was /run/gpu-power-hold, /hold?secs=).
    let ev = ctl
        .hold(&mut fake, "all", "switch job 42", t(600), t(70))
        .unwrap();
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(
                e,
                ClockEvent::Woke {
                    source: WakeSource::Hold,
                    ..
                }
            ))
            .count(),
        3
    );
    let ev = run(&mut ctl, &mut fake, 71, 660, 10);
    assert!(!lowered(&ev, "A"), "no clock-down while held");
    // The hold expires at 670. A hold is not work (gpu-power parity), so A, idle since 0, lowers at once.
    let ev = run(&mut ctl, &mut fake, 670, 680, 10);
    assert!(lowered(&ev, "A"));
    assert!(
        ctl.hold(&mut fake, "A", "x", t(7200), t(700)).is_err(),
        "ttl over 1 h"
    );
    assert!(ctl.hold(&mut fake, "Z", "x", t(60), t(700)).is_err());
    // Release by reason.
    ctl.hold(&mut fake, "A", "bench", t(600), t(700)).unwrap();
    ctl.release("A", "bench");
    assert!(ctl.card("A").unwrap().holds.is_empty());
}

#[test]
fn inflight_requests_block_idle() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    ctl.set_inflight("A", 1, t(0));
    let ev = run(&mut ctl, &mut fake, 1, 200, 10);
    assert!(!lowered(&ev, "A"));
    ctl.set_inflight("A", 0, t(200));
    let ev = run(&mut ctl, &mut fake, 210, 260, 10);
    assert!(lowered(&ev, "A"));
}

#[test]
fn external_lock_removal_relocks_then_backs_off() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    let mut all = Vec::new();
    for (i, at) in [70u64, 80, 90].into_iter().enumerate() {
        // Someone ran -rgc on A: the card idles at a higher clock, util still 0.
        fake.apply(A, GpuCommand::ResetGpuClocks).unwrap();
        fake.card(A).reading.sm_mhz = Some(1312);
        fake.clear();
        let ev = run(&mut ctl, &mut fake, at, at, 1);
        if i < 2 {
            assert_eq!(fake.sent(A), [LOW], "re-locked");
        }
        all.extend(ev);
    }
    assert_eq!(alerts(&all, AlertKind::LockRemovedExternally).len(), 2);
    assert_eq!(alerts(&all, AlertKind::BackingOff).len(), 1);
    let a = ctl.card("A").unwrap();
    assert_eq!(a.phase, Phase::Serving);
    // Stands down for 15 minutes.
    fake.card(A).reading.sm_mhz = Some(135);
    let ev = run(&mut ctl, &mut fake, 100, 980, 20);
    assert!(!lowered(&ev, "A"));
    let ev = run(&mut ctl, &mut fake, 1000, 1000, 1);
    assert!(lowered(&ev, "A"));
}

#[test]
fn unreadable_card_goes_back_to_serving() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.fail_reads = true;
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 61, 61, 1);
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(
                e,
                ClockEvent::Woke {
                    source: WakeSource::Error,
                    ..
                }
            ))
            .count(),
        3
    );
    assert_eq!(fake.sent(A), [GpuCommand::ResetGpuClocks]);
    // And it never idles blind.
    let ev = run(&mut ctl, &mut fake, 62, 200, 5);
    assert!(!lowered(&ev, "A"));
}

#[test]
fn lock_failure_backs_off_at_max() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    fake.fail.push((A.into(), LOW));
    fake.clear();
    let ev = run(&mut ctl, &mut fake, 60, 60, 1);
    assert!(!lowered(&ev, "A"));
    assert_eq!(alerts(&ev, AlertKind::CommandFailed).len(), 1);
    assert_eq!(fake.sent(A), [LOW, GpuCommand::ResetGpuClocks]);
    assert!(
        ctl.status(t(61))
            .iter()
            .find(|s| s.card == "A")
            .unwrap()
            .backing_off
    );
}

#[test]
fn dry_run_never_touches_hardware() {
    let (mut ctl, mut fake) = setup();
    ctl.set_mode(&mut fake, Mode::DryRun, t(0));
    ctl.start(&mut fake, t(0));
    let ev = run(&mut ctl, &mut fake, 60, 60, 1);
    assert!(
        ev.iter()
            .any(|e| matches!(e, ClockEvent::Lowered { dry_run: true, .. }))
    );
    ctl.wake(&mut fake, "A", WakeSource::Router, "", t(70));
    assert!(fake.log.is_empty(), "{:?}", fake.log);
    assert!(ctl.lowered_uuids().is_empty());
    // Switching to on brings virtual-low cards back to serving first.
    run(&mut ctl, &mut fake, 130, 130, 1);
    ctl.set_mode(&mut fake, Mode::On, t(131));
    assert!(ctl.cards().all(|c| c.phase == Phase::Serving));
}

#[test]
fn mode_off_never_idles() {
    let (mut ctl, mut fake) = setup();
    ctl.set_mode(&mut fake, Mode::Off, t(0));
    let ev = run(&mut ctl, &mut fake, 0, 300, 30);
    assert!(!ev.iter().any(|e| matches!(e, ClockEvent::Lowered { .. })));
    assert!(fake.log.is_empty());
}

#[test]
fn status_reports_savings_and_policy() {
    let (mut ctl, mut fake) = setup();
    ctl.start(&mut fake, t(0));
    fake.card(A).reading.power_draw_w = Some(40.0);
    run(&mut ctl, &mut fake, 1, 59, 1);
    run(&mut ctl, &mut fake, 60, 60, 1);
    fake.card(A).reading.power_draw_w = Some(24.0);
    run(&mut ctl, &mut fake, 61, 420, 1);
    let s = ctl.status(t(420));
    let a = s.iter().find(|s| s.card == "A").unwrap();
    assert_eq!(a.serving, "unlocked");
    assert_eq!(a.expected_max_mhz, Some(1597));
    assert!(a.saved_wh > 1.5 && a.saved_wh < 1.7, "{}", a.saved_wh);
    assert_eq!(
        a.last_assert.as_deref(),
        Some("A unlocked (boost to 1597), PL 350 W, idle-low ✓")
    );
    serde_json::to_string(&s).unwrap();
}
