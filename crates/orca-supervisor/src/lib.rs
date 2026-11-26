//! `abhoth supervisor`: the umbrella of the Abhoth Supervisor (SUPERVISOR.md 2.2).
//!
//! - [`hub`]: the telemetry hub, implementing `orca_sensors::feed::{TelemetryFeed,
//!   TelemetrySink}`. The HTTP endpoints (`/api/v1/supervisor/snapshot`, `/stream`) serve it.
//!
//! Not wired into the `abhoth` CLI yet (SV9 waits for the router config loader, SSH and SSE).

pub mod hub;

pub use hub::{HubConfig, Resolution, TelemetryHub};
