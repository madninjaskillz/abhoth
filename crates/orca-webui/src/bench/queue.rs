//! `BenchQueue` (Bench page → Queue): setups added one by one or as "All" expansions, run in
//! order but grouped by server layout (an item that fits the live layout goes first, so the
//! server switches as rarely as possible). Started with one confirmation for the whole queue;
//! each item goes through the runner like any run. When it finishes, is paused or stopped, the
//! server is switched back to the layout that was live when it started. Kept in queue.json, so
//! it survives restarts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use orca_core::profiles::agent_modes;
use orca_core::profiles::catalog::ProfileCatalog;
use orca_core::profiles::service::ProfileService;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use super::CancelToken;
use super::runner::BenchRunner;
use super::time;

const RESTORE_KEY: &str = "queue";

/// One queued benchmark: a suite's tasks in one setup (profile[:variant] + mode), `reps` times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchQueueItem {
    pub id: String,
    pub suite: String,
    /// Canonical profile reference; `None` = the config's own.
    pub profile: Option<String>,
    /// auto, hands-on, orchestrator, peer; `None` for a bridge profile.
    pub mode: Option<String>,
    pub reps: i32,
    /// queued, started, done, failed, cancelled.
    pub status: String,
    pub batch: Option<String>,
    pub note: Option<String>,
    pub runs: Vec<String>,
    pub added: String,
    pub started: Option<String>,
    pub ended: Option<String>,
}

impl Default for BenchQueueItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            suite: String::new(),
            profile: None,
            mode: None,
            reps: 1,
            status: "queued".into(),
            batch: None,
            note: None,
            runs: Vec::new(),
            added: "0001-01-01T00:00:00+00:00".into(),
            started: None,
            ended: None,
        }
    }
}

/// queue.json: the items, whether the queue runs, and where to switch back to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchQueueFile {
    pub running: bool,
    pub current: Option<String>,
    /// The layout that was live when the queue started.
    pub restore_layout: Option<String>,
    pub restore_scheduled: bool,
    pub confirmed_at: Option<String>,
    pub items: Vec<BenchQueueItem>,
    /// The router calendar's bench entry protecting this run of the queue, and its end (epoch).
    pub protection: Option<String>,
    pub protection_until: Option<i64>,
}

/// A saved profile by name, or per-card picks (`cards`: composed into a profile on the server).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NewItem {
    pub suite: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub reps: Option<i32>,
    pub cards: Option<serde_json::Map<String, Value>>,
}

impl NewItem {
    pub fn new(suite: &str, profile: Option<&str>, mode: Option<&str>, reps: i32) -> Self {
        Self {
            suite: Some(suite.into()),
            profile: profile.map(str::to_string),
            mode: mode.map(str::to_string),
            reps: Some(reps),
            cards: None,
        }
    }
}

/// Bench protection (SUPERVISOR.md 9, Q10): while the queue runs, a calendar entry on the
/// router keeps on-demand switches off the server. The router link implements it
/// (`router_admin::RouterBenchProtect`); failures are logged and never stop the bench.
#[async_trait::async_trait]
pub trait BenchProtect: Send + Sync {
    /// Starts protection for about `seconds`: the calendar entry id and when it ends (epoch).
    async fn begin(&self, run: &str, seconds: i64) -> Result<(String, i64), String>;
    /// Moves the end to `seconds` from now; returns the new end.
    async fn extend(&self, id: &str, seconds: i64) -> Result<i64, String>;
    async fn end(&self, id: &str) -> Result<(), String>;
}

/// The first protection window; when less than [`PROTECT_MARGIN`] is left it is extended.
pub const PROTECT_SECONDS: i64 = 2 * 3600;
pub const PROTECT_MARGIN: i64 = 20 * 60;

pub struct BenchQueue {
    runner: Arc<BenchRunner>,
    protect: Mutex<Option<Arc<dyn BenchProtect>>>,
    profiles: Option<Arc<ProfileService>>,
    file: PathBuf,
    q: Mutex<BenchQueueFile>,
    wake: Arc<Notify>,
    changed: watch::Sender<u64>,
    dispose: CancelToken,
    loop_task: Mutex<Option<JoinHandle<()>>>,
}

fn now() -> String {
    time::iso_json(SystemTime::now())
}

impl BenchQueue {
    pub fn new(
        runner: Arc<BenchRunner>,
        dir: PathBuf,
        profiles: Option<Arc<ProfileService>>,
        start_loop: bool,
    ) -> Arc<Self> {
        let file = dir.join("queue.json");
        let mut q = load(&file);
        // Runs of an item that was in progress were lost with the process: it runs again.
        for i in q.items.iter_mut().filter(|i| i.status == "started") {
            i.status = "queued".into();
            i.batch = None;
            i.note = Some("restarted after an Abhoth restart".into());
        }
        q.current = None;
        if q.restore_scheduled
            && !q.running
            && let Some(back) = &q.restore_layout
        {
            runner.schedule_restore(RESTORE_KEY, back);
        }
        let (changed, _) = watch::channel(0u64);
        let queue = Arc::new(Self {
            runner,
            protect: Mutex::new(None),
            profiles,
            file,
            q: Mutex::new(q),
            wake: Arc::new(Notify::new()),
            changed,
            dispose: CancelToken::new(),
            loop_task: Mutex::new(None),
        });
        queue.save(&queue.q.lock().unwrap());
        if start_loop {
            let h = tokio::spawn(Self::run_loop(
                Arc::downgrade(&queue),
                queue.wake.clone(),
                queue.runner.subscribe(),
                queue.dispose.clone(),
            ));
            *queue.loop_task.lock().unwrap() = Some(h);
        }
        queue
    }

    /// Turns bench protection on (the server sets it when a router is linked).
    pub fn set_protect(&self, p: Arc<dyn BenchProtect>) {
        *self.protect.lock().unwrap() = Some(p);
        self.notify();
    }

    /// Keeps the protection in step with the queue: begun (or extended) while it runs or an
    /// item is in progress, ended after that.
    async fn sync_protection(&self) {
        let Some(p) = self.protect.lock().unwrap().clone() else {
            return;
        };
        let (want, id, until) = {
            let q = self.q.lock().unwrap();
            (
                q.running || q.current.is_some(),
                q.protection.clone(),
                q.protection_until,
            )
        };
        let now = time::unix(SystemTime::now()).0;
        let result = match (want, id) {
            (true, None) => {
                let run = format!("queue {}", chrono::Utc::now().format("%Y%m%d-%H%M"));
                p.begin(&run, PROTECT_SECONDS).await.map(|(id, until)| {
                    tracing::info!("Bench queue: protected by calendar entry {id}");
                    (Some(id), Some(until))
                })
            }
            (true, Some(id)) if until.unwrap_or(0) - now < PROTECT_MARGIN => p
                .extend(&id, PROTECT_SECONDS / 2)
                .await
                .map(|u| (Some(id), Some(u))),
            (false, Some(id)) => p.end(&id).await.map(|_| {
                tracing::info!("Bench queue: protection {id} ended");
                (None, None)
            }),
            _ => return,
        };
        match result {
            Ok((id, until)) => {
                let mut q = self.q.lock().unwrap();
                q.protection = id;
                q.protection_until = until;
                self.save(&q);
            }
            Err(e) => tracing::warn!("Bench queue: calendar protection: {e}"),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify(&self) {
        self.wake.notify_one();
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    // ── editing ─────────────────────────────────────────────────────────────────────────

    /// Add items at the end (invalid ones are refused with the reason).
    pub async fn add(&self, items: Vec<NewItem>) -> (i32, Vec<String>) {
        // Per-card picks first: each becomes a profile name (a saved one or a composed one).
        let mut list = Vec::new();
        let mut skipped = Vec::new();
        for n in items {
            match (&n.cards, &self.profiles) {
                (Some(cards), Some(p)) => match p.compose(&Value::Object(cards.clone())).await {
                    Ok(name) => list.push(NewItem {
                        profile: Some(name),
                        cards: None,
                        ..n
                    }),
                    Err(e) => skipped.push(format!(
                        "{} · {}: {e}",
                        n.suite.as_deref().unwrap_or(""),
                        Value::Object(cards.clone())
                    )),
                },
                _ => list.push(n),
            }
        }
        let catalog: Option<ProfileCatalog> = match &self.profiles {
            Some(p) => p.get_catalog().await.0,
            None => None,
        };
        let suites: BTreeMap<String, super::BenchSuite> = self
            .runner
            .suites()
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        let mut add = Vec::new();
        for n in list {
            let label = format!(
                "{} · {} · {}",
                n.suite.as_deref().unwrap_or(""),
                n.profile.as_deref().unwrap_or("(config)"),
                n.mode.as_deref().unwrap_or("-")
            );
            let Some(suite) = n.suite.as_ref().and_then(|s| suites.get(s)) else {
                skipped.push(format!("{label}: no such suite"));
                continue;
            };
            if suite.targets.as_ref().is_some_and(|t| !t.is_empty()) {
                skipped.push(format!(
                    "{label}: a targeted suite runs on its own endpoints, not on a profile"
                ));
                continue;
            }
            let mut profile = None;
            let mut def = None;
            if let Some(p) = n.profile.as_deref().filter(|p| !p.is_empty()) {
                let Some(key) = catalog.as_ref().and_then(|c| c.resolve(Some(p))) else {
                    skipped.push(format!("{label}: unknown profile"));
                    continue;
                };
                def = catalog.as_ref().and_then(|c| c.find(&key).cloned());
                profile = Some(key);
            }
            let mut mode = None;
            if agent_modes::is_bridge(def.as_ref()) {
                if n.mode.as_deref().is_some_and(|m| !m.is_empty()) {
                    skipped.push(format!("{label}: a bridge profile has no mode"));
                    continue;
                }
            } else {
                let wanted = n
                    .mode
                    .clone()
                    .unwrap_or_else(|| agent_modes::default_of(def.as_ref()).to_string());
                match agent_modes::normalize(Some(&wanted)) {
                    Some(m) => mode = Some(m.to_string()),
                    None => {
                        skipped.push(format!("{label}: unknown mode"));
                        continue;
                    }
                }
            }
            add.push(BenchQueueItem {
                id: crate::util::short_id(10),
                suite: suite.name.clone(),
                profile,
                mode,
                reps: n.reps.unwrap_or(1).clamp(1, 100),
                added: now(),
                ..BenchQueueItem::default()
            });
        }
        let added = add.len() as i32;
        {
            let mut q = self.q.lock().unwrap();
            q.items.extend(add);
            self.save(&q);
        }
        self.notify();
        (added, skipped)
    }

    /// Remove an item; one in progress is cancelled (its runs stop) and kept as cancelled.
    pub fn remove(&self, id: &str) -> bool {
        let mut batch = None;
        {
            let mut q = self.q.lock().unwrap();
            let Some(i) = q.items.iter().position(|i| i.id == id) else {
                return false;
            };
            if q.items[i].status == "started" {
                batch = q.items[i].batch.clone();
                q.items[i].note = Some("cancelled".into());
            } else {
                q.items.remove(i);
            }
            self.save(&q);
        }
        if let Some(b) = batch {
            self.runner.cancel_batch(&b);
        }
        self.notify();
        true
    }

    /// Move an item to a position among the items (0 = first).
    pub fn move_to(&self, id: &str, to: i32) -> bool {
        {
            let mut q = self.q.lock().unwrap();
            let Some(i) = q.items.iter().position(|i| i.id == id) else {
                return false;
            };
            let item = q.items.remove(i);
            let at = (to.max(0) as usize).min(q.items.len());
            q.items.insert(at, item);
            self.save(&q);
        }
        self.notify();
        true
    }

    /// Remove every item not in progress (`finished_only`: only the finished ones).
    pub fn clear(&self, finished_only: bool) -> i32 {
        let n;
        {
            let mut q = self.q.lock().unwrap();
            let before = q.items.len();
            q.items.retain(|i| {
                !(i.status != "started"
                    && (!finished_only
                        || matches!(i.status.as_str(), "done" | "failed" | "cancelled")))
            });
            n = (before - q.items.len()) as i32;
            self.save(&q);
        }
        self.notify();
        n
    }

    /// Put finished items back in the queue (e.g. after a failure).
    pub fn requeue(&self, id: Option<&str>) -> i32 {
        let mut n = 0;
        {
            let mut q = self.q.lock().unwrap();
            for i in q.items.iter_mut().filter(|i| {
                id.is_none_or(|id| i.id == id)
                    && matches!(i.status.as_str(), "failed" | "cancelled")
            }) {
                i.status = "queued".into();
                i.batch = None;
                i.note = None;
                i.runs.clear();
                n += 1;
            }
            self.save(&q);
        }
        self.notify();
        n
    }

    // ── running ─────────────────────────────────────────────────────────────────────────

    /// Start (or resume) the queue; `confirmed`: the person saw the plan and agreed.
    pub async fn start(&self, confirmed: bool) -> Option<String> {
        if !confirmed {
            return Some(
                "Starting the queue needs a confirmation (it may switch the server's layout)."
                    .into(),
            );
        }
        let mut live = None;
        if let Some(p) = &self.profiles {
            let (catalog, error) = p.get_catalog().await;
            match catalog {
                Some(c) if !c.from_cache => {
                    live = c.resolve_layout(c.live.as_ref().and_then(|l| l.layout.as_deref()));
                }
                _ => {
                    return Some(format!(
                        "profiles API unreachable: {}",
                        error.unwrap_or_default()
                    ));
                }
            }
        }
        {
            let mut q = self.q.lock().unwrap();
            if !q.items.iter().any(|i| i.status == "queued") {
                return Some("Nothing is queued.".into());
            }
            // A switch-back still pending from a pause: keep its target.
            let pending = self.runner.cancel_restore(RESTORE_KEY);
            let kept = if q.restore_scheduled {
                q.restore_layout.clone()
            } else {
                None
            };
            q.restore_layout = pending.or(kept).or(live);
            q.restore_scheduled = false;
            q.running = true;
            q.confirmed_at = Some(now());
            self.save(&q);
        }
        self.notify();
        None
    }

    /// Pause: the item in progress finishes (or, `stop_current`, is cancelled); then the server
    /// switches back.
    pub fn pause(&self, stop_current: bool) {
        let mut batch = None;
        {
            let mut q = self.q.lock().unwrap();
            q.running = false;
            let current = q.current.clone();
            if stop_current
                && let Some(item) = q.items.iter_mut().find(|i| Some(&i.id) == current.as_ref())
                && let Some(b) = item.batch.clone()
            {
                batch = Some(b);
                item.note = Some("cancelled".into());
            }
            self.save(&q);
        }
        if let Some(b) = batch {
            self.runner.cancel_batch(&b);
        }
        self.notify();
    }

    async fn run_loop(
        weak: Weak<Self>,
        wake: Arc<Notify>,
        mut runner_changed: watch::Receiver<u64>,
        dispose: CancelToken,
    ) {
        loop {
            tokio::select! {
                _ = wake.notified() => {}
                r = runner_changed.changed() => {
                    if r.is_err() {
                        // The runner is gone: wait for the timer only.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = dispose.cancelled() => return,
            }
            let Some(this) = weak.upgrade() else { return };
            tokio::select! {
                _ = this.tick() => {}
                _ = dispose.cancelled() => return,
            }
        }
    }

    /// One step: settle the item in progress, start the next (grouped by layout), or switch
    /// back when done.
    pub async fn tick(&self) {
        self.sync_protection().await;
        let (current, running) = {
            let q = self.q.lock().unwrap();
            let cur = q
                .current
                .as_ref()
                .and_then(|c| q.items.iter().find(|i| &i.id == c))
                .map(|i| (i.id.clone(), i.batch.clone()));
            (cur, q.running)
        };
        let mut current = current;
        if let Some((cid, Some(batch))) = &current {
            let runs = self.runner.batch_runs(batch);
            if runs
                .iter()
                .any(|r| matches!(r.1.as_str(), "queued" | "setup" | "running" | "scoring"))
            {
                return;
            }
            {
                let mut q = self.q.lock().unwrap();
                if let Some(item) = q.items.iter_mut().find(|i| &i.id == cid) {
                    item.runs = runs.iter().map(|r| r.0.clone()).collect();
                    item.ended = Some(now());
                    item.status = if runs.is_empty() || runs.iter().all(|r| r.1 == "stopped") {
                        "cancelled"
                    } else if runs.iter().any(|r| r.1 == "error") {
                        "failed"
                    } else {
                        "done"
                    }
                    .into();
                    if item.status == "failed" {
                        item.note = runs
                            .iter()
                            .find(|r| r.1 == "error")
                            .and_then(|r| r.3.clone());
                    } else if item.status == "cancelled" && item.note.is_none() {
                        item.note = Some("cancelled".into());
                    }
                }
                q.current = None;
                self.save(&q);
            }
            current = None;
            self.notify();
        }
        if current.is_some() {
            return;
        }
        if !running {
            self.schedule_restore_if_due();
            return;
        }
        let Some(next) = self.pick_next().await else {
            {
                let mut q = self.q.lock().unwrap();
                q.running = false;
                self.save(&q);
            }
            tracing::info!("Bench queue finished");
            self.schedule_restore_if_due();
            self.notify();
            return;
        };
        // The queue switches back once, at the end: its items don't each switch back.
        let r = self.runner.enqueue_setup(
            &next.suite,
            next.profile.as_deref(),
            next.mode.as_deref(),
            next.reps,
            true,
            false,
        );
        {
            let mut q = self.q.lock().unwrap();
            if let Some(item) = q.items.iter_mut().find(|i| i.id == next.id) {
                match r {
                    Ok((b, n)) => {
                        item.batch = Some(b);
                        item.status = "started".into();
                        item.started = Some(now());
                        item.note = Some(format!("{n} run(s)"));
                    }
                    Err(e) => {
                        item.status = "failed".into();
                        item.note = Some(e);
                        item.ended = Some(now());
                    }
                }
                if item.status == "started" {
                    q.current = Some(next.id.clone());
                }
            }
            self.save(&q);
        }
        self.notify();
    }

    fn schedule_restore_if_due(&self) {
        {
            let mut q = self.q.lock().unwrap();
            if q.restore_scheduled && !self.runner.has_restore(RESTORE_KEY) {
                // The runner has switched back (or found the server already there, or gave
                // up): settled. Left set, the next start would schedule the same switch again.
                q.restore_scheduled = false;
                q.restore_layout = None;
                self.save(&q);
                return;
            }
            let Some(back) = q.restore_layout.clone() else {
                return;
            };
            if q.restore_scheduled {
                return;
            }
            q.restore_scheduled = true;
            self.save(&q);
            self.runner.schedule_restore(RESTORE_KEY, &back);
        }
        self.notify();
    }

    /// The next queued item: the first one that fits the live layout, else the first in order.
    async fn pick_next(&self) -> Option<BenchQueueItem> {
        let queued: Vec<BenchQueueItem> = self
            .q
            .lock()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.status == "queued")
            .cloned()
            .collect();
        let first = queued.first()?.clone();
        let Some(p) = &self.profiles else {
            return Some(first);
        };
        let Some(catalog) = p.get_catalog().await.0 else {
            return Some(first);
        };
        Some(
            queued
                .into_iter()
                .find(|i| match &i.profile {
                    None => true,
                    Some(pr) => catalog.find(pr).is_none_or(|d| catalog.fits(d)),
                })
                .unwrap_or(first),
        )
    }

    // ── what the page shows ───────────────────────────────────────────────────────────────

    pub fn state(&self) -> Value {
        let gate = self.runner.gate();
        let has_restore = self.runner.has_restore(RESTORE_KEY);
        let q = self.q.lock().unwrap().clone();
        let items: Vec<Value> = q
            .items
            .iter()
            .map(|i| {
                json!({
                    "id": i.id, "suite": i.suite, "profile": i.profile, "mode": i.mode,
                    "reps": i.reps, "batch": i.batch, "note": i.note, "runs": i.runs,
                    "added": i.added, "started": i.started, "ended": i.ended,
                    "status": i.status,
                    "detail": if i.status == "started" { Some(self.detail(i, gate.as_deref())) } else { None },
                })
            })
            .collect();
        json!({
            "running": q.running,
            "current": q.current,
            "restoreLayout": q.restore_layout,
            "restoreScheduled": q.restore_scheduled && has_restore,
            "gate": gate,
            "items": items,
        })
    }

    /// "running", "switching: ..." or "waiting: ..." for the item in progress.
    fn detail(&self, item: &BenchQueueItem, gate: Option<&str>) -> String {
        let runs = item
            .batch
            .as_deref()
            .map(|b| self.runner.batch_runs(b))
            .unwrap_or_default();
        if runs
            .iter()
            .any(|r| matches!(r.1.as_str(), "setup" | "running" | "scoring"))
        {
            return "running".into();
        }
        match gate {
            None => "starting".into(),
            Some(g) if g.to_lowercase().starts_with("switching") => g.to_string(),
            Some(g) => format!("waiting: {g}"),
        }
    }

    /// What starting the queue would do: the items in run order (grouped by layout, from the
    /// live one), the switches between groups, the expected run time, who is on the server now.
    pub async fn plan(&self, results: &[Value]) -> Value {
        let queued: Vec<BenchQueueItem> = self
            .q
            .lock()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.status == "queued")
            .cloned()
            .collect();
        let (catalog, error) = match &self.profiles {
            Some(p) => p.get_catalog().await,
            None => (None, None),
        };
        let live = catalog
            .as_ref()
            .and_then(|c| c.resolve_layout(c.live.as_ref().and_then(|l| l.layout.as_deref())));
        let layout_of = |i: &BenchQueueItem| -> Option<String> {
            let c = catalog.as_ref()?;
            let p = i.profile.as_deref()?;
            c.resolve_layout(c.find(p).and_then(|d| d.layout.as_deref()))
        };

        // Order as the queue runs them: items on the current layout first, then the next group.
        let mut order: Vec<BenchQueueItem> = Vec::new();
        let mut left = queued.clone();
        let mut at = live.clone();
        while !left.is_empty() {
            let idx = left
                .iter()
                .position(|i| {
                    let l = layout_of(i);
                    l.is_none() || l == at
                })
                .unwrap_or(0);
            let pick = left.remove(idx);
            if let Some(l) = layout_of(&pick) {
                at = Some(l);
            }
            order.push(pick);
        }

        let mut groups = Vec::new();
        let (mut run_total, mut switch_total) = (0f64, 0f64);
        let mut unknown_tasks = 0;
        let mut switch_count = 0;
        at = live.clone();
        for g in chunk(&order, &layout_of) {
            let layout = layout_of(&g[0]);
            let mut switch_seconds: Option<i32> = None;
            let switching = layout.is_some() && layout != at;
            if switching
                && let Some(p) = &self.profiles
                && let Some(pr) = &g[0].profile
            {
                switch_seconds = p.plan(pr).await.ok().map(|plan| plan.estimate_seconds);
                switch_total += switch_seconds.unwrap_or(180) as f64;
                switch_count += 1;
            }
            let mut group_run = 0f64;
            for i in &g {
                for t in self.runner.suite_tasks(&i.suite) {
                    let (sec, known) =
                        self.task_estimate(results, &t, i.profile.as_deref(), i.mode.as_deref());
                    group_run += sec * i.reps as f64;
                    if !known {
                        unknown_tasks += 1;
                    }
                }
            }
            run_total += group_run;
            groups.push(json!({
                "layout": layout.clone().or_else(|| at.clone()),
                "switchFrom": if switching { at.clone() } else { None },
                "switchSeconds": switch_seconds,
                "items": g.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
                "runSeconds": group_run.round_ties_even(),
            }));
            if layout.is_some() {
                at = layout;
            }
        }
        let back_seconds: Option<i32> =
            (at.is_some() && live.is_some() && at != live && self.profiles.is_some())
                .then_some(180);
        if let Some(b) = back_seconds {
            switch_total += b as f64;
        }
        json!({
            "live": live,
            "error": error,
            "leases": catalog.as_ref().and_then(|c| c.live.as_ref()).and_then(|l| l.leases.clone()),
            "quietWindow": catalog.as_ref().and_then(|c| c.live.as_ref()).is_some_and(|l| l.quiet_window),
            "items": order.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            "groups": groups,
            "switches": switch_count + i32::from(back_seconds.is_some()),
            "switchSeconds": switch_total.round_ties_even(),
            "runSeconds": run_total.round_ties_even(),
            "backTo": live,
            "backSeconds": back_seconds,
            "unknownTasks": unknown_tasks,
            "totalSeconds": (switch_total + run_total).round_ties_even(),
        })
    }

    /// Expected seconds for one run of a task: the median wall time of earlier runs in the
    /// same setup, else of any run of the task; else (unknown) half the task's timeout.
    fn task_estimate(
        &self,
        results: &[Value],
        task: &str,
        profile: Option<&str>,
        mode: Option<&str>,
    ) -> (f64, bool) {
        let s = |r: &Value, n: &str| r.get(n).and_then(Value::as_str).map(str::to_string);
        let wall = |r: &Value| r.get("wall_s").and_then(Value::as_f64).filter(|d| *d > 0.0);
        let same: Vec<&Value> = results
            .iter()
            .filter(|r| s(r, "task").as_deref() == Some(task))
            .collect();
        let arm = mode.or(profile);
        let exact: Vec<f64> = same
            .iter()
            .filter(|r| s(r, "profile").as_deref() == profile && s(r, "arm").as_deref() == arm)
            .filter_map(|r| wall(r))
            .collect();
        let mut pool = if exact.is_empty() {
            same.iter().filter_map(|r| wall(r)).collect()
        } else {
            exact
        };
        if pool.is_empty() {
            return (self.runner.task_timeout_seconds(task) as f64 / 2.0, false);
        }
        pool.sort_by(f64::total_cmp);
        (pool[pool.len() / 2], true)
    }

    // ── file ────────────────────────────────────────────────────────────────────────────

    /// Write queue.json (call with the lock held).
    fn save(&self, q: &BenchQueueFile) {
        let tmp = PathBuf::from(format!("{}.tmp", self.file.display()));
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, serde_json::to_string_pretty(q).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    /// Stop the loop (the server is stopping).
    pub async fn stop(&self) {
        self.dispose.cancel();
        let h = self.loop_task.lock().unwrap().take();
        if let Some(h) = h {
            let _ = tokio::time::timeout(Duration::from_secs(10), h).await;
        }
    }
}

fn load(file: &std::path::Path) -> BenchQueueFile {
    match std::fs::read_to_string(file) {
        Ok(text) => match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(
                    "Bench queue: {} unreadable, starting empty: {e}",
                    file.display()
                );
                BenchQueueFile::default()
            }
        },
        Err(_) => BenchQueueFile::default(),
    }
}

/// The ordered items cut into runs of the same layout (an item with no layout joins the group
/// it follows).
fn chunk(
    order: &[BenchQueueItem],
    layout_of: &dyn Fn(&BenchQueueItem) -> Option<String>,
) -> Vec<Vec<BenchQueueItem>> {
    let mut out = Vec::new();
    let mut cur: Vec<BenchQueueItem> = Vec::new();
    let mut at: Option<String> = None;
    for i in order {
        let l = layout_of(i);
        if !cur.is_empty() && l.is_some() && at.is_some() && l != at {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(i.clone());
        if l.is_some() {
            at = l;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
