//! The Benchmarks page (port of `OpenOrca.WebUi.Bench`): suites of arms × tasks × reps, run one
//! by one (or one per target), each in a fresh workspace from the task kit with its own
//! OpenOrca home, shown live as a read-only project tab, scored by the kit's checker and
//! recorded as a line of `results.jsonl`; the persistent queue of setups; server layout
//! switches through the profiles API; the summary table and the live event stream.

pub mod endpoints;
pub mod queue;
pub mod result_row;
pub mod runner;
pub mod summary;
pub mod targets;
pub mod time;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::live::LiveSession;

pub use queue::BenchQueue;
pub use result_row::BenchResultRow;
pub use runner::BenchRunner;
pub use targets::{BenchLease, BenchTarget, BenchTargetLeaser, DirectEndpointLeaser};

/// Starts a run's session in a folder with the given environment (OPENORCA_HOME, ...).
pub type BenchSessionFactory =
    Arc<dyn Fn(&str, &BTreeMap<String, String>) -> Arc<dyn LiveSession> + Send + Sync>;

/// The time, for runs (tests move it).
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Where the bench runner keeps things and how it starts a run's session (`BenchOptions`).
#[derive(Clone)]
pub struct BenchOptions {
    /// suites/*.json, runs/<id>/{ws,home,check}, results.jsonl, queue.json.
    pub dir: PathBuf,
    /// The task kit (task data) and its driver, benchkit.py.
    pub kit_dir: PathBuf,
    pub kit_script: PathBuf,
    pub python: String,
    /// The config.json each run's home starts from (lead mode and --set applied on top).
    pub base_config: PathBuf,
    /// pig-slots registry: untargeted runs start only when it shows no leases.
    pub registry_url: Option<String>,
    /// HA_URL / HA_TOKEN [/ HA_ENTITY] for the power reading (never logged).
    pub power_env_file: Option<PathBuf>,
    pub power_sensor: String,
    /// Extra environment for runs and checks (e.g. REALTASKS_REPO).
    pub environment: BTreeMap<String, String>,
    pub session_factory: BenchSessionFactory,
    /// No run starts or runs inside this UK-time window (minutes after midnight).
    pub quiet_from: u32,
    pub quiet_to: u32,
    /// How a targeted suite's runs get their target.
    pub leaser: Arc<dyn BenchTargetLeaser>,
    pub clock: Clock,
    pub poll_interval: Duration,
    pub gate_interval: Duration,
    /// How often a server layout switch job is polled.
    pub switch_poll: Duration,
}

impl BenchOptions {
    pub fn new(
        dir: PathBuf,
        kit_dir: PathBuf,
        kit_script: PathBuf,
        base_config: PathBuf,
        session_factory: BenchSessionFactory,
    ) -> Self {
        Self {
            dir,
            kit_dir,
            kit_script,
            python: if cfg!(windows) { "python" } else { "python3" }.into(),
            base_config,
            registry_url: None,
            power_env_file: None,
            power_sensor: "sensor.workshop_store_delta_3_max_ac_output_power".into(),
            environment: BTreeMap::new(),
            session_factory,
            quiet_from: 6 * 60 + 45,
            quiet_to: 7 * 60 + 30,
            leaser: Arc::new(DirectEndpointLeaser::default()),
            clock: Arc::new(SystemTime::now),
            poll_interval: Duration::from_secs(2),
            gate_interval: Duration::from_secs(15),
            switch_poll: Duration::from_secs(2),
        }
    }
}

/// One arm of a suite: a lead mode, config overrides, and optionally an inference profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchArm {
    pub name: String,
    pub lead_mode: String,
    pub set: Option<Vec<String>>,
    pub profile: Option<String>,
}

impl Default for BenchArm {
    fn default() -> Self {
        Self {
            name: String::new(),
            lead_mode: "auto".into(),
            set: None,
            profile: None,
        }
    }
}

impl BenchArm {
    pub fn new(
        name: &str,
        lead_mode: &str,
        set: Option<Vec<String>>,
        profile: Option<&str>,
    ) -> Self {
        Self {
            name: name.into(),
            lead_mode: lead_mode.into(),
            set,
            profile: profile.map(str::to_string),
        }
    }

    /// An arm whose turns run on a bridge engine ("claude", "codex").
    pub fn for_engine(engine: &str, name: Option<&str>) -> Self {
        Self::new(
            name.unwrap_or(engine),
            "auto",
            Some(vec![format!("agent.engine={engine}")]),
            None,
        )
    }
}

/// A suite file: arms × tasks × reps. `profile` is the arms' profile when they name none.
/// With `targets` the suite is "targeted": its runs go straight to those endpoints, one run per
/// target at a time, targets in parallel. `target_mode`: "pool" (default) or "each".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchSuite {
    pub name: String,
    pub arms: Vec<BenchArm>,
    pub tasks: Vec<String>,
    pub reps: i32,
    pub timeout_seconds: Option<i32>,
    pub description: Option<String>,
    pub profile: Option<String>,
    pub targets: Option<Vec<BenchTarget>>,
    pub target_mode: Option<String>,
}

impl Default for BenchSuite {
    fn default() -> Self {
        Self {
            name: String::new(),
            arms: Vec::new(),
            tasks: Vec::new(),
            reps: 1,
            timeout_seconds: None,
            description: None,
            profile: None,
            targets: None,
            target_mode: None,
        }
    }
}

impl BenchSuite {
    pub fn new(name: &str, arms: Vec<BenchArm>, tasks: &[&str]) -> Self {
        Self {
            name: name.into(),
            arms,
            tasks: tasks.iter().map(|t| t.to_string()).collect(),
            ..Self::default()
        }
    }
}

/// What a run has used so far (while it runs) or in all, from its own log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchLive {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub content_tokens: i64,
    pub tool_tokens: i64,
}

/// A cancellation flag that can be awaited (`CancellationToken`).
#[derive(Clone, Default)]
pub struct CancelToken(Arc<(AtomicBool, Notify)>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.1.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}
