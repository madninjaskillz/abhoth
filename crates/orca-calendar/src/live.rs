//! Live detection (SUPERVISOR.md 8.1): runs of a signal (the `run_daily.sh` process of a
//! channel, a lock file, the bench queue) as the host probe reports them. The calendar merges
//! runs into occurrences: a run inside a planned occurrence extends it while it lasts plus
//! `grace`; a run with no planned occurrence starts an unplanned one.
//!
//! A run whose entry has not been reported for [`STALE_SECONDS`] is assumed to be still going
//! ("stale means wait", as the guard board does).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const STALE_SECONDS: i64 = 45;
/// A signal that comes back within this long after it went away continues the same run.
pub const REJOIN_SECONDS: i64 = 90;
/// Runs older than this are forgotten.
pub const KEEP_SECONDS: i64 = 14 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub entry: String,
    /// the first report that saw the signal
    pub start: i64,
    /// the last report that saw it
    pub last_seen: i64,
    pub ongoing: bool,
    /// what was seen (`pid 4121`, `queue running: kv-ab`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a report changed (for `router.db` and the audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Started(Run),
    Rejoined(Run),
    Ended(Run),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Live {
    pub runs: Vec<Run>,
    /// entry -> the last report time
    pub last_report: BTreeMap<String, i64>,
}

impl Live {
    /// A reading for `entry` taken at `at`. Readings older than the last one are ignored.
    pub fn report(
        &mut self,
        entry: &str,
        at: i64,
        present: bool,
        detail: Option<String>,
    ) -> Option<Change> {
        if self.last_report.get(entry).is_some_and(|t| *t > at) {
            return None;
        }
        self.last_report.insert(entry.to_string(), at);
        let last = self
            .runs
            .iter_mut()
            .filter(|r| r.entry == entry)
            .max_by_key(|r| r.start);
        match (present, last) {
            (true, Some(r)) if r.ongoing => {
                r.last_seen = at;
                if detail.is_some() {
                    r.detail = detail;
                }
                None
            }
            (true, Some(r)) if at - r.last_seen <= REJOIN_SECONDS => {
                r.ongoing = true;
                r.last_seen = at;
                Some(Change::Rejoined(r.clone()))
            }
            (true, _) => {
                let r = Run {
                    entry: entry.to_string(),
                    start: at,
                    last_seen: at,
                    ongoing: true,
                    detail,
                };
                self.runs.push(r.clone());
                Some(Change::Started(r))
            }
            (false, Some(r)) if r.ongoing => {
                // After a silence the run was assumed to go on: it ends at this report.
                if at - r.last_seen > STALE_SECONDS {
                    r.last_seen = at;
                }
                r.ongoing = false;
                Some(Change::Ended(r.clone()))
            }
            (false, _) => None,
        }
    }

    pub fn runs_of<'a>(&'a self, entry: &'a str) -> impl Iterator<Item = &'a Run> + 'a {
        self.runs.iter().filter(move |r| r.entry == entry)
    }

    /// Is the entry's detector silent (no report for [`STALE_SECONDS`])?
    pub fn stale(&self, entry: &str, now: i64) -> bool {
        self.last_report
            .get(entry)
            .is_none_or(|t| now - t > STALE_SECONDS)
    }

    /// The end of a run as of `now`: its last sighting, or `now` while it goes on (or while
    /// its detector is silent: stale means still running).
    pub fn run_end(&self, r: &Run, now: i64) -> i64 {
        if r.ongoing {
            now.max(r.last_seen)
        } else {
            r.last_seen
        }
    }

    pub fn prune(&mut self, now: i64) {
        self.runs
            .retain(|r| r.ongoing || now - r.last_seen < KEEP_SECONDS);
    }
}
