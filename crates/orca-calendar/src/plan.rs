//! Occurrences (SUPERVISOR.md 8.1): the planned windows of every entry in a time range, with
//! `after` sequencing, `lead`/`tail`, exdates, admin skips and early ends, and live runs merged
//! in. States: `scheduled → lead → active → (extended) → tail → done`, or `skipped`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::entry::{DetectMode, Entry};
use crate::live::{Live, Run};
use crate::recur::{to_local, to_utc};
use crate::{Calendar, Priority};

/// Admin changes to single occurrences (from `router.db`).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub skipped: BTreeSet<String>,
    /// occurrence id -> the time it was ended
    pub ended: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Scheduled,
    Lead,
    Active,
    Tail,
    Extended,
    Done,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Occurrence {
    pub entry: String,
    /// `<entry>@<nominal epoch>` (planned) or `<entry>@run<start epoch>` (unplanned)
    pub id: String,
    pub title: String,
    pub priority: Priority,
    /// the recurrence's own start (before `after` moves it)
    pub nominal: i64,
    /// planned start and end
    pub start: i64,
    pub end: i64,
    /// the blocked window: from `start - lead` (or the detected start) to `end + tail` (or the
    /// last sighting + grace)
    pub block_start: i64,
    pub block_end: i64,
    /// `end + tail` as planned (past it, a longer block is a detection extension)
    pub tail_end: i64,
    pub unplanned: bool,
    /// `block_end` was pushed out by live detection
    pub extended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    pub skipped: bool,
    /// ended early by an admin
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

impl Occurrence {
    pub fn phase(&self, now: i64) -> Phase {
        if self.skipped {
            return Phase::Skipped;
        }
        if now < self.block_start {
            Phase::Scheduled
        } else if now >= self.block_end {
            Phase::Done
        } else if now < self.start {
            Phase::Lead
        } else if now < self.end {
            Phase::Active
        } else if now < self.tail_end {
            Phase::Tail
        } else {
            Phase::Extended
        }
    }

    /// Does the occurrence block now?
    pub fn on(&self, now: i64) -> bool {
        !self.skipped && now >= self.block_start && now < self.block_end
    }

    /// The effective end of the work (not the tail): the last sighting of a run, else the
    /// planned end. `after` successors start here.
    pub fn work_end(&self, live: &Live, now: i64) -> i64 {
        if let Some(e) = self.ended_at {
            return e;
        }
        match &self.run {
            Some(r) => live.run_end(r, now).max(self.start.min(self.end)),
            None => self.end,
        }
    }
}

/// How far back expansion looks so occurrences that started earlier but still block are found.
const LOOKBACK: i64 = 2 * 86_400;

impl Calendar {
    /// Every occurrence whose blocked window meets `[from, to)`, sorted by `block_start`.
    pub fn occurrences(
        &self,
        from: i64,
        to: i64,
        live: &Live,
        ov: &Overrides,
        now: i64,
    ) -> Vec<Occurrence> {
        let mut by_entry: BTreeMap<String, Vec<Occurrence>> = BTreeMap::new();
        for id in self.order() {
            let Some(e) = self.entries.get(id) else {
                continue;
            };
            if !e.cfg.enabled || e.frequent_info() {
                continue;
            }
            let pred = e.cfg.after.as_ref().and_then(|p| by_entry.get(p));
            let occ = self.expand(e, from - LOOKBACK, to, pred, live, ov, now);
            by_entry.insert(id.clone(), occ);
        }
        let mut out: Vec<Occurrence> = by_entry
            .into_values()
            .flatten()
            .filter(|o| o.block_end > from && o.block_start < to)
            .collect();
        out.sort_by(|a, b| {
            a.block_start
                .cmp(&b.block_start)
                .then(a.entry.cmp(&b.entry))
        });
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn expand(
        &self,
        e: &Entry,
        from: i64,
        to: i64,
        pred: Option<&Vec<Occurrence>>,
        live: &Live,
        ov: &Overrides,
        now: i64,
    ) -> Vec<Occurrence> {
        let lfrom = to_local(e.tz, from - e.duration - e.lead - e.tail);
        let lto = to_local(e.tz, to + e.lead);
        let mut planned = Vec::new();
        for t in e.when.starts(e.tz, lfrom, lto) {
            if e.exdates.contains(&t) || e.until.is_some_and(|u| t > u) {
                continue;
            }
            let nominal = to_utc(e.tz, t);
            let mut start = nominal;
            if let Some(ps) = pred {
                // The predecessor's occurrence that began in the day before (or at) this one.
                if let Some(p) = ps
                    .iter()
                    .filter(|p| {
                        !p.skipped
                            && !p.unplanned
                            && p.nominal <= nominal
                            && p.nominal > nominal - 86_400
                    })
                    .max_by_key(|p| p.nominal)
                {
                    start = start.max(p.work_end(live, now));
                }
            }
            let end = start + e.duration;
            let id = format!("{}@{}", e.id, nominal);
            let skipped = ov.skipped.contains(&id);
            planned.push(Occurrence {
                entry: e.id.clone(),
                title: e.cfg.title.clone(),
                priority: e.priority(),
                nominal,
                start,
                end,
                block_start: start - e.lead,
                block_end: end + e.tail,
                tail_end: end + e.tail,
                unplanned: false,
                extended: false,
                run: None,
                skipped,
                ended_at: None,
                id,
            });
        }
        // Live runs.
        if let Some(det) = &e.cfg.detect {
            let mode = det.mode;
            let mut unplanned = Vec::new();
            for r in live.runs_of(&e.id) {
                let rend = live.run_end(r, now);
                let hit = planned
                    .iter_mut()
                    .filter(|o| {
                        !o.skipped && r.start < o.block_end && rend + e.grace > o.block_start
                    })
                    .min_by_key(|o| (o.block_start - r.start).abs());
                match hit {
                    Some(o) => {
                        if mode != DetectMode::Extend {
                            o.block_start = o.block_start.min(r.start);
                        }
                        if mode != DetectMode::Start && rend + e.grace > o.block_end {
                            o.block_end = rend + e.grace;
                            o.extended = true;
                        }
                        o.run = Some(r.clone());
                    }
                    None if mode != DetectMode::Extend => {
                        let id = format!("{}@run{}", e.id, r.start);
                        unplanned.push(Occurrence {
                            entry: e.id.clone(),
                            title: e.cfg.title.clone(),
                            priority: e.priority(),
                            nominal: r.start,
                            start: r.start,
                            end: rend,
                            block_start: r.start,
                            block_end: rend + e.grace,
                            tail_end: rend,
                            unplanned: true,
                            extended: false,
                            run: Some(r.clone()),
                            skipped: ov.skipped.contains(&id),
                            ended_at: None,
                            id,
                        });
                    }
                    None => {}
                }
            }
            planned.extend(unplanned);
        }
        for o in planned.iter_mut() {
            if let Some(at) = ov.ended.get(&o.id) {
                o.ended_at = Some(*at);
                o.block_end = o.block_end.min(*at);
                o.end = o.end.min(*at);
            }
        }
        // No trimming to the window here: an `after` successor needs its predecessor even when
        // that one starts after `to`. `occurrences` trims.
        planned
    }

    /// The next `n` occurrences of one entry from `now` (the edit preview).
    pub fn next_of(
        &self,
        entry: &str,
        now: i64,
        n: usize,
        live: &Live,
        ov: &Overrides,
    ) -> Vec<Occurrence> {
        let mut horizon = 14 * 86_400;
        loop {
            let v: Vec<Occurrence> = self
                .occurrences(now, now + horizon, live, ov, now)
                .into_iter()
                .filter(|o| o.entry == entry && o.block_end > now)
                .collect();
            if v.len() >= n || horizon >= 800 * 86_400 {
                return v.into_iter().take(n).collect();
            }
            horizon *= 4;
        }
    }
}
