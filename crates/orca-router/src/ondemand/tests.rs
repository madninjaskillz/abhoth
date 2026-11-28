//! The Decide cases of test_ondemand.py (2026-11-24/25), ported.

use super::*;

const NOW: f64 = 1_800_000_000.0;
const INKLING: &str = "inkling2-mini";
const TARGET: &str = "dense-pi-abl-v3fork";

fn cfg() -> OnDemandConfig {
    OnDemandConfig {
        enabled: true,
        ..OnDemandConfig::default()
    }
}

fn models() -> IndexMap<String, OnDemandModel> {
    let mut m = IndexMap::new();
    m.insert(
        "inkling-2-mini".into(),
        OnDemandModel {
            model: INKLING.into(),
            engine: "ninfer".into(),
            engine_profile: "ninfer-merged-inkling-4lane".into(),
            served_id: String::new(),
            per_card: Default::default(),
        },
    );
    m.insert(
        TARGET.into(),
        OnDemandModel {
            model: TARGET.into(),
            engine: "ninfer".into(),
            engine_profile: "ninfer-merged-4lane".into(),
            served_id: String::new(),
            per_card: Default::default(),
        },
    );
    m
}

fn guards() -> Guards {
    Guards {
        at: NOW - 2.0,
        ..Guards::default()
    }
}

/// cards(a, b) with overrides: (card, field, value).
fn cards(a: &str, b: &str, kw: &[(&str, &str, Option<f64>)]) -> BTreeMap<String, CardIn> {
    let base = |m: &str| CardIn {
        model: Some(m.into()),
        offline: false,
        busy: Some(false),
        last_busy: Some(NOW - 600.0),
        last_change: Some(NOW - 3600.0),
        excluded: false,
    };
    let mut out = BTreeMap::from([("A".to_string(), base(a)), ("B".to_string(), base(b))]);
    for (c, f, v) in kw {
        let e = out.get_mut(*c).unwrap();
        match *f {
            "busy" => e.busy = v.map(|x| x != 0.0),
            "lastBusy" => e.last_busy = *v,
            "lastChange" => e.last_change = *v,
            "offline" => e.offline = v.is_some(),
            _ => panic!("{f}"),
        }
    }
    out
}

struct Call {
    cards: BTreeMap<String, CardIn>,
    guards: Option<Guards>,
    quiet: Option<u64>,
    hold: bool,
    job: Option<Job>,
    leases: Option<Vec<(String, String)>>,
    pending: Option<Pending>,
    cfg: OnDemandConfig,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            cards: cards(INKLING, INKLING, &[]),
            guards: Some(guards()),
            quiet: None,
            hold: false,
            job: None,
            leases: Some(vec![]),
            pending: None,
            cfg: cfg(),
        }
    }
}

fn run(target: &str, c: Call) -> Decision {
    decide(
        target,
        NOW,
        &c.cfg,
        &models(),
        &c.cards,
        c.guards.as_ref(),
        c.quiet,
        c.hold,
        c.job.as_ref(),
        c.leases.as_deref(),
        c.pending.as_ref(),
    )
}

fn card_of(d: &Decision) -> &str {
    match d {
        Decision::Switch { card, .. } => card,
        other => panic!("not a switch: {other:?}"),
    }
}

fn with(kw: &[(&str, &str, Option<f64>)]) -> Call {
    Call {
        cards: cards(INKLING, INKLING, kw),
        ..Call::default()
    }
}

#[test]
fn generic_aliases_are_unknown() {
    for alias in ["qwen3.8-27b", "jarvis-brain", "qwen3.8-flash-next-a"] {
        assert_eq!(run(alias, Call::default()).action(), "unknown");
    }
}

#[test]
fn live_passthrough() {
    assert_eq!(
        run("inkling-2-mini", Call::default()),
        Decision::Live {
            cards: vec!["A".into(), "B".into()]
        }
    );
    let d = run(
        TARGET,
        Call {
            cards: cards(INKLING, TARGET, &[]),
            ..Call::default()
        },
    );
    assert_eq!(
        d,
        Decision::Live {
            cards: vec!["B".into()]
        }
    );
}

#[test]
fn off() {
    let d = run(
        TARGET,
        Call {
            cfg: OnDemandConfig {
                enabled: false,
                ..cfg()
            },
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "block");
}

#[test]
fn pick_idle_longest() {
    let c = |a: Option<f64>, b: Option<f64>| with(&[("A", "lastBusy", a), ("B", "lastBusy", b)]);
    assert_eq!(
        card_of(&run(TARGET, c(Some(NOW - 100.0), Some(NOW - 50.0)))),
        "A"
    );
    assert_eq!(
        card_of(&run(TARGET, c(Some(NOW - 50.0), Some(NOW - 100.0)))),
        "B"
    );
    assert_eq!(card_of(&run(TARGET, c(None, Some(NOW - 900.0)))), "A"); // never busy
    assert_eq!(card_of(&run(TARGET, c(None, None))), "B"); // tie: B
}

#[test]
fn busy_card_skipped() {
    let d = run(
        TARGET,
        with(&[
            ("A", "lastBusy", Some(NOW - 900.0)),
            ("A", "busy", Some(1.0)),
            ("B", "lastBusy", Some(NOW - 100.0)),
        ]),
    );
    assert_eq!(card_of(&d), "B");
    // B busy 3 s ago: within the grace
    let d = run(
        TARGET,
        with(&[
            ("A", "lastBusy", Some(NOW - 900.0)),
            ("B", "lastBusy", Some(NOW - 3.0)),
        ]),
    );
    assert_eq!(card_of(&d), "A");
}

#[test]
fn both_busy_queues() {
    let d = run(
        TARGET,
        with(&[("A", "busy", Some(1.0)), ("B", "busy", Some(1.0))]),
    );
    assert_eq!(d.action(), "wait");
    assert!(d.reason().contains("busy"));
    let d = run(
        TARGET,
        with(&[("A", "busy", None), ("B", "busy", Some(1.0))]),
    );
    assert_eq!(d.action(), "wait");
}

#[test]
fn min_stay() {
    let d = run(
        TARGET,
        with(&[
            ("A", "lastChange", Some(NOW - 60.0)),
            ("B", "lastChange", Some(NOW - 120.0)),
        ]),
    );
    assert_eq!(d.action(), "wait");
    assert!(d.reason().contains("minimum stay"));
    let d = run(
        TARGET,
        with(&[
            ("A", "lastChange", Some(NOW - 60.0)),
            ("A", "lastBusy", None),
            ("B", "lastChange", Some(NOW - 301.0)),
        ]),
    );
    assert_eq!(card_of(&d), "B");
}

#[test]
fn pending_shares_one_switch() {
    let p = Pending {
        job: "j1".into(),
        card: "B".into(),
        started: NOW,
        estimate: None,
        layout: "x".into(),
    };
    let d = run(
        TARGET,
        Call {
            pending: Some(p),
            ..Call::default()
        },
    );
    assert_eq!(
        d,
        Decision::Wait {
            reason: format!("switching card B to {TARGET}"),
            switching: true,
            card: Some("B".into())
        }
    );
}

#[test]
fn other_job_waits() {
    let d = run(
        TARGET,
        Call {
            job: Some(Job {
                state: "running".into(),
                layout: "x".into(),
            }),
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "wait");
}

#[test]
fn leases_wait() {
    let d = run(
        TARGET,
        Call {
            leases: Some(vec![("A1".into(), "orca".into())]),
            ..with(&[
                ("A", "lastBusy", None),
                ("B", "lastBusy", Some(NOW - 100.0)),
            ])
        },
    );
    assert_eq!(card_of(&d), "B"); // a lease on A: B can still switch
    let d = run(
        TARGET,
        Call {
            leases: Some(vec![
                ("A1".into(), "orca".into()),
                ("B2".into(), "orca".into()),
            ]),
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "wait");
    assert!(d.reason().contains("leases"));
    let d = run(
        TARGET,
        Call {
            leases: None,
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "wait");
}

#[test]
fn blocked_windows() {
    let g = |f: fn(&mut Guards)| {
        let mut x = guards();
        f(&mut x);
        Call {
            guards: Some(x),
            ..Call::default()
        }
    };
    let cases: Vec<(Call, &str)> = vec![
        (
            Call {
                quiet: Some(1200),
                ..Call::default()
            },
            "06:45",
        ),
        (g(|x| x.video = Some("ai-news".into())), "video"),
        (g(|x| x.bench = Some("item-3".into())), "bench"),
        (g(|x| x.music = true), "music"),
        (
            Call {
                hold: true,
                ..Call::default()
            },
            "hold",
        ),
        (with(&[("B", "offline", Some(1.0))]), "offline"),
    ];
    for (c, word) in cases {
        match run(TARGET, c) {
            Decision::Block { reason, retry } => {
                assert!(reason.contains(word), "{reason}");
                assert!(retry > 0);
            }
            other => panic!("{word}: {other:?}"),
        }
    }
    let d = run(
        TARGET,
        Call {
            quiet: Some(1200),
            ..Call::default()
        },
    );
    assert!(matches!(d, Decision::Block { retry: 1200, .. }));
}

#[test]
fn live_is_served_whatever_the_guards_say() {
    let d = run(
        "inkling-2-mini",
        Call {
            quiet: Some(1200),
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "live");
}

#[test]
fn stale_guards_wait() {
    let d = run(
        TARGET,
        Call {
            guards: Some(Guards {
                at: NOW - 120.0,
                ..guards()
            }),
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "wait");
    let d = run(
        TARGET,
        Call {
            guards: None,
            ..Call::default()
        },
    );
    assert_eq!(d.action(), "wait");
    // the control plane: no guards file (the switch checks the guard board itself)
    let mut cfg = cfg();
    cfg.control_gate = true;
    let d = run(
        TARGET,
        Call {
            guards: None,
            cfg,
            ..Call::default()
        },
    );
    assert_ne!(d.action(), "wait", "{d:?}");
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

#[test]
fn quiet_window_in_uk_time() {
    let ep = |t: chrono::DateTime<Utc>| t.timestamp() as f64;
    // 2026-11-25 07:00 UK (GMT) = 07:00 UTC: 1800 s left (test_quiet_retry)
    assert_eq!(quiet(ep(utc(2026, 11, 25, 7, 0, 0))), Some(1800));
    // summer: 07:00 BST = 06:00 UTC
    assert_eq!(quiet(ep(utc(2026, 7, 1, 6, 0, 0))), Some(1800));
    // at least 60
    assert_eq!(quiet(ep(utc(2026, 11, 25, 7, 29, 30))), Some(60));
    assert_eq!(quiet(ep(utc(2026, 11, 25, 7, 30, 0))), None);
    assert_eq!(quiet(ep(utc(2026, 11, 25, 6, 44, 59))), None);
    // DST edges 2026: 29 March and 25 October at 01:00 UTC
    assert!(!is_bst(utc(2026, 3, 29, 0, 59, 0)));
    assert!(is_bst(utc(2026, 3, 29, 1, 0, 0)));
    assert!(is_bst(utc(2026, 10, 25, 0, 59, 0)));
    assert!(!is_bst(utc(2026, 10, 25, 1, 0, 0)));
}

#[test]
fn last_changes_from_live_json() {
    let live = json!({"lastSwitch": {"job": "j-ondemand", "at": "2026-11-24T10:00:00Z"}});
    let t = utc(2026, 11, 24, 10, 0, 0).timestamp() as f64;
    let cards = vec!["A".to_string(), "B".to_string()];
    let mine = HashMap::from([("j-ondemand".to_string(), "B".to_string())]);
    let got = last_changes(&live, &mine, &HashMap::new(), &cards);
    assert_eq!(got.get("B"), Some(&t));
    assert_eq!(got.get("A"), None);
    // any other switch: both cards
    let both = last_changes(&live, &HashMap::new(), &HashMap::new(), &cards);
    assert_eq!((both["A"], both["B"]), (t, t));
}

#[test]
fn excluded_cards_are_never_picked() {
    // A idles longest but is excluded (dense-pi's card): B is picked
    let mut c = with(&[("A", "lastBusy", Some(NOW - 900.0))]);
    c.cards.get_mut("A").unwrap().excluded = true;
    assert_eq!(card_of(&run(TARGET, c)), "B");
    // both excluded: never a switch
    let mut c = with(&[]);
    for v in c.cards.values_mut() {
        v.excluded = true;
    }
    let d = run(TARGET, c);
    assert_ne!(d.action(), "switch", "{d:?}");
}

#[test]
fn guard_data_is_fresh_for_45_seconds() {
    let at = |age: f64| Call {
        guards: Some(Guards {
            at: NOW - age,
            ..guards()
        }),
        ..Call::default()
    };
    assert_eq!(run(TARGET, at(45.0)).action(), "switch");
    assert_eq!(run(TARGET, at(46.0)).action(), "wait");
}

#[test]
fn gate_off_is_todays_behaviour() {
    use crate::guards::Caller;
    use crate::switch::gate::{ChatAdmission, RateLimiter};
    let anon = Caller {
        principal: None,
        grants: vec![],
    };
    // no-switch header, no key, and a limiter far past its limits
    let adm = ChatAdmission::for_test(TARGET, anon, true);
    let lim = RateLimiter::default();
    for i in 0..50 {
        lim.record(None, NOW - i as f64);
    }
    let mut od = cfg();
    assert!(!od.control_gate, "off by default");
    assert_eq!(crate::ondemand::gate(&od, &adm, &lim, NOW), Ok(None));
    od.control_gate = true;
    assert!(crate::ondemand::gate(&od, &adm, &lim, NOW).is_err());
}
