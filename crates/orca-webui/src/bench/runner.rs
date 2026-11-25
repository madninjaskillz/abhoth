//! `BenchRunner`: runs bench suites. Each run gets a fresh workspace from the task kit and a
//! session with its own home (the arm's lead mode and settings), shown live as a read-only
//! project tab and scored by the kit's checker; a row per run goes to results.jsonl. Never
//! inside the 06:45-07:30 UK window. An untargeted suite runs one run at a time, alone, and only
//! while pig-slots shows no leases; a targeted suite runs one run per target, targets in
//! parallel, without that gate.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime};

use orca_core::profiles::catalog::ProfileDefinition;
use orca_core::profiles::service::ProfileService;
use orca_core::profiles::{agent_modes, config_overrides};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use super::targets::{BenchLease, BenchTarget};
use super::time::{self, in_quiet_window, round};
use super::{BenchArm, BenchLive, BenchOptions, BenchSuite, CancelToken};
use crate::audit::AuditLog;
use crate::git::service::{self as git, GitResult};
use crate::projects::manager::{ProjectManager, ProjectRuntime};

/// A run the runner knows (queued, running or recently ended).
#[derive(Clone)]
struct BenchRun {
    id: String,
    suite: String,
    arm: BenchArm,
    task: String,
    rep: i32,
    timeout_seconds: Option<i32>,
    /// The inference profile the run uses (the arm's, else the suite's).
    profile: Option<String>,
    /// The Enqueue call the run came from ("<time>-<suite>").
    batch: Option<String>,
    /// Targeted runs: the targets it may run on (one for "each" suites).
    candidates: Option<Vec<BenchTarget>>,
    target: Option<BenchTarget>,
    live: Option<BenchLive>,
    pass: Option<bool>,
    tests_passed: Option<i64>,
    tests_total: Option<i64>,
    wall_seconds: Option<f64>,
    model: Option<String>,
    cancel: Option<CancelToken>,
    /// queued, setup, running, scoring, passed, failed, timeout, stopped, error.
    status: String,
    note: Option<String>,
    project_id: Option<String>,
    /// Somebody confirmed on the Bench page that this run may switch the server's layout.
    switch_confirmed: bool,
    /// Switch back to the layout that was live once the batch has no run left.
    restore_after: bool,
    started_at: Option<SystemTime>,
    ended_at: Option<SystemTime>,
}

#[derive(Default)]
struct Inner {
    runs: BTreeMap<String, BenchRun>,
    queue: Vec<String>,
    running: Vec<String>,
    recent: VecDeque<String>,
    /// Batches that switched the server: the layout that was live before (in insertion order).
    restore: Vec<(String, String)>,
    stamp_base: String,
    stamp_dup: i32,
    paused: bool,
    gate: Option<String>,
}

impl Inner {
    fn run(&self, id: &str) -> &BenchRun {
        &self.runs[id]
    }

    fn remember(&mut self, id: &str) {
        self.recent.push_front(id.to_string());
        if self.recent.len() > 500
            && let Some(old) = self.recent.pop_back()
            && !self.queue.contains(&old)
            && !self.running.contains(&old)
            && !self.recent.contains(&old)
        {
            self.runs.remove(&old);
        }
    }

    /// The next run that may start and where. An untargeted run runs alone and in order:
    /// nothing behind it starts until it has run. A targeted run takes the first of its targets
    /// no running run is using; one whose targets are all busy lets later runs pass.
    fn pick(&self, held: &HashSet<String>) -> (Option<String>, Option<BenchTarget>) {
        if self.paused
            || self
                .running
                .iter()
                .any(|id| self.run(id).candidates.is_none())
        {
            return (None, None);
        }
        let busy: HashSet<String> = self
            .running
            .iter()
            .filter_map(|id| self.run(id).target.as_ref().map(BenchTarget::key))
            .collect();
        for id in &self.queue {
            let r = self.run(id);
            let Some(candidates) = &r.candidates else {
                return if self.running.is_empty() && !held.contains(id) {
                    (Some(id.clone()), None)
                } else {
                    (None, None)
                };
            };
            if held.contains(id) {
                continue;
            }
            if let Some(free) = candidates.iter().find(|c| !busy.contains(&c.key())) {
                return (Some(id.clone()), Some(free.clone()));
            }
        }
        (None, None)
    }
}

static USAGE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Slot usage (?<route>.+?)(?: on (?<slot>\S+))?: prompt (?<p>\d+), cached (?<c>\d+), re-read \d+, output (?<o>\d+), reasoning (?<r>\d+), (?<ms>\d+) ms \| (?<d>.*)$").unwrap()
});

// EngineEventRenderer's "Bridge turn success: cost 0.0123 USD, in 1234 / out 56 tokens, 3 model turns, 4567 ms".
static BRIDGE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Bridge turn \w+: cost (?<cost>[\d.Ee+-]*) USD, in (?<in>\d*) / out (?<out>\d*) tokens, (?<turns>\d*) model turns").unwrap()
});

static SUITE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$").unwrap());

static ENV_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());

static UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]+").unwrap());

/// What a kit command printed: exit code, its last stdout line as JSON, the stderr tail.
type KitOutput = (i32, Option<Value>, String);

pub struct BenchRunner {
    pub options: BenchOptions,
    projects: Arc<ProjectManager>,
    audit: Arc<AuditLog>,
    profiles: Option<Arc<ProfileService>>,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    wake: Arc<Notify>,
    changed: watch::Sender<u64>,
    results_write: tokio::sync::Mutex<()>,
    dispose: CancelToken,
    tasks: Mutex<Option<Vec<Value>>>,
    work: Mutex<Vec<JoinHandle<()>>>,
    loop_task: Mutex<Option<JoinHandle<()>>>,
}

fn safe(s: &str) -> String {
    UNSAFE.replace_all(s, "-").trim_matches('-').to_string()
}

fn status_message(status: reqwest::StatusCode) -> String {
    format!(
        "Response status code does not indicate success: {} ({}).",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}

fn unsafe_run_id(id: &str) -> bool {
    id.contains('/') || id.contains('\\') || id.contains("..")
}

/// The last `max` characters of `s`.
fn tail_chars(s: &str, max: usize) -> &str {
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let skip = n - max;
    let at = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    &s[at..]
}

impl BenchRunner {
    pub fn new(
        options: BenchOptions,
        projects: Arc<ProjectManager>,
        audit: Arc<AuditLog>,
        profiles: Option<Arc<ProfileService>>,
    ) -> Arc<Self> {
        let _ = std::fs::create_dir_all(options.dir.join("suites"));
        let _ = std::fs::create_dir_all(options.dir.join("runs"));
        let (changed, _) = watch::channel(0u64);
        let runner = Arc::new(Self {
            options,
            projects,
            audit,
            profiles,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            inner: Mutex::new(Inner::default()),
            wake: Arc::new(Notify::new()),
            changed,
            results_write: tokio::sync::Mutex::new(()),
            dispose: CancelToken::new(),
            tasks: Mutex::new(None),
            work: Mutex::new(Vec::new()),
            loop_task: Mutex::new(None),
        });
        runner.seed_suites();
        let weak = Arc::downgrade(&runner);
        let (wake, dispose, interval) = (
            runner.wake.clone(),
            runner.dispose.clone(),
            runner.options.gate_interval,
        );
        let handle = tokio::spawn(Self::run_loop(weak, wake, dispose, interval));
        *runner.loop_task.lock().unwrap() = Some(handle);
        runner
    }

    // ── state for the page ─────────────────────────────────────────────────────────────

    pub fn paused(&self) -> bool {
        self.inner.lock().unwrap().paused
    }

    /// Why the next run is waiting (slots leased, quiet window), or `None`.
    pub fn gate(&self) -> Option<String> {
        self.inner.lock().unwrap().gate.clone()
    }

    pub fn results_file(&self) -> PathBuf {
        self.options.dir.join("results.jsonl")
    }

    /// Changes (a run's line, the queue, the gate): the value rises on each.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn changed(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn notify(&self) {
        self.wake.notify_one();
        self.changed();
    }

    /// The bare openorca mirror the real tasks start from (REALTASKS_REPO, else <kit>/openorca.git).
    pub fn mirror_path(&self) -> PathBuf {
        match self.options.environment.get("REALTASKS_REPO") {
            Some(r) => PathBuf::from(r),
            None => self.options.kit_dir.join("openorca.git"),
        }
    }

    fn kit_environment(&self) -> BTreeMap<String, String> {
        let mut env = self.options.environment.clone();
        if !env.contains_key("REALTASKS_REPO") && self.mirror_path().is_dir() {
            env.insert(
                "REALTASKS_REPO".into(),
                self.mirror_path().to_string_lossy().into_owned(),
            );
        }
        env
    }

    /// The mirror's state for the page: present, newest commit, when it was last refreshed.
    pub async fn mirror(&self) -> Value {
        let path = self.mirror_path();
        let shown = path.to_string_lossy().into_owned();
        if !path.is_dir() {
            return json!({"exists": false, "path": shown});
        }
        let head = git::run(
            &path,
            &["log", "-1", "--all", "--format=%h %cs %s"],
            None,
            None,
            None,
        )
        .await;
        let origin = git::run(&path, &["remote", "get-url", "origin"], None, None, None).await;
        let stamp = ["FETCH_HEAD", "packed-refs", "HEAD"]
            .iter()
            .filter_map(|f| {
                std::fs::metadata(path.join(f))
                    .and_then(|m| m.modified())
                    .ok()
            })
            .max();
        let updated = match stamp {
            Some(t) => {
                let (s, n) = time::unix(t);
                crate::util::iso_from_unix(s, n)
            }
            None => "0001-01-01T00:00:00".into(),
        };
        json!({
            "exists": true,
            "path": shown,
            "head": head.output.trim(),
            "origin": if origin.ok() { Some(origin.output.trim().to_string()) } else { None },
            "updated": updated,
        })
    }

    /// "Refresh mirror": fetch every ref from the mirror's origin. Not while a run is in progress.
    pub async fn refresh_mirror(&self) -> GitResult {
        let busy = !self.inner.lock().unwrap().running.is_empty();
        let failed = |msg: String| GitResult {
            exit_code: 2,
            output: String::new(),
            error: msg,
            code: None,
        };
        if busy {
            return failed("A run is in progress; refresh the mirror between runs.".into());
        }
        let path = self.mirror_path();
        if !path.is_dir() {
            return failed(format!("No mirror at {}.", path.display()));
        }
        let r = git::run(
            &path,
            &["remote", "update", "--prune"],
            None,
            None,
            Some(Duration::from_secs(600)),
        )
        .await;
        self.changed();
        r
    }

    // ── suites ─────────────────────────────────────────────────────────────────────────

    pub fn suites(&self) -> Vec<BenchSuite> {
        let dir = self.options.dir.join("suites");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json") && p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut list = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            match serde_json::from_str::<BenchSuite>(text.trim_start_matches('\u{feff}')) {
                Ok(mut s) => {
                    s.name = f
                        .file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    list.push(s);
                }
                Err(e) => {
                    tracing::warn!("Bench suite {} is not valid JSON: {e}", f.display());
                }
            }
        }
        list
    }

    fn seed_suites(&self) {
        // Each built-in suite is written once (recorded in .seeded): new built-ins appear in an
        // existing bench folder, and one the user deleted stays deleted.
        let dir = self.options.dir.join("suites");
        let record = dir.join(".seeded");
        let mut seeded: BTreeSet<String> = if record.is_file() {
            std::fs::read_to_string(&record)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        } else {
            std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.extension().is_some_and(|x| x == "json"))
                        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                        .collect()
                })
                .unwrap_or_default()
        };
        if !record.is_file() && !seeded.is_empty() {
            // Folders seeded before the record existed.
            for n in ["validate-undo", "fanout-ez-arms", "fanout8"] {
                seeded.insert(n.into());
            }
        }
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        let mut write = |name: &str, s: BenchSuite| {
            if !seeded.insert(name.to_string()) {
                return;
            }
            let f = dir.join(format!("{name}.json"));
            if !f.exists() {
                let _ = std::fs::write(&f, serde_json::to_string_pretty(&s).unwrap_or_default());
            }
            let text: String = seeded.iter().map(|l| format!("{l}{nl}")).collect();
            let _ = std::fs::write(&record, text);
        };
        let arm = |n: &str, mode: &str| BenchArm::new(n, mode, None, None);
        let suite = |name: &str, arms: Vec<BenchArm>, tasks: &[&str], desc: &str| BenchSuite {
            description: Some(desc.into()),
            ..BenchSuite::new(name, arms, tasks)
        };
        write(
            "validate-undo",
            suite(
                "validate-undo",
                vec![arm("auto", "auto")],
                &["fanout-ez-undo"],
                "One fanout-ez component (Undo), auto Lead: a short end-to-end check of the runner.",
            ),
        );
        write(
            "fanout-ez-arms",
            suite(
                "fanout-ez-arms",
                vec![
                    arm("auto", "auto"),
                    arm("hands-on", "hands-on"),
                    BenchArm::new(
                        "peer",
                        "auto",
                        Some(vec!["slots.agentsPerCopy=3".into()]),
                        None,
                    ),
                ],
                &["fanout-ez"],
                "Five C# components: does handing them to helpers beat one agent? auto vs hands-on vs more helpers per copy.",
            ),
        );
        write(
            "fanout8",
            suite(
                "fanout8",
                vec![arm("auto", "auto"), arm("hands-on", "hands-on")],
                &["fanout8"],
                "Eight small Python modules (the original fan-out task).",
            ),
        );
        write(
            "realtasks",
            suite(
                "realtasks",
                vec![arm("auto", "auto"), arm("hands-on", "hands-on")],
                &["agents-per-copy", "head-tail-output", "context-overflow"],
                "Three past OpenOrca commits as issues, scored with the commits' own held-back tests (needs the openorca mirror).",
            ),
        );
        write(
            "realtask-one",
            suite(
                "realtask-one",
                vec![arm("auto", "auto")],
                &["agents-per-copy"],
                "One real task (slot allocator: agents per copy), auto Lead.",
            ),
        );
        write(
            "realtask-engines",
            suite(
                "realtask-engines",
                vec![
                    arm("native", "auto"),
                    BenchArm::for_engine("claude", None),
                    BenchArm::for_engine("codex", None),
                ],
                &["agents-per-copy"],
                "One real task on each engine: native Abhoth, Claude Code and Codex (the bridges need their CLI logged in).",
            ),
        );
        write(
            "profiles-flash-dense",
            suite(
                "profiles-flash-dense",
                vec![
                    BenchArm::new("flash2", "auto", None, Some("flash2")),
                    BenchArm::new("dense2", "auto", None, Some("dense2")),
                ],
                &["fanout-ez"],
                "Five C# components on two inference profiles: Flash x2 against dense 27B x2. Switches the server layout between them (about 3-4 min each way, restarting the model servers).",
            ),
        );
    }

    /// The kit's tasks (name, timeout), read once.
    pub async fn tasks(&self) -> Result<Vec<Value>, String> {
        if let Some(t) = self.tasks.lock().unwrap().clone() {
            return Ok(t);
        }
        let (rc, json, _) = self.kit(&["list"], Duration::from_secs(60)).await?;
        if rc == 0
            && let Some(Value::Array(a)) = json
        {
            *self.tasks.lock().unwrap() = Some(a);
        }
        Ok(self.tasks.lock().unwrap().clone().unwrap_or_default())
    }

    // ── control ────────────────────────────────────────────────────────────────────────

    /// Why a suite can't run, or `None`.
    pub fn problem(suite: &BenchSuite) -> Option<String> {
        if suite.arms.is_empty() || suite.tasks.is_empty() {
            return Some("The suite has no arms or no tasks.".into());
        }
        let names: HashSet<&str> = suite.arms.iter().map(|a| a.name.as_str()).collect();
        if suite.arms.iter().any(|a| a.name.trim().is_empty()) || names.len() != suite.arms.len() {
            return Some("Each arm needs a name of its own.".into());
        }
        if let Some(bad) = suite
            .arms
            .iter()
            .flat_map(|a| a.set.iter().flatten())
            .find(|kv| kv.find('=').is_none_or(|i| i == 0))
        {
            return Some(format!("An override needs key=value: {bad}"));
        }
        if suite.tasks.iter().any(|t| t.trim().is_empty()) {
            return Some("A task name is empty.".into());
        }
        if !(1..=100).contains(&suite.reps) {
            return Some("Reps must be between 1 and 100.".into());
        }
        if let Some(mode) = suite.target_mode.as_deref().filter(|m| !m.is_empty())
            && mode != "pool"
            && mode != "each"
        {
            return Some("targetMode is \"pool\" or \"each\".".into());
        }
        let targets = match &suite.targets {
            Some(t) if !t.is_empty() => t,
            _ => return None,
        };
        for t in targets {
            if t.name.trim().is_empty() {
                return Some("Each target needs a name.".into());
            }
            let ok = reqwest::Url::parse(t.endpoint.trim())
                .is_ok_and(|u| u.scheme() == "http" || u.scheme() == "https");
            if !ok {
                return Some(format!(
                    "Target {}: the endpoint must be an http(s) URL such as http://host:8080/v1.",
                    t.name
                ));
            }
            if let Some(env) = t.api_key_env.as_deref().filter(|e| !e.is_empty())
                && !ENV_NAME.is_match(env)
            {
                return Some(format!(
                    "Target {}: apiKeyEnv is the NAME of an environment variable, not a key.",
                    t.name
                ));
            }
        }
        let names: HashSet<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        let keys: HashSet<String> = targets.iter().map(BenchTarget::key).collect();
        if names.len() != targets.len() || keys.len() != targets.len() {
            return Some("Each target needs its own name and its own endpoint.".into());
        }
        if suite.profile.as_deref().is_some_and(|p| !p.is_empty())
            || suite
                .arms
                .iter()
                .any(|a| a.profile.as_deref().is_some_and(|p| !p.is_empty()))
        {
            return Some(
                "Profile arms switch the whole server's layout, so they can't run in a suite with targets."
                    .into(),
            );
        }
        None
    }

    /// Save a suite composed on the page as suites/<name>.json. An existing suite is replaced
    /// only when `overwrite`.
    pub fn save_suite(&self, suite: BenchSuite, overwrite: bool) -> Result<BenchSuite, String> {
        let name = suite.name.trim().to_string();
        if !SUITE_NAME.is_match(&name) {
            return Err(
                "A suite name is 1-64 letters, digits, dots, dashes or underscores.".into(),
            );
        }
        let targeted = suite.targets.as_ref().is_some_and(|t| !t.is_empty());
        let suite = BenchSuite {
            name: name.clone(),
            target_mode: if targeted { suite.target_mode } else { None },
            targets: if targeted { suite.targets } else { None },
            ..suite
        };
        if let Some(p) = Self::problem(&suite) {
            return Err(p);
        }
        let file = self.options.dir.join("suites").join(format!("{name}.json"));
        if file.exists() && !overwrite {
            return Err(format!("A suite named {name} already exists."));
        }
        std::fs::write(
            &file,
            serde_json::to_string_pretty(&suite).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        self.changed();
        Ok(suite)
    }

    /// Queue every run of a suite. Returns how many.
    pub fn enqueue(&self, suite_name: &str) -> Result<i32, String> {
        self.enqueue_batch(suite_name, false).map(|(_, n)| n)
    }

    /// "Run a new one in this setup": the suite's tasks with one arm (profile + mode), `reps`
    /// times. Results keep the suite's name; the arm is named after the mode.
    pub fn enqueue_setup(
        &self,
        suite_name: &str,
        profile: Option<&str>,
        mode: Option<&str>,
        reps: i32,
        confirm_switch: bool,
        restore_after: bool,
    ) -> Result<(String, i32), String> {
        let suite = self
            .suites()
            .into_iter()
            .find(|s| s.name == suite_name)
            .ok_or_else(|| format!("No such suite: {suite_name}"))?;
        if suite.targets.as_ref().is_some_and(|t| !t.is_empty()) {
            return Err("A targeted suite runs on its own endpoints, not on a profile.".into());
        }
        let own = profile.filter(|p| !p.is_empty());
        let arm = match mode.filter(|m| !m.trim().is_empty()) {
            // A bridge: no mode.
            None => BenchArm::new(profile.unwrap_or("default"), "auto", Some(vec![]), own),
            Some(m) => {
                let m = agent_modes::normalize(Some(m)).ok_or_else(|| {
                    format!("Unknown mode {m} (one of {}).", agent_modes::ALL.join(", "))
                })?;
                BenchArm::new(m, m, Some(agent_modes::bench_sets(m)), own)
            }
        };
        let adhoc = BenchSuite {
            arms: vec![arm],
            reps: reps.clamp(1, 100),
            profile: None,
            ..suite
        };
        self.enqueue_suite(&adhoc, confirm_switch, restore_after)
    }

    /// The tasks a saved suite runs (for estimates).
    pub fn suite_tasks(&self, suite_name: &str) -> Vec<String> {
        self.suites()
            .into_iter()
            .find(|s| s.name == suite_name)
            .map(|s| s.tasks)
            .unwrap_or_default()
    }

    /// Seconds a task may take at most (its kit timeout).
    pub fn task_timeout_seconds(&self, task: &str) -> i32 {
        self.tasks
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|ts| {
                ts.iter()
                    .find(|t| t.get("name").and_then(Value::as_str) == Some(task))
            })
            .and_then(|t| t.get("timeout").and_then(Value::as_i64))
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(1800)
    }

    /// Every run of a batch the runner still knows: (id, status, project tab, note).
    pub fn batch_runs(&self, batch: &str) -> Vec<(String, String, Option<String>, Option<String>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .running
            .iter()
            .chain(inner.queue.iter())
            .chain(inner.recent.iter())
            .map(|id| inner.run(id))
            .filter(|r| r.batch.as_deref() == Some(batch))
            .map(|r| {
                (
                    r.id.clone(),
                    r.status.clone(),
                    r.project_id.clone(),
                    r.note.clone(),
                )
            })
            .collect()
    }

    /// Switch the server back to `layout` once nothing of `key` is left (waits for leases).
    pub fn schedule_restore(&self, key: &str, layout: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            match inner.restore.iter_mut().find(|(k, _)| k == key) {
                Some(e) => e.1 = layout.to_string(),
                None => inner.restore.push((key.into(), layout.into())),
            }
        }
        self.notify();
    }

    /// Drop a pending switch-back; returns the layout it would have switched to.
    pub fn cancel_restore(&self, key: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let i = inner.restore.iter().position(|(k, _)| k == key)?;
        Some(inner.restore.remove(i).1)
    }

    pub fn has_restore(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .restore
            .iter()
            .any(|(k, _)| k == key)
    }

    /// Queue every run of a suite; returns the batch's id and how many runs.
    pub fn enqueue_batch(
        &self,
        suite_name: &str,
        confirm_switch: bool,
    ) -> Result<(String, i32), String> {
        let suite = self
            .suites()
            .into_iter()
            .find(|s| s.name == suite_name)
            .ok_or_else(|| format!("No such suite: {suite_name}"))?;
        self.enqueue_suite(&suite, confirm_switch, true)
    }

    fn enqueue_suite(
        &self,
        suite: &BenchSuite,
        confirm_switch: bool,
        restore_after: bool,
    ) -> Result<(String, i32), String> {
        if let Some(p) = Self::problem(suite) {
            return Err(p);
        }
        let mut stamp = time::stamp((self.options.clock)());
        let mut n = 0;
        // Arms on different server layouts run profile by profile (one layout switch per
        // profile); within a profile: rep by rep, task by task, arm by arm.
        let profile_of = |a: &BenchArm| a.profile.clone().or_else(|| suite.profile.clone());
        let mut profile_order: Vec<Option<String>> = Vec::new();
        for a in &suite.arms {
            let p = profile_of(a);
            if !profile_order.contains(&p) {
                profile_order.push(p);
            }
        }
        // Targeted: "each" = a run per target; "pool" = one run, on whichever target is free.
        let targets = suite.targets.clone().filter(|t| !t.is_empty());
        let each = targets.is_some() && suite.target_mode.as_deref() == Some("each");
        let placements: Vec<Option<Vec<BenchTarget>>> = match &targets {
            None => vec![None],
            Some(t) if each => t.iter().map(|x| Some(vec![x.clone()])).collect(),
            Some(t) => vec![Some(t.clone())],
        };
        let batch;
        {
            let mut inner = self.inner.lock().unwrap();
            // Two suites queued in the same second still get ids of their own.
            if stamp == inner.stamp_base {
                inner.stamp_dup += 1;
                stamp = format!("{stamp}-{}", inner.stamp_dup);
            } else {
                inner.stamp_base = stamp.clone();
                inner.stamp_dup = 1;
            }
            batch = format!("{stamp}-{}", safe(&suite.name));
            for profile in &profile_order {
                for rep in 1..=suite.reps.max(1) {
                    for task in &suite.tasks {
                        for arm in suite.arms.iter().filter(|a| &profile_of(a) == profile) {
                            for place in &placements {
                                n += 1;
                                let target_part = match (each, place) {
                                    (true, Some(p)) => format!("-{}", safe(&p[0].name)),
                                    _ => String::new(),
                                };
                                let id = format!(
                                    "{stamp}-{n:03}-{}-{}{target_part}-r{rep}",
                                    safe(task),
                                    safe(&arm.name)
                                );
                                let run = BenchRun {
                                    id: id.clone(),
                                    suite: suite.name.clone(),
                                    arm: arm.clone(),
                                    task: task.clone(),
                                    rep,
                                    timeout_seconds: suite.timeout_seconds,
                                    profile: profile.clone(),
                                    batch: Some(batch.clone()),
                                    candidates: place.clone(),
                                    target: if each {
                                        place.as_ref().map(|p| p[0].clone())
                                    } else {
                                        None
                                    },
                                    live: None,
                                    pass: None,
                                    tests_passed: None,
                                    tests_total: None,
                                    wall_seconds: None,
                                    model: None,
                                    cancel: None,
                                    status: "queued".into(),
                                    note: None,
                                    project_id: None,
                                    switch_confirmed: confirm_switch,
                                    restore_after,
                                    started_at: None,
                                    ended_at: None,
                                };
                                inner.runs.insert(id.clone(), run);
                                inner.queue.push(id);
                            }
                        }
                    }
                }
            }
        }
        self.notify();
        Ok((batch, n))
    }

    pub fn pause(&self) {
        self.inner.lock().unwrap().paused = true;
        self.notify();
    }

    pub fn resume(&self) {
        self.inner.lock().unwrap().paused = false;
        self.notify();
    }

    /// Drop the queue and stop every running run.
    pub fn stop(&self) {
        self.cancel_where(|_| true);
    }

    /// Cancel one run: taken off the queue, or its session stopped. False when it isn't queued
    /// or running.
    pub fn cancel(&self, run_id: &str) -> bool {
        self.cancel_where(|r| r.id == run_id) > 0
    }

    /// Cancel what is left of a batch: its queued runs and its running ones. Returns how many.
    pub fn cancel_batch(&self, batch: &str) -> i32 {
        self.cancel_where(|r| r.batch.as_deref() == Some(batch))
    }

    fn cancel_where(&self, which: impl Fn(&BenchRun) -> bool) -> i32 {
        let mut n = 0;
        let stop: Vec<CancelToken>;
        {
            let mut inner = self.inner.lock().unwrap();
            let drop: Vec<String> = inner
                .queue
                .iter()
                .filter(|id| which(inner.run(id)))
                .cloned()
                .collect();
            for id in drop {
                inner.queue.retain(|q| q != &id);
                if let Some(r) = inner.runs.get_mut(&id) {
                    r.status = "stopped".into();
                    r.note = Some("removed from the queue".into());
                }
                inner.remember(&id);
                n += 1;
            }
            stop = inner
                .running
                .iter()
                .map(|id| inner.run(id))
                .filter(|r| which(r))
                .filter_map(|r| r.cancel.clone())
                .collect();
        }
        // Only the run's own token: its tab is closed, which stops that run's session.
        for t in stop {
            t.cancel();
            n += 1;
        }
        self.notify();
        n
    }

    pub fn state(&self) -> Value {
        let suites = self.suites();
        let inner = self.inner.lock().unwrap();
        let restores: Map<String, Value> = inner
            .restore
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let list = |ids: &mut dyn Iterator<Item = &String>| -> Vec<Value> {
            ids.map(|id| self.describe(inner.run(id))).collect()
        };
        json!({
            "paused": inner.paused,
            "gate": inner.gate,
            "restores": restores,
            "current": inner.running.first().map(|id| self.describe(inner.run(id))),
            "running": list(&mut inner.running.iter()),
            "queue": list(&mut inner.queue.iter()),
            "recent": list(&mut inner.recent.iter()),
            "suites": suites,
        })
    }

    /// Every run the runner knows: running, queued, then ended (newest first).
    pub fn runs(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        inner
            .running
            .iter()
            .chain(inner.queue.iter())
            .chain(inner.recent.iter())
            .map(|id| self.describe(inner.run(id)))
            .collect()
    }

    /// (paused, gate, running, queued) for the event stream's status line.
    pub fn status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "paused": inner.paused,
            "gate": inner.gate,
            "running": inner.running.len(),
            "queued": inner.queue.len(),
        })
    }

    fn describe(&self, r: &BenchRun) -> Value {
        let elapsed = r.started_at.map(|s| {
            let end = r.ended_at.unwrap_or_else(|| (self.options.clock)());
            let secs = match end.duration_since(s) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            secs.round_ties_even().max(0.0)
        });
        json!({
            "id": r.id, "suite": r.suite, "arm": r.arm.name, "task": r.task, "rep": r.rep,
            "status": r.status, "note": r.note, "profile": r.profile,
            "projectId": r.project_id,
            "startedAt": r.started_at.map(time::iso_json),
            "endedAt": r.ended_at.map(time::iso_json),
            "batch": r.batch, "target": r.target.as_ref().map(|t| t.name.clone()),
            "elapsed": elapsed,
            "wallSeconds": r.wall_seconds, "pass": r.pass, "testsPassed": r.tests_passed,
            "testsTotal": r.tests_total, "model": r.model,
            "live": r.live,
        })
    }

    /// results.jsonl rows (newest last). `include_archived`: first the rows of every
    /// archive-*/results.jsonl (oldest folder first), each marked with `"archive": "<folder>"`.
    /// The files are only read, never rewritten.
    pub fn results(&self, include_archived: bool) -> Vec<Value> {
        let mut rows = Vec::new();
        if include_archived && let Ok(rd) = std::fs::read_dir(&self.options.dir) {
            let mut dirs: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("archive-"))
                })
                .collect();
            dirs.sort();
            for d in dirs {
                let name = d.file_name().unwrap().to_string_lossy().into_owned();
                read_rows(&d.join("results.jsonl"), Some(&name), &mut rows);
            }
        }
        read_rows(&self.results_file(), None, &mut rows);
        rows
    }

    /// A run's checker output (score.json, kit log), for the results table.
    pub fn output(&self, run_id: &str) -> Option<String> {
        if unsafe_run_id(run_id) {
            return None;
        }
        let check = self.options.dir.join("runs").join(run_id).join("check");
        if !check.is_dir() {
            return None;
        }
        let mut sb = String::new();
        for name in ["score.json", "kit.log"] {
            let Ok(bytes) = std::fs::read(check.join(name)) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            sb.push_str(&format!("── {name} ──\n{}\n\n", tail_chars(&text, 60_000)));
        }
        Some(sb)
    }

    /// The tail of a run's own logs (its home/logs), for the run detail.
    pub fn logs(&self, run_id: &str, max_chars: usize) -> Option<String> {
        if unsafe_run_id(run_id) {
            return None;
        }
        let logs = self
            .options
            .dir
            .join("runs")
            .join(run_id)
            .join("home")
            .join("logs");
        if !logs.is_dir() {
            return None;
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&logs)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        files.sort();
        let mut sb = String::new();
        for f in files {
            let Ok(bytes) = std::fs::read(&f) else {
                continue;
            };
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            sb.push_str(&format!(
                "── {name} ──\n{}\n\n",
                String::from_utf8_lossy(&bytes)
            ));
        }
        if sb.chars().count() > max_chars {
            Some(format!("…\n{}", tail_chars(&sb, max_chars)))
        } else {
            Some(sb)
        }
    }

    // ── the loop ───────────────────────────────────────────────────────────────────────

    async fn run_loop(
        weak: Weak<Self>,
        wake: Arc<Notify>,
        dispose: CancelToken,
        interval: Duration,
    ) {
        loop {
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(interval) => {}
                _ = dispose.cancelled() => return,
            }
            let Some(this) = weak.upgrade() else { return };
            if this.dispose.is_cancelled() {
                return;
            }
            tokio::select! {
                _ = this.tick() => {}
                _ = dispose.cancelled() => return,
            }
        }
    }

    async fn tick(self: &Arc<Self>) {
        // A batch that switched the server and has no run left: switch back first.
        if let Some(restoring) = self.restore_due().await {
            self.set_gate(Some(restoring));
            return;
        }
        // Start everything that may start now; the first reason something is held back is shown.
        let mut gate: Option<String> = None;
        let mut held: HashSet<String> = HashSet::new();
        loop {
            let (next, target) = self.inner.lock().unwrap().pick(&held);
            let Some(next) = next else { break };
            if let Some(why) = self.gate_for(&next).await {
                gate.get_or_insert(why);
                held.insert(next);
                continue;
            }
            let untargeted = self
                .inner
                .lock()
                .unwrap()
                .runs
                .get(&next)
                .is_some_and(|r| r.candidates.is_none());
            if untargeted {
                // The run's profile needs another server layout: switch now (no leases are
                // held, and it's outside the quiet window).
                let (problem, fatal) = self.ensure_layout(&next).await;
                if let Some(problem) = problem {
                    if fatal {
                        let profile = self
                            .inner
                            .lock()
                            .unwrap()
                            .runs
                            .get(&next)
                            .and_then(|r| r.profile.clone());
                        self.fail_profile(profile.as_deref(), &problem);
                        continue;
                    }
                    gate.get_or_insert(problem);
                    held.insert(next);
                    continue;
                }
            }
            let mut inner = self.inner.lock().unwrap();
            // Stopped or cancelled while it was being checked, or the target went to another run.
            let (again, same_target) = inner.pick(&held);
            if again.as_deref() != Some(next.as_str())
                || same_target.as_ref().map(BenchTarget::key)
                    != target.as_ref().map(BenchTarget::key)
            {
                continue;
            }
            inner.queue.retain(|q| q != &next);
            let token = CancelToken::new();
            if let Some(r) = inner.runs.get_mut(&next) {
                r.target = target;
                r.cancel = Some(token.clone());
            }
            inner.running.push(next.clone());
            drop(inner);
            let this = self.clone();
            let handle = tokio::spawn(async move { this.work(next, token).await });
            let mut work = self.work.lock().unwrap();
            work.retain(|h| !h.is_finished());
            work.push(handle);
        }
        self.set_gate(gate);
    }

    async fn work(self: Arc<Self>, id: String, token: CancelToken) {
        if let Err(e) = self.run(&id, &token).await {
            tracing::error!("Bench run {id}: {e}");
            let mut inner = self.inner.lock().unwrap();
            if let Some(r) = inner.runs.get_mut(&id)
                && matches!(
                    r.status.as_str(),
                    "queued" | "setup" | "running" | "scoring"
                )
            {
                r.status = if token.is_cancelled() {
                    "stopped"
                } else {
                    "error"
                }
                .into();
                r.note = Some(e);
            }
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running.retain(|r| r != &id);
            inner.remember(&id);
            if let Some(r) = inner.runs.get_mut(&id) {
                r.cancel = None;
            }
        }
        self.notify();
    }

    fn set_gate(&self, why: Option<String>) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.gate == why {
                return;
            }
            inner.gate = why;
        }
        self.changed();
    }

    fn run_info(&self, id: &str) -> Option<BenchRun> {
        self.inner.lock().unwrap().runs.get(id).cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut BenchRun)) {
        if let Some(r) = self.inner.lock().unwrap().runs.get_mut(id) {
            f(r);
        }
    }

    /// `None` when the run may start; else why not.
    async fn gate_for(&self, id: &str) -> Option<String> {
        let run = self.run_info(id)?;
        let now = (self.options.clock)();
        let timeout = run
            .timeout_seconds
            .unwrap_or_else(|| self.task_timeout_seconds(&run.task));
        if in_quiet_window(
            now,
            Duration::from_secs(timeout.max(0) as u64),
            self.options.quiet_from,
            self.options.quiet_to,
        ) {
            return Some(format!(
                "quiet window {}–{} UK (a run would overlap it)",
                time::hhmm(self.options.quiet_from),
                time::hhmm(self.options.quiet_to)
            ));
        }
        // A targeted run talks to its own endpoint and holds no pig-slots slot.
        if let Some(reg) = &self.options.registry_url
            && run.candidates.is_none()
        {
            let url = format!("{}/list", reg.trim_end_matches('/'));
            let text = match self.http.get(url).send().await {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(t) => t,
                    Err(e) => return Some(format!("slot registry unreachable: {e}")),
                },
                Ok(r) => {
                    return Some(format!(
                        "slot registry unreachable: {}",
                        status_message(r.status())
                    ));
                }
                Err(e) => {
                    return Some(format!("slot registry unreachable: {}", e.without_url()));
                }
            };
            let doc: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => return Some(format!("slot registry unreachable: {e}")),
            };
            if let Some(slots) = doc.get("slots").and_then(Value::as_object) {
                let leased: Vec<String> = slots
                    .iter()
                    .filter(|(_, v)| {
                        !matches!(v, Value::Null | Value::Bool(false)) && v.as_str() != Some("")
                    })
                    .map(|(k, v)| match lease_holder(v) {
                        Some(who) => format!("{k} ({who})"),
                        None => k.clone(),
                    })
                    .collect();
                if !leased.is_empty() {
                    return Some(format!(
                        "waiting for {} to finish (never pre-empted; Cancel stops waiting)",
                        leased.join(", ")
                    ));
                }
            }
        }
        None
    }

    /// Make the server run the run's profile layout. `(None, _)` when it does; `(why, false)`
    /// to wait and try again; `(why, true)` when the profile can't be had.
    async fn ensure_layout(&self, id: &str) -> (Option<String>, bool) {
        let Some(run) = self.run_info(id) else {
            return (None, false);
        };
        let Some(name) = run.profile.clone() else {
            return (None, false);
        };
        let Some(profiles) = &self.profiles else {
            return (
                Some(format!(
                    "the arm uses profile {name}, but no profiles API is configured (profiles.serverUrl)"
                )),
                true,
            );
        };
        let (catalog, error) = profiles.get_catalog().await;
        let catalog = match catalog {
            Some(c) if !c.from_cache => c,
            _ => {
                return (
                    Some(format!(
                        "profiles API unreachable: {}",
                        error.unwrap_or_default()
                    )),
                    false,
                );
            }
        };
        let Some(profile) = catalog.find(&name).cloned() else {
            return (Some(format!("unknown profile {name}")), true);
        };
        if catalog.fits(&profile) {
            return (None, false);
        }
        let layout = profile.layout.clone().unwrap_or_default();
        let live = catalog.live.as_ref().and_then(|l| l.layout.clone());
        if !run.switch_confirmed {
            return (
                Some(format!(
                    "profile {name} needs the server switched to {layout} ({} is live), and nobody confirmed that switch: start it from the Bench page, which shows the estimate and asks first",
                    live.as_deref().unwrap_or("unknown")
                )),
                true,
            );
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if run.restore_after
                && let Some(b) = &run.batch
                && !inner.restore.iter().any(|(k, _)| k == b)
                && let Some(before) = live.as_ref().filter(|l| !l.is_empty())
            {
                inner.restore.push((b.clone(), before.clone()));
            }
        }
        self.set_gate(Some(format!(
            "switching the server to {layout} for profile {name}"
        )));
        self.audit.write(
            None,
            "bench.layout_switch",
            json!({"run": run.id, "profile": name, "layout": profile.layout, "from": live}),
        );
        let job = match profiles
            .request_switch(
                &name,
                &format!("{} bench runner", orca_core::brand::BINARY_NAME),
                Some(&format!("bench {}: {}", run.suite, run.arm.name)),
            )
            .await
        {
            Ok(j) => j,
            Err(e) => return (Some(format!("server switch failed: {e}")), false),
        };
        if job.state.as_deref() == Some("refused") {
            return (
                Some(format!(
                    "server switch refused: {}",
                    job.error.unwrap_or_default()
                )),
                false,
            );
        }
        let job = profiles
            .wait_for_job(
                job,
                |s| self.set_gate(Some(format!("switching to {layout}: {}", s.msg))),
                Duration::from_secs(45 * 60),
                Some(self.options.switch_poll),
            )
            .await;
        if job.succeeded() {
            tracing::info!(
                "Bench: server switched to {layout} for {name} in {:?}s",
                job.seconds
            );
            return (None, false);
        }
        (
            Some(format!(
                "server switch to {layout} {}: {}",
                job.state.unwrap_or_default(),
                job.error.unwrap_or_default()
            )),
            true,
        )
    }

    /// Switch back for the first batch that switched the server and has nothing queued or
    /// running any more. `None` when there is nothing to do (or it just finished); else what it
    /// is waiting for. Waits for leases like a run does.
    async fn restore_due(&self) -> Option<String> {
        let (batch, layout) = {
            let inner = self.inner.lock().unwrap();
            inner
                .restore
                .iter()
                .find(|(b, _)| {
                    !inner
                        .queue
                        .iter()
                        .chain(inner.running.iter())
                        .any(|id| inner.run(id).batch.as_deref() == Some(b.as_str()))
                })
                .cloned()?
        };
        let profiles = self.profiles.as_ref()?;
        let (catalog, error) = profiles.get_catalog().await;
        let catalog = match catalog {
            Some(c) if !c.from_cache => c,
            _ => {
                return Some(format!(
                    "switching the server back to {layout} after {batch}: profiles API unreachable ({})",
                    error.unwrap_or_default()
                ));
            }
        };
        let live = catalog.live.as_ref().and_then(|l| l.layout.clone());
        let same = match (
            catalog.resolve_layout(live.as_deref()),
            catalog.resolve_layout(Some(&layout)),
        ) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
            (None, None) => true,
            _ => false,
        };
        if same {
            self.inner
                .lock()
                .unwrap()
                .restore
                .retain(|(b, _)| b != &batch);
            return None;
        }
        if let Some(leases) = catalog
            .live
            .as_ref()
            .and_then(|l| l.leases.as_ref())
            .filter(|l| !l.is_empty())
        {
            let who: Vec<String> = leases
                .iter()
                .map(|l| match l.who.as_deref().filter(|w| !w.is_empty()) {
                    Some(w) => format!("{} ({})", l.slot, w.trim()),
                    None => l.slot.clone(),
                })
                .collect();
            return Some(format!(
                "switching the server back to {layout} after {batch} once {} finish",
                who.join(", ")
            ));
        }
        self.audit.write(
            None,
            "bench.layout_restore",
            json!({"batch": batch, "layout": layout, "from": live}),
        );
        let job = match profiles
            .request_layout_switch(
                &layout,
                &format!("{} bench runner", orca_core::brand::BINARY_NAME),
                Some(&format!("bench {batch} ended: back to {layout}")),
            )
            .await
        {
            Ok(j) => j,
            Err(e) => {
                return Some(format!(
                    "switching the server back to {layout}: {e}; trying again"
                ));
            }
        };
        if job.state.as_deref() == Some("refused") {
            return Some(format!(
                "switching the server back to {layout}: refused for now ({}); trying again",
                job.error.unwrap_or_default()
            ));
        }
        let job = profiles
            .wait_for_job(
                job,
                |s| self.set_gate(Some(format!("switching back to {layout}: {}", s.msg))),
                Duration::from_secs(45 * 60),
                Some(self.options.switch_poll),
            )
            .await;
        // Done, or failed (the server rolled back): never loop on it.
        self.inner
            .lock()
            .unwrap()
            .restore
            .retain(|(b, _)| b != &batch);
        tracing::info!(
            "Bench: switch back to {layout} after {batch}: {}",
            job.state.unwrap_or_default()
        );
        None
    }

    /// The profile can't run: its queued runs end as errors (other profiles carry on).
    fn fail_profile(&self, profile: Option<&str>, why: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            let ids: Vec<String> = inner
                .queue
                .iter()
                .filter(|id| inner.run(id).profile.as_deref() == profile)
                .cloned()
                .collect();
            for id in ids {
                inner.queue.retain(|q| q != &id);
                if let Some(r) = inner.runs.get_mut(&id) {
                    r.status = "error".into();
                    r.note = Some(why.into());
                }
                inner.remember(&id);
            }
        }
        tracing::warn!("Bench: profile {profile:?} dropped: {why}");
        self.notify();
    }

    // ── one run ────────────────────────────────────────────────────────────────────────

    fn set(&self, id: &str, status: &str, note: Option<String>) {
        self.update(id, |r| {
            r.status = status.into();
            r.note = note;
        });
        self.changed();
    }

    async fn run(&self, id: &str, token: &CancelToken) -> Result<(), String> {
        let run = self.run_info(id).ok_or("the run vanished")?;
        let dir = self.options.dir.join("runs").join(id);
        let (ws, home, check) = (dir.join("ws"), dir.join("home"), dir.join("check"));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let started_at = (self.options.clock)();
        self.update(id, |r| r.started_at = Some(started_at));
        self.set(id, "setup", None);
        self.audit.write(
            None,
            "bench.run",
            json!({"run": id, "task": run.task, "arm": run.arm.name}),
        );

        let mut tab: Option<Arc<ProjectRuntime>> = None;
        let mut lease: Option<Box<dyn BenchLease>> = None;
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("suite".into(), json!(run.suite));
        row.insert("arm".into(), json!(run.arm.name));
        row.insert("lead_mode".into(), json!(run.arm.lead_mode));
        row.insert(
            "sets".into(),
            json!(run.arm.set.clone().unwrap_or_default()),
        );
        row.insert("task".into(), json!(run.task));
        row.insert("rep".into(), json!(run.rep));
        row.insert("profile".into(), json!(run.profile));
        row.insert("batch".into(), json!(run.batch));
        let target = self.run_info(id).and_then(|r| r.target);
        if let Some(t) = &target {
            row.insert("target".into(), json!(t.name));
            row.insert("target_endpoint".into(), json!(t.public_endpoint()));
        }
        let mut t0: Option<SystemTime> = None;
        let mut t1: Option<SystemTime> = None;

        let body = async {
            self.tasks().await?;
            let ws_s = ws.to_string_lossy().into_owned();
            let (rc, setup, err) = self
                .kit(&["setup", &run.task, &ws_s], Duration::from_secs(600))
                .await?;
            let prompt = match &setup {
                Some(Value::Object(s)) if rc == 0 && s.contains_key("prompt") => s["prompt"]
                    .as_str()
                    .ok_or("The requested operation requires an element of type 'String', but the target element has a different type.")?
                    .to_string(),
                _ => {
                    return Err(format!(
                        "setup failed: {}",
                        setup.as_ref().map(Value::to_string).unwrap_or(err)
                    ));
                }
            };
            let setup_timeout = setup
                .as_ref()
                .and_then(|s| s.get("timeout"))
                .and_then(Value::as_i64)
                .unwrap_or(1800);
            let timeout = Duration::from_secs(
                run.timeout_seconds
                    .map(i64::from)
                    .unwrap_or(setup_timeout)
                    .max(0) as u64,
            );
            let mut profile: Option<ProfileDefinition> = None;
            if let Some(t) = &target {
                lease = Some(self.options.leaser.acquire(t, id).await);
            } else if let Some(profiles) = &self.profiles {
                let (catalog, _) = profiles.get_catalog().await;
                if let Some(pn) = &run.profile {
                    profile = Some(
                        catalog
                            .as_ref()
                            .and_then(|c| c.find(pn).cloned())
                            .ok_or_else(|| format!("unknown profile {pn}"))?,
                    );
                }
                if let Some(c) = &catalog
                    && !c.from_cache
                    && let Some(live) = c.live.as_ref().and_then(|l| l.layout.clone())
                {
                    row.insert("layout".into(), json!(live));
                }
            }
            let sets = lease.as_ref().map(|l| l.sets());
            let config = self.write_home(&home, &run.arm, profile.as_ref(), sets.as_deref())?;
            row.insert(
                "settings".into(),
                snapshot(&config, run.profile.as_deref()).unwrap_or(Value::Null),
            );

            let mut env = self.kit_environment();
            env.insert("OPENORCA_HOME".into(), home.to_string_lossy().into_owned());
            let session = (self.options.session_factory)(&ws_s, &env);
            let rt = self.projects.add_transient(
                &ws_s,
                &format!("bench · {} · {} r{}", run.task, run.arm.name, run.rep),
                session.clone(),
            );
            let project_id = rt.id().to_string();
            tab = Some(rt);
            self.update(id, |r| r.project_id = Some(project_id));
            self.set(id, "running", None);
            let start = (self.options.clock)();
            t0 = Some(start);
            if let Some(refused) = session.submit_prompt(&prompt, "bench").await {
                return Err(format!("the session refused the prompt: {refused}"));
            }

            let deadline = start + timeout;
            let mut idle = 0;
            let mut seen_busy = false;
            loop {
                tokio::time::sleep(self.options.poll_interval).await;
                let snap = session.get_snapshot().await?;
                let busy = snap.busy || snap.queued > 0;
                seen_busy |= busy;
                let live = live_of(&metrics(&home));
                self.update(id, |r| {
                    if snap.model.is_some() {
                        r.model = snap.model.clone();
                    }
                    r.live = Some(live);
                });
                self.changed();
                idle = if busy { 0 } else { idle + 1 };
                let now = (self.options.clock)();
                if idle >= 2
                    && (seen_busy
                        || now.duration_since(start).unwrap_or_default() > Duration::from_secs(20))
                {
                    break;
                }
                if now > deadline {
                    session.cancel().await;
                    let note = format!("timed out after {:.0} min", timeout.as_secs_f64() / 60.0);
                    self.update(id, |r| r.note = Some(note));
                    row.insert("timeout".into(), json!(true));
                    break;
                }
                if in_quiet_window(
                    now,
                    Duration::ZERO,
                    self.options.quiet_from,
                    self.options.quiet_to,
                ) {
                    session.cancel().await;
                    self.update(id, |r| r.note = Some("stopped for the quiet window".into()));
                    row.insert("quiet_window_stop".into(), json!(true));
                    break;
                }
            }
            t1 = Some((self.options.clock)());
            Ok::<(), String>(())
        };
        let outcome = tokio::select! {
            r = body => Some(r),
            _ = token.cancelled() => None,
        };
        match outcome {
            None => {
                t1 = t1.or_else(|| Some((self.options.clock)()));
                if let Some(t) = &tab {
                    t.session.cancel().await;
                }
                self.update(id, |r| {
                    r.status = "stopped".into();
                    r.note = Some("stopped".into());
                });
            }
            Some(Err(e)) => {
                t1 = t1.or_else(|| Some((self.options.clock)()));
                tracing::warn!("Bench run {id}: {e}");
                self.update(id, |r| {
                    r.status = "error".into();
                    r.note = Some(e);
                });
            }
            Some(Ok(())) => {}
        }
        // Close the tab: stops the run's session (which gives its slot lease back).
        if let Some(t) = &tab {
            let _ = self.projects.remove(t.id()).await;
        }
        self.update(id, |r| r.project_id = None);
        // After the session has stopped: whatever the lease holds is free for the next run.
        let lease_model = lease.as_ref().and_then(|l| l.model());
        if let Some(l) = &lease {
            l.dispose().await;
        }

        let status = self.run_info(id).map(|r| r.status).unwrap_or_default();
        if status != "stopped" && status != "error" {
            let note = self.run_info(id).and_then(|r| r.note);
            self.set(id, "scoring", note);
            let (ws_s, check_s) = (
                ws.to_string_lossy().into_owned(),
                check.to_string_lossy().into_owned(),
            );
            let (rc, score, err) = self
                .kit(
                    &["check", &run.task, &ws_s, &check_s],
                    Duration::from_secs(45 * 60),
                )
                .await?;
            match &score {
                Some(Value::Object(sc)) if rc == 0 => {
                    const WRONG: &str = "The requested operation requires an element of a different type, or the checker left a field out.";
                    let pass = sc.get("pass").and_then(Value::as_bool).ok_or(WRONG)?;
                    let reason = sc.get("reason").ok_or(WRONG)?;
                    let tp = sc
                        .get("tests_passed")
                        .and_then(Value::as_i64)
                        .ok_or(WRONG)?;
                    let tt = sc.get("tests_total").and_then(Value::as_i64).ok_or(WRONG)?;
                    row.insert("pass".into(), json!(pass));
                    row.insert("reason".into(), reason.clone());
                    row.insert("tests_passed".into(), json!(tp));
                    row.insert("tests_total".into(), json!(tt));
                    let timed_out = row.contains_key("timeout");
                    self.update(id, |r| {
                        r.pass = Some(pass);
                        r.tests_passed = Some(tp);
                        r.tests_total = Some(tt);
                        r.status = if timed_out {
                            "timeout"
                        } else if pass {
                            "passed"
                        } else {
                            "failed"
                        }
                        .into();
                        if r.note.is_none() && !pass {
                            r.note = reason.as_str().map(str::to_string);
                        }
                    });
                }
                _ => {
                    let why = score.as_ref().map(Value::to_string).unwrap_or(err);
                    self.update(id, |r| {
                        r.status = "error".into();
                        r.note = Some(format!("checker failed: {why}"));
                    });
                }
            }
        }
        let now = self.run_info(id).ok_or("the run vanished")?;
        row.insert("status".into(), json!(now.status));
        row.insert("note".into(), json!(now.note));
        row.insert(
            "started".into(),
            json!(t0.or(Some(started_at)).map(time::iso_o)),
        );
        row.insert("ended".into(), json!(t1.map(time::iso_o)));
        if let (Some(a), Some(b)) = (t0, t1) {
            let wall = round(b.duration_since(a).unwrap_or_default().as_secs_f64(), 1);
            row.insert("wall_s".into(), json!(wall));
            self.update(id, |r| r.wall_seconds = Some(wall));
        }
        let m = metrics(&home);
        let live = live_of(&m);
        for (k, v) in m {
            row.insert(k, v);
        }
        let mut model = now.model.clone();
        if model.is_none() {
            model = lease_model;
        }
        self.update(id, |r| {
            r.live = Some(live);
            r.model = model.clone();
        });
        if let Some(m) = &model {
            row.insert("model".into(), json!(m));
        }
        if let (Some(a), Some(b)) = (t0, t1)
            && let Some(w) = self.power(a, b).await
        {
            row.insert("mean_w".into(), json!(w));
        }
        row.insert("orca_version".into(), json!(env!("CARGO_PKG_VERSION")));
        let ended = (self.options.clock)();
        self.update(id, |r| r.ended_at = Some(ended));
        // Runs on different targets end at the same moment: one line at a time.
        {
            let _guard = self.results_write.lock().await;
            use std::io::Write;
            let line = Value::Object(row).to_string();
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.results_file())
                .map_err(|e| e.to_string())?;
            f.write_all(format!("{line}\n").as_bytes())
                .map_err(|e| e.to_string())?;
        }
        self.audit.write(
            None,
            "bench.result",
            json!({"run": id, "status": now.status}),
        );
        self.changed();
        Ok(())
    }

    /// A run's home: the base config, the profile's client settings, then the arm's lead mode
    /// and settings; tools auto-approved (nobody is there to answer). Returns the config written.
    pub fn write_home(
        &self,
        home: &Path,
        arm: &BenchArm,
        profile: Option<&ProfileDefinition>,
        target_sets: Option<&[String]>,
    ) -> Result<Value, String> {
        std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(&self.options.base_config).map_err(|e| e.to_string())?;
        let mut config: Value =
            serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        if !config.is_object() {
            return Err("The node must be of type 'JsonObject'.".into());
        }
        if let Some(p) = profile {
            for (path, value) in &p.client {
                config_overrides::set(&mut config, path, Some(value)).map_err(|e| e.0)?;
            }
        }
        obj(&mut config, "agent").insert("leadMode".into(), json!(arm.lead_mode));
        obj(&mut config, "permissions").insert("autoApproveAll".into(), json!(true));
        for kv in arm.set.iter().flatten() {
            apply_set(&mut config, kv)?;
        }
        // The target last: where the run is served is the suite's choice, not the arm's.
        for kv in target_sets.unwrap_or_default() {
            apply_set(&mut config, kv)?;
        }
        if target_sets.is_none()
            && self.options.registry_url.is_some()
            && let Some(problem) = leasing_problem(&config)
        {
            return Err(problem);
        }
        std::fs::write(
            home.join("config.json"),
            serde_json::to_string_pretty(&config).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        Ok(config)
    }

    /// Time-weighted mean AC output (W) over the run, from Home Assistant history.
    async fn power(&self, start: SystemTime, end: SystemTime) -> Option<f64> {
        let file = self
            .options
            .power_env_file
            .as_ref()
            .filter(|f| f.is_file())?;
        let text = std::fs::read_to_string(file).ok()?;
        let mut env = BTreeMap::new();
        for l in text.lines().map(str::trim) {
            if l.is_empty() || l.starts_with('#') || !l.contains('=') {
                continue;
            }
            let i = l.find('=').unwrap();
            env.insert(
                l[..i].trim().to_string(),
                l[i + 1..].trim().trim_matches(['"', '\'']).to_string(),
            );
        }
        let mut url = env.get("HA_URL")?.trim_end_matches('/').to_string();
        let token = env.get("HA_TOKEN")?;
        if !url.ends_with("/api") {
            url.push_str("/api");
        }
        let q = format!(
            "{url}/history/period/{}?end_time={}&filter_entity_id={}&minimal_response",
            crate::util::escape_data(&time::iso_o(start)),
            crate::util::escape_data(&time::iso_o(end)),
            self.options.power_sensor
        );
        let res = match self.http.get(q).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::info!("Bench power reading failed: {}", e.without_url());
                return None;
            }
        };
        if !res.status().is_success() {
            return None;
        }
        let doc: Value = serde_json::from_str(&res.text().await.ok()?).ok()?;
        let series = doc.as_array()?.first()?.as_array()?;
        let mut pts = Vec::new();
        for row in series {
            let ts = row
                .get("last_changed")
                .or_else(|| row.get("last_updated"))
                .and_then(Value::as_str);
            let w = row
                .get("state")
                .and_then(Value::as_str)
                .and_then(|s| s.trim().parse::<f64>().ok());
            if let (Some(t), Some(w)) = (ts.and_then(time::parse_iso), w) {
                pts.push((t, w));
            }
        }
        time_weighted_mean(pts, start, end)
    }

    /// Runs benchkit.py; the last stdout line is its JSON.
    async fn kit(&self, args: &[&str], timeout: Duration) -> Result<KitOutput, String> {
        let mut cmd = tokio::process::Command::new(&self.options.python);
        cmd.arg("-I")
            .arg(&self.options.kit_script)
            .arg(&self.options.kit_dir)
            .args(args)
            .current_dir(&self.options.dir)
            .envs(self.kit_environment())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("MSBUILDDISABLENODEREUSE", "1")
            .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = cmd.spawn().map_err(|e| {
            format!(
                "An error occurred trying to start process '{}': {e}",
                self.options.python
            )
        })?;
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_) => return Ok((-1, None, "timed out".into())),
            Ok(Err(e)) => return Err(e.to_string()),
            Ok(Ok(o)) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout
            .split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let json = lines.last().and_then(|l| serde_json::from_str(l).ok());
        let err = String::from_utf8_lossy(&out.stderr);
        Ok((
            out.status.code().unwrap_or(-1),
            json,
            tail_chars(&err, 2000).to_string(),
        ))
    }

    /// Stop the loop and every run (the server is stopping).
    pub async fn stop_all(&self) {
        self.dispose.cancel();
        let tokens: Vec<CancelToken> = {
            let inner = self.inner.lock().unwrap();
            inner
                .running
                .iter()
                .filter_map(|id| inner.run(id).cancel.clone())
                .collect()
        };
        for t in tokens {
            t.cancel();
        }
        let work: Vec<JoinHandle<()>> = std::mem::take(&mut *self.work.lock().unwrap());
        let lp = self.loop_task.lock().unwrap().take();
        let all = async {
            for h in work {
                let _ = h.await;
            }
            if let Some(h) = lp {
                let _ = h.await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(30), all).await;
    }
}

/// A results file's rows; a line that doesn't parse is skipped, never the rest of the file.
pub fn read_rows(file: &Path, archive: Option<&str>, into: &mut Vec<Value>) {
    let Ok(bytes) = std::fs::read(file) else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match archive {
            None => into.push(v),
            Some(a) => {
                if let Some(o) = v.as_object_mut() {
                    o.insert("archive".into(), json!(a));
                    into.push(v);
                }
            }
        }
    }
}

/// "Abhoth lead, orca@DeepThought" from a pig-slots lease, or `None`.
pub fn lease_holder(lease: &Value) -> Option<String> {
    let o = lease.as_object()?;
    let parts: Vec<&str> = ["label", "owner"]
        .iter()
        .filter_map(|k| o.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The run's resolved inference settings (no keys), as results rows record them.
pub fn snapshot(config: &Value, profile: Option<&str>) -> Option<Value> {
    let mut resolved: orca_core::config::OrcaConfig =
        serde_json::from_value(config.clone()).ok()?;
    resolved.active_profile = profile.map(str::to_string);
    Some(orca_core::profiles::snapshot(&resolved))
}

/// Why a run with this config wouldn't lease its slots from pig-slots, or `None` when it will.
/// Bridge engines (claude, codex) don't use the local slots.
pub fn leasing_problem(config: &Value) -> Option<String> {
    let engine = config
        .get("agent")
        .and_then(|a| a.get("engine"))
        .and_then(Value::as_str)
        .unwrap_or("native");
    if !engine.eq_ignore_ascii_case("native") {
        return None;
    }
    let Some(slots) = config.get("slots").and_then(Value::as_object) else {
        return Some(
            "The bench config has no slots section, so runs wouldn't lease slots from pig-slots."
                .into(),
        );
    };
    if slots.get("enabled") == Some(&Value::Bool(false)) {
        return Some(
            "The bench config has slots.enabled=false, so runs wouldn't lease slots from pig-slots."
                .into(),
        );
    }
    if slots
        .get("registryUrl")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Some(
            "The bench config has no slots.registryUrl, so runs wouldn't lease slots from pig-slots."
                .into(),
        );
    }
    if slots
        .get("copies")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Some(
            "The bench config has no slots.copies, so runs wouldn't lease slots from pig-slots."
                .into(),
        );
    }
    None
}

fn obj<'a>(parent: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    let p = parent.as_object_mut().expect("an object");
    if !p.get(key).is_some_and(Value::is_object) {
        p.insert(key.into(), json!({}));
    }
    p.get_mut(key).unwrap().as_object_mut().unwrap()
}

/// "a.b.c=<json or text>", like the bench scripts' --set.
pub fn apply_set(root: &mut Value, kv: &str) -> Result<(), String> {
    let eq = match kv.find('=') {
        Some(i) if i > 0 => i,
        _ => return Err(format!("--set needs key=value: {kv}")),
    };
    let parts: Vec<&str> = kv[..eq].split('.').collect();
    let raw = &kv[eq + 1..];
    let value = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw));
    let bad = || format!("Can't set {kv}: the path goes through a value that isn't an object.");
    let mut node = root;
    for part in &parts[..parts.len() - 1] {
        node = match node {
            Value::Array(arr) => {
                let idx: usize = part.parse().map_err(|_| bad())?;
                match arr.get_mut(idx) {
                    Some(v) if !v.is_null() => v,
                    _ => return Err(format!("No such index in {kv}")),
                }
            }
            Value::Object(o) => {
                if !o.get(*part).is_some_and(|v| !v.is_null()) {
                    o.insert(part.to_string(), json!({}));
                }
                o.get_mut(*part).unwrap()
            }
            _ => return Err(bad()),
        };
    }
    let last = parts[parts.len() - 1];
    match node {
        Value::Array(arr) if last.parse::<usize>().is_ok() => {
            let i: usize = last.parse().unwrap();
            let slot = arr
                .get_mut(i)
                .ok_or_else(|| format!("No such index in {kv}"))?;
            *slot = value;
        }
        Value::Object(o) => {
            o.insert(last.to_string(), value);
        }
        _ => return Err(bad()),
    }
    Ok(())
}

/// `(?:^|\s)name=(\d+)` in a usage line's detail.
fn field(detail: &str, name: &str) -> Option<i64> {
    let prefix = format!("{name}=");
    let mut rest = detail;
    let mut at_start = true;
    loop {
        let i = rest.find(&prefix)?;
        let before_ok = if i == 0 {
            at_start
        } else {
            rest[..i].chars().last().is_some_and(char::is_whitespace)
        };
        let after = &rest[i + prefix.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if before_ok && !digits.is_empty() {
            return digits.parse().ok();
        }
        rest = &rest[i + 1..];
        at_start = false;
    }
}

/// Totals from the run's own log ("Slot usage" lines, or a bridge's "Bridge turn" lines):
/// requests, tokens, decode speed, helpers.
pub fn metrics(home: &Path) -> Map<String, Value> {
    let (mut requests, mut prompt, mut cached, mut output, mut reasoning) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    let (mut content, mut tool, mut pred_n, mut pred_ms, mut lead_n, mut lead_ms) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    let mut cost = 0f64;
    let mut bridge_turns = 0i64;
    let mut helpers: BTreeSet<String> = BTreeSet::new();
    let mut slots: BTreeSet<String> = BTreeSet::new();
    let mut per_copy: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let logs = home.join("logs");
    if let Ok(rd) = std::fs::read_dir(&logs) {
        for f in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            if f.extension().is_none_or(|x| x != "log") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&f) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines() {
                if let Some(b) = BRIDGE_LINE.captures(line) {
                    // A bridge engine's turn: the CLI's own totals.
                    bridge_turns += 1;
                    if let Ok(c) = b["cost"].parse::<f64>() {
                        cost += c;
                    }
                    if let Ok(v) = b["in"].parse::<i64>() {
                        prompt += v;
                    }
                    if let Ok(v) = b["out"].parse::<i64>() {
                        output += v;
                    }
                    if let Ok(v) = b["turns"].parse::<i64>() {
                        requests += v;
                    }
                    continue;
                }
                let Some(m) = USAGE_LINE.captures(line) else {
                    continue;
                };
                requests += 1;
                prompt += m["p"].parse::<i64>().unwrap_or(0);
                cached += m["c"].parse::<i64>().unwrap_or(0);
                output += m["o"].parse::<i64>().unwrap_or(0);
                reasoning += m["r"].parse::<i64>().unwrap_or(0);
                let d = &m["d"];
                content += field(d, "content").unwrap_or(0);
                tool += field(d, "tool").unwrap_or(0);
                let n = field(d, "predicted_n");
                let ms = field(d, "predicted_ms");
                let route = m["route"].trim().to_string();
                let lead = route.to_lowercase().starts_with("lead");
                let slot = m.name("slot").map(|s| s.as_str().to_string());
                if let Some(s) = slot.as_ref().filter(|s| !s.is_empty()) {
                    slots.insert(s.clone());
                }
                if let (Some(n), Some(ms)) = (n.filter(|v| *v > 0), ms.filter(|v| *v > 0)) {
                    pred_n += n;
                    pred_ms += ms;
                    // The copy is the slot's name without its number: "A2" -> A.
                    if let Some(s) = &slot {
                        let copy = s.trim_end_matches(|c: char| c.is_ascii_digit());
                        if !copy.is_empty() {
                            let e = per_copy.entry(copy.to_string()).or_default();
                            e.0 += n;
                            e.1 += ms;
                        }
                    }
                    if lead {
                        lead_n += n;
                        lead_ms += ms;
                    }
                }
                if !lead {
                    helpers.insert(route);
                }
            }
        }
    }
    let tps = |n: i64, ms: i64| round(n as f64 * 1000.0 / ms as f64, 1);
    let mut out = Map::new();
    out.insert("requests".into(), json!(requests));
    out.insert("prompt_tokens".into(), json!(prompt));
    out.insert("cached_tokens".into(), json!(cached));
    out.insert("output_tokens".into(), json!(output));
    out.insert("reasoning_tokens".into(), json!(reasoning));
    out.insert("content_tokens".into(), json!(content));
    out.insert("tool_tokens".into(), json!(tool));
    out.insert(
        "decode_tps".into(),
        json!((pred_ms > 0).then(|| tps(pred_n, pred_ms))),
    );
    out.insert(
        "lead_tps".into(),
        json!((lead_ms > 0).then(|| tps(lead_n, lead_ms))),
    );
    out.insert("helpers".into(), json!(helpers.len()));
    out.insert(
        "helper_routes".into(),
        json!((!helpers.is_empty()).then(|| helpers.iter().collect::<Vec<_>>())),
    );
    out.insert(
        "slots".into(),
        json!((!slots.is_empty()).then(|| slots.iter().collect::<Vec<_>>())),
    );
    out.insert(
        "copy_tps".into(),
        if per_copy.is_empty() {
            Value::Null
        } else {
            Value::Object(
                per_copy
                    .iter()
                    .filter(|(_, (_, ms))| *ms > 0)
                    .map(|(k, (n, ms))| (k.clone(), json!(tps(*n, *ms))))
                    .collect(),
            )
        },
    );
    out.insert(
        "bridge_turns".into(),
        json!((bridge_turns > 0).then_some(bridge_turns)),
    );
    out.insert(
        "cost_usd".into(),
        json!((bridge_turns > 0).then(|| round(cost, 4))),
    );
    out
}

fn live_of(m: &Map<String, Value>) -> BenchLive {
    let n = |k: &str| m.get(k).and_then(Value::as_i64).unwrap_or(0);
    BenchLive {
        requests: n("requests"),
        prompt_tokens: n("prompt_tokens"),
        cached_tokens: n("cached_tokens"),
        output_tokens: n("output_tokens"),
        reasoning_tokens: n("reasoning_tokens"),
        content_tokens: n("content_tokens"),
        tool_tokens: n("tool_tokens"),
    }
}

/// The time-weighted mean of (time, watts) points over `start`..`end`; `None` when no time is
/// covered.
pub fn time_weighted_mean(
    mut pts: Vec<(SystemTime, f64)>,
    start: SystemTime,
    end: SystemTime,
) -> Option<f64> {
    pts.sort_by_key(|p| p.0);
    let secs = |a: SystemTime, b: SystemTime| match b.duration_since(a) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    };
    let (mut area, mut total) = (0.0, 0.0);
    for i in 0..pts.len() {
        let from = pts[i].0.max(start);
        let to = if i + 1 < pts.len() { pts[i + 1].0 } else { end }.min(end);
        let s = secs(from, to);
        if s > 0.0 {
            area += pts[i].1 * s;
            total += s;
        }
    }
    (total > 0.0).then(|| round(area / total, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_config_must_lease_from_the_registry() {
        let c = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            leasing_problem(&c(
                r#"{"slots":{"registryUrl":"http://h:8099","copies":[{"name":"A"}]}}"#
            )),
            None
        );
        assert!(
            leasing_problem(&c(r#"{"slots":{"copies":[{"name":"A"}]}}"#))
                .unwrap()
                .contains("registryUrl")
        );
        assert!(leasing_problem(&c(r#"{"slots":{"enabled":false,"registryUrl":"http://h:8099","copies":[{"name":"A"}]}}"#)).unwrap().contains("enabled=false"));
        assert!(
            leasing_problem(&c(r#"{"agent":{}}"#))
                .unwrap()
                .contains("no slots section")
        );
        assert!(
            leasing_problem(&c(
                r#"{"slots":{"registryUrl":"http://h:8099","copies":[]}}"#
            ))
            .unwrap()
            .contains("copies")
        );
        // A bridge engine arm doesn't use the local slots.
        assert_eq!(
            leasing_problem(&c(r#"{"agent":{"engine":"claude"}}"#)),
            None
        );
    }

    #[test]
    fn engine_arms_set_the_engine_and_bridge_turns_count_in_the_metrics() {
        let arm = BenchArm::for_engine("codex", None);
        assert_eq!(arm.set.as_deref().unwrap(), ["agent.engine=codex"]);
        let mut config = json!({"agent": {}});
        apply_set(&mut config, &arm.set.as_ref().unwrap()[0]).unwrap();
        assert_eq!(config["agent"]["engine"], "codex");
        assert_eq!(leasing_problem(&config), None);

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("logs")).unwrap();
        std::fs::write(
            home.path().join("logs").join("openorca.log"),
            "info: Bridge turn success: cost 0.25 USD, in 1000 / out 200 tokens, 4 model turns, 9000 ms\ninfo: Bridge turn success: cost 0.5 USD, in 3000 / out 100 tokens, 2 model turns, 4000 ms\n",
        )
        .unwrap();
        let m = metrics(home.path());
        assert_eq!(m["bridge_turns"], 2);
        assert_eq!(m["cost_usd"], 0.75);
        assert_eq!(m["prompt_tokens"], 4000);
        assert_eq!(m["output_tokens"], 300);
        assert_eq!(m["requests"], 6);
    }

    #[test]
    fn apply_set_parses_json_walks_arrays_and_keeps_text() {
        let mut c = json!({"slots": {"copies": [{"name": "A", "slots": 3}]}});
        apply_set(&mut c, "slots.copies.0.slots=5").unwrap();
        apply_set(&mut c, "pi.extraTools=true").unwrap();
        apply_set(&mut c, "lmStudio.baseUrl=http://x/v1").unwrap();
        apply_set(&mut c, "a.b=").unwrap();
        assert_eq!(c["slots"]["copies"][0]["slots"], 5);
        assert_eq!(c["pi"]["extraTools"], true);
        assert_eq!(c["lmStudio"]["baseUrl"], "http://x/v1");
        assert_eq!(c["a"]["b"], "");
        assert!(apply_set(&mut c, "nokey").is_err());
        assert!(apply_set(&mut c, "=1").is_err());
        assert!(apply_set(&mut c, "slots.copies.4.slots=1").is_err());
    }

    #[test]
    fn usage_lines_give_tokens_speed_slots_and_helpers() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("logs")).unwrap();
        std::fs::write(
            home.path().join("logs").join("openorca-2026-11-17.log"),
            "21:12:34.625 [INF] Slots: Slot usage lead on A1: prompt 7543, cached 4663, re-read 2880, output 142, reasoning 90, 31390 ms | content=0 tool=52 calls=bash prompt_n=2880 cache_n=4663 prompt_ms=5216 predicted_n=142 predicted_ms=5602\n\
21:13:43.754 [INF] Slots: Slot usage agent 1 (Undo) on B1: prompt 8085, cached 4663, re-read 3422, output 641, reasoning 130, 68604 ms | content=498 tool=13 calls=- prompt_n=3422 cache_n=4663 prompt_ms=5341 predicted_n=641 predicted_ms=43889\n",
        )
        .unwrap();
        let m = metrics(home.path());
        assert_eq!(m["requests"], 2);
        assert_eq!(m["helpers"], 1);
        assert_eq!(m["output_tokens"], 783);
        assert_eq!(m["content_tokens"], 498);
        assert_eq!(m["tool_tokens"], 65);
        assert_eq!(
            m["decode_tps"],
            round(783.0 * 1000.0 / (5602.0 + 43889.0), 1)
        );
        assert_eq!(m["lead_tps"], round(142.0 * 1000.0 / 5602.0, 1));
        assert_eq!(m["slots"], json!(["A1", "B1"]));
        assert_eq!(m["helper_routes"], json!(["agent 1 (Undo)"]));
        assert_eq!(m["copy_tps"]["A"], round(142.0 * 1000.0 / 5602.0, 1));
        assert_eq!(m["copy_tps"]["B"], round(641.0 * 1000.0 / 43889.0, 1));
        assert_eq!(m["bridge_turns"], Value::Null);
    }

    #[test]
    fn power_mean_is_time_weighted() {
        let t = time::parse_iso("2026-11-17T20:00:00Z").unwrap();
        let min = |m: i64| {
            if m >= 0 {
                t + Duration::from_secs(m as u64 * 60)
            } else {
                t - Duration::from_secs((-m) as u64 * 60)
            }
        };
        let pts = vec![(min(-5), 100.0), (min(5), 300.0)];
        assert_eq!(time_weighted_mean(pts, t, min(10)), Some(200.0));
        assert_eq!(time_weighted_mean(vec![], t, min(10)), None);
    }

    #[test]
    fn suite_problems() {
        let ok = BenchSuite {
            reps: 3,
            targets: Some(vec![BenchTarget::new("a", "http://127.0.0.1:18080/v1")]),
            ..BenchSuite::new(
                "x",
                vec![BenchArm::new("base", "auto", None, None)],
                &["demo"],
            )
        };
        assert_eq!(BenchRunner::problem(&ok), None);
        let refused = |s: BenchSuite, why: &str| {
            let p = BenchRunner::problem(&s).unwrap_or_default();
            assert!(p.contains(why), "{p} should say {why}");
        };
        refused(
            BenchSuite {
                arms: vec![],
                ..ok.clone()
            },
            "no arms",
        );
        refused(
            BenchSuite {
                arms: vec![
                    BenchArm::new("a", "auto", None, None),
                    BenchArm::new("a", "auto", None, None),
                ],
                ..ok.clone()
            },
            "name of its own",
        );
        refused(
            BenchSuite {
                arms: vec![BenchArm::new("a", "auto", Some(vec!["nokey".into()]), None)],
                ..ok.clone()
            },
            "key=value",
        );
        refused(
            BenchSuite {
                reps: 0,
                ..ok.clone()
            },
            "Reps",
        );
        refused(
            BenchSuite {
                targets: Some(vec![BenchTarget::new("t", "ftp://host/x")]),
                ..ok.clone()
            },
            "http(s)",
        );
    }

    #[test]
    fn field_matches_whole_names_only() {
        assert_eq!(field("content=0 tool=52 calls=bash", "tool"), Some(52));
        assert_eq!(field("xtool=5 tool=7", "tool"), Some(7));
        assert_eq!(field("tool=", "tool"), None);
        assert_eq!(
            field("predicted_n=142 predicted_ms=5602", "predicted_ms"),
            Some(5602)
        );
    }
}
