//! A fake GPU host and router plane with fault injection, for the orchestrator's tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::*;
use crate::guards::{Caller, GuardBoard};
use crate::leases::{LeaseTable, SlotView};

#[derive(Default)]
pub struct HostState {
    pub running: Spec,
    pub foreign: BTreeMap<String, Vec<String>>,
    /// stop_once answers "still there" this many times per card (a zombie container)
    pub zombie_tries: BTreeMap<String, u32>,
    /// GPU memory readings per card after a stop, popped one per poll (then 0)
    pub gpu_used: BTreeMap<String, Vec<u64>>,
    /// health polls before a started card answers Ok
    pub health_polls: u32,
    polls: BTreeMap<String, u32>,
    /// (card, model) that exits while starting, with its log tail, and maybe an OOM line
    pub exits: BTreeMap<(String, String), (String, Option<String>)>,
    /// models whose verify chat never answers
    pub chat_hangs: BTreeSet<String>,
    /// after this many host calls, every call fails (SSH dropped)
    pub drop_after: Option<usize>,
    pub kills: Vec<String>,
    pub clocks: BTreeMap<String, u32>,
    pub restore_fails: bool,
    pub lock_fails: bool,
    pub precondition: Option<String>,
    pub holds: Vec<u64>,
    pub released: u32,
    pub locked: bool,
}

#[derive(Default, Clone)]
pub struct FakeHost {
    pub st: Arc<Mutex<HostState>>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FakeHost {
    pub fn with(running: Spec) -> Self {
        let h = FakeHost::default();
        h.st.lock().unwrap().running = running;
        h
    }

    fn call(&self, what: String) -> HostResult<()> {
        let mut c = self.calls.lock().unwrap();
        c.push(what);
        let n = c.len();
        drop(c);
        match self.st.lock().unwrap().drop_after {
            Some(k) if n > k => Err(HostError("ssh: connection reset".into())),
            _ => Ok(()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn calls_like(&self, prefix: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .collect()
    }
}

impl HostOps for FakeHost {
    async fn observe(&self) -> HostResult<Observed> {
        self.call("observe".into())?;
        let s = self.st.lock().unwrap();
        Ok(Observed {
            spec: s.running.clone(),
            foreign: s.foreign.clone(),
        })
    }

    async fn preconditions(&self, _t: &Spec) -> Result<(), String> {
        self.call("preconditions".into()).map_err(|e| e.0)?;
        match &self.st.lock().unwrap().precondition {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    async fn restore_models(&self, card: &str, spec: &CardSpec) -> HostResult<Vec<String>> {
        self.call(format!("restore {card} {}", spec.model))?;
        if self.st.lock().unwrap().restore_fails {
            return Err(HostError("sha256 mismatch".into()));
        }
        Ok(vec![format!("models: {} present", spec.model)])
    }

    async fn lock(&self) -> HostResult<()> {
        self.call("lock".into())?;
        let mut s = self.st.lock().unwrap();
        if s.lock_fails {
            return Err(HostError("held by another process".into()));
        }
        s.locked = true;
        Ok(())
    }

    async fn unlock(&self) {
        let _ = self.call("unlock".into());
        self.st.lock().unwrap().locked = false;
    }

    async fn stop_once(&self, card: &str, _spec: &CardSpec) -> HostResult<bool> {
        self.call(format!("stop {card}"))?;
        let mut s = self.st.lock().unwrap();
        if let Some(n) = s.zombie_tries.get_mut(card)
            && *n > 0
        {
            *n -= 1;
            return Ok(false);
        }
        s.running.insert(card.to_string(), CardSpec::off());
        Ok(true)
    }

    async fn gpu_used_mib(&self, card: &str) -> HostResult<u64> {
        self.call(format!("gpu {card}"))?;
        let mut s = self.st.lock().unwrap();
        Ok(match s.gpu_used.get_mut(card) {
            Some(v) if !v.is_empty() => v.remove(0),
            _ => 0,
        })
    }

    async fn before_start(&self, card: &str, _spec: &CardSpec) -> HostResult<Vec<String>> {
        self.call(format!("before {card}"))?;
        Ok(vec![])
    }

    async fn start(&self, card: &str, spec: &CardSpec) -> HostResult<()> {
        self.call(format!("start {card} {}", spec.model))?;
        let mut s = self.st.lock().unwrap();
        s.running.insert(card.to_string(), spec.clone());
        s.polls.insert(card.to_string(), 0);
        Ok(())
    }

    async fn health(&self, card: &str, spec: &CardSpec) -> Health {
        if self.call(format!("health {card}")).is_err() {
            return Health::NotYet("ssh down".into());
        }
        let mut s = self.st.lock().unwrap();
        if s.running.get(card) != Some(spec) {
            return Health::NotYet("not running that".into());
        }
        if let Some((tail, oom)) = s
            .exits
            .get(&(card.to_string(), spec.model.clone()))
            .cloned()
        {
            if let Some(k) = oom
                && !s.kills.contains(&k)
            {
                s.kills.push(k);
            }
            s.running.insert(card.to_string(), CardSpec::off());
            return Health::Exited(tail);
        }
        let need = s.health_polls;
        let p = s.polls.entry(card.to_string()).or_insert(need);
        if *p < need {
            *p += 1;
            return Health::NotYet("loading".into());
        }
        Health::Ok
    }

    async fn verify_chat(&self, card: &str, spec: &CardSpec) -> Result<(), String> {
        self.call(format!("chat {card}")).map_err(|e| e.0)?;
        if self.st.lock().unwrap().chat_hangs.contains(&spec.model) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn clocks(&self, card: &str) -> HostResult<Clocks> {
        self.call(format!("clocks {card}"))?;
        Ok(Clocks {
            max_sm_mhz: self
                .st
                .lock()
                .unwrap()
                .clocks
                .get(card)
                .copied()
                .unwrap_or(1597),
        })
    }

    async fn oom_kills(&self) -> HostResult<Vec<String>> {
        self.call("kills".into())?;
        Ok(self.st.lock().unwrap().kills.clone())
    }

    async fn gpu_power_hold(&self, secs: u64) -> HostResult<()> {
        self.call(format!("hold {secs}"))?;
        self.st.lock().unwrap().holds.push(secs);
        Ok(())
    }

    async fn gpu_power_release(&self) {
        let _ = self.call("release".into());
        self.st.lock().unwrap().released += 1;
    }
}

/// A hook run on every slot-view change: (table, new view) -> a note for the test.
pub type SlotHook = Box<dyn Fn(&mut LeaseTable, &SlotView) -> Option<String> + Send + Sync>;

pub struct FakePlane {
    pub table: Mutex<LeaseTable>,
    pub view: Mutex<SlotView>,
    pub views: Mutex<Vec<SlotView>>,
    pub notes: Mutex<Vec<String>>,
    pub hook: Mutex<Option<SlotHook>>,
    pub board: GuardBoard,
    pub withdrawn: Mutex<Vec<Vec<String>>>,
    pub commits: Mutex<Vec<(Option<String>, Vec<String>)>>,
    pub now: f64,
}

pub const NOW: f64 = 1_800_000_000.0;

impl FakePlane {
    pub fn new(view: SlotView) -> Self {
        FakePlane {
            table: Mutex::new(LeaseTable::default()),
            view: Mutex::new(view),
            views: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
            board: GuardBoard::default(),
            withdrawn: Mutex::new(Vec::new()),
            commits: Mutex::new(Vec::new()),
            now: NOW,
        }
    }

    /// A pig-slots claim against the current view.
    pub fn claim(&self, key: &str, extra: Value) -> Value {
        let mut b: Map<String, Value> = extra.as_object().cloned().unwrap_or_default();
        b.insert("key".into(), json!(key));
        let v = self.view.lock().unwrap().clone();
        self.table
            .lock()
            .unwrap()
            .claim(&v, &b, self.now, 90.0, None)
            .1
    }
}

impl Plane for FakePlane {
    fn set_slots(&self, view: SlotView) {
        *self.view.lock().unwrap() = view.clone();
        self.views.lock().unwrap().push(view.clone());
        if let Some(h) = self.hook.lock().unwrap().as_ref()
            && let Some(n) = h(&mut self.table.lock().unwrap(), &view)
        {
            self.notes.lock().unwrap().push(n);
        }
    }

    fn held_on(&self, cards: &[String]) -> Vec<(String, String)> {
        let v = self.view.lock().unwrap().clone();
        self.table
            .lock()
            .unwrap()
            .held_on(&v, cards, self.now, 90.0)
    }

    fn blocks(
        &self,
        action: Action,
        cards: &[String],
        caller: &Caller,
        layout: &str,
    ) -> Vec<Block> {
        self.board
            .check(action, cards, caller, Some(layout), self.now)
    }

    fn withdraw_routes(&self, cards: &[String]) {
        self.withdrawn.lock().unwrap().push(cards.to_vec());
    }

    fn commit_routes(&self, layout: Option<&str>, _spec: &Spec, ready: &[String]) {
        self.commits
            .lock()
            .unwrap()
            .push((layout.map(str::to_string), ready.to_vec()));
    }
}
