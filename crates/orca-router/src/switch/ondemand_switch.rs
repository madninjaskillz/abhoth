//! On-demand switches through the in-process orchestrator (control plane on) instead of the
//! profiles API (ROUTER-CONFIG.md 7.2). `ondemand::start` asks the coordinator's switcher when
//! one is set; it is set only when `control.enabled`, so with the flag off nothing changes.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{CardSpec, HostOps, Orchestrator, Plane, Spec, SwitchRequest, holder_of, restart_set};
use crate::config::OnDemandModel;
use crate::guards::{Action, Caller};

/// What came of asking for a switch.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// The target already runs.
    Already(String),
    /// Another switch is running: wait.
    Busy(String),
    /// A job started (`job` is the handle for [`OnDemandSwitcher::job`]).
    Started {
        job: String,
        estimate: Option<f64>,
        layout: String,
    },
    /// Not possible this way (restarts both cards, unknown pick).
    Refused(String),
}

pub type AskFuture<'a> = Pin<Box<dyn Future<Output = Ask> + Send + 'a>>;

pub trait OnDemandSwitcher: Send + Sync {
    /// Switch `card` to `pick` for the client name `target`.
    fn ask<'a>(
        &'a self,
        target: &'a str,
        card: &'a str,
        pick: &'a OnDemandModel,
        who: &'a str,
        caller: Caller,
        allow_both_cards: bool,
    ) -> AskFuture<'a>;
    /// `(state, error)` of a job started here: `running`, then layoutctl's final states.
    fn job(&self, id: &str) -> Option<(String, Option<String>)>;
}

/// Job handle -> (state, error).
type JobStates = Arc<Mutex<HashMap<String, (String, Option<String>)>>>;

/// Builds a card spec for an on-demand pick (engine, engine profile, model).
pub type PickSpec = Arc<dyn Fn(&OnDemandModel) -> Option<CardSpec> + Send + Sync>;

/// The orchestrator as the on-demand switcher.
pub struct InProcess<H: HostOps + 'static, P: Plane + 'static> {
    orch: Arc<Orchestrator<H, P>>,
    pick: PickSpec,
    /// What runs now and its layout name (updated when a job ends).
    current: Arc<Mutex<(Option<String>, Spec)>>,
    jobs: JobStates,
    n: AtomicU64,
}

impl<H: HostOps + 'static, P: Plane + 'static> InProcess<H, P> {
    pub fn new(
        orch: Arc<Orchestrator<H, P>>,
        pick: PickSpec,
        active: Option<String>,
        running: Spec,
    ) -> Self {
        InProcess {
            orch,
            pick,
            current: Arc::new(Mutex::new((active, running))),
            jobs: Arc::default(),
            n: AtomicU64::new(0),
        }
    }
}

impl<H: HostOps + 'static, P: Plane + 'static> OnDemandSwitcher for InProcess<H, P> {
    fn ask<'a>(
        &'a self,
        target: &'a str,
        card: &'a str,
        pick: &'a OnDemandModel,
        who: &'a str,
        caller: Caller,
        allow_both_cards: bool,
    ) -> AskFuture<'a> {
        Box::pin(async move {
            let Some(want) = (self.pick)(pick) else {
                return Ask::Refused(format!(
                    "{target}: {} / {} / {} is not a known engine profile",
                    pick.engine, pick.engine_profile, pick.model
                ));
            };
            let (active, now) = self.current.lock().unwrap().clone();
            let mut to = now.clone();
            // An instance that spans this card (tensor parallel) has to go for it.
            while let Some(h) = holder_of(&to, card) {
                to.insert(h, CardSpec::off());
            }
            to.insert(card.to_string(), want);
            let layout = format!("on-demand:{card}={target}");
            if to == now {
                return Ask::Already(format!("{target} already runs on card {card}"));
            }
            let moving = restart_set(&self.orch.cfg, &now, &to);
            if moving.len() > 1 && !allow_both_cards {
                return Ask::Refused(format!(
                    "bringing up {target} restarts cards {}: switch by hand",
                    moving.join(", ")
                ));
            }
            if self.orch.busy() {
                return Ask::Busy("a switch is already running".into());
            }
            let id = format!("od-{}", self.n.fetch_add(1, Ordering::SeqCst) + 1);
            self.jobs
                .lock()
                .unwrap()
                .insert(id.clone(), ("running".into(), None));
            let req = SwitchRequest {
                layout: layout.clone(),
                target: to.clone(),
                active,
                requested_by: format!("router-ondemand:{target}")
                    .chars()
                    .take(80)
                    .collect(),
                reason: format!(
                    "on demand: card {card} -> {target} (requested by {who}; card {card} idle longest)"
                ),
                caller,
                action: Action::OnDemand,
            };
            let estimate = Some(super::estimate(&self.orch.cfg, &to, &moving) as f64);
            let (orch, jobs, current, jid, lay) = (
                self.orch.clone(),
                self.jobs.clone(),
                self.current.clone(),
                id.clone(),
                layout.clone(),
            );
            tokio::spawn(async move {
                let r = orch.switch(req).await;
                if r.state == "done" {
                    *current.lock().unwrap() = (Some(lay), to);
                }
                jobs.lock()
                    .unwrap()
                    .insert(jid, (r.state.clone(), r.error.clone()));
            });
            Ask::Started {
                job: id,
                estimate,
                layout,
            }
        })
    }

    fn job(&self, id: &str) -> Option<(String, Option<String>)> {
        self.jobs.lock().unwrap().get(id).cloned()
    }
}
