//! Sticky inference slots (port of `OpenOrca.Core.Slots`): the pig-slots registry client and the
//! local lock-file fallback, the allocator with balanced agent placement, the per-copy model
//! lookup (`CopyModels`), the slot-pinned chat client and the process-wide slot runtime.
//!
//! Sub-agents lease through [`SlotAllocator`]: `acquire(&SlotRequest::agent(id, label), None)`
//! waits for a slot, `try_acquire` never waits, `release` gives it back, and
//! [`SlotRuntime::client_for`] builds the pinned client for a lease.

pub mod allocator;
pub mod client;
pub mod copy_models;
pub mod http;
pub mod local;
pub mod registry;
pub mod runtime;
pub mod testing;

pub use allocator::{
    AGENT_KIND, AllocatorOptions, Clock, LEAD_KIND, SlotAllocator, SlotLease, SlotRequest,
};
pub use client::{
    LeadSampling, SlotClientFactory, SlotDefaults, SlottedChatClient, wire_message_count,
};
pub use copy_models::{CopyModelClient, CopyModels};
pub use http::{HttpSlotHealthProbe, HttpSlotRegistry};
pub use local::LocalSlotRegistry;
pub use registry::{SlotClaim, SlotClaimResult, SlotError, SlotHealthProbe, SlotRegistry};
pub use runtime::{LEAD_OWNER, SlotRuntime};

#[cfg(test)]
mod allocator_tests;
#[cfg(test)]
mod client_tests;
