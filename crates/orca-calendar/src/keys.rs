//! Event keys (SUPERVISOR.md 9): an event's own API key, `abe_…`. Stored like D140 keys (only
//! the SHA-256, shown once, revocable, rotatable). A key works **only** while an occurrence of
//! its event blocks (lead, active, extended, tail) or within `grace_before` / `grace_after` of
//! one; outside that it is refused with 403 and never falls back to user permissions.
//!
//! Scopes are least privilege (default `chat` + `use_reserved`). Nothing here can let a key:
//! start a switch, an engine or on-demand from a probe; touch excluded cards or models; skip
//! budgets (use is charged to the owner); act outside its window or after its owner is
//! disabled; or change thermal safety (fan profiles and clock holds still go through hostd's
//! limits). Those checks are structural: [`authorize`] runs them before any scope.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::effects::Effect;
use crate::entry::Entry;
use crate::live::Live;
use crate::now::NowState;
use crate::plan::{Occurrence, Overrides};
use crate::{Calendar, Issue, Priority, Safety, recur};

pub const PREFIX: &str = "abe_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScope {
    /// chat / images / music on the event's routes
    Chat,
    /// placement on the event's reserved cards and lanes
    UseReserved,
    /// slot leases on reserved cards
    Lease,
    /// layout switches despite `block_switch`, only to `layouts`
    Switch,
    /// offline / online for the reserved cards
    CardState,
    /// clock holds on the reserved cards (TTL ≤ the occurrence's end)
    HoldClocks,
    /// a fan profile among the configured ones (never below safety)
    FanProfile,
    /// on-demand switches for the event's routes, within its cards
    OnDemand,
}

impl KeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyScope::Chat => "chat",
            KeyScope::UseReserved => "use_reserved",
            KeyScope::Lease => "lease",
            KeyScope::Switch => "switch",
            KeyScope::CardState => "card_state",
            KeyScope::HoldClocks => "hold_clocks",
            KeyScope::FanProfile => "fan_profile",
            KeyScope::OnDemand => "on_demand",
        }
    }

    pub const ALL: [KeyScope; 8] = [
        KeyScope::Chat,
        KeyScope::UseReserved,
        KeyScope::Lease,
        KeyScope::Switch,
        KeyScope::CardState,
        KeyScope::HoldClocks,
        KeyScope::FanProfile,
        KeyScope::OnDemand,
    ];
}

fn d_scopes() -> Vec<KeyScope> {
    vec![KeyScope::Chat, KeyScope::UseReserved]
}
fn d_before() -> String {
    "5m".into()
}
fn d_after() -> String {
    "15m".into()
}

/// `[calendar.entries.<id>.key]`: the key's policy. The key itself is in the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicy {
    #[serde(default = "d_scopes")]
    pub scopes: Vec<KeyScope>,
    /// routes for `chat` / `on_demand` (empty: the routes whose targets include the reserved cards)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    /// layouts `switch` may go to (empty: no switch at all)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layouts: Vec<String>,
    /// `switch` to any layout (the admin ticked "all layouts"; shown with a warning)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_layouts: bool,
    #[serde(default = "d_before")]
    pub grace_before: String,
    #[serde(default = "d_after")]
    pub grace_after: String,
    /// also lift blocks and reservations of lower-priority events (warning)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overrides_lower: bool,
}

impl Default for KeyPolicy {
    fn default() -> Self {
        Self {
            scopes: d_scopes(),
            routes: vec![],
            layouts: vec![],
            all_layouts: false,
            grace_before: d_before(),
            grace_after: d_after(),
            overrides_lower: false,
        }
    }
}

impl KeyPolicy {
    pub fn has(&self, s: KeyScope) -> bool {
        self.scopes.contains(&s)
    }
    pub fn grace(&self) -> (i64, i64) {
        (
            crate::dur::parse(&self.grace_before).unwrap_or(300),
            crate::dur::parse(&self.grace_after).unwrap_or(900),
        )
    }
}

/// Checks on a key policy when the calendar is compiled.
pub fn validate_policy(
    id: &str,
    k: &KeyPolicy,
    e: &Entry,
    safety: &Safety,
    issues: &mut Vec<Issue>,
) {
    let at = format!("calendar.entries.{id}.key");
    for (f, v) in [
        ("grace_before", &k.grace_before),
        ("grace_after", &k.grace_after),
    ] {
        match crate::dur::parse(v) {
            Ok(x) if x > 6 * 3600 => issues.push(Issue::error(
                &format!("{at}.{f}"),
                "grace is at most 6h (a key should be useless most of the day)".into(),
            )),
            Ok(_) => {}
            Err(m) => issues.push(Issue::error(&format!("{at}.{f}"), m)),
        }
    }
    if k.has(KeyScope::Switch) {
        if k.all_layouts {
            issues.push(Issue::warning(
                &at,
                "switch to all layouts: this key can change every card's layout during the event"
                    .into(),
            ));
        } else if k.layouts.is_empty() {
            issues.push(Issue::warning(
                &at,
                "switch is granted but layouts is empty, so it allows no switch".into(),
            ));
        }
        for l in &k.layouts {
            if safety.unsafe_layouts.contains(l) {
                issues.push(Issue::error(
                    &format!("{at}.layouts"),
                    format!("layout '{l}' touches an excluded card or model; no event key may switch to it"),
                ));
            }
        }
    } else if !k.layouts.is_empty() || k.all_layouts {
        issues.push(Issue::warning(
            &format!("{at}.layouts"),
            "layouts are listed but the switch scope is not granted".into(),
        ));
    }
    if k.overrides_lower {
        issues.push(Issue::warning(
            &at,
            "overrides_lower: this key also lifts blocks and reservations of lower-priority events"
                .into(),
        ));
    }
    let reserved = e.reserved_cards();
    let needs_cards = [
        KeyScope::UseReserved,
        KeyScope::Lease,
        KeyScope::CardState,
        KeyScope::HoldClocks,
    ];
    if reserved.is_empty()
        && k.scopes.iter().any(|s| needs_cards.contains(s))
        && !e
            .effects
            .iter()
            .any(|x| matches!(x, Effect::ReserveSlots { .. }))
    {
        issues.push(Issue::warning(
            &at,
            "the key has card scopes but the event reserves no card (they allow nothing yet)"
                .into(),
        ));
    }
}

// ── the key itself ────────────────────────────────────────────────────────────────────────────

/// A freshly minted key: the secret (shown once) and what is stored.
#[derive(Debug, Clone)]
pub struct NewKey {
    pub secret: String,
    /// the first 12 characters, for lists and logs
    pub prefix: String,
    /// hex SHA-256 of the secret
    pub hash: String,
}

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A new `abe_` key with 190 bits of randomness.
pub fn generate() -> Result<NewKey, String> {
    let mut raw = [0u8; 40];
    getrandom::fill(&mut raw).map_err(|e| format!("no randomness: {e}"))?;
    // 62^32 ≈ 2^190; the modulo bias over a u8 is negligible for this purpose but we reject
    // bytes ≥ 248 anyway (248 = 4 × 62) to keep it exact.
    let mut s = String::from(PREFIX);
    let mut pool = raw.to_vec();
    while s.len() < PREFIX.len() + 32 {
        if pool.is_empty() {
            let mut more = [0u8; 16];
            getrandom::fill(&mut more).map_err(|e| format!("no randomness: {e}"))?;
            pool.extend_from_slice(&more);
        }
        let b = pool.remove(0);
        if b < 248 {
            s.push(ALPHABET[(b % 62) as usize] as char);
        }
    }
    Ok(NewKey {
        prefix: s[..12].to_string(),
        hash: hash(&s),
        secret: s,
    })
}

/// Hex SHA-256 of a key (what the database stores).
pub fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn looks_like_event_key(s: &str) -> bool {
    s.starts_with(PREFIX)
        && s.len() == PREFIX.len() + 32
        && s[PREFIX.len()..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A stored key (from `router.db`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    pub id: i64,
    pub event: String,
    pub prefix: String,
    pub owner: String,
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked: Option<i64>,
    /// after a rotation: the old key stops at this time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retire_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
}

impl KeyRecord {
    pub fn live(&self, now: i64) -> bool {
        self.revoked.is_none() && self.retire_at.is_none_or(|t| now < t)
    }
}

/// When the old key stops after a rotation at `now` (9.7): the end of the current or next
/// occurrence, or 24 h, whichever is sooner, so a running pipeline is not cut off.
pub fn retire_time(cal: &Calendar, event: &str, now: i64, live: &Live, ov: &Overrides) -> i64 {
    let day = now + 86_400;
    let (_, after) = cal
        .entries
        .get(event)
        .and_then(|e| e.key.as_ref())
        .map(KeyPolicy::grace)
        .unwrap_or((300, 900));
    cal.next_of(event, now, 1, live, ov)
        .first()
        .map(|o| (o.block_end + after).min(day))
        .unwrap_or(day)
}

// ── authorization ─────────────────────────────────────────────────────────────────────────────

/// What a request with an event key wants to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// a chat/images/music request on `route`; `cards` = the route's target cards (if known)
    Chat {
        route: String,
        cards: Vec<String>,
        model: Option<String>,
    },
    /// placement on `card` (the router asks this per candidate card)
    UseCard {
        card: String,
    },
    Lease {
        card: String,
    },
    /// `probe`: the request had no chat body, was a GET, or carried `x-abhoth-no-switch`
    Switch {
        layout: String,
        probe: bool,
    },
    CardState {
        card: String,
        online: bool,
    },
    HoldClocks {
        card: String,
        ttl: i64,
    },
    FanProfile {
        profile: String,
    },
    OnDemand {
        route: String,
        card: String,
        probe: bool,
    },
}

impl KeyAction {
    pub fn name(&self) -> &'static str {
        match self {
            KeyAction::Chat { .. } => "chat",
            KeyAction::UseCard { .. } => "use_card",
            KeyAction::Lease { .. } => "lease",
            KeyAction::Switch { .. } => "switch",
            KeyAction::CardState { .. } => "card_state",
            KeyAction::HoldClocks { .. } => "hold_clocks",
            KeyAction::FanProfile { .. } => "fan_profile",
            KeyAction::OnDemand { .. } => "on_demand",
        }
    }

    fn cards(&self) -> Vec<&str> {
        match self {
            KeyAction::Chat { cards, .. } => cards.iter().map(String::as_str).collect(),
            KeyAction::UseCard { card }
            | KeyAction::Lease { card }
            | KeyAction::CardState { card, .. }
            | KeyAction::HoldClocks { card, .. }
            | KeyAction::OnDemand { card, .. } => vec![card.as_str()],
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum KeyDecision {
    Allow {
        event: String,
        occurrence: String,
        /// the events whose calendar blocks this request may pass (own + lower with overrides_lower)
        lifts: Vec<String>,
        /// the key's grants end here (holds and TTLs are clipped to it)
        until: i64,
        /// budgets: usage is charged to the event's owner, tagged with the event
        charge_to: String,
        /// hold TTL after clipping (`hold_clocks` only)
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<i64>,
        /// the action still needs hostd's limits check (fan profiles, holds)
        hostd_validates: bool,
    },
    Deny {
        status: u16,
        reason: String,
        /// the next time the key works (outside-window denials)
        #[serde(skip_serializing_if = "Option::is_none")]
        next: Option<i64>,
    },
}

fn deny(status: u16, reason: impl Into<String>) -> KeyDecision {
    KeyDecision::Deny {
        status,
        reason: reason.into(),
        next: None,
    }
}

/// The occurrence of `event` whose key window holds `now`, else the next window's start.
pub fn window(
    cal: &Calendar,
    event: &str,
    now: i64,
    live: &Live,
    ov: &Overrides,
) -> Result<Occurrence, Option<i64>> {
    let Some(e) = cal.entries.get(event) else {
        return Err(None);
    };
    let (before, after) = e.key.as_ref().map(KeyPolicy::grace).unwrap_or((300, 900));
    let occ = cal.occurrences(now - after - 1, now + before + 1, live, ov, now);
    if let Some(o) = occ
        .into_iter()
        .filter(|o| o.entry == event && !o.skipped)
        .filter(|o| now >= o.block_start - before && now < o.block_end + after)
        .min_by_key(|o| o.block_start)
    {
        return Ok(o);
    }
    Err(cal
        .next_of(event, now, 1, live, ov)
        .into_iter()
        .find(|o| !o.skipped)
        .map(|o| o.block_start - before))
}

/// May a request with `key` do `action` now? `owner_enabled`: the D140 owner is still enabled.
#[allow(clippy::too_many_arguments)]
pub fn authorize(
    cal: &Calendar,
    st: &NowState,
    live: &Live,
    ov: &Overrides,
    key: &KeyRecord,
    owner_enabled: bool,
    action: &KeyAction,
    safety: &Safety,
    now: i64,
) -> KeyDecision {
    // 1. The key itself.
    if !key.live(now) {
        return deny(401, "event key revoked");
    }
    if !owner_enabled {
        return deny(403, "the event key's owner is disabled");
    }
    let Some(e) = cal.entries.get(&key.event) else {
        return deny(403, format!("event '{}' no longer exists", key.event));
    };
    if !e.cfg.enabled {
        return deny(403, format!("event '{}' is disabled", key.event));
    }
    // 2. Structural limits (9.5), before any scope.
    match action {
        KeyAction::Switch { probe: true, .. } | KeyAction::OnDemand { probe: true, .. } => {
            return deny(
                403,
                "a probe (GET, no chat body, or x-abhoth-no-switch) never starts a switch, an engine or on-demand",
            );
        }
        _ => {}
    }
    if let Some(c) = action
        .cards()
        .into_iter()
        .find(|c| safety.excluded_cards.iter().any(|x| x == c))
    {
        return deny(403, format!("card {c} is excluded from the control plane"));
    }
    if let KeyAction::Chat { model: Some(m), .. } = action
        && safety.excluded_models.contains(m)
    {
        return deny(403, format!("model {m} is excluded"));
    }
    if let KeyAction::Switch { layout, .. } = action
        && safety.unsafe_layouts.contains(layout)
    {
        return deny(
            403,
            format!("layout {layout} touches an excluded card or model"),
        );
    }
    // 3. The window.
    let occ = match window(cal, &key.event, now, live, ov) {
        Ok(o) => o,
        Err(next) => {
            let when = next
                .map(|t| {
                    format!(
                        " (next: {})",
                        recur::to_local(cal.tz, t).format("%Y-%m-%d %H:%M")
                    )
                })
                .unwrap_or_default();
            return KeyDecision::Deny {
                status: 403,
                reason: format!("event key outside its window{when}"),
                next,
            };
        }
    };
    let policy = e.key.clone().unwrap_or_default();
    let (_, after) = policy.grace();
    let until = occ.block_end + after;
    let reserved = e.reserved_cards();
    let lifts = cal.liftable(&key.event, st);
    // 4. Scopes.
    let need = |s: KeyScope| -> Result<(), KeyDecision> {
        if policy.has(s) {
            Ok(())
        } else {
            Err(deny(
                403,
                format!(
                    "the event key for {} does not have the {} scope",
                    key.event,
                    s.as_str()
                ),
            ))
        }
    };
    // A reserved card held by another event: 409 (8.4), unless the key may lift that event.
    let held = |card: &str| -> Result<(), KeyDecision> {
        match st.holders.get(card) {
            Some(h) if !lifts.contains(&h.entry) => Err(deny(
                409,
                format!(
                    "card {card} reserved by {} until ~{}",
                    h.entry,
                    recur::to_local(cal.tz, h.until).format("%H:%M")
                ),
            )),
            _ => Ok(()),
        }
    };
    let own_card = |card: &str| -> Result<(), KeyDecision> {
        if reserved.iter().any(|c| c == card) {
            held(card)
        } else {
            Err(deny(
                403,
                format!("card {card} is not reserved by {}", key.event),
            ))
        }
    };
    let route_ok = |route: &str, cards: &[String]| {
        if policy.routes.is_empty() {
            // Default: routes whose targets include the reserved cards.
            cards.is_empty() || cards.iter().any(|c| reserved.contains(c))
        } else {
            policy.routes.iter().any(|r| r == route)
        }
    };
    let mut ttl = None;
    let mut hostd = false;
    let r: Result<(), KeyDecision> = (|| match action {
        KeyAction::Chat { route, cards, .. } => {
            need(KeyScope::Chat)?;
            if !route_ok(route, cards) {
                return Err(deny(
                    403,
                    format!("route {route} is not one of the event's routes"),
                ));
            }
            Ok(())
        }
        KeyAction::UseCard { card } => {
            need(KeyScope::UseReserved)?;
            own_card(card)
        }
        KeyAction::Lease { card } => {
            need(KeyScope::Lease)?;
            own_card(card)
        }
        KeyAction::Switch { layout, .. } => {
            need(KeyScope::Switch)?;
            if policy.all_layouts || policy.layouts.iter().any(|l| l == layout) {
                Ok(())
            } else {
                Err(deny(
                    403,
                    format!("the event key may not switch to layout {layout}"),
                ))
            }
        }
        KeyAction::CardState { card, .. } => {
            need(KeyScope::CardState)?;
            own_card(card)
        }
        KeyAction::HoldClocks { card, ttl: want } => {
            need(KeyScope::HoldClocks)?;
            own_card(card)?;
            ttl = Some((*want).min(until - now).max(0));
            hostd = true;
            Ok(())
        }
        KeyAction::FanProfile { .. } => {
            need(KeyScope::FanProfile)?;
            hostd = true;
            Ok(())
        }
        KeyAction::OnDemand { route, card, .. } => {
            need(KeyScope::OnDemand)?;
            if !route_ok(route, std::slice::from_ref(card)) {
                return Err(deny(
                    403,
                    format!("route {route} is not one of the event's routes"),
                ));
            }
            own_card(card)
        }
    })();
    if let Err(d) = r {
        return d;
    }
    KeyDecision::Allow {
        event: key.event.clone(),
        occurrence: occ.id,
        lifts,
        until,
        charge_to: key.owner.clone(),
        ttl,
        hostd_validates: hostd,
    }
}

/// Preempting others' streams needs priority high or above (9.4).
pub fn may_preempt(p: Priority) -> bool {
    p >= Priority::High
}
