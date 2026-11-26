//! `abhoth supervisor`: the umbrella of the Abhoth Supervisor (SUPERVISOR.md 2.2).
//!
//! - [`hub`]: the telemetry hub, implementing `orca_sensors::feed::{TelemetryFeed,
//!   TelemetrySink}`. The HTTP endpoints (`/api/v1/supervisor/snapshot`, `/stream`) serve it.
//!
//! - [`config`]: the supervisor config (`[supervisor]`, policy tables, `[mqtt]`,
//!   `[homeassistant]`);
//! - [`hostd`]: the hostd client (telemetry into the hub, status → `clocks`, requests);
//! - [`router`]: the router admin API poller (`engines`, `leases`) and the `ControlPort`;
//! - [`alerts`]: orca-alerts over the hub (built-in conditions, expression rules, sinks);
//! - [`run`]: `abhoth-supervisor run|check --config FILE`.
//!
//! Standalone binary `abhoth-supervisor`; not wired into the `abhoth` CLI yet.

pub mod alerts;
pub mod config;
pub mod hostd;
pub mod hub;
pub mod router;
pub mod run;

pub use hub::{HubConfig, Resolution, TelemetryHub};
