//! `calendar.toml` (SUPERVISOR.md 8.1, 9.8): the `[calendar]` table as written, and the entries
//! compiled for expansion. Config is the source of truth (15 Q12); `router.db` holds only what
//! happened (occurrence state, live runs, key use).

use chrono::NaiveDateTime;
use chrono_tz::Tz;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::effects::{Effect, Target};
use crate::keys::KeyPolicy;
use crate::recur::{Cron, Recurrence, Rrule, parse_local};
use crate::{Issue, dur};

/// The anchor of an RRULE without `start`: a Monday long ago, so `FREQ=DAILY;BYHOUR=7` is
/// simply every day at 07:00 and weekly intervals count from a fixed week.
pub const DEFAULT_ANCHOR: &str = "2024-01-01T00:00";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectMode {
    /// a planned occurrence is extended while its signal lasts
    Extend,
    /// a signal with no planned occurrence starts an unplanned one
    Start,
    #[default]
    Both,
}

/// `detect.process`: a process whose argv names a script, labelled by an argument.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDetect {
    /// a word of argv ending with this (`run_daily.sh`)
    pub argv_any: String,
    /// the argument whose value labels the run (`--channel`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_arg: Option<String>,
    /// the label this entry watches (`game-news`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// the label of a run without `label_arg` (`ai-news`: `run_daily.sh` alone is AI News)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessDetect>,
    /// a file that exists while the job runs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_file: Option<String>,
    /// a JSON file with a truthy `running` / `current` (the bench queue)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_file: Option<String>,
    /// an HTTP JSON endpoint and the key that is truthy while running
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<String>,
    #[serde(default)]
    pub mode: DetectMode,
    /// how long after the signal ends the occurrence still holds (default 10m)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<String>,
}

impl DetectConfig {
    /// A stable id of the signal, for the live tracker and the UI.
    pub fn signal(&self) -> String {
        if let Some(p) = &self.process {
            return format!(
                "process:{}:{}",
                p.argv_any,
                p.label.as_deref().or(p.label_default.as_deref()).unwrap_or("*")
            );
        }
        if let Some(f) = &self.lock_file {
            return format!("lock_file:{f}");
        }
        if let Some(f) = &self.json_file {
            return format!("json_file:{f}");
        }
        if let Some(u) = &self.http {
            return format!("http:{u}");
        }
        "none".into()
    }
}

/// One entry as written under `[calendar.entries.<id>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryConfig {
    pub title: String,
    /// the D140 admin who created it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// one-off: a local time (`2026-12-01T20:00`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    /// the RRULE's DTSTART (local); default a fixed Monday in 2024
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// imported from iCal: the VEVENT's UID (the times are in `rrule`/`at`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ics: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<DetectConfig>,
    /// becomes active when this entry's occurrence ends (Game News after AI News)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Value>,
    #[serde(default)]
    pub priority: Priority,
    /// the event key's policy (the key itself lives hashed in the database)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyPolicy>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// no occurrences after this local time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// local start times to leave out
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exdates: Vec<String>,
    /// where it came from: `cron:48`, `ics:<subscription>`, `bench`, `ui`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn yes() -> bool {
    true
}

/// `[calendar.subscriptions.<id>]`: an iCal feed (read-only, off by default, 15 Q13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    /// `https://…` or `secret:<id>` (a feed URL with a token in it is a secret)
    pub url: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_refresh")]
    pub refresh: String,
    /// the effects every imported event gets
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Value>,
    #[serde(default)]
    pub priority: Priority,
    /// floating times in the feed are read in this zone (default the calendar's)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

fn d_refresh() -> String {
    "15m".into()
}

/// `[calendar]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    /// the calendar replaces nothing until this is on (behind the control plane's flag too)
    #[serde(default)]
    pub enabled: bool,
    /// default `router.timezone`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub entries: IndexMap<String, EntryConfig>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub subscriptions: IndexMap<String, Subscription>,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timezone: None,
            entries: IndexMap::new(),
            subscriptions: IndexMap::new(),
        }
    }
}

impl CalendarConfig {
    /// From the raw `[calendar]` table the router's schema keeps.
    pub fn from_raw(raw: &IndexMap<String, Value>) -> Result<Self, String> {
        let v = Value::Object(raw.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        serde_json::from_value(v).map_err(|e| format!("calendar: {e}"))
    }

    /// From a `calendar.toml` text (`[calendar]` at the top level).
    pub fn from_toml(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            calendar: Option<CalendarConfig>,
        }
        let f: File = toml::from_str(text).map_err(|e| format!("calendar.toml: {e}"))?;
        Ok(f.calendar.unwrap_or_default())
    }
}

/// An entry ready for expansion.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub cfg: EntryConfig,
    pub tz: Tz,
    pub when: Recurrence,
    pub duration: i64,
    pub lead: i64,
    pub tail: i64,
    pub grace: i64,
    pub effects: Vec<Effect>,
    pub exdates: Vec<NaiveDateTime>,
    pub until: Option<NaiveDateTime>,
    pub key: Option<KeyPolicy>,
}

impl Entry {
    pub fn priority(&self) -> Priority {
        self.cfg.priority
    }

    /// The cards this entry reserves (`reserve_card`).
    pub fn reserved_cards(&self) -> Vec<String> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::ReserveCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every card the entry names in an effect (for the excluded-cards check).
    pub fn named_cards(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.effects {
            match e {
                Effect::ReserveCard { card, .. } => out.push(card.clone()),
                Effect::ReserveSlots { target: Target::Card(c), .. }
                | Effect::HoldClocks { target: Target::Card(c) }
                | Effect::BlockEnsure { target: Target::Card(c) }
                | Effect::ClockProfile { target: Target::Card(c), .. } => out.push(c.clone()),
                _ => {}
            }
        }
        out
    }

    /// An `info` entry that fires more than 24 times a day (every-minute collectors, the
    /// 10-minute upload retry): listed with its schedule, not expanded into occurrences.
    pub fn frequent_info(&self) -> bool {
        self.info_only()
            && match &self.when {
                Recurrence::Cron(c) => c.runs_per_day() > 24,
                Recurrence::Rrule { rule, .. } => {
                    matches!(rule.freq, crate::recur::Freq::Minutely | crate::recur::Freq::Hourly)
                }
                Recurrence::Once(_) => false,
            }
    }

    /// An `info` entry (shown only, protects nothing).
    pub fn info_only(&self) -> bool {
        self.effects.iter().all(|e| matches!(e, Effect::Info))
    }
}

pub fn parse_tz(s: &str) -> Result<Tz, String> {
    s.parse::<Tz>().map_err(|_| format!("unknown time zone '{s}'"))
}

/// Compile one entry; errors make the entry unusable (it is left out), warnings do not.
pub fn compile(id: &str, c: &EntryConfig, default_tz: Tz, issues: &mut Vec<Issue>) -> Option<Entry> {
    let at = |field: &str| format!("calendar.entries.{id}.{field}");
    let first = issues.len();
    let err = |f: &str, m: String, issues: &mut Vec<Issue>| {
        issues.push(Issue::error(&at(f), m));
    };
    let tz = match &c.tz {
        Some(t) => match parse_tz(t) {
            Ok(z) => z,
            Err(e) => {
                err("tz", e, issues);
                default_tz
            }
        },
        None => default_tz,
    };
    let n_when = [c.at.is_some(), c.rrule.is_some(), c.cron.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    if n_when != 1 && !(n_when == 0 && c.after.is_some()) {
        err(
            "when",
            "give exactly one of `at`, `rrule` or `cron`".into(),
            issues,
        );
        return None;
    }
    let when = if let Some(a) = &c.at {
        match parse_local(a) {
            Ok(t) => Recurrence::Once(t),
            Err(e) => {
                err("at", e, issues);
                return None;
            }
        }
    } else if let Some(r) = &c.rrule {
        let anchor = match parse_local(c.start.as_deref().unwrap_or(DEFAULT_ANCHOR)) {
            Ok(t) => t,
            Err(e) => {
                err("start", e, issues);
                return None;
            }
        };
        match Rrule::parse(r) {
            Ok(rule) => Recurrence::Rrule { rule, anchor },
            Err(e) => {
                err("rrule", e, issues);
                return None;
            }
        }
    } else if let Some(cr) = &c.cron {
        match Cron::parse(cr) {
            Ok(x) => Recurrence::Cron(x),
            Err(e) => {
                err("cron", e, issues);
                return None;
            }
        }
    } else {
        // `after` alone: daily, following its predecessor.
        Recurrence::Rrule {
            rule: Rrule::parse("FREQ=DAILY").ok()?,
            anchor: parse_local(DEFAULT_ANCHOR).ok()?,
        }
    };
    let d = |f: &str, v: &Option<String>, dflt: i64, issues: &mut Vec<Issue>| match v {
        None => dflt,
        Some(s) => match dur::parse(s) {
            Ok(x) => x,
            Err(e) => {
                issues.push(Issue::error(&at(f), e));
                dflt
            }
        },
    };
    let mut effects = Vec::new();
    for (i, v) in c.effects.iter().enumerate() {
        match Effect::parse(v) {
            Ok(e) => effects.push(e),
            Err(e) => {
                issues.push(Issue::error(&format!("calendar.entries.{id}.effects[{i}]"), e));
            }
        }
    }
    if effects.is_empty() {
        effects.push(Effect::Info);
    }
    let duration = d("duration", &c.duration, 0, issues);
    let lead = d("lead", &c.lead, 0, issues);
    let tail = d("tail", &c.tail, 0, issues);
    let grace = d(
        "detect.grace",
        &c.detect.as_ref().and_then(|x| x.grace.clone()),
        600,
        issues,
    );
    let duration = if duration == 0 {
        if !effects.iter().all(|e| matches!(e, Effect::Info)) {
            issues.push(Issue::error(
                &at("duration"),
                "an entry with effects needs a duration".into(),
            ));
        }
        60
    } else {
        duration
    };
    let mut exdates = Vec::new();
    for x in &c.exdates {
        match parse_local(x) {
            Ok(t) => exdates.push(t),
            Err(e) => {
                issues.push(Issue::error(&at("exdates"), e));
            }
        }
    }
    let until = match &c.until {
        None => None,
        Some(u) => match parse_local(u) {
            Ok(t) => Some(t),
            Err(e) => {
                issues.push(Issue::error(&at("until"), e));
                None
            }
        },
    };
    if let Some(det) = &c.detect {
        let n = [
            det.process.is_some(),
            det.lock_file.is_some(),
            det.json_file.is_some(),
            det.http.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if n != 1 {
            issues.push(Issue::error(
                &at("detect"),
                "give exactly one of process, lock_file, json_file or http".into(),
            ));
        }
    }
    let ok = issues[first..].iter().all(|i| !i.is_error());
    ok.then(|| Entry {
        id: id.to_string(),
        cfg: c.clone(),
        tz,
        when,
        duration,
        lead,
        tail,
        grace,
        effects,
        exdates,
        until,
        key: c.key.clone(),
    })
}
