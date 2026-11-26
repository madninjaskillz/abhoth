//! What the calendar does at one moment (SUPERVISOR.md 8.2, 8.4): the blocks of every active
//! occurrence (a union), who holds each reserved card (higher priority, then whoever started
//! first), slot reserves, clock holds, profiles (the more conservative one wins), muted alert
//! rules, and which events an event key may lift (its own, plus lower-priority ones when its
//! policy says `overrides_lower`).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::effects::{Effect, OnReserve, Target};
use crate::live::Live;
use crate::plan::{Occurrence, Overrides};
use crate::{Calendar, Priority, recur};

/// What a calendar block stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockAction {
    Switch,
    OnDemand,
    Ensure,
    /// placing requests on a card (a `reserve_card` held by this event)
    Use,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalBlock {
    pub entry: String,
    pub occurrence: String,
    pub action: BlockAction,
    pub target: Target,
    pub until: i64,
    pub priority: Priority,
    pub reason: String,
    /// `reserve_card` only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_reserve: Option<OnReserve>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preempt_after: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Holder {
    pub entry: String,
    pub occurrence: String,
    pub priority: Priority,
    pub since: i64,
    pub until: i64,
    pub on_reserve: OnReserve,
    /// other active events that also reserve this card (their keys get 409 here)
    pub contenders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlotReserve {
    pub entry: String,
    pub target: Target,
    pub n: u32,
    pub until: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfileClaim {
    pub entry: String,
    pub profile: String,
    pub until: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NowState {
    pub at: i64,
    pub active: Vec<Occurrence>,
    pub blocks: Vec<CalBlock>,
    /// card -> holder
    pub holders: BTreeMap<String, Holder>,
    pub slot_reserves: Vec<SlotReserve>,
    /// targets with clock holds, and until when
    pub holds: Vec<(Target, String, i64)>,
    /// clock profiles per target text, every claim (pick with [`conservative`])
    pub clock_profiles: BTreeMap<String, Vec<ProfileClaim>>,
    /// fan profiles per fan (`*` = all fans)
    pub fan_profiles: BTreeMap<String, Vec<ProfileClaim>>,
    /// (selector, entry, until)
    pub mutes: Vec<(String, String, i64)>,
}

/// The most conservative claim: highest `rank` (cooling beats quiet: the higher fan duty, the
/// higher clock cap). Unknown profiles rank first so nothing unknown can lower cooling.
pub fn conservative(
    claims: &[ProfileClaim],
    rank: impl Fn(&str) -> Option<i64>,
) -> Option<&ProfileClaim> {
    claims.iter().max_by_key(|c| {
        (
            rank(&c.profile).unwrap_or(i64::MAX),
            std::cmp::Reverse(c.entry.clone()),
        )
    })
}

fn hhmm(cal: &Calendar, t: i64) -> String {
    recur::to_local(cal.tz, t).format("%H:%M").to_string()
}

impl Calendar {
    /// The state at `now`.
    pub fn now(&self, now: i64, live: &Live, ov: &Overrides) -> NowState {
        let active: Vec<Occurrence> = self
            .occurrences(now, now + 1, live, ov, now)
            .into_iter()
            .filter(|o| o.on(now))
            .collect();
        let mut st = NowState {
            at: now,
            ..Default::default()
        };
        // Card holders first (8.4): priority, then the earlier start, then the id.
        let mut claims: BTreeMap<String, Vec<(&Occurrence, OnReserve, i64)>> = BTreeMap::new();
        for o in &active {
            let Some(e) = self.entries.get(&o.entry) else {
                continue;
            };
            for eff in &e.effects {
                if let Effect::ReserveCard {
                    card,
                    on_reserve,
                    preempt_after,
                } = eff
                {
                    claims
                        .entry(card.clone())
                        .or_default()
                        .push((o, *on_reserve, *preempt_after));
                }
            }
        }
        let mut preempt: BTreeMap<String, i64> = BTreeMap::new();
        for (card, mut v) in claims {
            v.sort_by(|a, b| {
                b.0.priority
                    .cmp(&a.0.priority)
                    .then(a.0.block_start.cmp(&b.0.block_start))
                    .then(a.0.entry.cmp(&b.0.entry))
            });
            let (o, on_reserve, pa) = v[0];
            preempt.insert(card.clone(), pa);
            st.holders.insert(
                card,
                Holder {
                    entry: o.entry.clone(),
                    occurrence: o.id.clone(),
                    priority: o.priority,
                    since: o.block_start,
                    until: o.block_end,
                    on_reserve,
                    contenders: v[1..].iter().map(|x| x.0.entry.clone()).collect(),
                },
            );
        }
        for o in &active {
            let Some(e) = self.entries.get(&o.entry) else {
                continue;
            };
            let until = o.block_end;
            let why = |what: &str| format!("{what}: {} until ~{}", e.cfg.title, hhmm(self, until));
            let block = |action, target: &Target, reason: String| CalBlock {
                entry: o.entry.clone(),
                occurrence: o.id.clone(),
                action,
                target: target.clone(),
                until,
                priority: o.priority,
                reason,
                on_reserve: None,
                preempt_after: None,
            };
            for eff in &e.effects {
                match eff {
                    Effect::BlockSwitch { target } => {
                        st.blocks
                            .push(block(BlockAction::Switch, target, why("no switches")))
                    }
                    Effect::BlockOnDemand { target } => st.blocks.push(block(
                        BlockAction::OnDemand,
                        target,
                        why("no on-demand switches"),
                    )),
                    Effect::BlockEnsure { target } => {
                        st.blocks
                            .push(block(BlockAction::Ensure, target, why("no engine starts")))
                    }
                    Effect::ReserveCard { card, .. } => {
                        if let Some(h) = st.holders.get(card).filter(|h| h.occurrence == o.id) {
                            let mut b = block(
                                BlockAction::Use,
                                &Target::Card(card.clone()),
                                format!(
                                    "card {card} reserved by {} until ~{}",
                                    o.entry,
                                    hhmm(self, until)
                                ),
                            );
                            b.on_reserve = Some(h.on_reserve);
                            b.preempt_after = preempt.get(card).copied();
                            st.blocks.push(b);
                        }
                    }
                    Effect::ReserveSlots { target, n } => st.slot_reserves.push(SlotReserve {
                        entry: o.entry.clone(),
                        target: target.clone(),
                        n: *n,
                        until,
                    }),
                    Effect::HoldClocks { target } => {
                        st.holds.push((target.clone(), o.entry.clone(), until))
                    }
                    Effect::ClockProfile { profile, target } => st
                        .clock_profiles
                        .entry(target.text())
                        .or_default()
                        .push(ProfileClaim {
                            entry: o.entry.clone(),
                            profile: profile.clone(),
                            until,
                        }),
                    Effect::FanProfile { profile, fan } => st
                        .fan_profiles
                        .entry(fan.clone().unwrap_or_else(|| "*".into()))
                        .or_default()
                        .push(ProfileClaim {
                            entry: o.entry.clone(),
                            profile: profile.clone(),
                            until,
                        }),
                    Effect::MuteAlerts { rules } => {
                        st.mutes.push((rules.clone(), o.entry.clone(), until))
                    }
                    Effect::Info => {}
                }
            }
        }
        st.active = active;
        st
    }

    /// The events whose blocks and reservations a key of `event` may lift now: its own, and
    /// with `overrides_lower` the active events of strictly lower priority (9.3).
    pub fn liftable(&self, event: &str, st: &NowState) -> Vec<String> {
        let mut out = vec![event.to_string()];
        let Some(e) = self.entries.get(event) else {
            return out;
        };
        if !e.key.as_ref().is_some_and(|k| k.overrides_lower) {
            return out;
        }
        let mine = st
            .active
            .iter()
            .filter(|o| o.entry == event)
            .map(|o| o.priority)
            .max()
            .unwrap_or(e.priority());
        for o in &st.active {
            if o.priority < mine && !out.contains(&o.entry) {
                out.push(o.entry.clone());
            }
        }
        out
    }

    /// Is an alert rule muted now? `critical` is never muted (and the UI never is).
    pub fn muted(&self, st: &NowState, rule: &str, critical: bool) -> Option<String> {
        if critical {
            return None;
        }
        st.mutes
            .iter()
            .find(|(sel, _, _)| crate::effects::selector_matches(sel, rule))
            .map(|(_, e, _)| e.clone())
    }

    /// Pairs of entries that reserve the same card at overlapping times in `[from, to)`
    /// (the save warning and the `calendar_overlap` alert). One line per pair and card, at
    /// the first overlap.
    pub fn overlaps(&self, from: i64, to: i64) -> Vec<Overlap> {
        let none = Live::default();
        let ov = Overrides::default();
        let occ = self.occurrences(from, to, &none, &ov, from);
        let mut out: BTreeMap<(String, String, String), Overlap> = BTreeMap::new();
        let cards = |o: &Occurrence| {
            self.entries
                .get(&o.entry)
                .map(|e| e.reserved_cards())
                .unwrap_or_default()
        };
        for (i, a) in occ.iter().enumerate() {
            let ca = cards(a);
            if ca.is_empty() {
                continue;
            }
            for b in &occ[i + 1..] {
                if a.entry == b.entry
                    || b.block_start >= a.block_end
                    || a.block_start >= b.block_end
                {
                    continue;
                }
                for c in cards(b).into_iter().filter(|c| ca.contains(c)) {
                    let (x, y) = if a.entry < b.entry { (a, b) } else { (b, a) };
                    let key = (x.entry.clone(), y.entry.clone(), c.clone());
                    out.entry(key).or_insert_with(|| {
                        let at = a.block_start.max(b.block_start);
                        let until = a.block_end.min(b.block_end);
                        let local = recur::to_local(self.tz, at);
                        Overlap {
                            a: x.entry.clone(),
                            b: y.entry.clone(),
                            card: c.clone(),
                            at,
                            until,
                            equal_priority: x.priority == y.priority,
                            message: format!(
                                "{} and {} both reserve {} on {} {}-{}{}",
                                x.entry,
                                y.entry,
                                c,
                                local.format("%a %Y-%m-%d"),
                                local.format("%H:%M"),
                                hhmm(self, until),
                                if x.priority == y.priority {
                                    " at equal priority: the one that starts first keeps the card; set a priority"
                                } else {
                                    ""
                                }
                            ),
                        }
                    });
                }
            }
        }
        out.into_values().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overlap {
    pub a: String,
    pub b: String,
    pub card: String,
    pub at: i64,
    pub until: i64,
    pub equal_priority: bool,
    pub message: String,
}
