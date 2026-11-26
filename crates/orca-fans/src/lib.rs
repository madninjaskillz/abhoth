//! Fan control (SUPERVISOR.md 5): policy (curves, feed-forward, ramp, caps), the `FanBackend`
//! trait and the ESPHome, hwmon, IPMI, liquidctl, NVML, Windows and fake backends.
//!
//! **Skeleton.** The two types below are the contract with `orca-hostd`:
//!
//! - [`FansPolicy`] is the `fans` part of hostd's `policy.put` (compiled from `[fans.*]`,
//!   `[fan_backends.*]`, `[fan_profiles.*]`, `[fan_defaults]`). It is a placeholder until the
//!   fans builder defines it; keep it `Serialize + Deserialize + Default + Clone + PartialEq`.
//! - [`FanLimits`] is the `[fans]` table of hostd's root-owned limits file (2.3): the floors
//!   and caps no policy can cross. hostd refuses a whole policy that breaks them.
//!
//! Control temperatures and loads are `orca_sensors::Expr`s evaluated against hostd's
//! `orca_sensors::SensorStore` every 500 ms. A missing value is never 0 (D-SV4): for a device
//! backend, push nothing (the device fails safe); for a host backend, 100 % duty.

use serde::{Deserialize, Serialize};

/// The `fans` part of a hostd policy. Placeholder: the fans builder replaces the raw map with
/// typed fields (fans, backends, profiles, defaults).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FansPolicy {
    #[serde(flatten)]
    pub raw: serde_json::Map<String, serde_json::Value>,
}

/// The `[fans]` table of the hostd limits file. Bounds that no policy, profile, override,
/// calendar entry or event key can cross (D-SV5).
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
