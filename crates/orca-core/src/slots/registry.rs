//! The pig-slots lease protocol (`ISlotRegistry`, `ISlotOccupancy`, `ISlotHealthProbe`).

use async_trait::async_trait;

/// A registry or health failure (unreachable, non-2xx, unreadable answer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SlotError(pub String);

/// A lease request. Exactly one of `slot` (that slot only), `copy` (any free slot on that copy)
/// or neither (least-loaded copy, ties to the first) narrows the choice; `avoid` excludes copies.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SlotClaim {
    pub key: String,
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub label: String,
    pub kind: String,
    pub copy: Option<String>,
    pub slot: Option<String>,
    pub avoid: Option<Vec<String>>,
}

/// Answer to a claim: the slot and its copy, or `ok == false` with a reason ("full").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SlotClaimResult {
    pub ok: bool,
    pub slot: Option<String>,
    pub copy: Option<String>,
    pub ttl: Option<f64>,
    pub reason: Option<String>,
}

impl SlotClaimResult {
    pub fn full() -> Self {
        Self {
            ok: false,
            reason: Some("full".into()),
            ..Self::default()
        }
    }

    pub fn granted(slot: &str, copy: &str, ttl: Option<f64>) -> Self {
        Self {
            ok: true,
            slot: Some(slot.to_string()),
            copy: Some(copy.to_string()),
            ttl,
            reason: None,
        }
    }
}

/// Where leases live: the central pig-slots service or the local lock file. Implementations
/// fail when the store is unreachable; "no free slot" is a normal [`SlotClaimResult`].
#[async_trait]
pub trait SlotRegistry: Send + Sync {
    /// Claim a slot. Re-claiming a key that already holds one returns that slot.
    async fn claim(&self, claim: &SlotClaim) -> Result<SlotClaimResult, SlotError>;

    /// Renew leases; returns the keys the store no longer knows (expired).
    async fn renew(&self, keys: &[String]) -> Result<Vec<String>, SlotError>;

    async fn release(&self, key: &str) -> Result<(), SlotError>;

    /// Release every lease this host/process holds (shutdown).
    async fn release_all(&self, host: &str, pid: u32) -> Result<(), SlotError>;

    /// `ISlotOccupancy`: the slots holding a lease from anyone (pig-slots GET /list).
    /// `Ok(None)` for a store that can't list them.
    async fn busy_slots(&self) -> Result<Option<Vec<String>>, SlotError> {
        Ok(None)
    }
}

/// Is a copy up? (GET {copy}/health in production.)
#[async_trait]
pub trait SlotHealthProbe: Send + Sync {
    async fn is_healthy(&self, copy: &str) -> bool;
}
