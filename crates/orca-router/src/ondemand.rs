//! Built-in on-demand switching (ROUTER.md SHOULD, phase 3): a request for a catalog model no
//! card runs is decided here instead of in the profiles API relay. The decision is a port of
//! `ondemand.decide()` (pure, with its unit tests); layoutctl's runner still performs the switch,
//! asked through the profiles API (`POST /compose`, `GET /plan`, `POST /switch`, `GET /jobs/<id>`).
//!
//! `dryRun` (the default, and the only mode allowed in shadow) decides and logs, never asks for a
//! switch, and answers 503 with what it would have done.
//!
//! Draining (`POST /admin/cards/<card>/drain`) is here too: no new requests for the card, then
//! wait (bounded) for the ones in flight; layoutctl can switch after it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::{Datelike, TimeZone, Timelike, Utc};
use futures_util::StreamExt;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::config::{Deployment, OnDemandConfig, OnDemandModel, Snapshot};
use crate::proxy::{self, LogRec, RequestCtx, now_epoch};
use crate::state::AppState;

// ── the decision (ondemand.decide) ─────────────────────────────────────────────────────────

/// One card, as decide() sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardIn {
    /// The catalog model it runs.
    pub model: Option<String>,
    pub offline: bool,
    /// None = unknown.
    pub busy: Option<bool>,
    pub last_busy: Option<f64>,
    pub last_change: Option<f64>,
    /// Never picked for an on-demand switch (it may still serve the model when it runs it).
    pub excluded: bool,
}

/// Host guards written by the layouts runner (state/guards.json).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Guards {
    pub at: f64,
    pub video: Option<String>,
    pub bench: Option<String>,
    pub music: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub state: String,
    pub layout: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub job: String,
    pub card: String,
    pub started: f64,
    pub estimate: Option<f64>,
    pub layout: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Live {
        cards: Vec<String>,
    },
    Wait {
        reason: String,
        switching: bool,
        card: Option<String>,
    },
    Switch {
        card: String,
        reason: String,
    },
    Block {
        reason: String,
        retry: u64,
    },
    Unknown {
        reason: String,
    },
}

impl Decision {
    pub fn action(&self) -> &'static str {
        match self {
            Decision::Live { .. } => "live",
            Decision::Wait { .. } => "wait",
            Decision::Switch { .. } => "switch",
            Decision::Block { .. } => "block",
            Decision::Unknown { .. } => "unknown",
        }
    }

    pub fn reason(&self) -> String {
        match self {
            Decision::Live { cards } => format!("live on {}", cards.join(", ")),
            Decision::Wait { reason, .. }
            | Decision::Switch { reason, .. }
            | Decision::Block { reason, .. }
            | Decision::Unknown { reason } => reason.clone(),
        }
    }
}

fn wait(reason: impl Into<String>) -> Decision {
    Decision::Wait {
        reason: reason.into(),
        switching: false,
        card: None,
    }
}

/// The never-switch conditions: (reason, retry seconds).
pub fn blockers(
    g: &Guards,
    quiet: Option<u64>,
    hold: bool,
    offline: &[String],
) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Some(q) = quiet {
        out.push((
            "06:45-07:30 UK: AI News runs at 07:00, no switches".to_string(),
            q,
        ));
    }
    if let Some(v) = &g.video {
        out.push((format!("a video pipeline is running ({v})"), 300));
    }
    if let Some(b) = &g.bench {
        out.push((format!("the OpenOrca bench queue is running ({b})"), 600));
    }
    if g.music {
        out.push(("music mode is on".to_string(), 600));
    }
    if hold {
        out.push((
            "the gateway is on hold (layoutctl gateway --hold)".to_string(),
            300,
        ));
    }
    if !offline.is_empty() {
        let mut o = offline.to_vec();
        o.sort();
        out.push((format!("card(s) {} offline", o.join(", ")), 600));
    }
    out
}

/// `decide()`: what to do for a request for `target`. Pure: every input is passed in.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    target: &str,
    now: f64,
    cfg: &OnDemandConfig,
    models: &IndexMap<String, OnDemandModel>,
    cards: &BTreeMap<String, CardIn>,
    guards: Option<&Guards>,
    quiet: Option<u64>,
    hold: bool,
    job: Option<&Job>,
    leases: Option<&[(String, String)]>,
    pending: Option<&Pending>,
) -> Decision {
    if !cfg.enabled {
        return Decision::Block {
            reason: "on-demand switching is off (gateway.json onDemand.enabled)".into(),
            retry: 3600,
        };
    }
    let Some(md) = models.get(target) else {
        return Decision::Unknown {
            reason: format!("'{target}' is not a switchable catalog model"),
        };
    };
    let live: Vec<String> = cards
        .iter()
        .filter(|(_, v)| v.model.as_deref() == Some(md.model.as_str()) && !v.offline)
        .map(|(c, _)| c.clone())
        .collect();
    if !live.is_empty() {
        return Decision::Live { cards: live };
    }
    if let Some(p) = pending {
        return Decision::Wait {
            reason: format!("switching card {} to {target}", p.card),
            switching: true,
            card: Some(p.card.clone()),
        };
    }
    let offline: Vec<String> = cards
        .iter()
        .filter(|(_, v)| v.offline)
        .map(|(c, _)| c.clone())
        .collect();
    let stale = guards.is_none_or(|g| now - g.at > 45.0);
    let b = blockers(guards.unwrap_or(&Guards::default()), quiet, hold, &offline);
    if !b.is_empty() {
        return Decision::Block {
            reason: b
                .iter()
                .map(|(r, _)| r.as_str())
                .collect::<Vec<_>>()
                .join("; "),
            retry: b.iter().map(|(_, s)| *s).max().unwrap_or(60),
        };
    }
    if let Some(j) = job {
        return wait(format!("another switch is {} ({})", j.state, j.layout));
    }
    if stale {
        return wait("the layouts runner has not reported the video/bench guards for 45 s");
    }
    let Some(leases) = leases else {
        return wait("pig-slots is not answering");
    };
    let mut why = Vec::new();
    let mut ok = Vec::new();
    for (c, v) in cards {
        if v.excluded {
            why.push(format!("card {c} is excluded from on-demand switches"));
            continue;
        }
        if let Some(lc) = v.last_change {
            let stay = cfg.min_stay_seconds as f64 - (now - lc);
            if stay > 0.0 {
                why.push(format!(
                    "card {c} switched {} s ago (minimum stay {} s)",
                    (now - lc) as i64,
                    cfg.min_stay_seconds
                ));
                continue;
            }
        }
        let mine: Vec<String> = leases
            .iter()
            .filter(|(s, _)| {
                let sc = s.chars().next().map(|x| x.to_string()).unwrap_or_default();
                sc == *c || !cards.contains_key(&sc)
            })
            .map(|(s, w)| format!("{s} ({w})"))
            .collect();
        if !mine.is_empty() {
            why.push(format!(
                "card {c} has pig-slots leases: {}",
                mine.join(", ")
            ));
            continue;
        }
        let Some(busy) = v.busy else {
            why.push(format!("card {c}: load unknown"));
            continue;
        };
        let idle_for = v.last_busy.map(|t| now - t);
        if busy || idle_for.is_some_and(|i| i < cfg.idle_grace_seconds as f64) {
            why.push(format!("card {c} is busy"));
            continue;
        }
        ok.push(c.clone());
    }
    if ok.is_empty() {
        return wait(if why.is_empty() {
            "no card can switch".to_string()
        } else {
            why.join("; ")
        });
    }
    // the card idle longest (never seen busy = idle longest); a tie goes to B (the fast card A stays)
    let card = ok
        .iter()
        .min_by(|a, b| {
            let ka = (cards[*a].last_busy.unwrap_or(0.0), *a != "B");
            let kb = (cards[*b].last_busy.unwrap_or(0.0), *b != "B");
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
        .unwrap();
    Decision::Switch {
        reason: format!("card {card} idle longest"),
        card,
    }
}

// ── UK time (no tz database: the Europe/London rule) ───────────────────────────────────────

/// Is `utc` in British Summer Time (last Sunday of March 01:00 UTC to last Sunday of October
/// 01:00 UTC)?
pub fn is_bst(utc: chrono::DateTime<Utc>) -> bool {
    let y = utc.year();
    let last_sunday = |month: u32| {
        let mut d = Utc.with_ymd_and_hms(y, month, 31, 1, 0, 0).unwrap();
        while d.weekday() != chrono::Weekday::Sun {
            d -= chrono::Duration::days(1);
        }
        d
    };
    utc >= last_sunday(3) && utc < last_sunday(10)
}

/// London wall-clock (hour, minute, second) for an epoch.
pub fn uk_hms(epoch: f64) -> (u32, u32, u32) {
    let utc = Utc
        .timestamp_opt(epoch as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let local = if is_bst(utc) {
        utc + chrono::Duration::hours(1)
    } else {
        utc
    };
    (local.hour(), local.minute(), local.second())
}

/// Seconds until the 06:45-07:30 window ends when `epoch` is inside it (ondemand.quiet_retry,
/// at least 60), else None.
pub fn quiet(epoch: f64) -> Option<u64> {
    let (h, m, s) = uk_hms(epoch);
    let t = h * 3600 + m * 60 + s;
    let (start, end) = (6 * 3600 + 45 * 60, 7 * 3600 + 30 * 60);
    (start..end)
        .contains(&t)
        .then(|| u64::from(end - t).max(60))
}

/// Per-card epoch of the last change from live.json's lastSwitch: an on-demand switch the
/// router started (in `mine`: job -> card) changed one card, any other switch both.
pub fn last_changes(
    live: &Value,
    mine: &HashMap<String, String>,
    own: &HashMap<String, f64>,
    cards: &[String],
) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = own.clone();
    let ls = live.get("lastSwitch");
    let at = ls.and_then(|l| l.get("at")).and_then(Value::as_str);
    if let Some(t) =
        at.and_then(|a| chrono::DateTime::parse_from_rfc3339(&a.replace('Z', "+00:00")).ok())
    {
        let t = t.timestamp() as f64;
        let job = ls
            .and_then(|l| l.get("job"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let changed: Vec<String> = match mine.get(job) {
            Some(c) if cards.contains(c) => vec![c.clone()],
            _ => cards.to_vec(),
        };
        for c in changed {
            let e = out.entry(c).or_insert(0.0);
            *e = e.max(t);
        }
    }
    out
}

// ── the coordinator: one switch per target, however many requests wait ────────────────────

#[derive(Default)]
struct Inner {
    pending: HashMap<String, Pending>,
    failed: HashMap<String, (f64, String)>,
    /// job id -> card, for switches the router started
    jobs: HashMap<String, String>,
    /// card -> epoch of a switch the router started
    changed: HashMap<String, f64>,
    /// card -> (busy from /slots, last busy epoch)
    load: HashMap<String, (Option<bool>, Option<f64>)>,
    recent: VecDeque<Value>,
    noted: HashMap<String, (String, String)>,
}

#[derive(Default)]
pub struct Coordinator {
    inner: Mutex<Inner>,
    ensure_lock: tokio::sync::Mutex<()>,
    /// On-demand switches started (rate limits).
    pub limiter: crate::switch::gate::RateLimiter,
}

impl Coordinator {
    fn note(&self, target: &str, d: &Decision, who: &str, dry: bool) {
        let mut g = self.inner.lock().unwrap();
        let reason = d.reason();
        let key = (
            d.action().to_string(),
            reason.chars().filter(|c| !c.is_ascii_digit()).collect(),
        );
        if g.noted.get(target) == Some(&key) || d.action() == "live" {
            g.noted.insert(target.to_string(), key);
            return;
        }
        g.noted.insert(target.to_string(), key);
        let rec = json!({"at": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "event": "decision", "target": target, "who": who, "action": d.action(), "reason": reason,
            "dryRun": dry});
        tracing::info!("ondemand: {rec}");
        g.recent.push_front(rec);
        g.recent.truncate(30);
    }

    fn log(&self, rec: Value) {
        tracing::info!("ondemand: {rec}");
        let mut g = self.inner.lock().unwrap();
        g.recent.push_front(rec);
        g.recent.truncate(30);
    }
}

fn read_json(path: Option<&str>) -> Option<Value> {
    let p = path?;
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn guards_from(v: &Value) -> Guards {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    Guards {
        at: v.get("at").and_then(Value::as_f64).unwrap_or(0.0),
        video: s("video"),
        bench: s("bench"),
        music: v.get("music").and_then(Value::as_bool).unwrap_or(false),
    }
}

async fn get_json(st: &AppState, url: &str) -> Option<Value> {
    let r = st
        .http
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

async fn post_json(st: &AppState, url: &str, body: &Value) -> Option<(u16, Value)> {
    let r = st
        .http
        .post(url)
        .timeout(Duration::from_secs(15))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .ok()?;
    let code = r.status().as_u16();
    Some((code, r.json().await.unwrap_or(Value::Null)))
}

/// Everything decide() needs, gathered now.
struct Inputs {
    cards: BTreeMap<String, CardIn>,
    guards: Option<Guards>,
    quiet: Option<u64>,
    hold: bool,
    job: Option<Job>,
    leases: Option<Vec<(String, String)>>,
}

async fn inputs(st: &AppState, snap: &Snapshot, now: f64) -> Inputs {
    let od = &snap.config.on_demand;
    let names: Vec<String> = snap.config.upstreams.keys().cloned().collect();
    let live = read_json(od.live_file.as_deref()).unwrap_or(Value::Null);
    let (jobs, changed, load) = {
        let g = st.ext.ondemand.inner.lock().unwrap();
        (g.jobs.clone(), g.changed.clone(), g.load.clone())
    };
    let changes = last_changes(&live, &jobs, &changed, &names);
    let mut cards = BTreeMap::new();
    for (c, u) in &snap.config.upstreams {
        let rt = st.card(c);
        let (polled, last_polled) = load.get(c).cloned().unwrap_or((None, None));
        // without polling, the router's own in-flight count is the load
        let busy = if rt.inflight > 0 {
            Some(true)
        } else if od.poll_slots {
            polled
        } else {
            Some(false)
        };
        let own_last = rt.last_busy.map(|t| now - t.elapsed().as_secs_f64());
        let last_busy = match (own_last, last_polled) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        cards.insert(
            c.clone(),
            CardIn {
                model: u.catalog_model.clone(),
                offline: !u.online,
                busy,
                last_busy,
                last_change: changes.get(c).copied().filter(|t| *t > 0.0),
                excluded: od.exclude_cards.iter().any(|x| x == c),
            },
        );
    }
    let guards = read_json(od.guards_file.as_deref()).map(|v| guards_from(&v));
    let hold = od
        .hold_file
        .as_deref()
        .is_some_and(|p| std::path::Path::new(p).exists());
    let api = od.profiles_api.trim_end_matches('/');
    let job = if api.is_empty() {
        None
    } else {
        get_json(st, &format!("{api}/live"))
            .await
            .and_then(|v| v.get("job").cloned())
            .filter(|j| !j.is_null())
            .map(|j| Job {
                state: j
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("running")
                    .to_string(),
                layout: j
                    .get("layout")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            })
    };
    let leases = if snap.config.slots.enabled {
        Some(st.ext.registry.held(snap))
    } else if let Some(u) = &od.pig_slots_url {
        get_json(st, &format!("{}/list", u.trim_end_matches('/')))
            .await
            .map(|v| {
                v.get("slots")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter(|(_, h)| h.is_object())
                            .map(|(s, h)| {
                                let who = h.get("owner").and_then(Value::as_str).unwrap_or("?");
                                (s.clone(), who.to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
    } else {
        None
    };
    Inputs {
        cards,
        guards,
        quiet: quiet(now),
        hold,
        job,
        leases,
    }
}

/// Settle pending switches that ended (done / refused / failed / timed out).
async fn progress(st: &AppState, snap: &Snapshot, now: f64) {
    let pend: Vec<(String, Pending)> = {
        let g = st.ext.ondemand.inner.lock().unwrap();
        g.pending
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let api = snap
        .config
        .on_demand
        .profiles_api
        .trim_end_matches('/')
        .to_string();
    for (target, p) in pend {
        let j = get_json(st, &format!("{api}/jobs/{}", p.job)).await;
        let state = j
            .as_ref()
            .and_then(|j| j.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("queued")
            .to_string();
        let running = state == "queued" || state == "running";
        if running && now - p.started < snap.config.on_demand.switch_timeout_seconds as f64 {
            continue;
        }
        let mut g = st.ext.ondemand.inner.lock().unwrap();
        g.pending.remove(&target);
        if state != "done" {
            let err = j
                .as_ref()
                .and_then(|j| j.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if running {
                        format!(
                            "switch not done after {} s",
                            snap.config.on_demand.switch_timeout_seconds
                        )
                    } else {
                        format!("switch {state}")
                    }
                });
            g.failed.insert(target.clone(), (now, err));
        }
        drop(g);
        st.ext.ondemand.log(
            json!({"event": "switch-ended", "target": target, "card": p.card,
            "job": p.job, "state": state}),
        );
    }
}

/// One decision for `target` (Coordinator.ensure): may start a switch (not in dry-run).
/// The control plane's gate before a switch starts: `Ok(principal)` to go ahead. With
/// `control_gate` off it always says yes, exactly as before the control plane.
pub fn gate(
    od: &OnDemandConfig,
    adm: &crate::switch::gate::ChatAdmission,
    limiter: &crate::switch::gate::RateLimiter,
    now: f64,
) -> Result<Option<String>, (String, u64)> {
    if !od.control_gate {
        return Ok(None);
    }
    let rules = crate::switch::gate::GateRules {
        require_key: od.require_key,
        per_hour: od.max_switches_per_hour,
        per_principal_per_hour: od.max_switches_per_principal_per_hour,
    };
    crate::switch::gate::admit(adm, &rules, limiter, now).map(|t| t.caller.principal)
}

pub async fn ensure(
    st: &AppState,
    snap: &Snapshot,
    adm: &crate::switch::gate::ChatAdmission,
    target: &str,
    who: &str,
) -> Decision {
    let co = &st.ext.ondemand;
    let _one = co.ensure_lock.lock().await;
    let od = &snap.config.on_demand;
    let now = now_epoch();
    if !od.dry_run {
        progress(st, snap, now).await;
    }
    let failed = co.inner.lock().unwrap().failed.get(target).cloned();
    let pending = co.inner.lock().unwrap().pending.get(target).cloned();
    if let Some((t, err)) = failed
        && now - t < od.failure_hold_seconds as f64
        && pending.is_none()
    {
        let d = Decision::Block {
            reason: format!("the switch to {target} did not work: {err}"),
            retry: (od.failure_hold_seconds as f64 - (now - t)) as u64 + 1,
        };
        co.note(target, &d, who, od.dry_run);
        return d;
    }
    let inp = inputs(st, snap, now).await;
    let d = decide(
        target,
        now,
        od,
        &od.models,
        &inp.cards,
        inp.guards.as_ref(),
        inp.quiet,
        inp.hold,
        inp.job.as_ref(),
        inp.leases.as_deref(),
        pending.as_ref(),
    );
    st.metrics.ondemand(d.action());
    let d = match d {
        Decision::Switch { card, reason } if od.dry_run => Decision::Block {
            reason: format!("dry run: would switch card {card} to {target} ({reason})"),
            retry: 60,
        },
        Decision::Switch { card, .. } => match gate(od, adm, &co.limiter, now) {
            Err((reason, retry)) => Decision::Block { reason, retry },
            Ok(principal) => {
                let d = start(st, snap, target, &card, who, &inp.cards, now).await;
                if od.control_gate
                    && matches!(
                        d,
                        Decision::Wait {
                            switching: true,
                            ..
                        }
                    )
                {
                    co.limiter.record(principal.as_deref(), now);
                }
                d
            }
        },
        other => other,
    };
    co.note(target, &d, who, od.dry_run);
    d
}

/// Ask the profiles API for the switch: compose {card: target's pick, other: what it runs},
/// check that only one card restarts (allowBothCards), then spool it.
async fn start(
    st: &AppState,
    snap: &Snapshot,
    target: &str,
    card: &str,
    who: &str,
    cards: &BTreeMap<String, CardIn>,
    now: f64,
) -> Decision {
    let od = &snap.config.on_demand;
    let api = od.profiles_api.trim_end_matches('/');
    let md = &od.models[target];
    let Some(live) = get_json(st, &format!("{api}/live")).await else {
        return Decision::Block {
            reason: "the profiles API is not answering".into(),
            retry: 60,
        };
    };
    let Some(cat) = get_json(st, &format!("{api}/profiles")).await else {
        return Decision::Block {
            reason: "the profiles API is not answering".into(),
            retry: 60,
        };
    };
    let layout = live.get("layout").and_then(Value::as_str).unwrap_or("");
    let picks = cat
        .pointer(&format!(
            "/layouts/{}/picks",
            layout.replace('~', "~0").replace('/', "~1")
        ))
        .cloned();
    let Some(Value::Object(picks)) = picks else {
        return Decision::Block {
            reason: format!("the live layout '{layout}' is not a catalog layout: switch by hand"),
            retry: 600,
        };
    };
    let mut want = Map::new();
    for c in cards.keys() {
        if c == card {
            want.insert(
                c.clone(),
                json!({"engine": md.engine, "engineProfile": md.engine_profile, "model": md.model}),
            );
        } else {
            want.insert(
                c.clone(),
                picks
                    .get(c)
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or(json!("off")),
            );
        }
    }
    let Some((code, comp)) =
        post_json(st, &format!("{api}/compose"), &json!({"cards": want})).await
    else {
        return Decision::Block {
            reason: "the profiles API is not answering".into(),
            retry: 60,
        };
    };
    let Some(name) = comp
        .get("layout")
        .and_then(Value::as_str)
        .filter(|_| code == 200)
    else {
        return Decision::Block {
            reason: format!(
                "{target} can't run on card {card}: {}",
                comp.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("compose refused")
            ),
            retry: 3600,
        };
    };
    if let Some(plan) = get_json(st, &format!("{api}/plan?layout={name}")).await {
        let changes = plan
            .get("changes")
            .and_then(Value::as_object)
            .map_or(0, |m| m.len());
        if changes > 1 && !od.allow_both_cards {
            return Decision::Block {
                reason: format!("bringing up {target} restarts both cards: switch by hand"),
                retry: 3600,
            };
        }
    }
    let req = json!({"layout": name, "requestedBy": format!("router-ondemand:{target}").chars().take(80).collect::<String>(),
        "reason": format!("on demand: card {card} -> {target} (requested by {who}; card {card} idle longest)")});
    let Some((code, r)) = post_json(st, &format!("{api}/switch"), &req).await else {
        return Decision::Block {
            reason: "the profiles API is not answering".into(),
            retry: 60,
        };
    };
    match code {
        200 => wait(format!("{name} is already live")),
        409 => wait(
            r.get("error")
                .and_then(Value::as_str)
                .unwrap_or("refused")
                .to_string(),
        ),
        202 => {
            let job = r
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let p = Pending {
                job: job.clone(),
                card: card.to_string(),
                started: now,
                estimate: r.get("estimateSeconds").and_then(Value::as_f64),
                layout: name.to_string(),
            };
            {
                let mut g = st.ext.ondemand.inner.lock().unwrap();
                g.pending.insert(target.to_string(), p);
                g.jobs.insert(job.clone(), card.to_string());
                g.changed.insert(card.to_string(), now);
            }
            st.ext.ondemand.log(
                json!({"event": "switch-started", "target": target, "card": card,
                "layout": name, "job": job, "who": who}),
            );
            Decision::Wait {
                reason: format!("switching card {card} to {target}"),
                switching: true,
                card: Some(card.to_string()),
            }
        }
        _ => Decision::Block {
            reason: r
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(format!("switch HTTP {code}")),
            retry: 60,
        },
    }
}

// ── the request path ───────────────────────────────────────────────────────────────────────

fn sse(v: &Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

fn note_chunk(target: &str, text: &str, first: bool) -> Bytes {
    let mut delta = json!({"reasoning_content": text});
    if first {
        delta["role"] = json!("assistant");
    }
    sse(
        &json!({"id": "chatcmpl-ondemand", "object": "chat.completion.chunk", "created": now_epoch() as i64,
        "model": target, "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
    )
}

fn fail_body(status: u16, msg: &str, retry: u64, etype: &str) -> Value {
    json!({"error": {"message": msg, "type": etype, "code": status, "retry_after": retry}})
}

fn wait_text(target: &str, d: &Decision) -> String {
    match d {
        Decision::Wait {
            switching: true,
            card,
            ..
        } => {
            format!(
                "Switching card {} to {target}…\n",
                card.as_deref().unwrap_or("?")
            )
        }
        other => format!(
            "Waiting to switch a card to {target}: {}…\n",
            other.reason()
        ),
    }
}

/// The deployment for the target on the cards that now run it (the router.json still lists it
/// as a relay name until layoutctl regenerates it).
fn live_deployment(snap: &Snapshot, dep: &Deployment, cards: &[String]) -> Deployment {
    let served = cards
        .first()
        .and_then(|c| snap.config.upstreams.get(c))
        .map(|u| u.served_id.clone())
        .unwrap_or_else(|| dep.served_id.clone());
    Deployment {
        cards: cards.to_vec(),
        relay: false,
        served_id: served,
        ..dep.clone()
    }
}

/// A request for a name no card runs, with on-demand on.
#[allow(clippy::too_many_arguments)]
pub async fn handle(
    st: Arc<AppState>,
    _snap: Arc<Snapshot>,
    ctx: RequestCtx,
    headers: HeaderMap,
    body: Map<String, Value>,
    dep: Deployment,
    adm: crate::switch::gate::ChatAdmission,
    mut rec: LogRec,
) -> Response {
    let started = ctx.started.unwrap_or_else(Instant::now);
    let target = dep.name.clone();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let who = headers
        .get("x-client")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| ctx.client.clone());
    rec.set("ondemand", true);
    if !stream {
        // wait in the handler, then forward
        let t0 = Instant::now();
        let mut switch_t0: Option<Instant> = None;
        loop {
            let snap = st.snapshot();
            let d = ensure(&st, &snap, &adm, &target, &who).await;
            match d {
                Decision::Live { cards } => {
                    let live = live_deployment(&snap, &dep, &cards);
                    return proxy::forward_cards(
                        &st,
                        &snap,
                        &headers,
                        &body,
                        &[live],
                        &target,
                        rec,
                        started,
                    )
                    .await;
                }
                Decision::Unknown { reason } => {
                    return proxy::finish_resp(
                        &st,
                        rec,
                        started,
                        &target,
                        proxy::error(404, &reason, "invalid_request_error", None),
                    );
                }
                Decision::Block { reason, retry } => {
                    let msg =
                        format!("{target} is not live and can't be switched in now: {reason}");
                    let mut r = proxy::json_response(
                        503,
                        &fail_body(503, &msg, retry, "ondemand_unavailable"),
                    );
                    if let Ok(v) = retry.to_string().parse() {
                        r.headers_mut().insert("retry-after", v);
                    }
                    return proxy::finish_resp(&st, rec, started, &target, r);
                }
                Decision::Wait {
                    switching, reason, ..
                } => {
                    if switching {
                        let s0 = *switch_t0.get_or_insert_with(Instant::now);
                        if s0.elapsed().as_secs() > snap.config.on_demand.switch_timeout_seconds {
                            let r = proxy::json_response(
                                503,
                                &fail_body(
                                    503,
                                    &format!("switching to {target} took too long"),
                                    60,
                                    "ondemand_unavailable",
                                ),
                            );
                            return proxy::finish_resp(&st, rec, started, &target, r);
                        }
                    } else if switch_t0.is_none()
                        && t0.elapsed().as_secs() > snap.config.on_demand.queue_seconds
                    {
                        let msg = format!(
                            "no card could switch to {target} within {} s: {reason}",
                            snap.config.on_demand.queue_seconds
                        );
                        let r = proxy::json_response(
                            503,
                            &fail_body(503, &msg, 60, "ondemand_unavailable"),
                        );
                        return proxy::finish_resp(&st, rec, started, &target, r);
                    }
                }
                Decision::Switch { .. } => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    // streaming: SSE headers now, progress notes and keepalives while waiting, then the stream
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let st2 = st.clone();
    tokio::spawn(async move {
        let t0 = Instant::now();
        let mut switch_t0: Option<Instant> = None;
        let mut last_note: Option<String> = None;
        let send = |b: Bytes| {
            let tx = tx.clone();
            async move { tx.send(Ok(b)).await.is_ok() }
        };
        loop {
            let snap = st2.snapshot();
            let note_mode = snap.config.on_demand.stream_note == "reasoning";
            let d = ensure(&st2, &snap, &adm, &target, &who).await;
            let fail = |status: u16, msg: String, retry: u64| {
                sse(&fail_body(status, &msg, retry, "ondemand_unavailable"))
            };
            match &d {
                Decision::Live { cards } => {
                    if note_mode && last_note.is_some() {
                        let _ = send(note_chunk(
                            &target,
                            &format!(
                                "{target} is live on card {} after {} s.\n\n",
                                cards[0],
                                t0.elapsed().as_secs()
                            ),
                            false,
                        ))
                        .await;
                    }
                    let live = live_deployment(&snap, &dep, cards);
                    let resp = proxy::forward_cards(
                        &st2,
                        &snap,
                        &headers,
                        &body,
                        &[live],
                        &target,
                        rec,
                        started,
                    )
                    .await;
                    if resp.status() != 200 {
                        let text = axum::body::to_bytes(resp.into_body(), 4096)
                            .await
                            .unwrap_or_default();
                        let _ = send(fail(
                            502,
                            format!("after the switch: {}", String::from_utf8_lossy(&text)),
                            60,
                        ))
                        .await;
                        return;
                    }
                    let mut s = resp.into_body().into_data_stream();
                    while let Some(chunk) = s.next().await {
                        match chunk {
                            Ok(b) => {
                                if !send(b).await {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    return;
                }
                Decision::Unknown { reason } => {
                    let _ = send(fail(404, reason.clone(), 0)).await;
                    proxy::finish_resp(
                        &st2,
                        rec,
                        started,
                        &target,
                        proxy::json_response(404, &Value::Null),
                    );
                    return;
                }
                Decision::Block { reason, retry } => {
                    let _ = send(fail(
                        503,
                        format!("{target} is not live and can't be switched in now: {reason}"),
                        *retry,
                    ))
                    .await;
                    proxy::finish_resp(
                        &st2,
                        rec,
                        started,
                        &target,
                        proxy::json_response(503, &Value::Null),
                    );
                    return;
                }
                Decision::Wait { switching, .. } => {
                    let over = if *switching {
                        switch_t0
                            .get_or_insert_with(Instant::now)
                            .elapsed()
                            .as_secs()
                            > snap.config.on_demand.switch_timeout_seconds
                    } else {
                        switch_t0.is_none()
                            && t0.elapsed().as_secs() > snap.config.on_demand.queue_seconds
                    };
                    if over {
                        let _ = send(fail(
                            503,
                            format!("no switch to {target} in time: {}", d.reason()),
                            60,
                        ))
                        .await;
                        proxy::finish_resp(
                            &st2,
                            rec,
                            started,
                            &target,
                            proxy::json_response(503, &Value::Null),
                        );
                        return;
                    }
                    let text = wait_text(&target, &d);
                    let ok = if note_mode && last_note.as_deref() != Some(text.as_str()) {
                        let first = last_note.is_none();
                        last_note = Some(text.clone());
                        send(note_chunk(&target, &text, first)).await
                    } else {
                        send(Bytes::from_static(
                            b": waiting for the on-demand switch\n\n",
                        ))
                        .await
                    };
                    if !ok {
                        return; // the client went away; a switch already started carries on
                    }
                }
                Decision::Switch { .. } => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let body = Body::from_stream(tokio_stream_from(rx));
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| proxy::error(500, "response build failed", "internal_error", None))
}

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) })
}

/// GET /ondemand: settings, inputs and the recent decisions.
pub async fn view(st: &AppState) -> Value {
    let snap = st.snapshot();
    let od = &snap.config.on_demand;
    let g = st.ext.ondemand.inner.lock().unwrap();
    let pending: Map<String, Value> = g
        .pending
        .iter()
        .map(|(t, p)| {
            (
                t.clone(),
                json!({"job": p.job, "card": p.card, "layout": p.layout, "estimate": p.estimate}),
            )
        })
        .collect();
    let load: Map<String, Value> = g
        .load
        .iter()
        .map(|(c, (b, l))| (c.clone(), json!({"busy": b, "lastBusy": l})))
        .collect();
    json!({
        "enabled": od.enabled, "dryRun": od.dry_run, "profilesApi": od.profiles_api,
        "models": od.models.keys().collect::<Vec<_>>(),
        "pending": pending, "load": load,
        "recent": g.recent.iter().cloned().collect::<Vec<_>>(),
    })
}

/// Background: poll each card's engine /slots for idleness (every 3 s) and settle switches.
pub async fn background(st: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let snap = st.snapshot();
        let od = &snap.config.on_demand;
        if !od.enabled {
            continue;
        }
        if od.poll_slots {
            for (c, u) in &snap.config.upstreams {
                let busy = if !u.online {
                    Some(false)
                } else {
                    let mut rb = st
                        .http
                        .get(format!("{}/slots", u.url.trim_end_matches('/')))
                        .timeout(Duration::from_secs(3));
                    if let Some(k) = snap.key_for(c) {
                        rb = rb.header("authorization", format!("Bearer {k}"));
                    }
                    match rb.send().await {
                        Ok(r) if r.status().is_success() => {
                            r.json::<Value>().await.ok().and_then(|v| {
                                v.as_array().map(|a| {
                                    a.iter()
                                        .any(|x| x.get("is_processing") == Some(&Value::Bool(true)))
                                })
                            })
                        }
                        _ => None,
                    }
                };
                let mut g = st.ext.ondemand.inner.lock().unwrap();
                let e = g.load.entry(c.clone()).or_insert((None, None));
                e.0 = busy;
                if busy == Some(true) {
                    e.1 = Some(now_epoch());
                }
            }
        }
        if !od.dry_run && !st.ext.ondemand.inner.lock().unwrap().pending.is_empty() {
            let _one = st.ext.ondemand.ensure_lock.lock().await;
            progress(&st, &snap, now_epoch()).await;
        }
    }
}

/// `POST /admin/cards/<card>/drain` (wait for requests in flight, no new ones) and `/undrain`.
pub async fn card_admin(st: &Arc<AppState>, card: &str, action: &str, raw: &[u8]) -> Response {
    let snap = st.snapshot();
    if !snap.config.upstreams.contains_key(card) {
        return proxy::error(
            404,
            &format!("unknown card {card}"),
            "invalid_request_error",
            None,
        );
    }
    let body: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    match action {
        "drain" => {
            let secs = body
                .get("timeoutSeconds")
                .and_then(Value::as_u64)
                .unwrap_or(snap.config.on_demand.drain_timeout_seconds);
            st.set_draining(card, true);
            let deadline = Instant::now() + Duration::from_secs(secs);
            while st.inflight(card) > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let left = st.inflight(card);
            proxy::json_response(
                200,
                &json!({"card": card, "draining": true, "drained": left == 0, "inflight": left}),
            )
        }
        "undrain" => {
            st.set_draining(card, false);
            proxy::json_response(200, &json!({"card": card, "draining": false}))
        }
        _ => proxy::error(
            404,
            "unknown card action (drain, undrain)",
            "invalid_request_error",
            None,
        ),
    }
}

#[cfg(test)]
mod tests;
