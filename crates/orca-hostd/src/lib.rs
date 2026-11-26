//! `abhoth hostd`: the root host daemon of the Abhoth Supervisor (SUPERVISOR.md 2.3, 2.4).
//!
//! - [`protocol`]: the typed hostd protocol (framed JSON) and its framing.
//! - [`limits`]: the root-owned limits file that bounds every policy.
//! - [`sample`]: telemetry samplers (nvidia-smi, `/proc`, hwmon, ipmitool) with fixture tests.
//! - [`policy`]: `policy.put` validation against the limits, refused in full.
//! - [`clocks`]: the `orca_gpuctl` controller on its own thread, behind `LimitedControl`.
//! - [`daemon`]: shared state, the message handler, the telemetry loop, the socket server.
//! - [`files`]: last-known-good policy, heartbeat, lowered-cards file.
//! - [`cli`]: `run`, `boot`, `failsafe`, `collect`, `limits-check`, `policy-show`.
//!
//! Not wired into the `abhoth` CLI yet: the `abhoth-hostd` binary of this crate runs it
//! standalone. The fan loop is orca-fans (esphome and fake backends wired).

pub mod cli;
pub mod clocks;
pub mod daemon;
pub mod fans;
pub mod files;
pub mod limits;
pub mod policy;
pub mod protocol;
pub mod sample;
pub mod sniff;
