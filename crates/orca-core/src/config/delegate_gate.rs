//! How many wholly free model copies a hand-off needs (`DelegateGate`): auto mode's delegate
//! choice and peer mode's hand-off (slots.wholeCopyPerAgent). It is
//! agent.minFreeCopiesToDelegate, clamped so it never exceeds the copies that can ever be free
//! for helpers (all copies minus the one the Lead runs on); otherwise a layout like two copies
//! with the Lead on one could never delegate at all. 0 (or less) means no free-copy
//! requirement: a hand-off always stands, and the allocator may put a helper on the Lead's
//! copy when no other copy is free ([`shares_lead_copy`]).

use std::sync::atomic::{AtomicBool, Ordering};

use super::{OrcaConfig, SlotsConfig};

static CLAMP_LOGGED: AtomicBool = AtomicBool::new(false);

/// Copies helpers could ever have to themselves: every copy, less the Lead's when the Lead runs
/// on a slot (slots.lead null) and another copy exists. At least 1.
pub fn helper_copies(slots: &SlotsConfig) -> i32 {
    let copies = slots.copies.len() as i32;
    let lead_reserved = if slots.lead.is_none() && copies > 1 {
        1
    } else {
        0
    };
    (copies - lead_reserved).max(1)
}

/// 0 when agent.minFreeCopiesToDelegate is 0 or less (no requirement), else
/// clamp(agent.minFreeCopiesToDelegate, 1, [`helper_copies`]). Logs one line (once per process)
/// when the clamp lowers the configured value.
pub fn copies_needed(config: &OrcaConfig) -> i32 {
    let configured = config.agent.min_free_copies_to_delegate;
    if configured <= 0 {
        return 0;
    }
    let Some(slots) = &config.slots else {
        return configured;
    };
    let max = helper_copies(slots);
    if configured <= max {
        return configured;
    }
    if !CLAMP_LOGGED.swap(true, Ordering::SeqCst) {
        tracing::info!(
            "agent.minFreeCopiesToDelegate {configured} lowered to {max}: only {max} model copy/copies can ever be free for helpers (the Lead keeps its own)"
        );
    }
    max
}

/// agent.minFreeCopiesToDelegate 0: helpers may also run on the Lead's copy (one per copy with
/// slots.wholeCopyPerAgent, as on every other copy), after the copies the Lead is not on.
pub fn shares_lead_copy(config: &OrcaConfig) -> bool {
    config.agent.min_free_copies_to_delegate <= 0
}
