//! The Abhoth Supervisor's calendar (SUPERVISOR.md 8 and 9), pure: no IO, no clocks. The
//! router feeds it config, the time and detector readings, and asks what is blocked, who holds
//! a card, and whether an event key may do something now.
//!
//! - [`entry`]: `calendar.toml` and compiled entries;
//! - [`recur`]: RRULE, cron and one-off times in local time (DST-correct);
//! - [`plan`]: occurrences in a window, with `after` sequencing, lead/tail, skips, and live
//!   detection merged in ([`live`]);
//! - [`now`]: what the active occurrences do now: blocks, the card holders (8.4), slot
//!   reserves, clock holds, profiles (the more conservative wins), muted alerts;
//! - [`keys`]: event keys (`abe_…`): policy, validity windows, scopes, priority bypass;
//! - [`ics`] and [`cron_import`]: iCal import/subscribe and the TrueNAS cron import.

pub mod cron_import;
pub mod dur;
pub mod effects;
pub mod emit;
pub mod entry;
pub mod ics;
pub mod keys;
pub mod live;
pub mod now;
pub mod plan;
pub mod recur;

use std::collections::{BTreeMap, BTreeSet};

use chrono_tz::Tz;
use indexmap::IndexMap;
use serde::Serialize;

pub use effects::{Effect, OnReserve, Target};
pub use entry::{CalendarConfig, Entry, EntryConfig, Priority};
pub use live::Live;
pub use plan::{Occurrence, Overrides, Phase};

/// A config problem: errors leave the entry out, warnings are shown when saving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub path: String,
    pub message: String,
    pub severity: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

impl Issue {
    pub fn error(path: &str, message: String) -> Self {
        Self {
            path: path.into(),
            message,
            severity: Severity::Error,
        }
    }
    pub fn warning(path: &str, message: String) -> Self {
        Self {
            path: path.into(),
            message,
            severity: Severity::Warning,
        }
    }
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{s}: {}: {}", self.path, self.message)
    }
}

/// What the rest of the control plane knows that the calendar must respect (9.5).
#[derive(Debug, Clone, Default)]
pub struct Safety {
    /// `managed = false` and `on_demand = false` cards: outside every event
    pub excluded_cards: Vec<String>,
    /// excluded models (`dense-pi`)
    pub excluded_models: Vec<String>,
    /// layouts that place anything on an excluded card or model
    pub unsafe_layouts: Vec<String>,
}

/// The compiled calendar.
#[derive(Debug, Clone)]
pub struct Calendar {
    pub tz: Tz,
    pub entries: IndexMap<String, Entry>,
    /// entry ids with predecessors after them
    order: Vec<String>,
}

impl Calendar {
    /// Compile `[calendar]`. `default_tz` is `router.timezone`. Entries with errors are left
    /// out; every problem is returned.
    pub fn compile(cfg: &CalendarConfig, default_tz: &str, safety: &Safety) -> (Calendar, Vec<Issue>) {
        let mut issues = Vec::new();
        let tz_name = cfg.timezone.as_deref().unwrap_or(default_tz);
        let tz = entry::parse_tz(tz_name).unwrap_or_else(|e| {
            issues.push(Issue::error("calendar.timezone", e));
            chrono_tz::Europe::London
        });
        let mut entries = IndexMap::new();
        for (id, c) in &cfg.entries {
            if !valid_id(id) {
                issues.push(Issue::error(
                    &format!("calendar.entries.{id}"),
                    "ids are lower-case letters, digits, '-' and '_'".into(),
                ));
                continue;
            }
            if let Some(e) = entry::compile(id, c, tz, &mut issues) {
                entries.insert(id.clone(), e);
            }
        }
        // `after`: known entries, no cycles.
        let mut bad: BTreeSet<String> = BTreeSet::new();
        for (id, e) in &entries {
            if let Some(p) = &e.cfg.after {
                if !entries.contains_key(p) {
                    issues.push(Issue::error(
                        &format!("calendar.entries.{id}.after"),
                        format!("no entry '{p}'"),
                    ));
                    bad.insert(id.clone());
                    continue;
                }
                let mut seen = BTreeSet::from([id.clone()]);
                let mut cur = p.clone();
                loop {
                    if !seen.insert(cur.clone()) {
                        issues.push(Issue::error(
                            &format!("calendar.entries.{id}.after"),
                            "the `after` chain loops".into(),
                        ));
                        bad.insert(id.clone());
                        break;
                    }
                    match entries.get(&cur).and_then(|x| x.cfg.after.clone()) {
                        Some(n) => cur = n,
                        None => break,
                    }
                }
            }
        }
        entries.retain(|k, _| !bad.contains(k));
        let mut order: Vec<String> = Vec::new();
        let mut placed: BTreeSet<String> = BTreeSet::new();
        while order.len() < entries.len() {
            let before = order.len();
            for (id, e) in &entries {
                if placed.contains(id) {
                    continue;
                }
                if e.cfg.after.as_ref().is_none_or(|p| placed.contains(p)) {
                    order.push(id.clone());
                    placed.insert(id.clone());
                }
            }
            if order.len() == before {
                break;
            }
        }
        let cal = Calendar { tz, entries, order };
        cal.validate(safety, &mut issues);
        (cal, issues)
    }

    /// Entry ids, predecessors first.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Safety and policy checks (9.4, 9.5): warnings and errors for saving.
    fn validate(&self, safety: &Safety, issues: &mut Vec<Issue>) {
        for (id, e) in &self.entries {
            let at = |f: &str| format!("calendar.entries.{id}.{f}");
            for c in e.named_cards() {
                if safety.excluded_cards.contains(&c) {
                    issues.push(Issue::error(
                        &at("effects"),
                        format!("card {c} is excluded from the control plane; no event may name it"),
                    ));
                }
            }
            for eff in &e.effects {
                if let Effect::ReserveCard { on_reserve: OnReserve::Preempt, card, .. } = eff {
                    if e.priority() < Priority::High {
                        issues.push(Issue::error(
                            &at("effects"),
                            format!("on_reserve = \"preempt\" on card {card} needs priority high or critical"),
                        ));
                    } else {
                        issues.push(Issue::warning(
                            &at("effects"),
                            format!("on_reserve = \"preempt\" cancels other users' streams on card {card} after the grace; every cancel is audited"),
                        ));
                    }
                }
            }
            if let Some(k) = &e.key {
                keys::validate_policy(id, k, e, safety, issues);
            }
        }
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Occurrence ids an admin skipped (`skip`) or ended early (`cancel` while running), from
/// `router.db`.
pub fn overrides_from(rows: &[(String, String, i64)]) -> Overrides {
    let mut o = Overrides::default();
    for (occ, kind, at) in rows {
        match kind.as_str() {
            "skip" => {
                o.skipped.insert(occ.clone());
            }
            "cancel" => {
                o.ended.insert(occ.clone(), *at);
            }
            _ => {}
        }
    }
    o
}

/// For the UI: occurrences per entry in a window.
pub fn group_by_entry(occ: &[Occurrence]) -> BTreeMap<String, Vec<&Occurrence>> {
    let mut m: BTreeMap<String, Vec<&Occurrence>> = BTreeMap::new();
    for o in occ {
        m.entry(o.entry.clone()).or_default().push(o);
    }
    m
}

#[cfg(test)]
mod tests;
