//! The alert engine: signals in, notifications out. Pure and synchronous: the supervisor feeds
//! it signals and the time, persists [`Alert`]s, and hands notifications to the sinks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::rules::{AlertsConfig, Rule, Severity, compile, dur};

/// Something that happened, or a condition's current value.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// A one-off event (an OOM kill, a crash, a failed switch): opens (or bumps) an alert.
    /// Event alerts resolve themselves after `cooldown` without a repeat.
    Event {
        rule: String,
        /// identity parts and message variables (`card = "B"`, `detail = "…"`)
        vars: BTreeMap<String, String>,
        severity: Option<Severity>,
        /// a shared cause for grouping (`card:B`)
        cause: Option<String>,
        at: i64,
    },
    /// A condition evaluated now (`hot_card` for card A is true at 86 °C).
    Condition {
        rule: String,
        vars: BTreeMap<String, String>,
        active: bool,
        severity: Option<Severity>,
        cause: Option<String>,
        at: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    /// the condition holds but not yet for `for`
    Pending,
    Open,
    Resolved,
}

/// One alert per identity (`router.db`: first, last, count).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub identity: String,
    pub rule: String,
    pub severity: Severity,
    pub state: AlertState,
    pub message: String,
    pub first: i64,
    pub last: i64,
    pub count: u64,
    /// when it became pending / opened / resolved
    pub since: i64,
    pub last_sent: Option<i64>,
    pub resolved_at: Option<i64>,
    pub cause: Option<String>,
    /// one-off event alert (resolves itself)
    pub event: bool,
    /// the calendar entry muting it when it was last due (shown in the UI)
    pub muted_by: Option<String>,
    /// reopened within `cooldown` of its resolve: it opens again without a message
    #[serde(default)]
    pub flapping: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyKind {
    Firing,
    Repeat,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Child {
    pub identity: String,
    pub rule: String,
    pub message: String,
}

/// One message to the sinks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    pub kind: NotifyKind,
    pub identity: String,
    pub rule: String,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    pub count: u64,
    pub at: i64,
    /// other alerts grouped into this one (same cause, within the group window)
    pub children: Vec<Child>,
    /// replace-by tag for phone notifications (the identity)
    pub tag: String,
    pub sinks: Vec<String>,
}

/// Message variables substituted into `{name}`.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        match rest[i..].find('}') {
            Some(j) => {
                let name = &rest[i + 1..i + j];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[i..=i + j]),
                }
                rest = &rest[i + j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn identity(rule: &Rule, vars: &BTreeMap<String, String>) -> String {
    let mut id = rule.id.clone();
    for p in &rule.dedupe {
        id.push('/');
        id.push_str(vars.get(p).map(String::as_str).unwrap_or("-"));
    }
    id
}

struct Queued {
    n: Notification,
    cause: String,
    ready_at: i64,
}

pub struct Engine {
    rules: BTreeMap<String, Rule>,
    group_window: i64,
    alerts: BTreeMap<String, Alert>,
    queue: Vec<Queued>,
    /// identities changed since the last [`Engine::take_dirty`] (to persist)
    dirty: Vec<String>,
}

/// Mutes from the calendar: `(rule, critical) -> Some(entry)` when muted. Never mute critical.
pub type MuteFn<'a> = &'a dyn Fn(&str, bool) -> Option<String>;

impl Engine {
    pub fn new(cfg: &AlertsConfig) -> (Self, Vec<String>) {
        let (rules, issues) = compile(cfg);
        let group_window = dur(&cfg.group_window).unwrap_or(30);
        (
            Engine {
                rules: rules.into_iter().map(|r| (r.id.clone(), r)).collect(),
                group_window,
                alerts: BTreeMap::new(),
                queue: Vec::new(),
                dirty: Vec::new(),
            },
            issues,
        )
    }

    /// Restore persisted alerts (after a restart: no re-send of what was already sent).
    pub fn restore(&mut self, alerts: Vec<Alert>) {
        for a in alerts {
            self.alerts.insert(a.identity.clone(), a);
        }
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.get(id)
    }

    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.values()
    }

    pub fn alerts(&self) -> impl Iterator<Item = &Alert> {
        self.alerts.values()
    }

    pub fn open(&self) -> Vec<&Alert> {
        self.alerts
            .values()
            .filter(|a| a.state == AlertState::Open)
            .collect()
    }

    pub fn take_dirty(&mut self) -> Vec<Alert> {
        let ids = std::mem::take(&mut self.dirty);
        let mut seen = std::collections::BTreeSet::new();
        ids.into_iter()
            .filter(|i| seen.insert(i.clone()))
            .filter_map(|i| self.alerts.get(&i).cloned())
            .collect()
    }

    fn notify(&self, rule: &Rule, a: &Alert, kind: NotifyKind, at: i64) -> Notification {
        Notification {
            kind,
            identity: a.identity.clone(),
            rule: a.rule.clone(),
            severity: a.severity,
            title: format!(
                "{}{}",
                match kind {
                    NotifyKind::Resolved => "Resolved: ",
                    NotifyKind::Repeat => "Still: ",
                    NotifyKind::Firing => "",
                },
                a.rule.replace('_', " ")
            ),
            message: a.message.clone(),
            count: a.count,
            at,
            children: vec![],
            tag: a.identity.clone(),
            sinks: rule.sinks.clone(),
        }
    }

    /// Feed one signal. Notifications that are due now are returned (grouped ones wait in the
    /// queue until [`Engine::tick`]).
    pub fn observe(&mut self, s: Signal, mute: MuteFn<'_>) -> Vec<Notification> {
        let (rule_id, vars, active, sev, cause, at, event) = match s {
            Signal::Event {
                rule,
                vars,
                severity,
                cause,
                at,
            } => (rule, vars, true, severity, cause, at, true),
            Signal::Condition {
                rule,
                vars,
                active,
                severity,
                cause,
                at,
            } => (rule, vars, active, severity, cause, at, false),
        };
        let Some(rule) = self.rules.get(&rule_id).cloned() else {
            return vec![];
        };
        if !rule.enabled {
            return vec![];
        }
        let id = identity(&rule, &vars);
        let severity = sev.unwrap_or(rule.severity);
        let message = render(&rule.message, &vars);
        let mut out = Vec::new();
        let existing = self.alerts.get(&id).cloned();
        if !active {
            // A condition went away.
            if let Some(mut a) = existing {
                match a.state {
                    AlertState::Pending => {
                        self.alerts.remove(&id);
                    }
                    AlertState::Open => {
                        a.state = AlertState::Resolved;
                        a.resolved_at = Some(at);
                        a.since = at;
                        a.last = at;
                        let sent = a.last_sent.is_some();
                        self.alerts.insert(id.clone(), a.clone());
                        self.dirty.push(id.clone());
                        if rule.resolve && sent {
                            self.queue.retain(|q| q.n.identity != id);
                            let n = self.notify(&rule, &a, NotifyKind::Resolved, at);
                            if mute(&rule.id, severity == Severity::Critical).is_none() {
                                out.push(n);
                            }
                        }
                    }
                    AlertState::Resolved => {}
                }
            }
            return out;
        }
        let mut a = match existing {
            Some(a) => a,
            None => Alert {
                identity: id.clone(),
                rule: rule.id.clone(),
                severity,
                state: AlertState::Pending,
                message: message.clone(),
                first: at,
                last: at,
                count: 0,
                since: at,
                last_sent: None,
                resolved_at: None,
                cause: cause.clone(),
                event,
                muted_by: None,
                flapping: false,
            },
        };
        a.severity = a.severity.max(severity);
        a.message = message;
        a.last = at;
        a.cause = cause.clone().or(a.cause);
        let mut fire = None;
        match a.state {
            AlertState::Resolved => {
                // Reopened. Within cooldown of the resolve it is flapping: count, no re-send.
                let flapping = a.resolved_at.is_some_and(|r| at - r < rule.cooldown);
                a.state = if rule.hold_for > 0 && !event {
                    AlertState::Pending
                } else {
                    AlertState::Open
                };
                a.since = at;
                a.resolved_at = None;
                a.count += 1;
                if a.state == AlertState::Open && !flapping {
                    fire = Some(NotifyKind::Firing);
                }
                a.flapping = flapping;
                if !flapping {
                    a.last_sent = None;
                }
            }
            AlertState::Pending => {
                if event || at - a.since >= rule.hold_for {
                    a.state = AlertState::Open;
                    a.since = at;
                    if a.flapping {
                        a.flapping = false;
                    } else {
                        a.count += 1;
                        fire = Some(NotifyKind::Firing);
                    }
                }
            }
            AlertState::Open => {
                if event {
                    a.count += 1;
                }
                if a.last_sent.is_some_and(|t| at - t >= rule.repeat) {
                    fire = Some(NotifyKind::Repeat);
                }
            }
        }
        if a.state == AlertState::Pending && event {
            a.state = AlertState::Open;
            a.count += 1;
            fire = Some(NotifyKind::Firing);
        }
        if let Some(kind) = fire {
            let muted = mute(&rule.id, a.severity == Severity::Critical);
            a.muted_by = muted.clone();
            if muted.is_none() {
                a.last_sent = Some(at);
                let n = self.notify(&rule, &a, kind, at);
                match (&a.cause, kind) {
                    (Some(c), NotifyKind::Firing) if self.group_window > 0 => self.queue.push(Queued {
                        n,
                        cause: c.clone(),
                        ready_at: at + self.group_window,
                    }),
                    _ => out.push(n),
                }
            }
        }
        self.alerts.insert(id.clone(), a);
        self.dirty.push(id);
        out
    }

    /// Time passes: pending conditions that held long enough were already opened by their next
    /// signal; here, event alerts resolve after their cooldown, repeats are due, and grouped
    /// notifications whose window closed go out (one per cause, the first as the parent).
    pub fn tick(&mut self, now: i64, mute: MuteFn<'_>) -> Vec<Notification> {
        let mut out = Vec::new();
        // Event alerts close themselves after cooldown (no resolve message: nothing to clear).
        let mut changed = Vec::new();
        for a in self.alerts.values_mut() {
            let Some(rule) = self.rules.get(&a.rule) else { continue };
            if a.event && a.state == AlertState::Open && now - a.last >= rule.cooldown {
                a.state = AlertState::Resolved;
                a.resolved_at = Some(now);
                a.since = now;
                changed.push(a.identity.clone());
            }
        }
        self.dirty.extend(changed);
        // Repeats for open conditions.
        let ids: Vec<String> = self
            .alerts
            .values()
            .filter(|a| !a.event && a.state == AlertState::Open)
            .map(|a| a.identity.clone())
            .collect();
        for id in ids {
            let (rule, a) = match self.alerts.get(&id) {
                Some(a) => match self.rules.get(&a.rule) {
                    Some(r) => (r.clone(), a.clone()),
                    None => continue,
                },
                None => continue,
            };
            if a.last_sent.is_some_and(|t| now - t >= rule.repeat)
                && mute(&rule.id, a.severity == Severity::Critical).is_none()
            {
                out.push(self.notify(&rule, &a, NotifyKind::Repeat, now));
                if let Some(x) = self.alerts.get_mut(&id) {
                    x.last_sent = Some(now);
                }
                self.dirty.push(id);
            }
        }
        // Groups.
        let due: Vec<String> = self
            .queue
            .iter()
            .filter(|q| q.ready_at <= now)
            .map(|q| q.cause.clone())
            .collect();
        for cause in dedup(due) {
            let mut members: Vec<Queued> = Vec::new();
            let mut keep = Vec::new();
            for q in self.queue.drain(..) {
                if q.cause == cause {
                    members.push(q);
                } else {
                    keep.push(q);
                }
            }
            self.queue = keep;
            members.sort_by(|a, b| b.n.severity.cmp(&a.n.severity).then(a.n.at.cmp(&b.n.at)));
            let mut it = members.into_iter();
            if let Some(first) = it.next() {
                let mut n = first.n;
                for m in it {
                    n.children.push(Child {
                        identity: m.n.identity,
                        rule: m.n.rule,
                        message: m.n.message,
                    });
                    for s in m.n.sinks {
                        if !n.sinks.contains(&s) {
                            n.sinks.push(s);
                        }
                    }
                }
                if !n.children.is_empty() {
                    n.title = format!("{} (+{} related)", n.title, n.children.len());
                }
                out.push(n);
            }
        }
        out
    }

    /// Clear an alert by hand (the UI's "acknowledge and close").
    pub fn close(&mut self, identity: &str, now: i64) -> bool {
        match self.alerts.get_mut(identity) {
            Some(a) if a.state != AlertState::Resolved => {
                a.state = AlertState::Resolved;
                a.resolved_at = Some(now);
                a.since = now;
                self.dirty.push(identity.to_string());
                true
            }
            _ => false,
        }
    }
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}
