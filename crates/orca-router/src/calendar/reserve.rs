//! Card reservations on the data plane (SUPERVISOR.md 9.4, SV12): which candidate cards a chat
//! request may use, what happens to other users' leases on a reserved card (`wait` / `drain`),
//! and the `preempt` of others' in-flight streams.
//!
//! - **Placement:** a card held by an event takes no new requests from others; they go to the
//!   other candidate cards, and with none left get `503` "card B reserved by ai-news until
//!   ~08:30" with `Retry-After`. The event's own key (scope `use_reserved`) goes to its
//!   reserved cards first and falls back to free ones.
//! - **Leases:** new claims avoid reserved cards (an explicit one is refused). On renew, others'
//!   leases on the card are not extended (`wait`) or are revoked at once (`drain`, `preempt`).
//! - **Preempt:** `preempt_after` seconds into the reservation, streams from others on the card
//!   end with an SSE error event `preempted_by_reservation`; each one is audited.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use orca_calendar::effects::OnReserve;
use orca_calendar::keys::{KeyAction, KeyDecision, PREFIX};
use tokio::sync::watch;

use super::{CalendarService, now_epoch};
use crate::guards::{Caller, EventScope};

/// A candidate card held by an event, as seen by one request.
#[derive(Debug, Clone, PartialEq)]
pub struct CardHold {
    pub card: String,
    pub entry: String,
    pub until: i64,
    /// "08:30" in the calendar's zone
    pub until_local: String,
    pub on_reserve: OnReserve,
    /// the request's event key may use the card
    pub allowed: bool,
}

impl CardHold {
    pub fn message(&self) -> String {
        format!(
            "card {} reserved by {} until ~{}",
            self.card, self.entry, self.until_local
        )
    }
}

/// What a chat request may do.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatAccess {
    /// A bad, revoked or out-of-window event key, or a scope it lacks.
    Denied(u16, String),
    /// The holds on its candidate cards (empty: nothing reserved).
    Holds(Vec<CardHold>),
}

/// A held card, for the lease table.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseHold {
    pub card: String,
    pub entry: String,
    pub until_local: String,
    pub on_reserve: OnReserve,
}

impl LeaseHold {
    /// A lease made with this event's key (its principal is `event:<entry> key <id>`).
    pub fn owns(&self, principal: Option<&str>) -> bool {
        principal.is_some_and(|p| p.starts_with(&format!("event:{} ", self.entry)))
    }
    pub fn reason(&self) -> String {
        format!("reserved by {} until ~{}", self.entry, self.until_local)
    }
}

impl CalendarService {
    fn local_hhmm(&self, t: i64) -> String {
        orca_calendar::recur::to_local(self.tz(), t)
            .format("%H:%M")
            .to_string()
    }

    /// The holds on `candidates` for a chat to `model` carrying `bearer` (an `abe_` key is
    /// checked and its use audited; any other bearer is an ordinary caller here).
    pub fn chat_access(
        &self,
        bearer: Option<&str>,
        model: &str,
        candidates: &[String],
    ) -> ChatAccess {
        let now = now_epoch();
        let caller = match bearer.filter(|k| k.starts_with(PREFIX)) {
            Some(k) => {
                let cards: Vec<String> = candidates
                    .iter()
                    .filter(|c| !self.safety.excluded_cards.contains(c))
                    .cloned()
                    .collect();
                let action = KeyAction::Chat {
                    route: model.to_string(),
                    cards,
                    model: Some(model.to_string()),
                };
                match self.authorize(k, &action, true) {
                    KeyDecision::Deny { status, reason, .. } => {
                        return ChatAccess::Denied(status, reason);
                    }
                    KeyDecision::Allow { .. } => self.caller(k).unwrap_or_default(),
                }
            }
            None => Caller::default(),
        };
        let st = self.now_state(now);
        ChatAccess::Holds(
            candidates
                .iter()
                .filter_map(|c| {
                    let h = st.holders.get(c)?;
                    Some(CardHold {
                        card: c.clone(),
                        entry: h.entry.clone(),
                        until: h.until,
                        until_local: self.local_hhmm(h.until),
                        on_reserve: h.on_reserve,
                        allowed: caller.grants.iter().any(|g| {
                            g.event == h.entry && g.scopes.contains(&EventScope::UseReserved)
                        }),
                    })
                })
                .collect(),
        )
    }

    /// Every card held now (the lease table's view).
    pub fn lease_holds(&self, now: i64) -> Vec<LeaseHold> {
        self.now_state(now)
            .holders
            .iter()
            .map(|(card, h)| LeaseHold {
                card: card.clone(),
                entry: h.entry.clone(),
                until_local: self.local_hhmm(h.until),
                on_reserve: h.on_reserve,
            })
            .collect()
    }

    /// Fires the preempt of every `preempt` reservation whose `preempt_after` has passed (once
    /// per occurrence and card). Returns the (card, entry) pairs fired.
    pub fn preempt_tick(&self, now: i64) -> Vec<(String, String)> {
        let st = self.now_state(now);
        let mut fired = fired().lock().unwrap();
        let mut out = Vec::new();
        let mut live = HashSet::new();
        for (card, h) in &st.holders {
            if h.on_reserve != OnReserve::Preempt {
                continue;
            }
            let id = format!("{}|{card}", h.occurrence);
            live.insert(id.clone());
            if now >= h.since + h.preempt_after && fired.insert(id) {
                fire(card);
                self.audit(
                    &format!("calendar:{}", h.entry),
                    "calendar.preempt",
                    &format!("card {card}: others' in-flight requests cancelled"),
                );
                out.push((card.clone(), h.entry.clone()));
            }
        }
        fired.retain(|id| live.contains(id));
        out
    }

    /// One request cancelled by a preempt (audited per request, 9.4).
    pub fn preempted(&self, card: &str, model: &str) {
        let st = self.now_state(now_epoch());
        let entry = st
            .holders
            .get(card)
            .map(|h| h.entry.clone())
            .unwrap_or_default();
        self.audit(
            &format!("calendar:{entry}"),
            "calendar.preempt.request",
            &format!("card {card}, model {model}"),
        );
    }
}

/// Seconds since the epoch (the lease table asks the calendar at this time).
pub fn now() -> i64 {
    now_epoch()
}

fn fired() -> &'static Mutex<HashSet<String>> {
    static F: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

fn board() -> &'static Mutex<HashMap<String, watch::Sender<u64>>> {
    static B: OnceLock<Mutex<HashMap<String, watch::Sender<u64>>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// A receiver that changes when `card` is preempted (requests subscribe when they start).
pub fn preempt_watch(card: &str) -> watch::Receiver<u64> {
    let mut b = board().lock().unwrap();
    b.entry(card.to_string())
        .or_insert_with(|| watch::channel(0).0)
        .subscribe()
}

fn fire(card: &str) {
    if let Some(tx) = board().lock().unwrap().get(card) {
        tx.send_modify(|v| *v += 1);
    }
}

/// Runs [`CalendarService::preempt_tick`] every 5 s (started with the calendar).
pub fn spawn_preempt_ticker(svc: std::sync::Arc<CalendarService>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    rt.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            for (card, entry) in svc.preempt_tick(now_epoch()) {
                tracing::warn!(
                    "calendar: {entry} preempts card {card}: others' requests cancelled"
                );
            }
        }
    });
}
