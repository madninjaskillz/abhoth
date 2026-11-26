//! Who may start an on-demand switch (ROUTER-CONFIG.md 7.2, 7.4).
//!
//! A switch for a cold model can only start from a [`ChatAdmission`], and a `ChatAdmission`
//! can only be made by the chat handler (`proxy::chat`): its constructor is crate-private and
//! called in one place, after the body parsed and a route was resolved. `/health`,
//! `/health/liveliness`, `/v1/models`, `/v1/model/info`, `/status`, `/metrics`, the canary,
//! verify chats and the slot surfaces never make one, so they cannot reach
//! `ondemand::ensure`, which takes the admission. A test walks every path and method of every
//! listener to prove it (`tests/probe_safety.rs`).
//!
//! On top of that, an admission only becomes an [`OnDemandTicket`] when:
//! - the request did not carry `x-abhoth-no-switch: 1` (every request the router sends
//!   upstream carries it, so chained routers cannot switch each other's GPUs);
//! - a principal is known when keys are required (anonymous callers never switch);
//! - the rate limits allow it: switches per server per hour, per principal per hour.
//!
//! Event keys (Supervisor calendar) can lift their event's reservations in the guard board,
//! but nothing here: a ticket still needs a chat request, a key and the rate limits.

use std::collections::VecDeque;
use std::sync::Mutex;

use axum::http::HeaderMap;

use crate::guards::Caller;

/// The header that forbids any switch for this request.
pub const NO_SWITCH: &str = "x-abhoth-no-switch";

/// Proof that a request came through the chat handler for `route`.
#[derive(Debug, Clone)]
pub struct ChatAdmission {
    route: String,
    caller: Caller,
    no_switch: bool,
}

impl ChatAdmission {
    /// Made only by `proxy::chat` once the body parsed and the route resolved.
    pub(crate) fn from_chat(route: &str, headers: &HeaderMap, caller: Caller) -> Self {
        let no_switch = headers
            .get(NO_SWITCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                let v = v.trim();
                !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
            });
        ChatAdmission {
            route: route.to_string(),
            caller,
            no_switch,
        }
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    pub fn caller(&self) -> &Caller {
        &self.caller
    }

    pub fn no_switch(&self) -> bool {
        self.no_switch
    }

    /// For tests outside the crate's chat path.
    #[cfg(test)]
    pub fn for_test(route: &str, caller: Caller, no_switch: bool) -> Self {
        ChatAdmission {
            route: route.into(),
            caller,
            no_switch,
        }
    }
}

/// The principal (user or key id, never the key) of a request. The accounts integration (D140
/// key introspection) plugs in here; until it does no request has one, so with `require_key`
/// no request can start a switch.
pub fn principal_of(_headers: &HeaderMap) -> Option<String> {
    None
}

/// Permission to start one on-demand switch now.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OnDemandTicket {
    pub route: String,
    pub caller: Caller,
}

/// Sliding one-hour windows of switches started.
#[derive(Debug, Default)]
pub struct RateLimiter {
    started: Mutex<VecDeque<(f64, Option<String>)>>,
}

/// The limits and the key rule.
#[derive(Debug, Clone, Copy)]
pub struct GateRules {
    pub require_key: bool,
    pub per_hour: u32,
    pub per_principal_per_hour: u32,
}

impl Default for GateRules {
    fn default() -> Self {
        GateRules {
            require_key: true,
            per_hour: 4,
            per_principal_per_hour: 2,
        }
    }
}

impl RateLimiter {
    /// Would one more switch by `who` at `now` stay within the limits? `Err((why, retry))`.
    pub fn check(
        &self,
        rules: &GateRules,
        who: Option<&str>,
        now: f64,
    ) -> Result<(), (String, u64)> {
        let mut q = self.started.lock().unwrap();
        while q.front().is_some_and(|(t, _)| now - t >= 3600.0) {
            q.pop_front();
        }
        let retry = |t: f64| ((3600.0 - (now - t)).ceil() as u64).max(1);
        if rules.per_hour > 0 && q.len() >= rules.per_hour as usize {
            return Err((
                format!("{} on-demand switches in the last hour (limit)", q.len()),
                retry(q[q.len() - rules.per_hour as usize].0),
            ));
        }
        if let Some(w) = who {
            let mine: Vec<f64> = q
                .iter()
                .filter(|(_, p)| p.as_deref() == Some(w))
                .map(|(t, _)| *t)
                .collect();
            if rules.per_principal_per_hour > 0
                && mine.len() >= rules.per_principal_per_hour as usize
            {
                return Err((
                    format!(
                        "{} on-demand switches by this key in the last hour (limit)",
                        mine.len()
                    ),
                    retry(mine[mine.len() - rules.per_principal_per_hour as usize]),
                ));
            }
        }
        Ok(())
    }

    /// Switches started in the window (the whole history kept).
    pub fn count(&self) -> usize {
        self.started.lock().unwrap().len()
    }

    pub fn record(&self, who: Option<&str>, now: f64) {
        self.started
            .lock()
            .unwrap()
            .push_back((now, who.map(str::to_string)));
    }
}

/// Turn an admission into a ticket, or say why not (with a `Retry-After`).
pub fn admit(
    adm: &ChatAdmission,
    rules: &GateRules,
    limiter: &RateLimiter,
    now: f64,
) -> Result<OnDemandTicket, (String, u64)> {
    if adm.no_switch {
        return Err((
            format!("{NO_SWITCH} is set: this request may not switch a card"),
            3600,
        ));
    }
    let who = adm.caller.principal.as_deref();
    if rules.require_key && who.is_none() {
        return Err((
            "switching a card needs an API key with the switch permission".into(),
            3600,
        ));
    }
    limiter.check(rules, who, now)?;
    Ok(OnDemandTicket {
        route: adm.route.clone(),
        caller: adm.caller.clone(),
    })
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn caller(p: Option<&str>) -> Caller {
        Caller {
            principal: p.map(String::from),
            grants: vec![],
        }
    }

    #[test]
    fn header_key_and_rate_limits() {
        let rules = GateRules::default();
        let lim = RateLimiter::default();
        let t = 1_800_000_000.0;
        // the no-switch header always wins
        let mut h = HeaderMap::new();
        h.insert(NO_SWITCH, "1".parse().unwrap());
        let a = ChatAdmission::from_chat("dense", &h, caller(Some("k1")));
        assert!(admit(&a, &rules, &lim, t).is_err());
        let mut h0 = HeaderMap::new();
        h0.insert(NO_SWITCH, "0".parse().unwrap());
        assert!(!ChatAdmission::from_chat("dense", &h0, caller(None)).no_switch());
        // anonymous never switches when keys are required
        let anon = ChatAdmission::from_chat("dense", &HeaderMap::new(), caller(None));
        assert!(
            admit(&anon, &rules, &lim, t)
                .unwrap_err()
                .0
                .contains("API key")
        );
        // per principal: 2 an hour
        let k1 = ChatAdmission::from_chat("dense", &HeaderMap::new(), caller(Some("k1")));
        for i in 0..2 {
            admit(&k1, &rules, &lim, t + i as f64).unwrap();
            lim.record(Some("k1"), t + i as f64);
        }
        let (why, retry) = admit(&k1, &rules, &lim, t + 10.0).unwrap_err();
        assert!(why.contains("by this key"));
        assert_eq!(retry, 3590);
        // per server: 4 an hour
        let k2 = ChatAdmission::from_chat("dense", &HeaderMap::new(), caller(Some("k2")));
        for i in 0..2 {
            admit(&k2, &rules, &lim, t + 20.0 + i as f64).unwrap();
            lim.record(Some("k2"), t + 20.0 + i as f64);
        }
        let k3 = ChatAdmission::from_chat("dense", &HeaderMap::new(), caller(Some("k3")));
        assert!(
            admit(&k3, &rules, &lim, t + 30.0)
                .unwrap_err()
                .0
                .contains("last hour")
        );
        // an hour later the window has moved
        assert!(admit(&k3, &rules, &lim, t + 3601.0).is_ok());
        assert!(admit(&k1, &rules, &lim, t + 3602.0).is_ok());
    }
}
