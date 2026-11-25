//! Sticky slot allocator (`SlotAllocator`): leases one slot (copy + slot number) per holder from
//! the shared registry, falls back to the local lock file while the registry is unreachable
//! (retried after 20 s), renews registry leases every TTL/3 and re-takes expired ones, checks a
//! copy's health before handing out a slot on it, and fails a holder over to another copy when
//! its copy dies. Agents are placed on the least-loaded copy by every lease on it (balanced
//! placement, C# 9e38f45). Never fails for registry or health trouble: the worst case is "no
//! slot, use the fallback".

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::registry::{SlotClaim, SlotClaimResult, SlotHealthProbe, SlotRegistry};
use crate::config::{OrcaConfig, SlotsConfig};

/// Lease kind of the Lead (registry semantics; sub-agents are [`AGENT_KIND`]).
pub const LEAD_KIND: &str = "main";
/// Lease kind for sub-agents; only these count toward `slots.agentsPerCopy`.
pub const AGENT_KIND: &str = "agent";

const REGISTRY_RETRY: Duration = Duration::from_secs(20);
const COPY_DOWN_FOR: Duration = Duration::from_secs(30);

/// What a lease is for: owner id (unique in this process), kind and preferences.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SlotRequest {
    /// Logical owner, e.g. "lead" or an agent id; one lease per owner.
    pub owner: String,
    /// [`LEAD_KIND`] for the Lead, [`AGENT_KIND`] for sub-agents.
    pub kind: String,
    /// Shown in the registry.
    pub label: String,
    /// Try a slot on this copy first (the Lead prefers the faster card).
    pub prefer_copy: Option<String>,
    /// Try exactly this slot first (a follow-up re-claims its old slot).
    pub prefer_slot: Option<String>,
    /// The biggest task of its batch: placed on the least-loaded copy and counted as two
    /// leases there while it runs, so the other agents lean to the other copies.
    pub heavy: bool,
}

impl SlotRequest {
    pub fn new(owner: &str, kind: &str, label: &str) -> Self {
        Self {
            owner: owner.into(),
            kind: kind.into(),
            label: label.into(),
            ..Self::default()
        }
    }

    /// A sub-agent's request.
    pub fn agent(owner: &str, label: &str) -> Self {
        Self::new(owner, AGENT_KIND, label)
    }

    pub fn with_prefer_copy(mut self, copy: Option<&str>) -> Self {
        self.prefer_copy = copy.map(str::to_string);
        self
    }

    pub fn with_prefer_slot(mut self, slot: Option<&str>) -> Self {
        self.prefer_slot = slot.map(str::to_string);
        self
    }

    pub fn with_heavy(mut self, heavy: bool) -> Self {
        self.heavy = heavy;
        self
    }
}

#[derive(Debug, Default, Clone)]
struct LeaseState {
    slot: Option<String>,
    copy: Option<String>,
    via_registry: bool,
    released: bool,
    last_slot: Option<String>,
}

type LeaseListener = Arc<dyn Fn(&SlotLease) + Send + Sync>;

/// One holder's slot. `slot()` is `None` when the holder runs on the fallback client (copies
/// down, or its lease was lost and could not be re-taken). The slot can change during the
/// lease's life (failover, expiry); listeners added with [`SlotLease::on_changed`] run then.
pub struct SlotLease {
    request: SlotRequest,
    key: String,
    state: Mutex<LeaseState>,
    /// Serializes fills of this lease (upgrade, failover, heartbeat re-claim).
    gate: tokio::sync::Mutex<()>,
    listeners: Mutex<Vec<LeaseListener>>,
}

impl std::fmt::Debug for SlotLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotLease")
            .field("key", &self.key)
            .field("state", &*self.state.lock().unwrap())
            .finish()
    }
}

impl SlotLease {
    fn new(request: SlotRequest, key: String) -> Self {
        Self {
            request,
            key,
            state: Mutex::new(LeaseState::default()),
            gate: tokio::sync::Mutex::new(()),
            listeners: Mutex::new(Vec::new()),
        }
    }

    pub fn request(&self) -> &SlotRequest {
        &self.request
    }

    pub fn owner(&self) -> &str {
        &self.request.owner
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// "A2" etc., or `None` when on the fallback client.
    pub fn slot(&self) -> Option<String> {
        self.state.lock().unwrap().slot.clone()
    }

    /// The copy ("A") the slot is on, or `None`.
    pub fn copy(&self) -> Option<String> {
        self.state.lock().unwrap().copy.clone()
    }

    /// True when the lease lives in the central registry (it is renewed).
    pub fn via_registry(&self) -> bool {
        self.state.lock().unwrap().via_registry
    }

    pub fn released(&self) -> bool {
        self.state.lock().unwrap().released
    }

    fn set_released(&self, released: bool) {
        self.state.lock().unwrap().released = released;
    }

    /// The last slot this owner held: what a follow-up tries to re-claim.
    pub fn last_slot(&self) -> Option<String> {
        self.state.lock().unwrap().last_slot.clone()
    }

    /// Run `f` whenever the slot changes.
    pub fn on_changed(&self, f: impl Fn(&SlotLease) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Arc::new(f));
    }

    fn set(&self, slot: Option<String>, copy: Option<String>, via_registry: bool) {
        let changed = {
            let mut s = self.state.lock().unwrap();
            let changed = s.slot != slot;
            s.via_registry = via_registry && slot.is_some();
            if slot.is_some() {
                s.last_slot = slot.clone();
            }
            s.slot = slot;
            s.copy = copy;
            changed
        };
        if changed {
            let listeners = self.listeners.lock().unwrap().clone();
            for l in listeners {
                l(self);
            }
        }
    }
}

/// Monotonic clock (tests move it forward).
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Who / where the allocator leases as; defaults come from the machine.
#[derive(Clone, Default)]
pub struct AllocatorOptions {
    pub clock: Option<Clock>,
    /// Lease owner shown in the registry (default slots.owner, else "user@host").
    pub owner: Option<String>,
    pub host: Option<String>,
    pub pid: Option<u32>,
    /// `agent.minFreeCopiesToDelegate <= 0`: with slots.wholeCopyPerAgent an agent may also
    /// take a slot on the Lead's copy, after every copy the Lead is not on.
    pub agents_may_share_lead_copy: bool,
}

/// `Environment.MachineName`.
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "localhost".into())
}

/// `Environment.UserName`.
pub fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into())
}

type ChangeListener = Arc<dyn Fn() + Send + Sync>;

pub struct SlotAllocator {
    config: SlotsConfig,
    registry: Option<Arc<dyn SlotRegistry>>,
    local: Arc<dyn SlotRegistry>,
    health: Arc<dyn SlotHealthProbe>,
    clock: Clock,
    who: String,
    host: String,
    pid: u32,
    agents_may_share_lead_copy: bool,
    held: Mutex<HashMap<String, Arc<SlotLease>>>,
    copy_down_until: Mutex<HashMap<String, Instant>>,
    io: tokio::sync::Mutex<()>,
    wait_gate: tokio::sync::Mutex<()>,
    place: tokio::sync::Mutex<()>,
    freed: tokio::sync::Notify,
    registry_down_until: Mutex<Option<Instant>>,
    ttl_seconds: Mutex<f64>,
    heartbeat: Mutex<Option<tokio::task::JoinHandle<()>>>,
    heartbeat_running: AtomicBool,
    listeners: Mutex<Vec<ChangeListener>>,
    me: Weak<SlotAllocator>,
}

impl SlotAllocator {
    pub fn new(
        config: SlotsConfig,
        registry: Option<Arc<dyn SlotRegistry>>,
        local: Arc<dyn SlotRegistry>,
        health: Arc<dyn SlotHealthProbe>,
        options: AllocatorOptions,
    ) -> Arc<Self> {
        let host = options.host.unwrap_or_else(machine_name);
        let who = options
            .owner
            .or_else(|| config.owner.clone())
            .unwrap_or_else(|| format!("{}@{host}", user_name()));
        Arc::new_cyclic(|me| Self {
            config,
            registry,
            local,
            health,
            clock: options.clock.unwrap_or_else(|| Arc::new(Instant::now)),
            who,
            host,
            pid: options.pid.unwrap_or_else(std::process::id),
            agents_may_share_lead_copy: options.agents_may_share_lead_copy,
            held: Mutex::new(HashMap::new()),
            copy_down_until: Mutex::new(HashMap::new()),
            io: tokio::sync::Mutex::new(()),
            wait_gate: tokio::sync::Mutex::new(()),
            place: tokio::sync::Mutex::new(()),
            freed: tokio::sync::Notify::new(),
            registry_down_until: Mutex::new(None),
            ttl_seconds: Mutex::new(90.0),
            heartbeat: Mutex::new(None),
            heartbeat_running: AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
            me: me.clone(),
        })
    }

    /// The production allocator for `config`, or `None` when slots are off.
    pub fn create(config: &OrcaConfig) -> Option<Arc<Self>> {
        let slots = config.slots.as_ref().filter(|s| s.enabled)?;
        if slots.copies.is_empty() {
            return None;
        }
        let names = Self::slot_names(slots);
        let registry: Option<Arc<dyn SlotRegistry>> = slots
            .registry_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(|u| {
                Arc::new(super::http::HttpSlotRegistry::new(
                    u,
                    Duration::from_millis(slots.registry_timeout_ms.max(200) as u64),
                )) as Arc<dyn SlotRegistry>
            });
        let local = Arc::new(super::local::LocalSlotRegistry::new(
            slots
                .local_state_file
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(super::local::LocalSlotRegistry::default_state_file),
            names,
        ));
        let copies: Vec<(String, String)> = slots
            .copies
            .iter()
            .map(|c| (c.name.clone(), c.base_url.clone()))
            .collect();
        let health = Arc::new(super::http::HttpSlotHealthProbe::new(
            &copies,
            Duration::from_millis(slots.health_timeout_ms.max(100) as u64),
            None,
        ));
        Some(Self::new(
            slots.clone(),
            registry,
            local,
            health,
            AllocatorOptions {
                agents_may_share_lead_copy: config.agent.min_free_copies_to_delegate <= 0,
                ..AllocatorOptions::default()
            },
        ))
    }

    /// "A1", "A2", "B1", "B2" for copies A and B with 2 slots each.
    pub fn slot_names(config: &SlotsConfig) -> Vec<String> {
        config
            .copies
            .iter()
            .flat_map(|c| (1..=c.slots.max(1)).map(move |i| format!("{}{i}", c.name)))
            .collect()
    }

    pub fn config(&self) -> &SlotsConfig {
        &self.config
    }

    /// Lease owner shown in the registry ("user@host").
    pub fn who(&self) -> &str {
        &self.who
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn agents_may_share_lead_copy(&self) -> bool {
        self.agents_may_share_lead_copy
    }

    /// The copy the Lead asks for first.
    pub fn lead_copy(&self) -> String {
        self.config
            .lead_copy
            .clone()
            .or_else(|| self.config.copies.first().map(|c| c.name.clone()))
            .unwrap_or_else(|| "A".into())
    }

    /// Total slots across all copies.
    pub fn total_slots(&self) -> i32 {
        self.config.copies.iter().map(|c| c.slots.max(1)).sum()
    }

    /// Leases this process holds now.
    pub fn held(&self) -> Vec<Arc<SlotLease>> {
        self.held.lock().unwrap().values().cloned().collect()
    }

    /// True while the registry is configured and believed reachable.
    pub fn registry_up(&self) -> bool {
        self.registry.is_some()
            && self
                .registry_down_until
                .lock()
                .unwrap()
                .is_none_or(|until| (self.clock)() >= until)
    }

    /// Run `f` whenever a lease is taken, moved or released (UI refresh).
    pub fn on_leases_changed(&self, f: impl Fn() + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Arc::new(f));
    }

    fn leases_changed(&self) {
        let listeners = self.listeners.lock().unwrap().clone();
        for l in listeners {
            l();
        }
    }

    fn key_of(&self, owner: &str) -> String {
        format!("{}:{}:{owner}", self.who, self.pid)
    }

    fn put_held(&self, lease: &Arc<SlotLease>) {
        self.held
            .lock()
            .unwrap()
            .insert(lease.owner().to_string(), lease.clone());
    }

    fn remove_held(&self, lease: &Arc<SlotLease>) {
        let mut held = self.held.lock().unwrap();
        if held
            .get(lease.owner())
            .is_some_and(|l| Arc::ptr_eq(l, lease))
        {
            held.remove(lease.owner());
        }
    }

    /// A lease with no slot yet (fill it with [`Self::try_upgrade`]), for a holder that needs
    /// its client before the first claim, like the Lead.
    pub fn new_lease(&self, request: SlotRequest) -> Arc<SlotLease> {
        let key = self.key_of(&request.owner);
        Arc::new(SlotLease::new(request, key))
    }

    /// Try to take a slot now: `None` when every usable slot is busy (wait or borrow), a lease
    /// with a slot on a healthy copy, or (no healthy copy) a lease with no slot (use the
    /// fallback). An owner that already holds a lease gets it back.
    pub async fn try_acquire(&self, request: &SlotRequest) -> Option<Arc<SlotLease>> {
        let existing = self.held.lock().unwrap().get(&request.owner).cloned();
        if let Some(e) = &existing
            && e.slot().is_some()
        {
            return Some(e.clone());
        }
        let lease = existing.unwrap_or_else(|| self.new_lease(request.clone()));
        self.try_fill(&lease, request, None).await.then_some(lease)
    }

    /// Take a slot, waiting (FIFO among this process's waiters) while all are busy.
    /// `on_waiting` runs once when the wait starts. Cancel by dropping the future.
    pub async fn acquire(
        &self,
        request: &SlotRequest,
        on_waiting: Option<Box<dyn FnOnce() + Send>>,
    ) -> Arc<SlotLease> {
        if let Some(lease) = self.try_acquire(request).await {
            return lease;
        }
        if let Some(f) = on_waiting {
            f();
        }
        let _gate = self.wait_gate.lock().await;
        let retry = Duration::from_secs(self.config.retry_seconds.max(1) as u64);
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(lease) = self.try_acquire(request).await {
                return lease;
            }
            tokio::select! {
                _ = freed => {}
                _ = tokio::time::sleep(retry) => {}
            }
        }
    }

    /// The holder's copy failed (transport errors): mark it down for a while, give its slot
    /// back and take one on another copy. Returns true when the lease moved to another slot.
    pub async fn failover(&self, lease: &Arc<SlotLease>) -> bool {
        let _g = lease.gate.lock().await;
        let Some(dead) = lease.copy().filter(|_| !lease.released()) else {
            return false;
        };
        self.copy_down_until
            .lock()
            .unwrap()
            .insert(dead.to_lowercase(), (self.clock)() + COPY_DOWN_FOR);
        tracing::warn!(
            "Slot {:?} ({}): copy {dead} failing; failing over",
            lease.slot(),
            lease.owner()
        );
        self.release_store(lease).await;
        let old = lease.slot();
        let request = SlotRequest {
            prefer_slot: None,
            prefer_copy: None,
            ..lease.request.clone()
        };
        if !self.try_fill(lease, &request, Some(&dead)).await {
            lease.set(None, None, false);
            self.put_held(lease);
            self.signal();
        }
        let now = lease.slot();
        now.is_some() && now != old
    }

    /// A holder running on the fallback (no slot) tries again to get one: cheap, never waits.
    pub async fn try_upgrade(&self, lease: &Arc<SlotLease>) -> bool {
        let _g = lease.gate.lock().await;
        if lease.released() || lease.slot().is_some() {
            return lease.slot().is_some();
        }
        let request = SlotRequest {
            prefer_slot: lease.last_slot().or(lease.request.prefer_slot.clone()),
            ..lease.request.clone()
        };
        self.try_fill(lease, &request, None).await && lease.slot().is_some()
    }

    /// Give a lease back (finish, cancel, failure, timeout). Safe to call twice.
    pub async fn release(&self, lease: &Arc<SlotLease>) {
        if lease.released() {
            return;
        }
        lease.set_released(true);
        self.remove_held(lease);
        self.release_store(lease).await;
        lease.set(None, None, false);
        self.signal();
    }

    /// Give an idle holder's slot back but keep the lease usable: [`Self::try_upgrade`] takes
    /// a slot again (its last one first). False when it held none.
    pub async fn park(&self, lease: &Arc<SlotLease>) -> bool {
        {
            let _g = lease.gate.lock().await;
            if lease.released() || lease.slot().is_none() {
                return false;
            }
            self.remove_held(lease);
            self.release_store(lease).await;
            lease.set(None, None, false);
        }
        self.signal();
        self.leases_changed();
        true
    }

    /// Shutdown: release everything this process holds, bounded by `timeout`.
    pub async fn release_all(&self, timeout: Duration) {
        if let Some(h) = self.heartbeat.lock().unwrap().take() {
            h.abort();
        }
        let leases: Vec<Arc<SlotLease>> =
            self.held.lock().unwrap().drain().map(|(_, l)| l).collect();
        for lease in &leases {
            lease.set_released(true);
            lease.set(None, None, false);
        }
        let work = async {
            if let Err(e) = self.local.release_all(&self.host, self.pid).await {
                tracing::debug!("Local slot release failed: {e}");
            }
            if let Some(registry) = &self.registry
                && (self.registry_up() || leases.iter().any(|l| l.via_registry()))
                && let Err(e) = registry.release_all(&self.host, self.pid).await
            {
                tracing::debug!("Registry release-all failed (leases expire by TTL): {e}");
            }
        };
        let _ = tokio::time::timeout(timeout, work).await;
        self.signal();
    }

    /// Renew registry leases; take back any the registry lost (the same slot if still free,
    /// else any free slot, else the fallback until a slot frees up). Called by the heartbeat
    /// timer; public for tests.
    pub async fn heartbeat(&self) {
        let Some(registry) = &self.registry else {
            return;
        };
        let mine: Vec<Arc<SlotLease>> = self
            .held()
            .into_iter()
            .filter(|l| l.via_registry() && !l.released())
            .collect();
        if mine.is_empty() {
            return;
        }
        let lost = {
            let _io = self.io.lock().await;
            let keys: Vec<String> = mine.iter().map(|l| l.key.clone()).collect();
            match registry.renew(&keys).await {
                Ok(lost) => {
                    *self.registry_down_until.lock().unwrap() = None;
                    lost
                }
                Err(e) => {
                    self.mark_registry_down(&e.0);
                    return;
                }
            }
        };
        for key in lost {
            let Some(lease) = mine.iter().find(|l| l.key == key) else {
                continue;
            };
            if lease.released() {
                continue;
            }
            let was = lease.slot();
            tracing::warn!(
                "Slot lease {was:?} ({}) expired in the registry; re-claiming",
                lease.owner()
            );
            let request = SlotRequest {
                prefer_slot: was,
                ..lease.request.clone()
            };
            let _g = lease.gate.lock().await;
            if lease.released() {
                continue;
            }
            if !self.try_fill(lease, &request, None).await {
                lease.set(None, None, false);
                self.put_held(lease);
            }
        }
    }

    // ---- internals ----

    /// Claim (preferred slot -> preferred copy -> least-loaded), check the copy's health, retry
    /// on another copy if it is down. True when the lease now has a slot or (no healthy copy at
    /// all) is set to the fallback; false when everything usable is busy.
    async fn try_fill(
        &self,
        lease: &Arc<SlotLease>,
        request: &SlotRequest,
        avoid_extra: Option<&str>,
    ) -> bool {
        // One placement at a time, so concurrent spawns see each other's copy choice.
        let _place = self.place.lock().await;
        self.try_fill_core(lease, request, avoid_extra).await
    }

    async fn try_fill_core(
        &self,
        lease: &Arc<SlotLease>,
        request: &SlotRequest,
        avoid_extra: Option<&str>,
    ) -> bool {
        let now = (self.clock)();
        let mut down: Vec<String> = {
            let until = self.copy_down_until.lock().unwrap();
            self.config
                .copies
                .iter()
                .map(|c| c.name.clone())
                .filter(|c| {
                    until.get(&c.to_lowercase()).is_some_and(|u| *u > now)
                        || avoid_extra.is_some_and(|a| a.eq_ignore_ascii_case(c))
                })
                .collect()
        };
        let full = self.full_copies(request);
        let contains = |list: &[String], c: &str| list.iter().any(|x| x.eq_ignore_ascii_case(c));

        while down.len() < self.config.copies.len() {
            // Every healthy copy already runs its share of our agents: wait for one to finish.
            if self
                .config
                .copies
                .iter()
                .all(|c| contains(&down, &c.name) || contains(&full, &c.name))
            {
                return false;
            }
            let excluded: Vec<String> = down.iter().chain(full.iter()).cloned().collect();
            let Some((result, via_registry)) =
                self.claim_preferred(lease, request, &excluded).await
            else {
                return false;
            };
            let copy = result
                .copy
                .clone()
                .unwrap_or_else(|| first_char(result.slot.as_deref().unwrap_or_default()));
            if self.health.is_healthy(&copy).await {
                lease.set_released(false);
                lease.set(result.slot.clone(), Some(copy), via_registry);
                self.put_held(lease);
                if via_registry {
                    self.start_heartbeat(result.ttl);
                }
                tracing::info!(
                    "Slot {} leased to {} ({}){}",
                    result.slot.as_deref().unwrap_or_default(),
                    lease.owner(),
                    request.kind,
                    if via_registry {
                        " via registry"
                    } else {
                        " via local file"
                    }
                );
                self.leases_changed();
                return true;
            }
            tracing::warn!("Copy {copy} failed its health check; trying another copy");
            self.copy_down_until.lock().unwrap().insert(
                copy.to_lowercase(),
                (self.clock)() + Duration::from_secs(10),
            );
            self.release_key(&lease.key, via_registry).await;
            down.push(copy);
        }

        // No healthy copy: run on the fallback (balancer) with no slot.
        tracing::warn!(
            "No healthy model copy; {} uses the fallback client",
            lease.owner()
        );
        lease.set_released(false);
        lease.set(None, None, false);
        self.put_held(lease);
        self.leases_changed();
        true
    }

    /// Copies that already run their share of this process's other agents: an agent request
    /// must not land there. Other kinds (the Lead) are never limited and never count.
    fn full_copies(&self, request: &SlotRequest) -> Vec<String> {
        let cap = if self.config.whole_copy_per_agent {
            1
        } else {
            self.config.agents_per_copy
        };
        if cap <= 0 || request.kind != AGENT_KIND {
            return Vec::new();
        }
        let mut full = self.agent_full_copies(request, cap);
        if self.config.whole_copy_per_agent && !self.agents_may_share_lead_copy {
            // The Lead's copy is never shared while another copy exists.
            let lead = self.lead_copies();
            if self
                .config
                .copies
                .iter()
                .any(|c| !lead.contains(&c.name.to_lowercase()))
            {
                for c in &self.config.copies {
                    if lead.contains(&c.name.to_lowercase())
                        && !full.iter().any(|f| f.eq_ignore_ascii_case(&c.name))
                    {
                        full.push(c.name.clone());
                    }
                }
            }
        }
        full
    }

    /// Copies (lowercase) our non-agent leases (the Lead) are on.
    fn lead_copies(&self) -> HashSet<String> {
        self.held()
            .iter()
            .filter(|l| !l.released() && l.request.kind != AGENT_KIND)
            .filter_map(|l| l.copy())
            .map(|c| c.to_lowercase())
            .collect()
    }

    /// Copies an agent could have to itself right now: not the Lead's, none of our agents on
    /// it and, when the registry answers, no lease from anyone else either.
    pub async fn free_whole_copies(&self) -> Vec<String> {
        let mut busy = self.lead_copies();
        let held = self.held();
        for l in held.iter().filter(|l| !l.released() && l.slot().is_some()) {
            if let Some(c) = l.copy() {
                busy.insert(c.to_lowercase());
            }
        }
        if let Some(registry) = &self.registry
            && self.registry_up()
        {
            let mine: HashSet<String> = held
                .iter()
                .filter(|l| !l.released())
                .filter_map(|l| l.slot())
                .map(|s| s.to_lowercase())
                .collect();
            match registry.busy_slots().await {
                Ok(Some(slots)) => {
                    for slot in slots {
                        if !mine.contains(&slot.to_lowercase()) && !slot.is_empty() {
                            busy.insert(self.copy_of_slot(&slot).to_lowercase());
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(
                    "pig-slots /list failed; counting free copies from our own leases only: {e}"
                ),
            }
        }
        self.config
            .copies
            .iter()
            .map(|c| c.name.clone())
            .filter(|c| !busy.contains(&c.to_lowercase()))
            .collect()
    }

    /// "A1" -> "A" (the longest configured copy name the slot starts with).
    fn copy_of_slot(&self, slot: &str) -> String {
        self.config
            .copies
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| slot.to_lowercase().starts_with(&n.to_lowercase()))
            .max_by_key(|n| n.len())
            .map(str::to_string)
            .unwrap_or_else(|| first_char(slot))
    }

    fn agent_full_copies(&self, request: &SlotRequest, cap: i32) -> Vec<String> {
        let mut running: HashMap<String, i32> = HashMap::new();
        for l in self.held() {
            if !l.released()
                && l.slot().is_some()
                && l.request.kind == AGENT_KIND
                && l.owner() != request.owner
                && let Some(c) = l.copy()
            {
                *running.entry(c.to_lowercase()).or_default() += 1;
            }
        }
        self.config
            .copies
            .iter()
            .map(|c| c.name.clone())
            .filter(|c| running.get(&c.to_lowercase()).copied().unwrap_or(0) >= cap)
            .collect()
    }

    /// For an agent: every copy, least loaded first; on a tie a copy the Lead is not on wins,
    /// then config order. Empty for other kinds.
    async fn agent_copy_order(&self, request: &SlotRequest) -> Vec<String> {
        if request.kind != AGENT_KIND {
            return Vec::new();
        }
        let load = self.copy_load(Some(&request.owner)).await;
        let mut lead = self.lead_copies();
        if lead.is_empty() {
            // The Lead parks its slot when idle; it comes back to its copy.
            lead.insert(self.lead_copy().to_lowercase());
        }
        let mut copies: Vec<(usize, String)> = self
            .config
            .copies
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        copies.sort_by_key(|(i, c)| {
            (
                load.get(&c.to_lowercase()).copied().unwrap_or(0),
                i32::from(lead.contains(&c.to_lowercase())),
                *i,
            )
        });
        copies.into_iter().map(|(_, c)| c).collect()
    }

    /// Load per copy (lowercase names) as the agent order counts it, leaving out
    /// `except_owner`'s own lease: every lease on it (ours, and other processes' when the
    /// registry answers /list) plus one more per heavy agent of ours.
    pub async fn copy_load(&self, except_owner: Option<&str>) -> HashMap<String, i32> {
        let held = self.held();
        let own = except_owner
            .and_then(|o| held.iter().find(|l| l.owner() == o))
            .and_then(|l| l.slot());
        let mut busy: Vec<String> = Vec::new();
        let add = |s: String, busy: &mut Vec<String>| {
            if !busy.iter().any(|b| b.eq_ignore_ascii_case(&s)) {
                busy.push(s);
            }
        };
        for l in &held {
            if !l.released()
                && Some(l.owner()) != except_owner
                && let Some(s) = l.slot()
            {
                add(s, &mut busy);
            }
        }
        if let Some(registry) = &self.registry
            && self.registry_up()
        {
            match registry.busy_slots().await {
                Ok(Some(slots)) => {
                    for slot in slots {
                        if !slot.is_empty()
                            && !own
                                .as_deref()
                                .is_some_and(|o| o.eq_ignore_ascii_case(&slot))
                        {
                            add(slot, &mut busy);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(
                    "pig-slots /list failed; counting copy load from our own leases only: {e}"
                ),
            }
        }
        let mut load: HashMap<String, i32> = HashMap::new();
        for s in busy {
            *load
                .entry(self.copy_of_slot(&s).to_lowercase())
                .or_default() += 1;
        }
        for l in &held {
            if !l.released()
                && l.slot().is_some()
                && l.request.heavy
                && l.request.kind == AGENT_KIND
                && Some(l.owner()) != except_owner
                && let Some(c) = l.copy()
            {
                *load.entry(c.to_lowercase()).or_default() += 1;
            }
        }
        load
    }

    async fn claim_preferred(
        &self,
        lease: &Arc<SlotLease>,
        request: &SlotRequest,
        down: &[String],
    ) -> Option<(SlotClaimResult, bool)> {
        let usable = |copy: &str| !down.iter().any(|d| d.eq_ignore_ascii_case(copy));
        if let Some(slot) = request.prefer_slot.as_deref().filter(|s| !s.is_empty())
            && usable(&first_char(slot))
        {
            let r = self.claim(lease, request, None, Some(slot), None).await;
            if r.0.ok {
                return Some(r);
            }
        }
        if let Some(copy) = request.prefer_copy.as_deref()
            && usable(copy)
        {
            let r = self.claim(lease, request, Some(copy), None, None).await;
            if r.0.ok {
                return Some(r);
            }
        }
        // Agents go to the least-loaded copy by every lease on it, and a tie never falls to
        // the Lead's copy.
        for copy in self.agent_copy_order(request).await {
            if !usable(&copy) {
                continue;
            }
            let r = self.claim(lease, request, Some(&copy), None, None).await;
            if r.0.ok {
                return Some(r);
            }
        }
        let any = self
            .claim(lease, request, None, None, Some(down.to_vec()))
            .await;
        if !any.0.ok {
            return None;
        }
        // The registry may hand back an existing lease on an avoided copy; give it back.
        let copy = any
            .0
            .copy
            .clone()
            .or_else(|| any.0.slot.as_deref().map(first_char))
            .unwrap_or_default();
        if !usable(&copy) {
            self.release_key(&lease.key, any.1).await;
            return None;
        }
        Some(any)
    }

    /// One claim through the registry, or the local file while the registry is down.
    async fn claim(
        &self,
        lease: &Arc<SlotLease>,
        request: &SlotRequest,
        copy: Option<&str>,
        slot: Option<&str>,
        avoid: Option<Vec<String>>,
    ) -> (SlotClaimResult, bool) {
        let claim = SlotClaim {
            key: lease.key.clone(),
            owner: self.who.clone(),
            host: self.host.clone(),
            pid: self.pid,
            label: request.label.clone(),
            kind: request.kind.clone(),
            copy: copy.map(str::to_string),
            slot: slot.map(str::to_string),
            avoid,
        };
        let _io = self.io.lock().await;
        if let Some(registry) = &self.registry
            && self.registry_up()
        {
            match registry.claim(&claim).await {
                Ok(r) => return (r, true),
                Err(e) => self.mark_registry_down(&e.0),
            }
        }
        match self.local.claim(&claim).await {
            Ok(r) => (r, false),
            Err(e) => {
                tracing::warn!("Local slot file unusable; treating as no free slot: {e}");
                (SlotClaimResult::full(), false)
            }
        }
    }

    async fn release_store(&self, lease: &Arc<SlotLease>) {
        if lease.slot().is_none() {
            return;
        }
        self.release_key(&lease.key, lease.via_registry()).await;
        self.leases_changed();
    }

    async fn release_key(&self, key: &str, via_registry: bool) {
        let _io = self.io.lock().await;
        match (&self.registry, via_registry) {
            (Some(registry), true) => {
                if let Err(e) = registry.release(key).await {
                    // The lease expires by TTL anyway.
                    self.mark_registry_down(&e.0);
                }
            }
            _ => {
                if let Err(e) = self.local.release(key).await {
                    tracing::debug!("Local slot release failed: {e}");
                }
            }
        }
    }

    fn mark_registry_down(&self, error: &str) {
        let now = (self.clock)();
        let mut until = self.registry_down_until.lock().unwrap();
        if until.is_none_or(|u| now >= u) {
            tracing::warn!(
                "Slot registry unreachable ({error}); using the local slot file for {}s",
                REGISTRY_RETRY.as_secs()
            );
        }
        *until = Some(now + REGISTRY_RETRY);
    }

    fn start_heartbeat(&self, ttl: Option<f64>) {
        if let Some(t) = ttl.filter(|t| *t > 0.0) {
            *self.ttl_seconds.lock().unwrap() = t;
        }
        let mut hb = self.heartbeat.lock().unwrap();
        if hb.is_some() {
            return;
        }
        let period = if self.config.heartbeat_seconds > 0 {
            Duration::from_secs(self.config.heartbeat_seconds as u64)
        } else {
            Duration::from_secs_f64((*self.ttl_seconds.lock().unwrap() / 3.0).max(5.0))
        };
        let me = self.me.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        *hb = Some(rt.spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                tick.tick().await;
                let Some(alloc) = me.upgrade() else { return };
                if alloc.heartbeat_running.swap(true, Ordering::SeqCst) {
                    continue;
                }
                alloc.heartbeat().await;
                alloc.heartbeat_running.store(false, Ordering::SeqCst);
            }
        }));
    }

    fn signal(&self) {
        self.freed.notify_waiters();
        self.leases_changed();
    }
}

impl Drop for SlotAllocator {
    fn drop(&mut self) {
        if let Some(h) = self.heartbeat.lock().unwrap().take() {
            h.abort();
        }
    }
}

fn first_char(s: &str) -> String {
    s.chars().take(1).collect()
}
