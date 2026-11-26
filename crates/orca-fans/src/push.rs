//! Device-curve input pushing: when to send a fan's control temperature and load to a device
//! that runs its own curve (ESPHome). This is `push-gpu-fans.py`'s main loop, step for step
//! (SUPERVISOR.md 5.4), so the parity tests can replay recorded inputs through both:
//!
//! - missing input → send nothing (the device fails that fan safe after its stale timeout);
//! - the load is gated by the fan's control temperature;
//! - a failed push waits `retry` before that fan is tried again;
//! - send on the first reading, `|ΔT| ≥ temp_delta`, a load change `≥ load_delta` (both only
//!   after `min_gap`), or when `heartbeat` has passed;
//! - the load goes first, then the temperature (the temperature marks the fan fresh);
//! - values are sent with one decimal; the load is clamped to the device's 0-150 range.

use crate::config::{LoadGate, SendPolicy};
use crate::policy::gate_load;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendReason {
    First,
    Temp,
    Load,
    Heartbeat,
}

impl SendReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SendReason::First => "first",
            SendReason::Temp => "temp",
            SendReason::Load => "load",
            SendReason::Heartbeat => "hb",
        }
    }
}

/// One push to a device: the load value (if any) then the temperature, already formatted.
#[derive(Debug, Clone, PartialEq)]
pub struct Push {
    pub load: Option<String>,
    pub temp: String,
    pub reason: SendReason,
    /// The gated, unclamped load (what later load deltas compare against).
    pub gated_load: Option<f64>,
}

/// The device's number range for the load entity (`GPU N Power`: 0-150 in the firmware).
pub const LOAD_RANGE: (f64, f64) = (0.0, 150.0);

/// Python's `"%.1f" % x` and Rust's `{:.1}` both round the exact binary value half-to-even,
/// so this is the pusher's formatting.
pub fn fmt1(x: f64) -> String {
    format!("{x:.1}")
}

#[derive(Debug, Clone, Default)]
struct Sent {
    temp: f64,
    load: Option<f64>,
    at: f64,
}

/// Per-fan send state.
#[derive(Debug, Clone)]
pub struct DevicePush {
    policy: SendPolicy,
    sent: Option<Sent>,
    retry_at: f64,
    /// True while the input is missing (for one log line per transition).
    pub missing: bool,
    /// True while pushes fail.
    pub failing: bool,
}

/// What [`DevicePush::step`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Input missing: nothing sent. `changed` is true on the transition to missing.
    Missing {
        changed: bool,
    },
    /// Waiting for the retry delay after a failure, or nothing to send.
    Idle,
    Send(Push),
}

impl DevicePush {
    pub fn new(policy: SendPolicy) -> DevicePush {
        DevicePush {
            policy,
            sent: None,
            retry_at: f64::NEG_INFINITY,
            missing: false,
            failing: false,
        }
    }

    /// One loop iteration for this fan. `temp`/`load` are the fan's current control
    /// temperature and raw load; `None` temp means missing or stale.
    pub fn step(
        &mut self,
        now: f64,
        temp: Option<f64>,
        load: Option<f64>,
        gate: Option<&LoadGate>,
    ) -> Step {
        let Some(temp) = temp else {
            let changed = !self.missing;
            self.missing = true;
            return Step::Missing { changed };
        };
        self.missing = false;
        let load = load.map(|l| match gate {
            Some(g) => gate_load(l, temp, g),
            None => l,
        });
        if now < self.retry_at {
            return Step::Idle;
        }
        let min_gap = self.policy.min_gap.as_secs_f64();
        let reason = match &self.sent {
            None => Some(SendReason::First),
            Some(s) => {
                let age = now - s.at;
                if age >= min_gap && (temp - s.temp).abs() >= self.policy.temp_delta {
                    Some(SendReason::Temp)
                } else if age >= min_gap
                    && load.is_some_and(|l| {
                        s.load
                            .is_none_or(|sl| (l - sl).abs() >= self.policy.load_delta)
                    })
                {
                    Some(SendReason::Load)
                } else if age >= self.policy.heartbeat.as_secs_f64() {
                    Some(SendReason::Heartbeat)
                } else {
                    None
                }
            }
        };
        match reason {
            None => Step::Idle,
            Some(reason) => Step::Send(Push {
                load: load.map(|l| fmt1(l.clamp(LOAD_RANGE.0, LOAD_RANGE.1))),
                temp: fmt1(temp),
                reason,
                gated_load: load,
            }),
        }
    }

    /// Record a successful push (the pusher stores the *rounded* temperature).
    pub fn sent_ok(&mut self, push: &Push, now: f64) {
        let temp = push.temp.parse().unwrap_or(f64::NAN);
        self.sent = Some(Sent {
            temp,
            load: push.gated_load,
            at: now,
        });
        self.failing = false;
    }

    /// Record a failed push: retry this fan after `retry`.
    pub fn send_failed(&mut self, now: f64) {
        self.retry_at = now + self.policy.retry.as_secs_f64();
        self.failing = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dp() -> DevicePush {
        DevicePush::new(SendPolicy::default())
    }

    fn sent(s: Step, now: f64, d: &mut DevicePush) -> Option<Push> {
        match s {
            Step::Send(p) => {
                d.sent_ok(&p, now);
                Some(p)
            }
            _ => None,
        }
    }

    #[test]
    fn formatting_rounds_like_python() {
        // Python: '%.1f' % 0.25 == '0.2', '%.1f' % 0.35 == '0.3' (0.35 is below .35 in binary),
        // '%.1f' % 12.25 == '12.2', '%.1f' % 12.75 == '12.8'.
        assert_eq!(fmt1(0.25), "0.2");
        assert_eq!(fmt1(0.35), "0.3");
        assert_eq!(fmt1(12.25), "12.2");
        assert_eq!(fmt1(12.75), "12.8");
        assert_eq!(fmt1(-0.0), "-0.0");
    }

    #[test]
    fn send_rules() {
        let mut d = dp();
        let p = sent(d.step(0.0, Some(40.04), Some(10.0), None), 0.0, &mut d).unwrap();
        assert_eq!(
            (p.reason, p.temp.as_str(), p.load.as_deref()),
            (SendReason::First, "40.0", Some("10.0"))
        );
        // Under 1 °C from the *rounded* 40.0, under 10 % load: nothing until the heartbeat.
        assert_eq!(d.step(5.0, Some(40.9), Some(19.0), None), Step::Idle);
        let p = sent(d.step(10.0, Some(40.9), Some(19.0), None), 10.0, &mut d).unwrap();
        assert_eq!(p.reason, SendReason::Heartbeat);
        // ΔT ≥ 1 but within min_gap: wait.
        assert_eq!(d.step(10.5, Some(42.0), None, None), Step::Idle);
        let p = sent(d.step(11.0, Some(42.0), None, None), 11.0, &mut d).unwrap();
        assert_eq!((p.reason, p.load), (SendReason::Temp, None));
        // Load appears after a None: a load send.
        let p = sent(d.step(12.0, Some(42.0), Some(1.0), None), 12.0, &mut d).unwrap();
        assert_eq!(p.reason, SendReason::Load);
    }

    #[test]
    fn missing_sends_nothing_and_failure_backs_off() {
        let mut d = dp();
        assert_eq!(
            d.step(0.0, None, Some(1.0), None),
            Step::Missing { changed: true }
        );
        assert_eq!(
            d.step(0.25, None, Some(1.0), None),
            Step::Missing { changed: false }
        );
        let Step::Send(_) = d.step(0.5, Some(50.0), None, None) else {
            panic!()
        };
        d.send_failed(0.5);
        assert_eq!(d.step(4.0, Some(50.0), None, None), Step::Idle);
        assert!(matches!(d.step(5.5, Some(50.0), None, None), Step::Send(_)));
    }

    #[test]
    fn load_clamped_and_gated() {
        let mut d = DevicePush::new(SendPolicy {
            heartbeat: Duration::from_secs(10),
            ..Default::default()
        });
        let g = LoadGate {
            by: None,
            lo: 52.0,
            hi: 64.0,
        };
        let Step::Send(p) = d.step(0.0, Some(70.0), Some(180.0), Some(&g)) else {
            panic!()
        };
        assert_eq!(p.load.as_deref(), Some("150.0"));
        assert_eq!(p.gated_load, Some(180.0));
        let mut d = dp();
        let Step::Send(p) = d.step(0.0, Some(58.0), Some(90.0), Some(&g)) else {
            panic!()
        };
        assert_eq!(p.load.as_deref(), Some("45.0"));
    }
}
