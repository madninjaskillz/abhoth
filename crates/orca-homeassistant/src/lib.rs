//! Abhoth Supervisor: Home Assistant (SUPERVISOR.md 11).
//!
//! - [`feed`]: follows the telemetry hub (`TelemetryFeed`) and builds the HA view;
//! - [`mqtt`]: the MQTT runtime (availability with LWT, reconnects, retained-message policy,
//!   discovery and state publishing, command handling);
//! - [`discovery`] and [`state`]: pure payload builders (golden-tested);
//! - [`commands`]: HA commands, checked and passed to a [`commands::ControlPort`] as the MQTT
//!   service principal; guards and audit live behind that port (D-SV12);
//! - [`rest`]: the HA REST client (state reads such as the EcoFlow AC output, service calls);
//! - [`notify`]: the MQTT alert payloads (alert JSON and the event entity); the `ha` notify sink is orca-alerts'.
//!
//! Everything is off unless `[mqtt].enabled` / `[homeassistant].enabled` is set, and nothing
//! here is wired into the binary yet (the supervisor umbrella does that, SV9/SV15).

pub mod commands;
pub mod config;
pub mod discovery;
pub mod feed;
pub mod model;
pub mod mqtt;
pub mod notify;
pub mod rest;
pub mod state;
pub mod topics;

pub use commands::{Command, ControlOutcome, ControlPort, FanMode, ServicePrincipal};
pub use config::{HomeAssistantConfig, MqttConfig, SecretRef};
pub use model::HaSnapshot;
