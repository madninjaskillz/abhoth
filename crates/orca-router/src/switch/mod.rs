//! Layout switches (ROUTER-CONFIG.md 5.3-5.5): plans, the orchestrator with rollback, and job
//! records in layoutctl's shape (`state/jobs/<id>.json`, `history.jsonl`) so the dash keeps
//! reading them.
//!
//! The orchestrator does not run commands itself. It drives two ports:
//! - [`HostOps`]: what a GPU host can do (observe, restore model files, stop once, read GPU
//!   memory, start, health, verify chat, clocks, OOM kills, locks, gpu-power hold). The control
//!   plane implements it with its executors and engine drivers; tests use a fake host with
//!   fault injection.
//! - [`Plane`]: the router's side (slot view and leases, guards, routing commit).
//!
//! The behaviours kept from `layoutctl.py switch` are listed on [`Orchestrator::switch`].

pub mod callers;
pub mod gate;
mod job;
pub mod plane;
pub mod profiles_shim;
mod run;
pub mod wiring;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use job::{FileJobSink, JobRecord, JobSink, MemJobSink, Step};
pub use run::{Orchestrator, SwitchRequest};

use crate::guards::{Action, Block, Caller};
use crate::leases::SlotView;

/// What one card runs. `engine = "off"` means nothing. Two specs are the same layout card when
/// they are equal (layoutctl compares its normalised card dicts the same way).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSpec {
    pub engine: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub lanes: u32,
    /// Anything else that makes it a different instance (build, options hash, NUMA mems).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, String>,
}

impl CardSpec {
    pub fn off() -> Self {
        CardSpec {
            engine: "off".into(),
            profile: String::new(),
            model: String::new(),
            lanes: 0,
            detail: BTreeMap::new(),
        }
    }

    pub fn is_off(&self) -> bool {
        self.engine == "off"
    }

    pub fn new(engine: &str, profile: &str, model: &str, lanes: u32) -> Self {
        CardSpec {
            engine: engine.into(),
            profile: profile.into(),
            model: model.into(),
            lanes,
            detail: BTreeMap::new(),
        }
    }
}

/// Card id -> what it runs, for every managed card of one server.
pub type Spec = BTreeMap<String, CardSpec>;

/// What the host runs now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    pub spec: Spec,
    /// Card -> compute holders on its GPU UUID that are not layout instances (refuse a switch
    /// that changes the card).
    pub foreign: BTreeMap<String, Vec<String>>,
}

/// Engine health while starting.
#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    Ok,
    NotYet(String),
    /// The process is gone; the log tail (secrets scrubbed) says why.
    Exited(String),
}

/// A host operation that failed (command error, timeout, SSH down).
#[derive(Debug, Clone, PartialEq)]
pub struct HostError(pub String);

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type HostResult<T> = Result<T, HostError>;

/// Clock reading of one card (6.6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clocks {
    /// `clocks.max.sm`: what the card may boost to now (capped or locked cards report less).
    pub max_sm_mhz: u32,
}

/// The GPU host, as the orchestrator needs it.
pub trait HostOps: Send + Sync {
    fn observe(&self) -> impl Future<Output = HostResult<Observed>> + Send;
    /// Build present, image present, model files present or restorable, templates render.
    /// `Err` is a refusal text.
    fn preconditions(&self, target: &Spec) -> impl Future<Output = Result<(), String>> + Send;
    /// Copy model files back from the archive (sha256 checked). Returns step messages.
    fn restore_models(
        &self,
        card: &str,
        spec: &CardSpec,
    ) -> impl Future<Output = HostResult<Vec<String>>> + Send;
    /// Take the server's host-wide locks (the old watchdog's) for the job.
    fn lock(&self) -> impl Future<Output = HostResult<()>> + Send;
    fn unlock(&self) -> impl Future<Output = ()> + Send;
    /// One stop attempt (`docker rm -f`, `docker stop -t`, ...): `true` when the instance is
    /// gone. A dying container that is still there (a zombie unpinning memory) answers `false`.
    fn stop_once(
        &self,
        card: &str,
        spec: &CardSpec,
    ) -> impl Future<Output = HostResult<bool>> + Send;
    fn gpu_used_mib(&self, card: &str) -> impl Future<Output = HostResult<u64>> + Send;
    /// `before_start` hooks (NUMA room, json-set). Returns step messages.
    fn before_start(
        &self,
        card: &str,
        spec: &CardSpec,
    ) -> impl Future<Output = HostResult<Vec<String>>> + Send;
    fn start(&self, card: &str, spec: &CardSpec) -> impl Future<Output = HostResult<()>> + Send;
    fn health(&self, card: &str, spec: &CardSpec) -> impl Future<Output = Health> + Send;
    /// The one-token chat, straight to the engine (never through routes).
    fn verify_chat(
        &self,
        card: &str,
        spec: &CardSpec,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn clocks(&self, card: &str) -> impl Future<Output = HostResult<Clocks>> + Send;
    /// Kernel OOM-kill lines seen so far (`dmesg`).
    fn oom_kills(&self) -> impl Future<Output = HostResult<Vec<String>>> + Send;
    fn gpu_power_hold(&self, secs: u64) -> impl Future<Output = HostResult<()>> + Send;
    fn gpu_power_release(&self) -> impl Future<Output = ()> + Send;
}

/// The router's side of a switch.
pub trait Plane: Send + Sync {
    /// Set the slot view claims see (freeze, withdraw, commit).
    fn set_slots(&self, view: SlotView);
    /// Live leases that refuse a switch of `cards` (incl. leases on unknown cards).
    fn held_on(&self, cards: &[String]) -> Vec<(String, String)>;
    /// What blocks `action` on `cards` now, with fresh guard data.
    fn blocks(&self, action: Action, cards: &[String], caller: &Caller, layout: &str)
    -> Vec<Block>;
    /// Routing: these cards serve nothing new from now on (in-flight requests finish).
    fn withdraw_routes(&self, cards: &[String]);
    /// Routing follows what runs now: `ready` cards are routable, the rest are not.
    fn commit_routes(&self, layout: Option<&str>, spec: &Spec, ready: &[String]);
}

/// Restart rules of one engine kind (5.5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineRules {
    /// Start before the others, one card at a time, each healthy before the next (Strata).
    pub sequential: bool,
    /// Share group: a member that stays restarts after an earlier card of the group restarts
    /// (Strata B maps the arena Strata A fills).
    pub share: Option<String>,
    /// Restart when a peer of these engines (re)starts on the server (ninfer: its pinned host
    /// memory must not take NUMA node 0 before the arena fill).
    pub restart_if_peer_starts: Vec<String>,
    pub start_estimate_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnMismatch {
    #[default]
    Warn,
    Fail,
}

/// Switch settings of one server.
#[derive(Debug, Clone)]
pub struct SwitchConfig {
    pub server: String,
    /// Managed cards in card order (A, B).
    pub cards: Vec<String>,
    pub engines: BTreeMap<String, EngineRules>,
    /// Slots per card are `{card}{n}` over the lanes.
    pub lease_settle: Duration,
    pub stop_timeout: Duration,
    pub stop_retry_every: Duration,
    pub gpu_free_below_mib: u64,
    pub gpu_free_timeout: Duration,
    pub health_timeout: Duration,
    pub health_every: Duration,
    pub verify_timeout: Duration,
    /// Card -> the SM clock it must be able to reach after a start (A: 1597).
    pub max_sm_mhz: BTreeMap<String, u32>,
    pub clock_mismatch: OnMismatch,
    /// gpu-power hold = the estimate plus this.
    pub hold_extra_seconds: u64,
}

impl SwitchConfig {
    /// layoutctl's timings for `cards`.
    pub fn deepthought_like(cards: &[&str]) -> Self {
        let mut engines = BTreeMap::new();
        engines.insert(
            "strata".to_string(),
            EngineRules {
                sequential: true,
                share: Some("expert-arena".into()),
                restart_if_peer_starts: vec![],
                start_estimate_seconds: 60,
            },
        );
        for e in ["ninfer", "llamacpp"] {
            engines.insert(
                e.to_string(),
                EngineRules {
                    sequential: false,
                    share: None,
                    restart_if_peer_starts: if e == "ninfer" {
                        vec!["strata".into()]
                    } else {
                        vec![]
                    },
                    start_estimate_seconds: 75,
                },
            );
        }
        SwitchConfig {
            server: "deepthought".into(),
            cards: cards.iter().map(|c| c.to_string()).collect(),
            engines,
            lease_settle: Duration::from_millis(1500),
            stop_timeout: Duration::from_secs(120),
            stop_retry_every: Duration::from_secs(3),
            gpu_free_below_mib: 1024,
            gpu_free_timeout: Duration::from_secs(90),
            health_timeout: Duration::from_secs(900),
            health_every: Duration::from_secs(3),
            verify_timeout: Duration::from_secs(180),
            max_sm_mhz: BTreeMap::new(),
            clock_mismatch: OnMismatch::Warn,
            hold_extra_seconds: 120,
        }
    }

    fn rules(&self, engine: &str) -> EngineRules {
        self.engines.get(engine).cloned().unwrap_or_default()
    }
}

/// The cards to stop and start for `cur` -> `target`, in card order (layoutctl `restart_set`,
/// written with the engine rules instead of engine names).
pub fn restart_set(cfg: &SwitchConfig, cur: &Spec, target: &Spec) -> Vec<String> {
    let off = CardSpec::off();
    let get = |s: &'_ Spec, c: &str| s.get(c).cloned().unwrap_or_else(|| off.clone());
    let mut changed: Vec<String> = cfg
        .cards
        .iter()
        .filter(|c| get(cur, c) != get(target, c))
        .cloned()
        .collect();
    // share groups: a stayer restarts when an earlier card of its group restarts
    for (i, c) in cfg.cards.iter().enumerate() {
        if changed.contains(c) {
            continue;
        }
        let t = get(target, c);
        let Some(g) = cfg.rules(&t.engine).share else {
            continue;
        };
        let earlier = cfg.cards[..i].iter().any(|e| {
            changed.contains(e) && cfg.rules(&get(target, e).engine).share.as_ref() == Some(&g)
        });
        if earlier {
            changed.push(c.clone());
        }
    }
    // peers: a stayer restarts when a listed engine (re)starts on the server
    let starting: Vec<String> = changed
        .iter()
        .map(|c| get(target, c).engine)
        .filter(|e| e != "off")
        .collect();
    for c in &cfg.cards {
        if changed.contains(c) {
            continue;
        }
        let t = get(target, c);
        if cfg
            .rules(&t.engine)
            .restart_if_peer_starts
            .iter()
            .any(|p| starting.contains(p))
        {
            changed.push(c.clone());
        }
    }
    let mut out: Vec<String> = cfg
        .cards
        .iter()
        .filter(|c| changed.contains(c))
        .cloned()
        .collect();
    out.dedup();
    out
}

/// Slot names for a spec (`{card}{n}` over each serving card's lanes, card order).
pub fn slot_names(cfg: &SwitchConfig, spec: &Spec) -> Vec<String> {
    let lanes: Vec<(String, u32)> = cfg
        .cards
        .iter()
        .filter_map(|c| {
            spec.get(c)
                .filter(|s| !s.is_off())
                .map(|s| (c.clone(), s.lanes))
        })
        .collect();
    SlotView::from_lanes(&lanes)
}

/// The estimate in seconds: sequential starts add up, parallel ones overlap, plus stops.
pub fn estimate(cfg: &SwitchConfig, target: &Spec, restart: &[String]) -> u64 {
    let mut seq = 0;
    let mut par = 0;
    for c in restart {
        let Some(t) = target.get(c).filter(|t| !t.is_off()) else {
            continue;
        };
        let r = cfg.rules(&t.engine);
        if r.sequential {
            seq += r.start_estimate_seconds;
        } else {
            par = par.max(r.start_estimate_seconds);
        }
    }
    if restart.is_empty() {
        0
    } else {
        15 + seq + par
    }
}
