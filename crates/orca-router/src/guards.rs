//! Guards (ROUTER-CONFIG.md 7.1), made generic for the Supervisor's calendar: everything that
//! stops the control plane from acting is a [`Reservation`]. A reservation blocks some
//! [`Action`]s on some cards (or all) until it ends, with a reason and a `Retry-After`.
//!
//! Reservations come from two kinds of source:
//! - a [`Guard`] the board asks at evaluation time (a schedule or calendar, admin holds, guard
//!   leases): always current, never stale;
//! - a **detector** whose readings are pushed in with [`GuardBoard::report`] (a host probe run
//!   over SSH every few seconds: the `run_daily.sh` process scan, the bench queue file, the
//!   music-mode flag). A detector that has not reported for `stale_after` (45 s) counts as
//!   **active** for `switch` and `on_demand` ("stale guards mean wait") and as inactive for
//!   `ensure` (starting a missing engine after a reboot is the safe default).
//!
//! The detectors below are pure functions over facts the host probe collects, so they are
//! tested without a host: [`video_runs`] (the `/proc/*/cmdline` scan, including a run waiting
//! on its lock), [`bench_running`] (the bench queue file) and a plain "file exists".

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a reservation can block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// any layout change (admin or on-demand)
    Switch,
    /// an on-demand switch (also blocked by everything that blocks `Switch`)
    OnDemand,
    /// reconcile starting a missing engine
    Ensure,
    Offline,
    Online,
    /// sending requests to a card (a reservation can keep a card for one event's callers)
    Use,
}

/// What an event key lets its holder do during its event (SUPERVISOR.md calendar). A grant
/// only lifts reservations that the same event made; it never lifts detectors (video, bench,
/// stale data), other events' reservations, excluded cards, thermal limits or the rule that
/// only chat requests may start a switch (those checks live outside the guard board).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// use the cards the event reserved
    UseReserved,
    /// switch layouts during the event; `Some(list)` = only to these layouts
    SwitchLayout(Option<Vec<String>>),
    /// bring a card online
    CardOnline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventGrant {
    pub event: String,
    pub scopes: Vec<EventScope>,
}

/// Who asks. Anonymous and internal callers have no grants.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Caller {
    /// user or key id (never the key)
    pub principal: Option<String>,
    /// grants of the event keys the request carried, for events active now
    pub grants: Vec<EventGrant>,
}

impl Caller {
    /// Does this caller's grant for `event` lift a reservation blocking `action`?
    pub fn lifts(&self, event: &str, action: Action, layout: Option<&str>) -> bool {
        self.grants.iter().filter(|g| g.event == event).any(|g| {
            g.scopes.iter().any(|s| match (s, action) {
                (EventScope::UseReserved, Action::Use) => true,
                (EventScope::CardOnline, Action::Online) => true,
                (EventScope::SwitchLayout(only), Action::Switch | Action::OnDemand) => match only {
                    None => true,
                    Some(list) => layout.is_some_and(|l| list.iter().any(|x| x == l)),
                },
                _ => false,
            })
        })
    }
}

/// Which cards a reservation covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    All,
    Cards(Vec<String>),
}

impl Scope {
    /// Does a reservation with this scope touch any of `cards`? (Empty `cards` = the server.)
    pub fn touches(&self, cards: &[String]) -> bool {
        match self {
            Scope::All => true,
            Scope::Cards(c) => cards.is_empty() || c.iter().any(|x| cards.contains(x)),
        }
    }
}

/// One thing that blocks actions now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    /// The source id (guard or detector), e.g. `video`, `bench`, `quiet-window`, `lease:<key>`.
    pub source: String,
    pub reason: String,
    pub blocks: Vec<Action>,
    #[serde(default)]
    pub scope: Scope,
    /// Epoch when it ends, if known (calendar entries, holds); `None` = while it is reported.
    #[serde(default)]
    pub until: Option<f64>,
    /// `Retry-After` for refused requests.
    pub retry_seconds: u64,
    /// The calendar event that made it: holders of that event's key may be let through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

impl Reservation {
    fn blocks_action(&self, a: Action) -> bool {
        self.blocks.contains(&a) || (a == Action::OnDemand && self.blocks.contains(&Action::Switch))
    }
}

/// A source the board asks at evaluation time (schedule, calendar, holds, guard leases).
pub trait Guard: Send + Sync {
    fn id(&self) -> &str;
    fn reservations(&self, now: f64) -> Vec<Reservation>;
}

/// Why an action is refused or must wait.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Block {
    pub source: String,
    pub reason: String,
    pub retry_seconds: u64,
    /// The source has not reported recently: wait, never act.
    pub stale: bool,
}

/// A pushed detector: what it may block and its last reading.
#[derive(Debug, Clone)]
struct Detector {
    blocks: Vec<Action>,
    scope: Scope,
    retry_seconds: u64,
    stale_after: f64,
    last: Option<(f64, Vec<Reservation>)>,
}

/// All guards of one server.
#[derive(Default)]
pub struct GuardBoard {
    detectors: Mutex<BTreeMap<String, Detector>>,
    guards: Mutex<Vec<std::sync::Arc<dyn Guard>>>,
}

/// The stale limit of ondemand.py and the design: 45 s.
pub const STALE_SECONDS: f64 = 45.0;

impl GuardBoard {
    /// Declare a pushed detector (from config). Until its first report it is stale.
    pub fn declare(&self, id: &str, blocks: Vec<Action>, scope: Scope, retry_seconds: u64) {
        self.detectors.lock().unwrap().insert(
            id.to_string(),
            Detector {
                blocks,
                scope,
                retry_seconds,
                stale_after: STALE_SECONDS,
                last: None,
            },
        );
    }

    /// Add a guard asked at evaluation time (a calendar, a schedule, holds).
    pub fn add_guard(&self, g: std::sync::Arc<dyn Guard>) {
        self.guards.lock().unwrap().push(g);
    }

    /// A detector's reading taken at `at`: `active` with a detail (e.g. the channels running),
    /// or not. A reading older than the one held is ignored.
    pub fn report(&self, id: &str, at: f64, active: Option<String>) {
        let mut d = self.detectors.lock().unwrap();
        let Some(det) = d.get_mut(id) else {
            return;
        };
        if det.last.as_ref().is_some_and(|(t, _)| *t > at) {
            return;
        }
        let res = active
            .map(|detail| {
                vec![Reservation {
                    source: id.to_string(),
                    reason: detail,
                    blocks: det.blocks.clone(),
                    scope: det.scope.clone(),
                    until: None,
                    retry_seconds: det.retry_seconds,
                    event: None,
                }]
            })
            .unwrap_or_default();
        det.last = Some((at, res));
    }

    /// Every reservation active now (stale detectors not included; see [`Self::blocks`]).
    pub fn active(&self, now: f64) -> Vec<Reservation> {
        let mut out: Vec<Reservation> = Vec::new();
        for d in self.detectors.lock().unwrap().values() {
            if let Some((_, r)) = &d.last {
                out.extend(r.iter().cloned());
            }
        }
        for g in self.guards.lock().unwrap().iter() {
            out.extend(
                g.reservations(now)
                    .into_iter()
                    .filter(|r| r.until.is_none_or(|u| u > now)),
            );
        }
        out
    }

    /// What blocks `action` on `cards` (empty = the whole server) at `now`. Stale detectors
    /// block `Switch` / `OnDemand` / `Offline` / `Online` but never `Ensure`.
    pub fn blocks(&self, action: Action, cards: &[String], now: f64) -> Vec<Block> {
        self.check(action, cards, &Caller::default(), None, now)
    }

    /// What blocks `caller` doing `action` on `cards` (switching to `layout`, for a switch).
    /// Reservations of an event the caller holds a fitting grant for are lifted; nothing else.
    pub fn check(
        &self,
        action: Action,
        cards: &[String],
        caller: &Caller,
        layout: Option<&str>,
        now: f64,
    ) -> Vec<Block> {
        let mut out = Vec::new();
        for (id, d) in self.detectors.lock().unwrap().iter() {
            let relevant = d.blocks.contains(&action)
                || (action == Action::OnDemand && d.blocks.contains(&Action::Switch));
            if !relevant || !d.scope.touches(cards) {
                continue;
            }
            let fresh = d.last.as_ref().filter(|(t, _)| now - t <= d.stale_after);
            match fresh {
                None if action != Action::Ensure && action != Action::Use => out.push(Block {
                    source: id.clone(),
                    reason: format!(
                        "guard '{id}' has not reported for {} s: waiting",
                        d.stale_after as u64
                    ),
                    retry_seconds: 30,
                    stale: true,
                }),
                None => {}
                Some((_, res)) => {
                    for r in res {
                        out.push(Block {
                            source: r.source.clone(),
                            reason: r.reason.clone(),
                            retry_seconds: r.retry_seconds,
                            stale: false,
                        });
                    }
                }
            }
        }
        for g in self.guards.lock().unwrap().iter() {
            for r in g.reservations(now) {
                if r.until.is_some_and(|u| u <= now)
                    || !r.blocks_action(action)
                    || !r.scope.touches(cards)
                    || r.event
                        .as_deref()
                        .is_some_and(|e| caller.lifts(e, action, layout))
                {
                    continue;
                }
                let retry = r
                    .until
                    .map(|u| ((u - now).ceil() as u64).max(60))
                    .unwrap_or(r.retry_seconds);
                out.push(Block {
                    source: r.source,
                    reason: r.reason,
                    retry_seconds: retry,
                    stale: false,
                });
            }
        }
        out
    }
}

// ── evaluation-time guards ─────────────────────────────────────────────────────────────────

/// A recurring daily window or a one-off interval (the seed of the Supervisor's calendar):
/// `06:45-07:30` London every day blocks switches and on-demand (AI News at 07:00).
#[derive(Debug, Clone)]
pub struct CalendarEntry {
    pub id: String,
    pub reason: String,
    pub blocks: Vec<Action>,
    pub scope: Scope,
    pub when: When,
    /// The event whose key holders it lets through (`None`: nobody, e.g. the quiet window).
    pub event: Option<String>,
}

#[derive(Debug, Clone)]
pub enum When {
    /// Every day, London wall-clock minutes from midnight `[start, end)`.
    DailyLondon { start_min: u32, end_min: u32 },
    /// Epoch seconds `[from, until)`.
    Once { from: f64, until: f64 },
}

impl CalendarEntry {
    /// The quiet window as layoutctl has it.
    pub fn quiet_window() -> Self {
        CalendarEntry {
            id: "quiet-window".into(),
            reason: "06:45-07:30 UK: AI News runs at 07:00, no switches".into(),
            blocks: vec![Action::Switch, Action::OnDemand],
            scope: Scope::All,
            when: When::DailyLondon {
                start_min: 6 * 60 + 45,
                end_min: 7 * 60 + 30,
            },
            event: None,
        }
    }

    /// `Some(until)` when the entry is on at `now`.
    pub fn on_until(&self, now: f64) -> Option<f64> {
        match &self.when {
            When::Once { from, until } => (now >= *from && now < *until).then_some(*until),
            When::DailyLondon { start_min, end_min } => {
                let (h, m, s) = crate::ondemand::uk_hms(now);
                let t = h * 3600 + m * 60 + s;
                let (a, b) = (start_min * 60, end_min * 60);
                (t >= a && t < b).then(|| now + f64::from(b - t))
            }
        }
    }
}

/// A list of calendar entries as one guard.
#[derive(Default)]
pub struct Calendar {
    pub entries: Mutex<Vec<CalendarEntry>>,
}

impl Guard for Calendar {
    fn id(&self) -> &str {
        "calendar"
    }
    fn reservations(&self, now: f64) -> Vec<Reservation> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| {
                e.on_until(now).map(|until| Reservation {
                    source: e.id.clone(),
                    reason: e.reason.clone(),
                    blocks: e.blocks.clone(),
                    scope: e.scope.clone(),
                    until: Some(until),
                    retry_seconds: 60,
                    event: e.event.clone(),
                })
            })
            .collect()
    }
}

/// Live guard leases (`kind: guard`, section 8.3) as reservations blocking switches.
pub fn lease_reservations(leases: &[crate::leases::GuardLease]) -> Vec<Reservation> {
    leases
        .iter()
        .map(|g| Reservation {
            source: format!("lease:{}", g.key),
            reason: format!("guard lease '{}' held by {} ({})", g.key, g.owner, g.reason),
            blocks: vec![Action::Switch, Action::OnDemand],
            scope: Scope::All,
            until: None,
            retry_seconds: 300,
            event: None,
        })
        .collect()
}

// ── detectors (pure: the host probe collects the facts) ────────────────────────────────────

/// How a process detector matches (`[guards.video] match = {...}`).
#[derive(Debug, Clone)]
pub struct ProcessMatch {
    /// A script name that must appear (by basename) in the first three argv entries.
    pub script: String,
    /// Allowed basenames of argv[0].
    pub exe: Vec<String>,
    /// The argument whose value labels the run (`--channel`), and the label without it.
    pub label_arg: String,
    pub label_default: String,
}

impl ProcessMatch {
    /// The video pipeline: `run_daily.sh` under a shell, labelled by `--channel`.
    pub fn video() -> Self {
        ProcessMatch {
            script: "run_daily.sh".into(),
            exe: ["bash", "sh", "dash", "run_daily.sh"]
                .map(String::from)
                .to_vec(),
            label_arg: "--channel".into(),
            label_default: "ai-news".into(),
        }
    }
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// `[(pid, label)]` of matching processes from `(pid, argv)` pairs (`/proc/*/cmdline`; a run
/// waiting for the run lock is a matching process too: it is part of the day's run).
pub fn video_runs(procs: &[(u32, Vec<String>)], m: &ProcessMatch) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for (pid, argv) in procs {
        let Some(a0) = argv.first() else { continue };
        let script = argv.iter().take(3).any(|a| basename(a) == m.script);
        if !script || !m.exe.iter().any(|e| e == basename(a0)) {
            continue;
        }
        let label = argv
            .iter()
            .position(|a| *a == m.label_arg)
            .filter(|i| i + 1 < argv.len())
            .map(|i| argv[i + 1].clone())
            .unwrap_or_else(|| m.label_default.clone());
        out.push((*pid, label));
    }
    out
}

/// The detector reading for a process scan: `Some("ai-news, game-news")` while any run lives.
pub fn video_reading(procs: &[(u32, Vec<String>)], m: &ProcessMatch) -> Option<String> {
    let mut labels: Vec<String> = video_runs(procs, m).into_iter().map(|(_, l)| l).collect();
    labels.sort();
    labels.dedup();
    (!labels.is_empty()).then(|| labels.join(", "))
}

/// Parse `/proc/<pid>/cmdline` bytes (NUL-separated) into argv.
pub fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

/// The bench queue file: running when `running` or `current` is truthy (ondemand.bench_running).
pub fn bench_running(queue: Option<&Value>) -> Option<String> {
    let q = queue?;
    let truthy = |v: Option<&Value>| match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    };
    if !(truthy(q.get("running")) || truthy(q.get("current"))) {
        return None;
    }
    Some(match q.get("current") {
        Some(Value::Object(o)) => o
            .get("id")
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_else(|| "queue running".into()),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => "queue running".into(),
    })
}

#[cfg(test)]
mod tests;
