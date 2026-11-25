//! Placement policy (ROUTER.md SHOULD, phase 2): which card a gateway request goes to first, and
//! per-card admission. Every policy is OFF by default (`placement` in router.json) until benched.
//!
//! Precedence: a lease (`x-abhoth-slot-key`, or a `prompt_cache_key` equal to a lease key) >
//! session affinity (rendezvous hash + sticky table) > least in flight.
//!
//! Admission: a card takes at most `lanes` admitted requests; while a card holds a Lead lease
//! (`kind: main`), `leadReserve` of its lanes are kept for the Lead. The Lead
//! (`x-abhoth-priority: lead`, or a request carrying a main lease's key) never queues. Others
//! wait for a lane (FIFO-ish via a notify), up to `queueSeconds`, then go anyway: ninfer's own
//! queue (429) is the backstop, never a router refusal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::config::Snapshot;
use crate::proxy::LogRec;
use crate::state::AppState;

/// An admitted request's lane; released (and waiters woken) on drop.
pub struct Permit {
    adm: Arc<AdmissionInner>,
    card: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut g = self.adm.state.lock().unwrap();
        if let Some(n) = g.admitted.get_mut(&self.card) {
            *n = n.saturating_sub(1);
        }
        drop(g);
        self.adm.notify.notify_waiters();
    }
}

pub struct Placement {
    /// Preferred cards, best first; empty = least in flight.
    pub order: Vec<String>,
    pub permit: Option<Permit>,
}

#[derive(Default)]
struct AdmState {
    admitted: HashMap<String, u32>,
    queued: HashMap<String, u32>,
}

#[derive(Default)]
struct AdmissionInner {
    state: Mutex<AdmState>,
    notify: Notify,
}

#[derive(Default, Clone)]
pub struct Admission {
    inner: Arc<AdmissionInner>,
}

impl Admission {
    pub fn queued(&self, card: &str) -> u32 {
        self.inner
            .state
            .lock()
            .unwrap()
            .queued
            .get(card)
            .copied()
            .unwrap_or(0)
    }

    pub fn admitted(&self, card: &str) -> u32 {
        self.inner
            .state
            .lock()
            .unwrap()
            .admitted
            .get(card)
            .copied()
            .unwrap_or(0)
    }

    pub fn wake(&self) {
        self.inner.notify.notify_waiters();
    }

    /// Try to admit on the first card of `order` that has room. `limit(card)` = lanes this
    /// request may use there; `load(card)` = requests in flight there now.
    fn try_admit(
        &self,
        order: &[String],
        limit: &dyn Fn(&str) -> u32,
        inflight: &dyn Fn(&str) -> u32,
    ) -> Option<Permit> {
        let mut g = self.inner.state.lock().unwrap();
        for c in order {
            let admitted = g.admitted.get(c).copied().unwrap_or(0);
            let load = admitted.max(inflight(c));
            if load < limit(c) {
                *g.admitted.entry(c.clone()).or_default() += 1;
                return Some(Permit {
                    adm: self.inner.clone(),
                    card: c.clone(),
                });
            }
        }
        None
    }

    fn force(&self, card: &str) -> Permit {
        let mut g = self.inner.state.lock().unwrap();
        *g.admitted.entry(card.to_string()).or_default() += 1;
        Permit {
            adm: self.inner.clone(),
            card: card.to_string(),
        }
    }

    fn set_queued(&self, card: &str, delta: i32) {
        let mut g = self.inner.state.lock().unwrap();
        let q = g.queued.entry(card.to_string()).or_default();
        *q = (*q as i32 + delta).max(0) as u32;
    }
}

/// Sticky session -> card table.
#[derive(Default)]
pub struct Affinity {
    table: Mutex<HashMap<String, (String, Instant)>>,
}

impl Affinity {
    pub fn get(&self, key: &str, ttl: Duration) -> Option<String> {
        let mut g = self.table.lock().unwrap();
        let now = Instant::now();
        g.retain(|_, (_, t)| now.duration_since(*t) < ttl);
        g.get(key).map(|(c, _)| c.clone())
    }

    pub fn set(&self, key: &str, card: &str) {
        self.table
            .lock()
            .unwrap()
            .insert(key.to_string(), (card.to_string(), Instant::now()));
    }

    pub fn len(&self) -> usize {
        self.table.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Rendezvous (highest random weight) choice of a card for `key`.
pub fn rendezvous(key: &str, cards: &[String]) -> Option<String> {
    cards
        .iter()
        .max_by_key(|c| {
            let mut h = Sha256::new();
            h.update(key.as_bytes());
            h.update(b"|");
            h.update(c.as_bytes());
            let d = h.finalize();
            u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
        })
        .cloned()
}

fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The session key: prompt_cache_key, x-session-id, x-litellm-session-id, metadata.session_id.
pub fn session_key(h: &HeaderMap, body: &Map<String, Value>) -> Option<String> {
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| header(h, "x-session-id"))
        .or_else(|| header(h, "x-litellm-session-id"))
        .or_else(|| {
            body.get("metadata")
                .and_then(|m| m.get("session_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        })
}

pub async fn place(
    st: &Arc<AppState>,
    snap: &Snapshot,
    headers: &HeaderMap,
    body: &Map<String, Value>,
    candidates: &[String],
    rec: &mut LogRec,
) -> Result<Placement, Box<Response>> {
    let p = &snap.config.placement;
    let reg = &st.ext.registry;
    let slots_on = snap.config.slots.enabled;
    let eligible = st.eligible(snap, candidates, &[]);
    let mut order: Vec<String> = Vec::new();

    // the lease this request carries, if any
    let lease_key = header(headers, "x-abhoth-slot-key").or_else(|| {
        body.get("prompt_cache_key")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let lease = if slots_on {
        lease_key
            .as_deref()
            .and_then(|k| reg.lease_for_key(snap, k))
    } else {
        None
    };
    let lead = header(headers, "x-abhoth-priority").is_some_and(|v| v.eq_ignore_ascii_case("lead"))
        || lease.as_ref().is_some_and(|(_, kind)| kind == "main");
    if lead {
        rec.set("priority", "lead");
    }
    if p.lease_aware
        && let Some((card, _)) = &lease
        && eligible.contains(card)
    {
        order.push(card.clone());
        rec.set("placement", "lease");
        st.metrics.lease_routed();
    }

    let sess = if p.affinity {
        session_key(headers, body)
    } else {
        None
    };
    let ttl = Duration::from_secs(p.affinity_ttl_seconds.max(1));
    let mut affine: Option<String> = None;
    if order.is_empty()
        && let Some(k) = &sess
    {
        let sticky = st.ext.affinity.get(k, ttl).filter(|c| eligible.contains(c));
        let card = sticky.or_else(|| rendezvous(k, &eligible));
        if let Some(c) = card {
            order.push(c.clone());
            affine = Some(c);
            rec.set("placement", "affinity");
        }
    }

    let lead_cards = if slots_on && p.admission {
        reg.lead_cards(snap)
    } else {
        Vec::new()
    };
    let limit = |c: &str| -> u32 {
        let lanes = snap.config.upstreams.get(c).map_or(1, |u| u.lanes.max(1));
        if !lead && lead_cards.iter().any(|l| l == c) {
            lanes.saturating_sub(p.lead_reserve).max(1)
        } else {
            lanes
        }
    };
    let inflight = |c: &str| st.load(c);
    let least = |order: &[String]| -> Vec<String> {
        // the preferred card(s), then the rest by load per lane
        let mut rest: Vec<String> = eligible
            .iter()
            .filter(|c| !order.contains(c))
            .cloned()
            .collect();
        rest.sort_by_key(|c| {
            let lanes = snap.config.upstreams.get(c).map_or(1, |u| u.lanes.max(1)) as u64;
            (
                st.load(c) as u64 * 1_000_000 / lanes,
                st.card(c).picks * 1_000_000 / lanes,
            )
        });
        order.iter().cloned().chain(rest).collect()
    };

    if !p.admission {
        let full = least(&order);
        if let Some(k) = &sess
            && let Some(c) = full.first()
        {
            st.ext.affinity.set(k, c);
        }
        return Ok(Placement {
            order: full,
            permit: None,
        });
    }

    // admission: Lead goes straight in on its preferred card
    let full = least(&order);
    let Some(first) = full.first().cloned() else {
        return Ok(Placement {
            order: full,
            permit: None,
        });
    };
    if lead {
        let permit = st.ext.admission.force(&first);
        return Ok(Placement {
            order: vec![first],
            permit: Some(permit),
        });
    }
    // an affine card that is full while another has room: move the session once
    let try_order: Vec<String> = full.clone();
    let deadline = Instant::now() + Duration::from_secs(p.queue_seconds);
    let mut queued = false;
    loop {
        let notified = st.ext.admission.inner.notify.notified();
        if let Some(permit) = st.ext.admission.try_admit(&try_order, &limit, &inflight) {
            if queued {
                st.ext.admission.set_queued(&first, -1);
            }
            let card = permit.card.clone();
            if let (Some(k), Some(a)) = (&sess, &affine) {
                if *a != card {
                    st.metrics.affinity(true);
                    rec.set("affinityMoved", format!("{a}->{card}"));
                } else {
                    st.metrics.affinity(false);
                }
                st.ext.affinity.set(k, &card);
            } else if let Some(k) = &sess {
                st.ext.affinity.set(k, &card);
            }
            let mut o = vec![card.clone()];
            o.extend(full.into_iter().filter(|c| *c != card));
            return Ok(Placement {
                order: o,
                permit: Some(permit),
            });
        }
        if !queued {
            queued = true;
            st.ext.admission.set_queued(&first, 1);
            st.metrics.queued();
            rec.set("queued", true);
        }
        let now = Instant::now();
        if now >= deadline {
            st.ext.admission.set_queued(&first, -1);
            rec.set("queueTimeout", true);
            let permit = st.ext.admission.force(&first);
            return Ok(Placement {
                order: full,
                permit: Some(permit),
            });
        }
        // re-check at least every 500 ms (a lane freed by traffic the router does not admit)
        let wait = (deadline - now).min(Duration::from_millis(500));
        let _ = tokio::time::timeout(wait, notified).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendezvous_is_stable_and_spreads() {
        let cards = vec!["A".to_string(), "B".to_string()];
        let a = rendezvous("session-1", &cards).unwrap();
        assert_eq!(rendezvous("session-1", &cards).unwrap(), a);
        // removing the other card never moves a key that was on the remaining one
        let only = vec![a.clone()];
        assert_eq!(rendezvous("session-1", &only).unwrap(), a);
        let on_a = (0..200)
            .filter(|i| rendezvous(&format!("s{i}"), &cards).unwrap() == "A")
            .count();
        assert!((60..140).contains(&on_a), "{on_a}");
    }

    #[test]
    fn session_key_order() {
        let mut h = HeaderMap::new();
        h.insert("x-session-id", "hdr".parse().unwrap());
        let b: Map<String, Value> =
            serde_json::from_str(r#"{"prompt_cache_key":"pck","metadata":{"session_id":"m"}}"#)
                .unwrap();
        assert_eq!(session_key(&h, &b).unwrap(), "pck");
        let b2: Map<String, Value> =
            serde_json::from_str(r#"{"metadata":{"session_id":"m"}}"#).unwrap();
        assert_eq!(session_key(&h, &b2).unwrap(), "hdr");
        assert_eq!(session_key(&HeaderMap::new(), &b2).unwrap(), "m");
    }
}
