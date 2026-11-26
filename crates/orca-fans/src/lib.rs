//! Fan control for the Abhoth Supervisor (SUPERVISOR.md 5).
//!
//! - [`config`]: the `[fan_backends.*]`, `[fan_defaults]`, `[fans.*]` and `[fan_profiles.*]`
//!   tables, strict parsing and validation against hostd's [`limits::FanLimits`].
//! - [`policy`]: curve, load gate, feed-forward, floors and caps, profiles, overrides, and the
//!   host-curve controller with its ramp.
//! - [`push`]: device-curve input pushing, step for step as `push-gpu-fans.py`.
//! - [`esp`]: the ESP32 firmware model, its SSE state, and the read-only-diff-first sync of
//!   its curve numbers.
//! - [`backend`]: the [`backend::FanBackend`] trait and the ESPHome, hwmon, IPMI, liquidctl,
//!   NVML, Windows (design only) and fake backends, plus [`backend::Shadow`].
//! - [`controller`]: [`controller::FansPolicy`] (the `fans` part of hostd's `policy.put`) and
//!   the per-tick [`controller::FanLoop`].
//! - [`alerts`]: stale, failsafe, stall, temperature and backend-error conditions.
//! - [`shadow`]: shadow-mode comparison against what the current pusher sends.
//!
//! **Not wired yet:** nothing outside this crate uses it. hostd runs it once the sensor
//! store (orca-sensors) feeds [`controller::FanInputs`].

pub mod alerts;
pub mod backend;
pub mod config;
pub mod controller;
pub mod digest;
pub mod esp;
pub mod inputs;
pub mod limits;
pub mod policy;
pub mod push;
pub mod replay;
pub mod shadow;

pub use controller::{FanInputs, FanLoop, FansPolicy, Sample};
pub use limits::FanLimits;
