//! The `[fans]` table of hostd's root-owned limits file (SUPERVISOR.md 2.3, D-SV5): bounds no
//! policy, profile, override, calendar entry or event key can cross. hostd refuses a whole
//! policy that breaks them. (The shape is the contract with `orca-hostd`; which backend kinds
//! may write lives in hostd's own limits and reaches [`crate::config::FansConfig::resolve`] as
//! a `may_write` callback.)

use serde::{Deserialize, Serialize};

use crate::config::{BackendConfig, BackendKind, FanSpec};

/// Overrides and fixed-duty profiles expire within this (SUPERVISOR.md 2.3: TTL ≤ 1 h).
pub const MAX_OVERRIDE_TTL_S: f64 = 3600.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanLimits {
    /// No fan's `min_duty` may be below this, in any mode or profile (%).
    pub min_duty: f64,
    /// No fan's `max_temp` may be above this (°C); at or above it the duty is 100 %.
    pub max_control_temp: f64,
    /// No fan's `stale` / `stale_input` may be longer than this (s).
    pub max_stale_s: f64,
}

impl Default for FanLimits {
    fn default() -> FanLimits {
        FanLimits {
            min_duty: 30.0,
            max_control_temp: 85.0,
            max_stale_s: 30.0,
        }
    }
}

impl FanLimits {
    pub fn check_fan(
        &self,
        f: &FanSpec,
        backend: &BackendConfig,
        may_write: &dyn Fn(BackendKind) -> bool,
    ) -> Result<(), String> {
        let p = &f.policy;
        if p.min_duty < self.min_duty {
            return Err(format!(
                "min_duty {} is below the limits file's {}",
                p.min_duty, self.min_duty
            ));
        }
        if p.min_duty > 100.0 {
            return Err("min_duty above 100".into());
        }
        if p.max_temp > self.max_control_temp {
            return Err(format!(
                "max_temp {} is above the limits file's max_control_temp {}",
                p.max_temp, self.max_control_temp
            ));
        }
        for (name, d) in [("stale", f.stale), ("stale_input", f.stale_input)] {
            if d.as_secs_f64() > self.max_stale_s {
                return Err(format!(
                    "{name} {:?} is longer than the limits file's {} s",
                    d, self.max_stale_s
                ));
            }
            if d.is_zero() {
                return Err(format!("{name} must be non-zero"));
            }
        }
        if let Some(last) = p.curve.0.last()
            && last[0] > p.max_temp
        {
            return Err(format!(
                "the curve's last point ({} °C) is above max_temp ({} °C)",
                last[0], p.max_temp
            ));
        }
        if backend.kind == BackendKind::Ipmi && backend.write && !may_write(BackendKind::Ipmi) {
            return Err("IPMI writes are not allowed by the limits file".into());
        }
        Ok(())
    }
}
