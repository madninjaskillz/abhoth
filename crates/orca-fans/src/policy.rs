//! Policy evaluation shared by both modes (SUPERVISOR.md 5.1): the curve, the load gate, the
//! feed-forward, the floors and caps, profiles, overrides, and the host-curve controller with
//! its ramp. All functions are pure; time is passed in as seconds on a monotonic clock.
//!
//! Safety rules (5.3) enforced here, in this order, after every other input:
//! 1. a missing or stale control temperature → fail safe (never a duty from a guess);
//! 2. the temperature at or above `max_temp` → 100 %, whatever the profile or override;
//! 3. `min_duty` is the floor in every mode;
//! 4. ramp-up is immediate; only ramp-down is rate-limited.

use std::time::Duration;

use crate::config::{Curve, FeedForward, FeedForwardPatch, LoadGate, Policy, ProfileConfig};

/// Linear interpolation over the curve's points, flat outside them.
pub fn curve_duty(curve: &Curve, temp: f64) -> f64 {
    let pts = &curve.0;
    let first = pts[0];
    let last = pts[pts.len() - 1];
    if temp <= first[0] {
        return first[1];
    }
    if temp >= last[0] {
        return last[1];
    }
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if temp <= b[0] {
            return a[1] + (b[1] - a[1]) * (temp - a[0]) / (b[0] - a[0]);
        }
    }
    last[1]
}

/// `load × clamp((temp − lo)/(hi − lo), 0, 1)`; a step at `hi` when `hi ≤ lo` (pusher).
pub fn gate_load(load: f64, temp: f64, gate: &LoadGate) -> f64 {
    let g = if gate.hi <= gate.lo {
        if temp >= gate.hi { 1.0 } else { 0.0 }
    } else {
        ((temp - gate.lo) / (gate.hi - gate.lo)).clamp(0.0, 1.0)
    };
    load * g
}

/// Load → duty floor, as the ESP firmware computes it: `min_duty` at or below `lo`,
/// `max_duty` at or above `hi`, linear between.
pub fn feed_forward_duty(ff: &FeedForward, min_duty: f64, load: f64) -> f64 {
    if load <= ff.lo {
        min_duty
    } else if ff.hi <= ff.lo || load >= ff.hi {
        ff.max_duty
    } else {
        min_duty + (ff.max_duty - min_duty) * (load - ff.lo) / (ff.hi - ff.lo)
    }
}

/// The policy after a profile is applied. Profiles can never change `max_temp` or
/// `min_duty`: [`ProfileConfig`] has no such fields, and a fixed duty is floored below.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub policy: Policy,
    pub fixed_duty: Option<f64>,
}

pub fn apply_profile(base: &Policy, profile: Option<&ProfileConfig>) -> Effective {
    let mut policy = base.clone();
    let mut fixed_duty = None;
    if let Some(p) = profile {
        if let Some(c) = &p.curve
            && let Ok(c) = Curve::validate(c)
        {
            policy.curve = c;
        }
        if let (Some(ff), Some(patch)) = (policy.feed_forward.as_mut(), p.feed_forward) {
            patch_ff(ff, patch);
        }
        fixed_duty = p.duty;
    }
    Effective { policy, fixed_duty }
}

fn patch_ff(ff: &mut FeedForward, p: FeedForwardPatch) {
    if let Some(v) = p.lo {
        ff.lo = v;
    }
    if let Some(v) = p.hi {
        ff.hi = v;
    }
    if let Some(v) = p.max_duty {
        ff.max_duty = v;
    }
}

/// An operator override (`override {fan, duty, ttl ≤ 1 h}`, SUPERVISOR.md 2.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Override {
    pub duty: f64,
    /// Monotonic seconds at which it expires.
    pub until: f64,
}

/// Why the target is what it is (for logs, the dashboard and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Curve,
    FeedForward,
    MinDuty,
    Profile,
    Override,
    MaxTemp,
    FailSafe,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub duty: f64,
    pub reason: Reason,
    /// The gated load that went into the feed-forward, if any.
    pub load: Option<f64>,
    pub ff_duty: Option<f64>,
}

/// "What the curve wants" for one fan, before ramping. `temp = None` means the control
/// temperature is missing or stale: fail safe.
pub fn target(
    eff: &Effective,
    temp: Option<f64>,
    load: Option<f64>,
    ovr: Option<Override>,
    now: f64,
) -> Target {
    let p = &eff.policy;
    let Some(t) = temp.filter(|t| t.is_finite()) else {
        return Target {
            duty: 100.0,
            reason: Reason::FailSafe,
            load: None,
            ff_duty: None,
        };
    };
    let load = load.filter(|l| l.is_finite()).map(|l| match &p.load_gate {
        Some(g) => gate_load(l, t, g),
        None => l,
    });
    let ff_duty = match (&p.feed_forward, load) {
        (Some(ff), Some(l)) => Some(feed_forward_duty(ff, p.min_duty, l)),
        _ => None,
    };
    let (mut duty, mut reason) = if let Some(o) = ovr.filter(|o| now < o.until) {
        (o.duty, Reason::Override)
    } else if let Some(d) = eff.fixed_duty {
        (d, Reason::Profile)
    } else {
        let c = curve_duty(&p.curve, t);
        match ff_duty {
            Some(f) if f > c => (f, Reason::FeedForward),
            _ => (c, Reason::Curve),
        }
    };
    if duty < p.min_duty {
        duty = p.min_duty;
        reason = Reason::MinDuty;
    }
    if t >= p.max_temp {
        duty = 100.0;
        reason = Reason::MaxTemp;
    }
    Target {
        duty: duty.clamp(0.0, 100.0),
        reason,
        load,
        ff_duty,
    }
}

/// What a host-curve fan should do now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostAction {
    /// Write this duty (0-100 %).
    SetDuty(f64),
    /// Input missing or stale: full speed or the hardware's auto mode (backend `on_exit`).
    FailSafe,
}

/// The host-curve controller of one fan (hwmon, IPMI zones, NVML, liquidctl without a device
/// profile). Holds the applied duty and ramps it down at the policy's rate.
#[derive(Debug, Clone)]
pub struct HostCurve {
    pub duty: f64,
    last: Option<f64>,
    pub failsafe: bool,
    stale: Duration,
}

impl HostCurve {
    /// Starts at 100 %, like the ESP firmware at boot.
    pub fn new(stale: Duration) -> HostCurve {
        HostCurve {
            duty: 100.0,
            last: None,
            failsafe: true,
            stale,
        }
    }

    /// One step. `temp_age` is the age of the control temperature in seconds (None when the
    /// value is missing).
    pub fn step(
        &mut self,
        now: f64,
        eff: &Effective,
        temp: Option<f64>,
        temp_age: Option<f64>,
        load: Option<f64>,
        ovr: Option<Override>,
    ) -> (HostAction, Target) {
        let dt = self.last.map(|l| (now - l).max(0.0)).unwrap_or(0.0);
        self.last = Some(now);
        let fresh = matches!(temp_age, Some(a) if a <= self.stale.as_secs_f64());
        let t = target(eff, if fresh { temp } else { None }, load, ovr, now);
        if t.reason == Reason::FailSafe {
            self.duty = 100.0;
            self.failsafe = true;
            return (HostAction::FailSafe, t);
        }
        self.failsafe = false;
        if t.duty >= self.duty {
            self.duty = t.duty;
        } else {
            let step = eff.policy.ramp_down.rate() * dt;
            self.duty = t.duty.max(self.duty - step);
        }
        (HostAction::SetDuty(self.duty), t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RampDown;

    fn pol() -> Policy {
        Policy {
            load_gate: Some(LoadGate {
                by: None,
                lo: 52.0,
                hi: 64.0,
            }),
            curve: Curve(vec![[45.0, 30.0], [78.0, 100.0]]),
            feed_forward: Some(FeedForward {
                lo: 20.0,
                hi: 90.0,
                max_duty: 80.0,
            }),
            min_duty: 30.0,
            max_temp: 85.0,
            ramp_down: RampDown {
                pct: 2.0,
                per: Duration::from_secs(2),
            },
        }
    }

    fn eff() -> Effective {
        Effective {
            policy: pol(),
            fixed_duty: None,
        }
    }

    #[test]
    fn curve_shape() {
        let c = Curve(vec![[45.0, 30.0], [78.0, 100.0]]);
        assert_eq!(curve_duty(&c, 20.0), 30.0);
        assert_eq!(curve_duty(&c, 45.0), 30.0);
        assert!((curve_duty(&c, 61.5) - 65.0).abs() < 1e-9);
        assert_eq!(curve_duty(&c, 78.0), 100.0);
        assert_eq!(curve_duty(&c, 90.0), 100.0);
    }

    #[test]
    fn gate_matches_pusher() {
        let g = LoadGate {
            by: None,
            lo: 52.0,
            hi: 64.0,
        };
        assert_eq!(gate_load(90.0, 50.0, &g), 0.0);
        assert_eq!(gate_load(90.0, 58.0, &g), 45.0);
        assert_eq!(gate_load(90.0, 70.0, &g), 90.0);
        let step = LoadGate {
            by: None,
            lo: 60.0,
            hi: 60.0,
        };
        assert_eq!(gate_load(50.0, 59.9, &step), 0.0);
        assert_eq!(gate_load(50.0, 60.0, &step), 50.0);
    }

    #[test]
    fn max_temp_is_always_full() {
        let e = Effective {
            policy: pol(),
            fixed_duty: Some(10.0),
        };
        let t = target(
            &e,
            Some(85.0),
            None,
            Some(Override {
                duty: 0.0,
                until: 1e9,
            }),
            0.0,
        );
        assert_eq!((t.duty, t.reason), (100.0, Reason::MaxTemp));
    }

    #[test]
    fn min_duty_floors_profiles_and_overrides() {
        let e = Effective {
            policy: pol(),
            fixed_duty: Some(5.0),
        };
        assert_eq!(target(&e, Some(40.0), None, None, 0.0).duty, 30.0);
        let t = target(
            &eff(),
            Some(40.0),
            None,
            Some(Override {
                duty: 0.0,
                until: 10.0,
            }),
            5.0,
        );
        assert_eq!((t.duty, t.reason), (30.0, Reason::MinDuty));
        // An expired override is ignored.
        let t = target(
            &eff(),
            Some(61.5),
            None,
            Some(Override {
                duty: 0.0,
                until: 10.0,
            }),
            11.0,
        );
        assert_eq!(t.reason, Reason::Curve);
    }

    #[test]
    fn missing_temp_fails_safe_and_nan_is_missing() {
        assert_eq!(
            target(&eff(), None, Some(50.0), None, 0.0).reason,
            Reason::FailSafe
        );
        assert_eq!(
            target(&eff(), Some(f64::NAN), None, None, 0.0).reason,
            Reason::FailSafe
        );
    }

    #[test]
    fn feed_forward_gated_by_temp() {
        // Cold card under full load: the gate removes the load term.
        let t = target(&eff(), Some(50.0), Some(100.0), None, 0.0);
        assert_eq!(
            (t.reason, t.load, t.ff_duty),
            (Reason::Curve, Some(0.0), Some(30.0))
        );
        assert!((t.duty - (30.0 + 70.0 * 5.0 / 33.0)).abs() < 1e-9);
        // Warm card: full load counts, FF max 80 beats the curve.
        let t = target(&eff(), Some(64.0), Some(100.0), None, 0.0);
        assert_eq!((t.duty, t.reason), (80.0, Reason::FeedForward));
    }

    #[test]
    fn ramp_up_immediate_ramp_down_slow() {
        let mut h = HostCurve::new(Duration::from_secs(30));
        let e = eff();
        // Boot: 100 %, then fresh cool data ramps down 1 %/s.
        assert_eq!(
            h.step(0.0, &e, Some(40.0), Some(0.0), None, None).0,
            HostAction::SetDuty(100.0)
        );
        assert_eq!(
            h.step(2.0, &e, Some(40.0), Some(0.0), None, None).0,
            HostAction::SetDuty(98.0)
        );
        assert_eq!(
            h.step(12.0, &e, Some(40.0), Some(0.0), None, None).0,
            HostAction::SetDuty(88.0)
        );
        // Hot again: straight up.
        assert_eq!(
            h.step(12.5, &e, Some(80.0), Some(0.0), None, None).0,
            HostAction::SetDuty(100.0)
        );
        // Down to the floor eventually, never below.
        for i in 0..200 {
            h.step(13.0 + i as f64, &e, Some(20.0), Some(0.0), None, None);
        }
        assert_eq!(h.duty, 30.0);
    }

    #[test]
    fn stale_input_fails_safe() {
        let mut h = HostCurve::new(Duration::from_secs(3));
        let e = eff();
        h.step(0.0, &e, Some(40.0), Some(0.0), None, None);
        let (a, t) = h.step(1.0, &e, Some(40.0), Some(3.5), None, None);
        assert_eq!(a, HostAction::FailSafe);
        assert_eq!(t.reason, Reason::FailSafe);
        assert!(h.failsafe);
        assert_eq!(
            h.step(2.0, &e, Some(40.0), None, None, None).0,
            HostAction::FailSafe
        );
    }

    #[test]
    fn profile_lowers_curve_but_not_cap() {
        let quiet = ProfileConfig {
            curve: Some(vec![[50.0, 25.0], [80.0, 100.0]]),
            feed_forward: Some(FeedForwardPatch {
                max_duty: Some(60.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let e = apply_profile(&pol(), Some(&quiet));
        assert_eq!(e.policy.max_temp, 85.0);
        assert_eq!(e.policy.min_duty, 30.0);
        assert_eq!(e.policy.feed_forward.unwrap().max_duty, 60.0);
        // 25 % from the quiet curve is floored to min_duty.
        assert_eq!(target(&e, Some(45.0), None, None, 0.0).duty, 30.0);
        assert_eq!(target(&e, Some(86.0), None, None, 0.0).duty, 100.0);
    }
}
