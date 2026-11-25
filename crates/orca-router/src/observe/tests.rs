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
            speed: None,
            completion_est: 0.0,
        },
    )
}

/// Speeds where B decodes `b_row` s/token/row at any batch and A `a_row`.
fn speeds(a_row: f64, b_row: f64) -> HashMap<String, Speed> {
    let mut m = HashMap::new();
    for (c, r) in [("A", a_row), ("B", b_row)] {
        let mut s = Speed::default();
        s.learn(1, 1000.0, 1000.0 * r, 4000.0, 1.0);
        m.insert(c.to_string(), s);
    }
    m
}

fn pick_est(
    actual: &str,
    a: u32,
    b: u32,
    hit: u64,
    sp: &HashMap<String, Speed>,
    est: f64,
) -> Choice {
    let lanes = lanes();
    let l = loads(a, b);
    choose(
        &on(),
        1024,
        &Arrival {
            actual,
            lanes: &lanes,
            loads: &l,
            lead_cards: &[],
            cache_hit: hit,
            speed: Some(sp),
            completion_est: est,
        },
    )
}

#[test]
fn ties_keep_the_actual_card() {
    assert_eq!(pick("B", 1, 1, &[], 0).card, "B");
    assert_eq!(pick("A", 0, 0, &[], 0).card, "A");
}

#[test]
fn least_load_moves_off_a_full_card() {
    let c = pick("A", 4, 1, &[], 0);
    assert_eq!((c.card.as_str(), c.reason), ("B", "least"));
}

#[test]
fn least_load_stays_without_a_predicted_gain() {
    // A has a free lane and nothing predicts a gain from moving: stay
    let c = pick("A", 3, 1, &[], 0);
    assert_eq!((c.card.as_str(), c.reason), ("A", "stay"));
    // the trial's two disagreements: an idler card, same speed, small gain
    let sp = speeds(0.02, 0.02);
    let c = pick_est("A", 1, 0, 0, &sp, 80.0);
    assert_eq!((c.card.as_str(), c.reason), ("A", "stay"));
    let c = pick_est("B", 0, 1, 0, &sp, 1286.0);
    assert_eq!((c.card.as_str(), c.reason), ("B", "stay"));
}

#[test]
fn least_load_moves_when_the_gain_clears_the_margin() {
    // B is 10 ms/token faster per row: 500 tokens = 5 s gain > 1 s
    let sp = speeds(0.03, 0.02);
    let c = pick_est("A", 1, 0, 0, &sp, 500.0);
    assert_eq!((c.card.as_str(), c.reason), ("B", "least"));
    // 50 tokens = 0.5 s: under the margin, stay
    let c = pick_est("A", 1, 0, 0, &sp, 50.0);
    assert_eq!((c.card.as_str(), c.reason), ("A", "stay"));
    // the cached prompt B would recompute eats the gain: 5 s batching gain minus
    // 17000 / 4000 tok/s = 4.25 s recompute = 0.75 s, under the margin
    let c = pick_est("A", 1, 0, 17_000, &sp, 500.0);
    assert_eq!(c.card.as_str(), "A");
    // the margin is configurable
    let lanes = lanes();
    let l = loads(1, 0);
    let c = choose(
        &PlacementConfig {
            min_move_gain_seconds: 0.25,
            ..on()
        },
        1024,
        &Arrival {
            actual: "A",
            lanes: &lanes,
            loads: &l,
            lead_cards: &[],
            cache_hit: 0,
            speed: Some(&sp),
            completion_est: 50.0,
        },
    );
    assert_eq!((c.card.as_str(), c.reason), ("B", "least"));
}

#[test]
fn gain_is_queue_plus_batching_minus_recompute() {
    let sp = speeds(0.03, 0.02);
    let g = gain(2.0, 100.0, sp.get("A"), sp.get("B"), 2, 1, 8000);
    assert!((g.batch - 1.0).abs() < 1e-9);
    assert!((g.cache - 2.0).abs() < 1e-9);
    assert!((g.net - 1.0).abs() < 1e-9);
    // unknown speeds: no batching term, default prefill 2000 tok/s
    let g = gain(0.0, 100.0, None, None, 2, 1, 4000);
    assert_eq!((g.batch, g.cache, g.net), (0.0, 2.0, -2.0));
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
            speed: None,
            completion_est: 0.0,
        },
    );
    assert_eq!((c.card.as_str(), c.would_queue), ("B", false));
}

#[test]
fn speed_learns_from_requests_and_falls_back_to_nearest_batch() {
    let mut s = Speed::default();
    // 700 tokens in 10 s alone on the card; 400 in 10 s with three others
    s.learn(1, 700.0, 10.0, 0.0, 0.0);
    s.learn(4, 400.0, 10.0, 5000.0, 1.0);
    assert!((s.row_secs(1) - 10.0 / 700.0).abs() < 1e-9);
    assert!((s.row_secs(4) - 10.0 / 400.0).abs() < 1e-9);
    assert!(
        (s.row_secs(3) - s.row_secs(4)).abs() < 1e-9
            || (s.row_secs(3) - s.row_secs(1)).abs() < 1e-9
    );
    assert_eq!(s.prefill_tps, 5000.0);
    // short answers (speculative decoding, first token) are not learned
    s.learn(8, 16.0, 1.0, 0.0, 0.0);
    assert_eq!(s.row[8], 0.0);
}

fn start(ts: i64, id: u64) -> Value {
    json!({"event": "request_start", "timestamp_unix_ms": ts, "server_instance_id": "s",
           "request": {"request_id": id}})
}

fn start_asking(ts: i64, id: u64, max: u64) -> Value {
    json!({"event": "request_start", "timestamp_unix_ms": ts, "server_instance_id": "s",
           "request": {"request_id": id, "requested_output_tokens": max}})
}

fn done_timed(ts: i64, id: u64, completion: u64, decode_secs: f64) -> Value {
    json!({"event": "request_done", "timestamp_unix_ms": ts, "server_instance_id": "s",
           "request": {"request_id": id, "stream": true},
           "engine_timing": {"queue_wait_seconds": 0.0},
           "timings_seconds": {"decode": decode_secs, "prefill": 0.5},
           "result": {"prefix_cache_hit_tokens": 0, "completion_tokens": completion,
                      "computed_prefill_tokens": 2000,
                      "prompt_tokens": 2000, "finish_reason": "stop_token"}})
}

#[test]
fn tracker_learns_speed_and_completion_and_applies_the_margin() {
    let p = on();
    let mut t = Tracker::new(lanes(), 0, 0);
    // A decodes 30 ms/token, B 20 ms/token; answers average 1000 tokens
    t.event("A", &start(1, 1), &p, 1024);
    t.event("A", &done_timed(2, 1, 1000, 30.0), &p, 1024);
    t.event("B", &start(3, 2), &p, 1024);
    t.event("B", &done_timed(4, 2, 1000, 20.0), &p, 1024);
    // one busy on A, B idle: moving a ~1000-token answer saves about 10 s
    t.event("A", &start(10, 3), &p, 1024);
    t.event("A", &start(11, 4), &p, 1024);
    let l = t.event("A", &done_timed(50, 4, 1000, 30.0), &p, 1024);
    assert_eq!(
        (l[0]["chosen"].as_str(), l[0]["reason"].as_str()),
        (Some("B"), Some("least"))
    );
    assert_eq!(l[0]["completionEst"], 1000);
    // the same, but the client asked for at most 20 tokens: 0.2 s, stay
    t.event("A", &start_asking(60, 5, 20), &p, 1024);
    let l = t.event("A", &done_timed(70, 5, 20, 0.6), &p, 1024);
    assert_eq!(
        (l[0]["chosen"].as_str(), l[0]["reason"].as_str()),
        (Some("A"), Some("stay"))
    );
    assert_eq!(l[0]["agree"], true);
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
