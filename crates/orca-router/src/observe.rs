//! Decision-only shadow (`observe` in router.json; ROUTER.md 11.2).
//!
//! The router follows each card's own ninfer request log (read-only), rebuilds every card's
//! true in-flight count at each real arrival (LiteLLM/qfn-lb, pinned aliases, benches and slot
//! clients alike), runs the placement policy on that state and scores its choice against the
//! card the request actually landed on. Nothing is sent to the engines, except the optional
//! canary (a tiny chat through LiteLLM and through the router, capped and skipped when cards
//! are busy or in the 06:45-07:30 UK window).
//!
//! Output, per UK day, in `observe.dir`: `decisions-YYYY-MM-DD.jsonl` (one line per scored
//! request, per canary, per error) and at day rollover `summary-YYYY-MM-DD.json` + `.md`.
//! `GET /observe[?date=YYYY-MM-DD]` gives the same summary on demand.
//!
//! The same live load also feeds the router's own placement (`feedPlacement`), so its
//! least-loaded and admission choices see traffic that does not pass through it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::config::{ObserveConfig, PlacementConfig, Snapshot};
use crate::state::AppState;

/// Live view shared with placement and `/observe`.
#[derive(Default)]
pub struct Observer {
    inner: Mutex<Live>,
}

#[derive(Default)]
struct Live {
    loads: HashMap<String, u32>,
    fresh: Option<Instant>,
    lead_cards: Vec<String>,
    last_error: Option<String>,
    scored: u64,
}

impl Observer {
    /// A card's in-flight count as the engine's own log shows it, while the follower is
    /// current (polled within 10 s).
    pub fn external_load(&self, card: &str) -> Option<u32> {
        let g = self.inner.lock().unwrap();
        match g.fresh {
            Some(t) if t.elapsed() < Duration::from_secs(10) => g.loads.get(card).copied(),
            _ => None,
        }
    }

    fn set(&self, loads: HashMap<String, u32>, lead_cards: Vec<String>) {
        let mut g = self.inner.lock().unwrap();
        g.loads = loads;
        g.lead_cards = lead_cards;
        g.fresh = Some(Instant::now());
    }

    fn error(&self, e: String) {
        self.inner.lock().unwrap().last_error = Some(e);
    }

    pub fn live(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "loads": g.loads,
            "current": g.fresh.is_some_and(|t| t.elapsed() < Duration::from_secs(10)),
            "leadCards": g.lead_cards,
            "scoredSinceStart": g.scored,
            "lastError": g.last_error,
        })
    }
}

// ── the policy, as a pure function ────────────────────────────────────────────────────────

/// What the router knows when a request arrives.
pub struct Arrival<'a> {
    pub actual: &'a str,
    /// eligible cards and their lanes, in config order
    pub lanes: &'a IndexMap<String, u32>,
    pub loads: &'a HashMap<String, u32>,
    pub lead_cards: &'a [String],
    pub cache_hit: u64,
    /// learned decode/prefill speed per card (missing: no batching gain is predicted)
    pub speed: Option<&'a HashMap<String, Speed>>,
    /// expected completion tokens, known at arrival (0: no batching gain is predicted)
    pub completion_est: f64,
}

/// Predicted seconds saved by running a request on `there` instead of `here` (the card it is
/// on): engine queue wait avoided, plus the batching speed-up (tokens out times the difference
/// in per-row decode time at the two batch sizes), minus the cached prompt that `there` would
/// have to recompute. Used both to score a choice (with what happened) and to make it (with
/// what is known at arrival).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gain {
    pub queue: f64,
    pub batch: f64,
    pub cache: f64,
    pub net: f64,
}

pub fn gain(
    queue: f64,
    completion: f64,
    here: Option<&Speed>,
    there: Option<&Speed>,
    batch_here: u32,
    batch_there: u32,
    cache_hit: u64,
) -> Gain {
    let (rx, ry) = (
        here.map_or(0.0, |s| s.row_secs(batch_here)),
        there.map_or(0.0, |s| s.row_secs(batch_there)),
    );
    let batch = if rx > 0.0 && ry > 0.0 {
        completion * (rx - ry)
    } else {
        0.0
    };
    let pre = there
        .map(|s| s.prefill_tps)
        .filter(|&t| t > 0.0)
        .unwrap_or(2000.0);
    let cache = cache_hit as f64 / pre;
    Gain {
        queue,
        batch,
        cache,
        net: queue + batch - cache,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub card: String,
    /// affinity | least | stay | admission-move | queue
    pub reason: &'static str,
    pub would_queue: bool,
}

/// The router's placement (placement.rs) applied to a known state: cache affinity (the stand-in
/// for session affinity), then least load per lane, then admission with the Lead reserve.
/// Least load keeps the actual card ("stay") while it has a free lane, unless the predicted net
/// gain of moving (`gain`, with queue wait 0 since a lane is free) exceeds
/// `minMoveGainSeconds`. Requests are treated as non-Lead; the Lead's long sessions keep their
/// card through cache affinity.
pub fn choose(p: &PlacementConfig, keep_tokens: u64, a: &Arrival) -> Choice {
    let load = |c: &str| a.loads.get(c).copied().unwrap_or(0);
    let lanes = |c: &str| a.lanes.get(c).copied().unwrap_or(1).max(1);
    let limit = |c: &str| {
        if p.admission && a.lead_cards.iter().any(|l| l == c) {
            lanes(c).saturating_sub(p.lead_reserve).max(1)
        } else {
            lanes(c)
        }
    };
    let mut order: Vec<String> = Vec::new();
    let mut base = "least";
    if p.affinity && a.cache_hit >= keep_tokens && a.lanes.contains_key(a.actual) {
        order.push(a.actual.to_string());
        base = "affinity";
    }
    let mut rest: Vec<&String> = a.lanes.keys().filter(|c| !order.contains(c)).collect();
    rest.sort_by_key(|c| {
        (
            u64::from(load(c)) * 1_000_000 / u64::from(lanes(c)),
            *c != a.actual,
        )
    });
    order.extend(rest.into_iter().cloned());
    let free_here = if p.admission {
        limit(a.actual)
    } else {
        lanes(a.actual)
    };
    if base == "least"
        && let Some(f) = order.first().cloned()
        && f != a.actual
        && a.lanes.contains_key(a.actual)
        && load(a.actual) < free_here
    {
        let g = gain(
            0.0,
            a.completion_est,
            a.speed.and_then(|m| m.get(a.actual)),
            a.speed.and_then(|m| m.get(f.as_str())),
            load(a.actual) + 1,
            load(&f) + 1,
            a.cache_hit,
        );
        if g.net <= p.min_move_gain_seconds {
            order.retain(|c| c != a.actual);
            order.insert(0, a.actual.to_string());
            base = "stay";
        }
    }
    let Some(first) = order.first().cloned() else {
        return Choice {
            card: a.actual.to_string(),
            reason: "least",
            would_queue: false,
        };
    };
    if !p.admission {
        return Choice {
            card: first,
            reason: base,
            would_queue: false,
        };
    }
    for c in &order {
        if load(c) < limit(c) {
            return Choice {
                card: c.clone(),
                reason: if *c == first { base } else { "admission-move" },
                would_queue: false,
            };
        }
    }
    Choice {
        card: first,
        reason: "queue",
        would_queue: true,
    }
}

// ── engine speed model (from finished requests' own decode and prefill times) ────────────
//
// Not from the 5 s throughput events: their decode rate is tokens over the whole window, idle
// time included, so a lightly used card looked several times slower per row than it is (the
// 2026-11-25 trial predicted -7.8 s for an 80-token answer).

#[derive(Clone, Debug)]
pub struct Speed {
    /// seconds per token per row, by decode batch size 1..=8 (index 0 unused)
    row: [f64; 9],
    prefill_tps: f64,
}

impl Default for Speed {
    fn default() -> Self {
        Self {
            row: [0.0; 9],
            prefill_tps: 0.0,
        }
    }
}

/// Requests shorter than this are not used for the decode rate (speculative decoding and
/// the first token make short answers unrepresentative).
const LEARN_MIN_TOKENS: f64 = 64.0;

fn ewma(old: f64, new: f64) -> f64 {
    if old == 0.0 {
        new
    } else {
        0.9 * old + 0.1 * new
    }
}

impl Speed {
    /// One finished request: `completion` tokens decoded in `decode_secs` while `batch` rows
    /// (it and the ones in flight when it started) shared the card; `prefill` tokens computed
    /// in `prefill_secs`.
    pub fn learn(
        &mut self,
        batch: u32,
        completion: f64,
        decode_secs: f64,
        prefill: f64,
        prefill_secs: f64,
    ) {
        if completion >= LEARN_MIN_TOKENS && decode_secs > 0.0 {
            let n = (batch as usize).clamp(1, 8);
            self.row[n] = ewma(self.row[n], decode_secs / completion);
        }
        if prefill >= 256.0 && prefill_secs > 0.0 {
            self.prefill_tps = ewma(self.prefill_tps, prefill / prefill_secs);
        }
    }

    /// seconds per token for one row at batch `n` (nearest learned size; 0 when unknown)
    pub fn row_secs(&self, n: u32) -> f64 {
        let n = (n as usize).clamp(1, 8);
        (0..8)
            .flat_map(|d| [n.checked_sub(d), Some(n + d)])
            .flatten()
            .filter(|&i| (1..=8).contains(&i))
            .map(|i| self.row[i])
            .find(|&s| s > 0.0)
            .unwrap_or(0.0)
    }
}

// ── following the logs ─────────────────────────────────────────────────────────────────────

struct Follow {
    card: String,
    path: PathBuf,
    offset: Option<u64>,
    partial: Vec<u8>,
}

impl Follow {
    /// Complete new lines since the last call. The first call starts at the end of the file;
    /// a file that shrank (engine restart) is read again from the start.
    fn read_new(&mut self) -> std::io::Result<(Vec<Vec<u8>>, bool)> {
        let mut f = File::open(&self.path)?;
        let len = f.metadata()?.len();
        let Some(off) = self.offset else {
            self.offset = Some(len);
            return Ok((Vec::new(), false));
        };
        let mut reset = false;
        let off = if len < off {
            self.partial.clear();
            reset = true;
            0
        } else {
            off
        };
        if len == off {
            self.offset = Some(off);
            return Ok((Vec::new(), reset));
        }
        f.seek(SeekFrom::Start(off))?;
        let mut chunk = Vec::new();
        (&mut f).take(64 << 20).read_to_end(&mut chunk)?;
        self.offset = Some(off + chunk.len() as u64);
        self.partial.extend_from_slice(&chunk);
        let mut lines = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            if line.len() > 1 {
                lines.push(line);
            }
        }
        Ok((lines, reset))
    }
}

struct Start {
    ts: i64,
    /// expected completion tokens at arrival: the recent average, capped by what was asked for
    completion_est: f64,
    loads: HashMap<String, u32>,
    lead_cards: Vec<String>,
    warm: bool,
}

/// The follower's state, separate from IO so tests can feed events.
pub struct Tracker {
    lanes: IndexMap<String, u32>,
    inflight: HashMap<String, HashMap<String, Start>>,
    speed: HashMap<String, Speed>,
    /// recent completion tokens (EWMA over all cards)
    completion_avg: f64,
    recoveries: HashMap<String, u64>,
    pub lead_cards: Vec<String>,
    started_ms: i64,
    warmup_ms: i64,
}

impl Tracker {
    pub fn new(lanes: IndexMap<String, u32>, started_ms: i64, warmup_seconds: u64) -> Self {
        Self {
            inflight: lanes.keys().map(|c| (c.clone(), HashMap::new())).collect(),
            speed: lanes
                .keys()
                .map(|c| (c.clone(), Speed::default()))
                .collect(),
            completion_avg: 0.0,
            recoveries: HashMap::new(),
            lanes,
            lead_cards: Vec::new(),
            started_ms,
            warmup_ms: warmup_seconds as i64 * 1000,
        }
    }

    pub fn loads(&self) -> HashMap<String, u32> {
        self.inflight
            .iter()
            .map(|(c, m)| (c.clone(), m.len() as u32))
            .collect()
    }

    /// An engine restarted (its log was reset): forget what was in flight there.
    pub fn reset(&mut self, card: &str) {
        if let Some(m) = self.inflight.get_mut(card) {
            m.clear();
        }
    }

    /// One log event on `card`; returns decision/error lines to write.
    pub fn event(
        &mut self,
        card: &str,
        ev: &Value,
        p: &PlacementConfig,
        keep_tokens: u64,
    ) -> Vec<Value> {
        let ts = ev
            .get("timestamp_unix_ms")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let key = || {
            format!(
                "{}#{}",
                ev.get("server_instance_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                ev.pointer("/request/request_id")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            )
        };
        let mut out = Vec::new();
        match ev.get("event").and_then(Value::as_str) {
            Some("request_start") => {
                let loads = self.loads();
                let asked = ev
                    .pointer("/request/requested_output_tokens")
                    .and_then(Value::as_f64)
                    .filter(|&t| t > 0.0)
                    .unwrap_or(f64::MAX);
                let s = Start {
                    ts,
                    completion_est: self.completion_avg.min(asked),
                    loads,
                    lead_cards: self.lead_cards.clone(),
                    warm: ts - self.started_ms < self.warmup_ms,
                };
                self.inflight
                    .entry(card.to_string())
                    .or_default()
                    .insert(key(), s);
                // drop starts whose done never came (2 h)
                for m in self.inflight.values_mut() {
                    m.retain(|_, s| ts - s.ts < 7_200_000);
                }
            }
            Some("request_done") => {
                let Some(s) = self.inflight.get_mut(card).and_then(|m| m.remove(&key())) else {
                    return out;
                };
                out.push(self.score(card, ev, &s, ts, p, keep_tokens));
                let num = |ptr: &str| ev.pointer(ptr).and_then(Value::as_f64).unwrap_or(0.0);
                let completion = num("/result/completion_tokens");
                if let Some(sp) = self.speed.get_mut(card) {
                    sp.learn(
                        s.loads.get(card).copied().unwrap_or(0) + 1,
                        completion,
                        num("/timings_seconds/decode"),
                        num("/result/computed_prefill_tokens"),
                        num("/timings_seconds/prefill"),
                    );
                }
                self.completion_avg = ewma(self.completion_avg, completion);
            }
            Some("throughput") => {
                let r = ev
                    .get("engine_recoveries")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let prev = self.recoveries.insert(card.to_string(), r);
                if let Some(prev) = prev
                    && r > prev
                {
                    out.push(json!({"t": iso(ts), "type": "error", "card": card,
                        "error": format!("engine recoveries {prev} -> {r}")}));
                }
            }
            _ => {}
        }
        out
    }

    fn score(
        &self,
        card: &str,
        ev: &Value,
        s: &Start,
        ts: i64,
        p: &PlacementConfig,
        keep_tokens: u64,
    ) -> Value {
        let num = |ptr: &str| ev.pointer(ptr).and_then(Value::as_f64).unwrap_or(0.0);
        let cache_hit = num("/result/prefix_cache_hit_tokens") as u64;
        let completion = num("/result/completion_tokens");
        let queue_wait = num("/engine_timing/queue_wait_seconds");
        let finish = ev
            .pointer("/result/finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let a = Arrival {
            actual: card,
            lanes: &self.lanes,
            loads: &s.loads,
            lead_cards: &s.lead_cards,
            cache_hit,
            speed: Some(&self.speed),
            completion_est: s.completion_est,
        };
        let ch = choose(p, keep_tokens, &a);
        let agree = ch.card == card;
        let mut line = json!({
            "t": iso(s.ts),
            "type": "decision",
            "card": card,
            "chosen": ch.card,
            "agree": agree,
            "reason": ch.reason,
            "wouldQueue": ch.would_queue,
            "loads": s.loads,
            "lead": s.lead_cards,
            "warm": s.warm,
            "queueWait": round3(queue_wait),
            "seconds": round3((ts - s.ts) as f64 / 1000.0),
            "prompt": num("/result/prompt_tokens") as u64,
            "completion": completion as u64,
            "completionEst": s.completion_est.round() as u64,
            "cacheHit": cache_hit,
            "stream": ev.pointer("/request/stream").and_then(Value::as_bool),
            "finish": finish,
        });
        if !agree {
            let y = ch.card.as_str();
            let load = |c: &str| s.loads.get(c).copied().unwrap_or(0);
            let lanes_y = self.lanes.get(y).copied().unwrap_or(1);
            let limit_y = if p.admission && s.lead_cards.iter().any(|l| l == y) {
                lanes_y.saturating_sub(p.lead_reserve).max(1)
            } else {
                lanes_y
            };
            let queue = if load(y) < limit_y { queue_wait } else { 0.0 };
            let g = gain(
                queue,
                completion,
                self.speed.get(card),
                self.speed.get(y),
                load(card) + 1,
                load(y) + 1,
                cache_hit,
            );
            line["gain"] = json!({
                "queue": round3(g.queue),
                "batch": round3(g.batch),
                "cache": round3(-g.cache),
                "net": round3(g.net),
            });
        }
        line
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The UK calendar date of an epoch in ms.
pub fn uk_date(ms: i64) -> String {
    let utc = Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now);
    let local = if crate::ondemand::is_bst(utc) {
        utc + chrono::Duration::hours(1)
    } else {
        utc
    };
    local.format("%Y-%m-%d").to_string()
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// ── output ────────────────────────────────────────────────────────────────────────────────

struct Out {
    dir: PathBuf,
    day: String,
    file: Option<File>,
}

impl Out {
    fn write(&mut self, line: &Value) {
        let ms = line
            .get("t")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or_else(now_ms, |d| d.timestamp_millis());
        let day = uk_date(ms);
        if day != self.day || self.file.is_none() {
            let _ = std::fs::create_dir_all(&self.dir);
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("decisions-{day}.jsonl")))
                .ok();
            self.day = day;
        }
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn pct(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[((s.len() - 1) as f64 * q).round() as usize]
}

const OK_FINISH: &[&str] = &[
    "stop",
    "stop_token",
    "length",
    "max_tokens",
    "tool_calls",
    "stop_sequence",
    "eos",
    "",
];

/// The day's summary from its decisions file: (JSON, Markdown).
pub fn summarize(dir: &Path, day: &str) -> (Value, String) {
    let text =
        std::fs::read_to_string(dir.join(format!("decisions-{day}.jsonl"))).unwrap_or_default();
    let mut n = 0u64;
    let mut warm = 0u64;
    let mut agree = 0u64;
    let mut reasons: IndexMap<String, u64> = IndexMap::new();
    let mut moved: IndexMap<String, u64> = IndexMap::new();
    let mut per_card: IndexMap<String, u64> = IndexMap::new();
    let mut finishes: IndexMap<String, u64> = IndexMap::new();
    let mut nets: Vec<f64> = Vec::new();
    let mut queue_gain = 0.0;
    let mut would_queue = 0u64;
    let mut waited = 0u64;
    let mut waited_s = 0.0;
    let mut top: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut abnormal = 0u64;
    let mut err_n = 0u64;
    let (mut can_n, mut can_err_r, mut can_err_l) = (0u64, 0u64, 0u64);
    let mut over: Vec<f64> = Vec::new();
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("decision") => {
                if v.get("warm").and_then(Value::as_bool) == Some(true) {
                    warm += 1;
                    continue;
                }
                n += 1;
                let card = v["card"].as_str().unwrap_or("").to_string();
                *per_card.entry(card.clone()).or_default() += 1;
                let reason = v["reason"].as_str().unwrap_or("").to_string();
                *reasons.entry(reason.clone()).or_default() += 1;
                let f = v["finish"].as_str().unwrap_or("").to_string();
                if !OK_FINISH.contains(&f.as_str()) {
                    abnormal += 1;
                }
                *finishes.entry(f).or_default() += 1;
                let qw = v["queueWait"].as_f64().unwrap_or(0.0);
                if qw > 0.5 {
                    waited += 1;
                    waited_s += qw;
                }
                if v["wouldQueue"].as_bool() == Some(true) {
                    would_queue += 1;
                }
                if v["agree"].as_bool() == Some(true) {
                    agree += 1;
                } else {
                    *moved.entry(reason).or_default() += 1;
                    let net = v
                        .pointer("/gain/net")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    queue_gain += v
                        .pointer("/gain/queue")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    nets.push(net);
                    top.push(v.clone());
                }
            }
            Some("canary") => {
                can_n += 1;
                let rs = v
                    .pointer("/router/status")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let ls = v
                    .pointer("/litellm/status")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if rs != 200 {
                    can_err_r += 1;
                }
                if ls != 200 {
                    can_err_l += 1;
                }
                if rs == 200 && ls == 200 {
                    let r = v
                        .pointer("/router/ms")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let l2 = v
                        .pointer("/litellm/ms")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    over.push(r - l2);
                }
            }
            Some("error") => {
                err_n += 1;
                if errors.len() < 50 {
                    errors.push(format!(
                        "{} {}",
                        v["t"].as_str().unwrap_or(""),
                        v["error"].as_str().unwrap_or("")
                    ));
                }
            }
            _ => {}
        }
    }
    top.sort_by(|a, b| {
        let g = |v: &Value| {
            v.pointer("/gain/net")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        g(b).partial_cmp(&g(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    top.truncate(5);
    let gain_pos = nets.iter().filter(|&&x| x > 0.0).count();
    let gain_1s = nets.iter().filter(|&&x| x > 1.0).count();
    let agree_pct = if n > 0 {
        agree as f64 * 100.0 / n as f64
    } else {
        0.0
    };
    let sum_net: f64 = nets.iter().sum();
    let j = json!({
        "day": day,
        "requests": n + warm,
        "scored": n,
        "warmup": warm,
        "perCard": per_card,
        "agreePct": round3(agree_pct),
        "agree": agree,
        "differ": n - agree,
        "reasons": reasons,
        "differByReason": moved,
        "predictedGain": {
            "netSumSeconds": round3(sum_net),
            "netMedianSeconds": round3(pct(&nets, 0.5)),
            "netP90Seconds": round3(pct(&nets, 0.9)),
            "positive": gain_pos,
            "over1s": gain_1s,
            "queueSeconds": round3(queue_gain),
        },
        "actualQueue": {"requestsWaitedOver0_5s": waited, "secondsWaited": round3(waited_s)},
        "wouldQueueEverywhere": would_queue,
        "finishes": finishes,
        "errors": {"abnormalFinishes": abnormal, "events": err_n, "list": errors},
        "canary": {
            "pairs": can_n,
            "routerErrors": can_err_r,
            "litellmErrors": can_err_l,
            "addedMsMedian": round3(pct(&over, 0.5)),
            "addedMsP90": round3(pct(&over, 0.9)),
        },
        "top": top,
    });
    let mut md = format!(
        "# Router shadow, {day}\n\n\
         - Requests seen: {} ({} scored, {} in warm-up)\n\
         - Per card: {}\n\
         - Agreement with what LiteLLM/qfn-lb (and pinned clients) did: **{:.1}%** ({} of {})\n\
         - Router would have chosen differently: {} (by reason: {})\n\
         - Predicted gain where it differs: net {:.1} s in total, median {:.2} s, p90 {:.2} s; \
         {} with a gain, {} over 1 s; queue time avoided {:.1} s\n\
         - Actual engine queueing: {} requests waited over 0.5 s, {:.1} s in total; \
         all cards full at arrival: {}\n\
         - Errors: {} abnormal finishes, {} error events\n\
         - Added latency (canary pairs {}): router minus LiteLLM median {:.0} ms, p90 {:.0} ms; \
         errors router {}, LiteLLM {}\n",
        n + warm,
        n,
        warm,
        serde_json::to_string(&j["perCard"]).unwrap_or_default(),
        agree_pct,
        agree,
        n,
        n - agree,
        serde_json::to_string(&j["differByReason"]).unwrap_or_default(),
        sum_net,
        pct(&nets, 0.5),
        pct(&nets, 0.9),
        gain_pos,
        gain_1s,
        queue_gain,
        waited,
        waited_s,
        would_queue,
        abnormal,
        err_n,
        can_n,
        pct(&over, 0.5),
        pct(&over, 0.9),
        can_err_r,
        can_err_l,
    );
    if !top.is_empty() {
        md.push_str("\nLargest predicted gains:\n\n");
        for t in &top {
            md.push_str(&format!(
                "- {} {} -> {} ({}), loads {}, queued {} s, {} tokens out, net {} s\n",
                t["t"].as_str().unwrap_or(""),
                t["card"].as_str().unwrap_or(""),
                t["chosen"].as_str().unwrap_or(""),
                t["reason"].as_str().unwrap_or(""),
                t["loads"],
                t["queueWait"],
                t["completion"],
                t.pointer("/gain/net").unwrap_or(&Value::Null),
            ));
        }
    }
    (j, md)
}

fn write_summary(dir: &Path, day: &str) {
    let (j, md) = summarize(dir, day);
    let _ = std::fs::write(
        dir.join(format!("summary-{day}.json")),
        serde_json::to_string_pretty(&j).unwrap_or_default(),
    );
    let _ = std::fs::write(dir.join(format!("summary-{day}.md")), md);
}

/// GET /observe[?date=]: the day's summary (today by default) plus the live view.
pub fn view(st: &AppState, query: Option<&str>) -> Value {
    let snap = st.snapshot();
    let o = &snap.config.observe;
    let day = query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("date=")))
        .map_or_else(|| uk_date(now_ms()), str::to_string);
    let (j, _) = summarize(Path::new(&o.dir), &day);
    json!({"enabled": o.enabled, "dir": o.dir, "live": st.ext.observer.live(), "summary": j})
}

// ── the loops ─────────────────────────────────────────────────────────────────────────────

fn lanes_of(snap: &Snapshot) -> IndexMap<String, u32> {
    snap.config
        .observe
        .ninfer_logs
        .keys()
        // only cards that run ninfer now (another engine writes no such log: it would look idle)
        .filter_map(|c| {
            let u = snap.config.upstreams.get(c)?;
            (u.online && u.engine == "ninfer").then(|| (c.clone(), u.lanes.max(1)))
        })
        .collect()
}

async fn lead_cards(st: &AppState, url: &str) -> Result<Vec<String>, String> {
    let v: Value = st
        .http
        .get(format!("{}/list", url.trim_end_matches('/')))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("pig-slots: {e}"))?
        .json()
        .await
        .map_err(|e| format!("pig-slots: {e}"))?;
    let mut out: Vec<String> = Vec::new();
    if let Some(slots) = v.get("slots").and_then(Value::as_object) {
        for (name, lease) in slots {
            if lease.get("kind").and_then(Value::as_str) == Some("main") {
                let card: String = name
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .collect();
                if !card.is_empty() && !out.contains(&card) {
                    out.push(card);
                }
            }
        }
    }
    Ok(out)
}

/// The follower: runs for the life of the process, idle while `observe.enabled` is false.
pub async fn run(st: Arc<AppState>) {
    let mut tracker: Option<Tracker> = None;
    let mut follows: Vec<Follow> = Vec::new();
    let mut out = Out {
        dir: PathBuf::new(),
        day: String::new(),
        file: None,
    };
    let mut sig = String::new();
    let mut last_pig = Instant::now() - Duration::from_secs(60);
    let mut today = uk_date(now_ms());
    let mut last_err = Instant::now() - Duration::from_secs(600);
    loop {
        let snap = st.snapshot();
        let o: &ObserveConfig = &snap.config.observe;
        if !o.enabled {
            tracker = None;
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // (re)start when the logs, lanes or output dir change
        let now_sig = format!("{:?}|{:?}|{}", o.ninfer_logs, lanes_of(&snap), o.dir);
        if tracker.is_none() || now_sig != sig {
            sig = now_sig;
            tracker = Some(Tracker::new(lanes_of(&snap), now_ms(), o.warmup_seconds));
            let live_cards = lanes_of(&snap);
            follows = o
                .ninfer_logs
                .iter()
                .filter(|(c, _)| live_cards.contains_key(*c))
                .map(|(c, p)| Follow {
                    card: c.clone(),
                    path: PathBuf::from(p),
                    offset: None,
                    partial: Vec::new(),
                })
                .collect();
            out = Out {
                dir: PathBuf::from(&o.dir),
                day: String::new(),
                file: None,
            };
            tracing::info!(
                "observe: following {} engine logs into {}",
                follows.len(),
                o.dir
            );
        }
        let tr = tracker.as_mut().unwrap();
        if let Some(url) = &o.pig_slots_url
            && last_pig.elapsed() >= Duration::from_secs(5)
        {
            last_pig = Instant::now();
            match lead_cards(&st, url).await {
                Ok(l) => tr.lead_cards = l,
                Err(e) => {
                    st.ext.observer.error(e.clone());
                    if last_err.elapsed() > Duration::from_secs(300) {
                        last_err = Instant::now();
                        out.write(&json!({"t": iso(now_ms()), "type": "error", "error": e}));
                    }
                }
            }
        }
        let mut evs: Vec<(i64, String, Value)> = Vec::new();
        for f in follows.iter_mut() {
            match f.read_new() {
                Ok((lines, reset)) => {
                    if reset {
                        tr.reset(&f.card);
                    }
                    for l in lines {
                        if let Ok(v) = serde_json::from_slice::<Value>(&l) {
                            let ts = v
                                .get("timestamp_unix_ms")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            evs.push((ts, f.card.clone(), v));
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("{}: {e}", f.path.display());
                    st.ext.observer.error(msg.clone());
                    if last_err.elapsed() > Duration::from_secs(300) {
                        last_err = Instant::now();
                        out.write(&json!({"t": iso(now_ms()), "type": "error", "error": msg}));
                    }
                }
            }
        }
        evs.sort_by_key(|(ts, _, _)| *ts);
        let mut scored = 0u64;
        for (_, card, v) in &evs {
            for line in tr.event(card, v, &snap.config.placement, o.cache_keep_tokens) {
                if line["type"] == "decision" {
                    scored += 1;
                }
                out.write(&line);
            }
        }
        st.ext.observer.set(tr.loads(), tr.lead_cards.clone());
        if scored > 0 {
            st.ext.observer.inner.lock().unwrap().scored += scored;
        }
        // day rollover: summarise the day that ended
        let d = uk_date(now_ms());
        if d != today {
            write_summary(Path::new(&o.dir), &today);
            tracing::info!("observe: wrote summary-{today}");
            today = d;
        }
        tokio::time::sleep(Duration::from_millis(o.poll_ms.max(200))).await;
    }
}

async fn timed_chat(st: &AppState, base: &str, body: &Value) -> Value {
    let t0 = Instant::now();
    let r = st
        .http
        .post(format!(
            "{}/v1/chat/completions",
            base.trim_end_matches('/')
        ))
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await;
    match r {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let card = resp
                .headers()
                .get("x-abhoth-card")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let _ = resp.bytes().await;
            json!({"status": status, "ms": round3(t0.elapsed().as_secs_f64() * 1000.0), "card": card})
        }
        Err(e) => json!({"status": 0, "ms": round3(t0.elapsed().as_secs_f64() * 1000.0),
                         "error": e.to_string()}),
    }
}

/// The canary: one tiny identical chat through LiteLLM and through the router every
/// `everySeconds`, alternating which goes first; skipped while any card is short of free lanes,
/// in the 06:45-07:30 UK window, or when the model is not a card deployment.
pub async fn canary(st: Arc<AppState>) {
    let mut flip = false;
    let mut out = Out {
        dir: PathBuf::new(),
        day: String::new(),
        file: None,
    };
    loop {
        let snap = st.snapshot();
        let o = &snap.config.observe;
        let c = &o.canary;
        let every = c.every_seconds;
        if !o.enabled || every == 0 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(every)).await;
        let snap = st.snapshot();
        let o = &snap.config.observe;
        let c = &o.canary;
        let (Some(lite), Some(rt)) = (&c.litellm_url, &c.router_url) else {
            continue;
        };
        if crate::ondemand::quiet(crate::proxy::now_epoch()).is_some() {
            continue;
        }
        // only a card deployment (never a relay name: that could switch a GPU)
        let deps = snap.deployments_named(&c.model);
        if deps.is_empty() || deps.iter().any(|(_, d)| d.cards.is_empty()) {
            continue;
        }
        let busy = deps.iter().flat_map(|(_, d)| d.cards.iter()).any(|card| {
            let lanes = snap
                .config
                .upstreams
                .get(card)
                .map_or(1, |u| u.lanes.max(1));
            let load = st.ext.observer.external_load(card);
            // no current view of the engines = do not send
            load.is_none_or(|l| lanes.saturating_sub(l) < c.min_free_lanes)
        });
        if busy {
            continue;
        }
        let body = json!({
            "model": c.model,
            "messages": [{"role": "user", "content": "Reply with the single word OK."}],
            "max_tokens": c.max_tokens,
            "chat_template_kwargs": {"enable_thinking": false},
        });
        flip = !flip;
        let (l, r) = if flip {
            let l = timed_chat(&st, lite, &body).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            (l, timed_chat(&st, rt, &body).await)
        } else {
            let r = timed_chat(&st, rt, &body).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            (timed_chat(&st, lite, &body).await, r)
        };
        if out.dir.as_os_str() != o.dir.as_str() {
            out = Out {
                dir: PathBuf::from(&o.dir),
                day: String::new(),
                file: None,
            };
        }
        out.write(
            &json!({"t": iso(now_ms()), "type": "canary", "model": c.model,
            "first": if flip {"litellm"} else {"router"}, "litellm": l, "router": r}),
        );
    }
}

/// `abhoth router observe-summary --dir D [--date YYYY-MM-DD]`: print a day's summary.
pub fn summary_cli(dir: &str, day: Option<String>) -> String {
    let day = day.unwrap_or_else(|| uk_date(now_ms()));
    summarize(Path::new(dir), &day).1
}

#[cfg(test)]
mod tests;
