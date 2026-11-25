//! `abhoth router`: the OpenAI-compatible gateway for DeepThought (ROUTER.md). Replaces LiteLLM
//! (`:8010`), qfn-lb (`:8092`), pig-slots (`:8099`) and the profiles API's on-demand relay.

pub mod cli;
pub mod config;
pub mod effort;
pub mod import;
pub mod metrics;
pub mod observe;
pub mod ondemand;
pub mod placement;
pub mod proxy;
pub mod reload;
pub mod reqlog;
pub mod rewrite;
pub mod server;
pub mod slots;
pub mod state;
pub mod status;

/// Per-process services next to the snapshot (slots registry, admission, affinity, on-demand).
pub struct Extensions {
    pub registry: slots::Registry,
    pub admission: placement::Admission,
    pub affinity: placement::Affinity,
    pub ondemand: ondemand::Coordinator,
    pub observer: observe::Observer,
}

impl Extensions {
    pub fn new(_snap: &config::Snapshot) -> Self {
        Self {
            registry: slots::Registry::default(),
            admission: placement::Admission::default(),
            affinity: placement::Affinity::default(),
            ondemand: ondemand::Coordinator::default(),
            observer: observe::Observer::default(),
        }
    }

    /// A request on `card` ended: wake admission waiters.
    pub fn notify_released(&self, _card: &str) {
        self.admission.wake();
    }
}
