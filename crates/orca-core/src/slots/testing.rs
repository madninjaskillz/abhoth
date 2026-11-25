//! In-memory registry and health probe for tests (port of TestKit's `FakeSlotRegistry` /
//! `FakeSlotHealth`), public so other crates' tests (orchestration, the CLI) can use them.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use super::registry::{SlotClaim, SlotClaimResult, SlotError, SlotHealthProbe, SlotRegistry};

#[derive(Debug, Clone)]
struct Held {
    key: String,
    label: String,
    pid: u32,
}

/// pig-slots semantics in memory: a key re-claims its own slot; "slot" takes only that slot,
/// "copy" any free slot on that copy, otherwise the least-loaded copy wins with ties to the
/// first; "avoid" excludes copies. `set_down(true)` makes every call fail; `expire` drops a
/// lease as a TTL would.
pub struct FakeSlotRegistry {
    slots: Vec<String>,
    held: Mutex<BTreeMap<String, Held>>,
    down: AtomicBool,
    pub ttl: f64,
    claims: AtomicUsize,
    renewals: AtomicUsize,
    /// `busy_slots` fails while claims still work.
    pub list_broken: AtomicBool,
}

fn copy_of(slot: &str) -> String {
    slot.chars().take(1).collect()
}

impl Default for FakeSlotRegistry {
    fn default() -> Self {
        Self::new(&[])
    }
}

impl FakeSlotRegistry {
    /// The registry's slots in order (default A1, A2, B1, B2).
    pub fn new(slots: &[&str]) -> Self {
        let slots = if slots.is_empty() {
            vec!["A1", "A2", "B1", "B2"]
        } else {
            slots.to_vec()
        };
        Self {
            slots: slots.into_iter().map(String::from).collect(),
            held: Mutex::new(BTreeMap::new()),
            down: AtomicBool::new(false),
            ttl: 90.0,
            claims: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
            list_broken: AtomicBool::new(false),
        }
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    pub fn claims(&self) -> usize {
        self.claims.load(Ordering::SeqCst)
    }

    pub fn renewals(&self) -> usize {
        self.renewals.load(Ordering::SeqCst)
    }

    /// slot -> key, for assertions.
    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.held
            .lock()
            .unwrap()
            .iter()
            .map(|(s, h)| (s.clone(), h.key.clone()))
            .collect()
    }

    pub fn label_of(&self, slot: &str) -> Option<String> {
        self.held.lock().unwrap().get(slot).map(|h| h.label.clone())
    }

    /// Hold a slot for someone else (another tool / user).
    pub fn occupy(&self, slot: &str) {
        self.occupy_as(slot, "someone-else");
    }

    pub fn occupy_as(&self, slot: &str, key: &str) {
        self.held.lock().unwrap().insert(
            slot.into(),
            Held {
                key: key.into(),
                label: "other".into(),
                pid: 1,
            },
        );
    }

    pub fn expire(&self, key: &str) {
        self.held.lock().unwrap().retain(|_, h| h.key != key);
    }

    pub fn free(&self, slot: &str) {
        self.held.lock().unwrap().remove(slot);
    }

    fn check(&self) -> Result<(), SlotError> {
        if self.down.load(Ordering::SeqCst) {
            Err(SlotError("registry unreachable (fake)".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl SlotRegistry for FakeSlotRegistry {
    async fn claim(&self, claim: &SlotClaim) -> Result<SlotClaimResult, SlotError> {
        self.check()?;
        let mut held = self.held.lock().unwrap();
        self.claims.fetch_add(1, Ordering::SeqCst);
        if let Some((slot, _)) = held.iter().find(|(_, h)| h.key == claim.key) {
            return Ok(SlotClaimResult::granted(
                slot,
                &copy_of(slot),
                Some(self.ttl),
            ));
        }
        let pick = if let Some(slot) = &claim.slot {
            (self.slots.contains(slot) && !held.contains_key(slot)).then(|| slot.clone())
        } else if let Some(copy) = &claim.copy {
            self.slots
                .iter()
                .find(|s| &copy_of(s) == copy && !held.contains_key(*s))
                .cloned()
        } else {
            let avoid = claim.avoid.clone().unwrap_or_default();
            let load = |c: &str| held.keys().filter(|s| copy_of(s) == c).count();
            let mut free: Vec<&String> = self
                .slots
                .iter()
                .filter(|s| !held.contains_key(*s) && !avoid.contains(&copy_of(s)))
                .collect();
            free.sort_by(|a, b| load(&copy_of(a)).cmp(&load(&copy_of(b))).then(a.cmp(b)));
            free.first().map(|s| (*s).clone())
        };
        let Some(pick) = pick else {
            return Ok(SlotClaimResult::full());
        };
        held.insert(
            pick.clone(),
            Held {
                key: claim.key.clone(),
                label: claim.label.clone(),
                pid: claim.pid,
            },
        );
        Ok(SlotClaimResult::granted(
            &pick,
            &copy_of(&pick),
            Some(self.ttl),
        ))
    }

    async fn renew(&self, keys: &[String]) -> Result<Vec<String>, SlotError> {
        self.check()?;
        let held = self.held.lock().unwrap();
        self.renewals.fetch_add(1, Ordering::SeqCst);
        let present: HashSet<&str> = held.values().map(|h| h.key.as_str()).collect();
        Ok(keys
            .iter()
            .filter(|k| !present.contains(k.as_str()))
            .cloned()
            .collect())
    }

    async fn release(&self, key: &str) -> Result<(), SlotError> {
        self.check()?;
        self.expire(key);
        Ok(())
    }

    async fn release_all(&self, _host: &str, pid: u32) -> Result<(), SlotError> {
        self.check()?;
        self.held.lock().unwrap().retain(|_, h| h.pid != pid);
        Ok(())
    }

    async fn busy_slots(&self) -> Result<Option<Vec<String>>, SlotError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(SlotError("registry down".into()));
        }
        if self.list_broken.load(Ordering::SeqCst) {
            return Err(SlotError("list broken".into()));
        }
        Ok(Some(self.held.lock().unwrap().keys().cloned().collect()))
    }
}

/// Health probe with a settable set of down copies.
#[derive(Default)]
pub struct FakeSlotHealth {
    down: Mutex<HashSet<String>>,
}

impl FakeSlotHealth {
    pub fn set_down(&self, copy: &str) {
        self.down.lock().unwrap().insert(copy.to_lowercase());
    }

    pub fn set_up(&self, copy: &str) {
        self.down.lock().unwrap().remove(&copy.to_lowercase());
    }
}

#[async_trait]
impl SlotHealthProbe for FakeSlotHealth {
    async fn is_healthy(&self, copy: &str) -> bool {
        !self.down.lock().unwrap().contains(&copy.to_lowercase())
    }
}
