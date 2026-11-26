//! Fan alert conditions (SUPERVISOR.md 5.3 rule 5, 10): stale input, device failsafe, stall
//! (RPM under 300 at more than 25 % duty for 15 s, as the ESP does), control temperature at or
//! above `temp_warn`, and backend errors lasting longer than `stale`. Raised and resolved as
//! edges; the alert subsystem (orca-alerts) does dedupe, grouping and sinks.

use crate::config::AlertPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FanAlertKind {
    Stale,
    Failsafe,
    Stalled,
    TempWarn,
    BackendError,
}

impl FanAlertKind {
    /// Built-in alert names (ROUTER-CONFIG.md 4.11 example: `fan_stale`, `fan_stalled` …).
    pub fn name(self) -> &'static str {
        match self {
            FanAlertKind::Stale => "fan_stale",
            FanAlertKind::Failsafe => "fan_failsafe",
            FanAlertKind::Stalled => "fan_stalled",
            FanAlertKind::TempWarn => "fan_temp_warn",
            FanAlertKind::BackendError => "fan_backend_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Edge {
    Raised {
        fan: String,
        kind: FanAlertKind,
        detail: String,
    },
    Resolved {
        fan: String,
        kind: FanAlertKind,
    },
}

pub const STALL_RPM: f64 = 300.0;
pub const STALL_DUTY: f64 = 25.0;
pub const STALL_FOR: f64 = 15.0;

/// One fan's observations at one instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct Observation {
    /// The control temperature is missing or stale.
    pub input_stale: bool,
    /// The device (or hostd, for host curves) reports failsafe.
    pub failsafe: Option<bool>,
    pub rpm: Option<f64>,
    /// Applied duty, %.
    pub duty: Option<f64>,
    /// The device's own stall flag, if it has one.
    pub device_stalled: Option<bool>,
    pub temp: Option<f64>,
    /// Seconds the backend has been failing (0 when healthy).
    pub backend_failing_for: f64,
}

#[derive(Debug, Clone)]
pub struct FanAlerts {
    fan: String,
    policy: AlertPolicy,
    stale_after: f64,
    active: Vec<FanAlertKind>,
    low_rpm_since: Option<f64>,
}

impl FanAlerts {
    pub fn new(fan: &str, policy: AlertPolicy, stale_after: f64) -> FanAlerts {
        FanAlerts {
            fan: fan.into(),
            policy,
            stale_after,
            active: Vec::new(),
            low_rpm_since: None,
        }
    }

    pub fn active(&self) -> &[FanAlertKind] {
        &self.active
    }

    pub fn observe(&mut self, now: f64, o: &Observation) -> Vec<Edge> {
        let mut want: Vec<(FanAlertKind, String)> = Vec::new();
        if self.policy.stale && o.input_stale {
            want.push((FanAlertKind::Stale, "no fresh control temperature".into()));
        }
        if self.policy.failsafe && o.failsafe == Some(true) {
            want.push((FanAlertKind::Failsafe, "fan in failsafe (100 %)".into()));
        }
        let low = matches!((o.rpm, o.duty), (Some(r), Some(d)) if r < STALL_RPM && d > STALL_DUTY);
        if low {
            self.low_rpm_since.get_or_insert(now);
        } else {
            self.low_rpm_since = None;
        }
        let stalled = o.device_stalled == Some(true)
            || self.low_rpm_since.is_some_and(|s| now - s >= STALL_FOR);
        if self.policy.stalled && stalled {
            want.push((
                FanAlertKind::Stalled,
                format!(
                    "{:.0} RPM at {:.0} % duty",
                    o.rpm.unwrap_or(0.0),
                    o.duty.unwrap_or(0.0)
                ),
            ));
        }
        if let (Some(w), Some(t)) = (self.policy.temp_warn, o.temp)
            && t >= w
        {
            want.push((FanAlertKind::TempWarn, format!("{t:.1} °C ≥ {w} °C")));
        }
        if o.backend_failing_for > self.stale_after {
            want.push((
                FanAlertKind::BackendError,
                format!("backend failing for {:.0} s", o.backend_failing_for),
            ));
        }
        let mut edges = Vec::new();
        for (k, detail) in &want {
            if !self.active.contains(k) {
                self.active.push(*k);
                edges.push(Edge::Raised {
                    fan: self.fan.clone(),
                    kind: *k,
                    detail: detail.clone(),
                });
            }
        }
        let wanted: Vec<FanAlertKind> = want.iter().map(|(k, _)| *k).collect();
        self.active.retain(|k| {
            if wanted.contains(k) {
                true
            } else {
                edges.push(Edge::Resolved {
                    fan: self.fan.clone(),
                    kind: *k,
                });
                false
            }
        });
        self.active.sort();
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol() -> AlertPolicy {
        AlertPolicy {
            temp_warn: Some(75.0),
            ..Default::default()
        }
    }

    #[test]
    fn stall_needs_15_seconds() {
        let mut a = FanAlerts::new("duct0", pol(), 30.0);
        let o = Observation {
            rpm: Some(0.0),
            duty: Some(60.0),
            ..Default::default()
        };
        assert!(a.observe(0.0, &o).is_empty());
        assert!(a.observe(14.0, &o).is_empty());
        let e = a.observe(15.0, &o);
        assert!(matches!(
            &e[..],
            [Edge::Raised {
                kind: FanAlertKind::Stalled,
                ..
            }]
        ));
        assert!(a.observe(16.0, &o).is_empty(), "raised once");
        let e = a.observe(
            17.0,
            &Observation {
                rpm: Some(1500.0),
                duty: Some(60.0),
                ..Default::default()
            },
        );
        assert_eq!(
            e,
            vec![Edge::Resolved {
                fan: "duct0".into(),
                kind: FanAlertKind::Stalled
            }]
        );
        // Low duty with 0 RPM (fan stopped on purpose) is not a stall.
        assert!(
            a.observe(
                40.0,
                &Observation {
                    rpm: Some(0.0),
                    duty: Some(20.0),
                    ..Default::default()
                }
            )
            .is_empty()
        );
        assert!(
            a.observe(
                60.0,
                &Observation {
                    rpm: Some(0.0),
                    duty: Some(20.0),
                    ..Default::default()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn stale_failsafe_temp_and_backend() {
        let mut a = FanAlerts::new("duct1", pol(), 30.0);
        let e = a.observe(
            0.0,
            &Observation {
                input_stale: true,
                failsafe: Some(true),
                temp: Some(80.0),
                backend_failing_for: 31.0,
                ..Default::default()
            },
        );
        let kinds: Vec<_> = e
            .iter()
            .map(|x| match x {
                Edge::Raised { kind, .. } => *kind,
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                FanAlertKind::Stale,
                FanAlertKind::Failsafe,
                FanAlertKind::TempWarn,
                FanAlertKind::BackendError
            ]
        );
        let e = a.observe(1.0, &Observation::default());
        assert_eq!(e.len(), 4);
        assert!(a.active().is_empty());
    }

    #[test]
    fn disabled_alerts_stay_quiet() {
        let mut a = FanAlerts::new(
            "x",
            AlertPolicy {
                temp_warn: None,
                stalled: false,
                failsafe: false,
                stale: false,
            },
            30.0,
        );
        let o = Observation {
            input_stale: true,
            failsafe: Some(true),
            device_stalled: Some(true),
            temp: Some(99.0),
            ..Default::default()
        };
        assert!(a.observe(0.0, &o).is_empty());
    }
}
