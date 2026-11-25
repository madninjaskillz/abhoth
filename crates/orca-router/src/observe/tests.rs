use super::*;

fn lanes() -> IndexMap<String, u32> {
    [("A".to_string(), 4), ("B".to_string(), 4)]
        .into_iter()
        .collect()
}

fn loads(a: u32, b: u32) -> HashMap<String, u32> {
    [("A".to_string(), a), ("B".to_string(), b)]
        .into_iter()
        .collect()
}

fn on() -> PlacementConfig {
    PlacementConfig {
        lease_aware: true,
        admission: true,
        affinity: true,
        ..PlacementConfig::default()
    }
}

fn pick(actual: &str, a: u32, b: u32, lead: &[&str], hit: u64) -> Choice {
    let lanes = lanes();
    let l = loads(a, b);
    let lead: Vec<String> = lead.iter().map(|s| s.to_string()).collect();
    choose(
        &on(),
        1024,
        &Arrival {
            actual,
            lanes: &lanes,
            loads: &l,
            lead_cards: &lead,
            cache_hit: hit,
        },
    )
}

#[test]
fn ties_keep_the_actual_card() {
    assert_eq!(pick("B", 1, 1, &[], 0).card, "B");
    assert_eq!(pick("A", 0, 0, &[], 0).card, "A");
}

#[test]
fn least_load_moves() {
    let c = pick("A", 3, 1, &[], 0);
    assert_eq!((c.card.as_str(), c.reason), ("B", "least"));
}

#[test]
fn cache_affinity_keeps_unless_full() {
    // a session with a warm cache stays even when the other card is idler
    let c = pick("A", 2, 0, &[], 4096);
    assert_eq!((c.card.as_str(), c.reason), ("A", "affinity"));
    // ... until its card is full: admission moves it once
    let c = pick("A", 4, 0, &[], 4096);
    assert_eq!((c.card.as_str(), c.reason), ("B", "admission-move"));
}

#[test]
fn lead_reserve_and_queue() {
    // A holds the Lead: non-Lead work may use 3 of its 4 lanes
    let c = pick("A", 3, 4, &["A"], 4096);
    assert_eq!(
        (c.card.as_str(), c.reason, c.would_queue),
        ("A", "queue", true)
    );
    let c = pick("A", 2, 4, &["A"], 4096);
    assert_eq!((c.card.as_str(), c.reason), ("A", "affinity"));
}

#[test]
fn admission_off_is_plain_least() {
    let lanes = lanes();
    let l = loads(4, 4);
    let c = choose(
        &PlacementConfig {
            affinity: true,
            ..PlacementConfig::default()
        },
        1024,
        &Arrival {
            actual: "B",
            lanes: &lanes,
            loads: &l,
            lead_cards: &[],
            cache_hit: 0,
        },
    );
    assert_eq!((c.card.as_str(), c.would_queue), ("B", false));
}

#[test]
fn speed_falls_back_to_nearest_batch() {
    let mut s = Speed::default();
    s.learn(
        &json!({"throughput_tokens_per_second": {"decode": 70.0, "prefill": 0.0},
                    "decode_batch": {"average_size": 1.0}}),
    );
    s.learn(
        &json!({"throughput_tokens_per_second": {"decode": 160.0, "prefill": 5000.0},
                    "decode_batch": {"average_size": 4.0}}),
    );
    assert!((s.row_secs(1) - 1.0 / 70.0).abs() < 1e-9);
    assert!((s.row_secs(4) - 4.0 / 160.0).abs() < 1e-9);
    assert!(
        (s.row_secs(3) - s.row_secs(4)).abs() < 1e-9
            || (s.row_secs(3) - s.row_secs(1)).abs() < 1e-9
    );
    assert_eq!(s.prefill_tps, 5000.0);
}

fn start(ts: i64, id: u64) -> Value {
    json!({"event": "request_start", "timestamp_unix_ms": ts, "server_instance_id": "s",
           "request": {"request_id": id}})
}

fn done(ts: i64, id: u64, hit: u64, qw: f64) -> Value {
    json!({"event": "request_done", "timestamp_unix_ms": ts, "server_instance_id": "s",
           "request": {"request_id": id, "stream": true},
           "engine_timing": {"queue_wait_seconds": qw},
           "result": {"prefix_cache_hit_tokens": hit, "completion_tokens": 100,
                      "prompt_tokens": 2000, "finish_reason": "stop_token"}})
}

#[test]
fn tracker_scores_against_the_actual_card() {
    let p = on();
    let mut t = Tracker::new(lanes(), 0, 0);
    let mut lines = Vec::new();
    // four requests pile onto A while B is idle (pinned client or nginx's view)
    for i in 0..4 {
        lines.extend(t.event("A", &start(1_000 + i, i as u64), &p, 1024));
    }
    assert_eq!(t.loads()["A"], 4);
    // a fifth on A: the router would have sent it to B
    lines.extend(t.event("A", &start(2_000, 9), &p, 1024));
    lines.extend(t.event("A", &done(9_000, 9, 0, 3.5), &p, 1024));
    // the first one ends: it arrived on an idle A, agree
    lines.extend(t.event("A", &done(9_500, 0, 0, 0.0), &p, 1024));
    assert_eq!(lines.len(), 2);
    let moved = &lines[0];
    assert_eq!(moved["chosen"], "B");
    assert_eq!(moved["agree"], false);
    assert_eq!(moved["reason"], "least");
    assert_eq!(moved["gain"]["queue"], 3.5);
    assert_eq!(moved["seconds"], 7.0);
    assert_eq!(lines[1]["agree"], true);
    assert_eq!(t.loads()["A"], 3);
    // a reset engine log forgets its in-flight requests
    t.reset("A");
    assert_eq!(t.loads()["A"], 0);
}

#[test]
fn recoveries_are_errors() {
    let p = on();
    let mut t = Tracker::new(lanes(), 0, 0);
    let ev =
        |r: u64| json!({"event": "throughput", "timestamp_unix_ms": 5, "engine_recoveries": r});
    assert!(t.event("B", &ev(0), &p, 1024).is_empty());
    let out = t.event("B", &ev(1), &p, 1024);
    assert_eq!(out[0]["type"], "error");
}

#[test]
fn follow_reads_only_new_complete_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("req.jsonl");
    std::fs::write(&path, "{\"old\":1}\n").unwrap();
    let mut f = Follow {
        card: "A".into(),
        path: path.clone(),
        offset: None,
        partial: Vec::new(),
    };
    assert!(f.read_new().unwrap().0.is_empty()); // starts at the end
    let mut h = OpenOptions::new().append(true).open(&path).unwrap();
    write!(h, "{{\"a\":1}}\n{{\"b\":").unwrap();
    let (lines, reset) = f.read_new().unwrap();
    assert_eq!((lines.len(), reset), (1, false));
    writeln!(h, "2}}").unwrap();
    assert_eq!(f.read_new().unwrap().0.len(), 1);
    // truncated (engine restart): read from the start again
    std::fs::write(&path, "{\"c\":3}\n").unwrap();
    let (lines, reset) = f.read_new().unwrap();
    assert_eq!((lines.len(), reset), (1, true));
}

#[test]
fn summary_counts_agreement_gain_and_canary() {
    let dir = tempfile::tempdir().unwrap();
    let day = "2026-11-26";
    let lines = [
        json!({"t": "2026-11-26T10:00:00.000Z", "type": "decision", "card": "A", "chosen": "A",
               "agree": true, "reason": "least", "wouldQueue": false, "warm": false,
               "queueWait": 0.0, "completion": 10, "finish": "stop_token"}),
        json!({"t": "2026-11-26T10:01:00.000Z", "type": "decision", "card": "A", "chosen": "B",
               "agree": false, "reason": "least", "wouldQueue": false, "warm": false,
               "queueWait": 2.0, "completion": 10, "finish": "stop_token", "loads": {"A": 4, "B": 0},
               "gain": {"queue": 2.0, "batch": 0.5, "cache": 0.0, "net": 2.5}}),
        json!({"t": "2026-11-26T10:02:00.000Z", "type": "decision", "card": "B", "chosen": "B",
               "agree": true, "reason": "least", "warm": true, "finish": "stop_token"}),
        json!({"t": "2026-11-26T10:03:00.000Z", "type": "canary",
               "litellm": {"status": 200, "ms": 500.0}, "router": {"status": 200, "ms": 480.0}}),
        json!({"t": "2026-11-26T10:04:00.000Z", "type": "error", "error": "pig-slots: timeout"}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.path().join(format!("decisions-{day}.jsonl")), text).unwrap();
    let (j, md) = summarize(dir.path(), day);
    assert_eq!(j["requests"], 3);
    assert_eq!(j["scored"], 2);
    assert_eq!(j["agreePct"], 50.0);
    assert_eq!(j["predictedGain"]["netSumSeconds"], 2.5);
    assert_eq!(j["predictedGain"]["over1s"], 1);
    assert_eq!(j["actualQueue"]["requestsWaitedOver0_5s"], 1);
    assert_eq!(j["canary"]["pairs"], 1);
    assert_eq!(j["canary"]["addedMsMedian"], -20.0);
    assert_eq!(j["errors"]["events"], 1);
    assert!(md.contains("**50.0%**"), "{md}");
}

#[test]
fn uk_dates_follow_bst() {
    // 2026-07-01 23:30 UTC is 00:30 on the 2nd in London; in January UTC = UK
    let summer = Utc
        .with_ymd_and_hms(2026, 7, 1, 23, 30, 0)
        .unwrap()
        .timestamp_millis();
    assert_eq!(uk_date(summer), "2026-07-02");
    let winter = Utc
        .with_ymd_and_hms(2026, 1, 1, 23, 30, 0)
        .unwrap()
        .timestamp_millis();
    assert_eq!(uk_date(winter), "2026-01-01");
}
