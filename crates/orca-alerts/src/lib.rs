//! Alerts (SUPERVISOR.md 10): rules, dedupe, cooldown, repeat, grouping and sinks.
//!
//! - **Rules** are built-in event rules (OOM kills, engine crashes, failed switches, hot cards,
//!   stale fan data, clock drift, …; on by default, editable under `[alerts.rules.<id>]`) and
//!   expression rules (`when = "max_avail(gpu.A.core, …) >= 85"`). The expression language is
//!   `orca-sensors`' (4.2); this crate takes the evaluated result as a [`Signal::Condition`].
//! - **Identity** is `rule/dedupe parts` (`hot_card/A`). One open alert per identity, with
//!   first, last and count. A condition must hold for `for` before it opens.
//! - **Re-sending** only after `repeat`. An alert that resolves and reopens within `cooldown`
//!   (flapping) is not re-sent; its count goes up.
//! - **Grouping:** alerts that share a `cause` (`card:B`) and open within `group_window` (30 s)
//!   go out as one notification listing the children.
//! - **Calendar mutes** (`mute_alerts`) silence sinks, never the UI and never `critical`.
//! - **Sinks**: [`sinks`] (HA mobile notify over HA REST, MQTT through the HA agent's
//!   publisher, ntfy, webhook, a fake for tests). Secrets come only from files.

pub mod engine;
pub mod rules;
pub mod sinks;

pub use engine::{Alert, AlertState, Engine, Notification, NotifyKind, Signal};
pub use rules::{AlertsConfig, RuleConfig, Severity};

#[cfg(test)]
mod tests;
