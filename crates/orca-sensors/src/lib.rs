//! Sensors for the Abhoth Supervisor (SUPERVISOR.md 4, D-SV4).
//!
//! - [`SensorId`]: a source path such as `gpu.A.hbm`, `bmc."CPU1 Temp"` or `sensor.ctrl_A`.
//! - [`Reading`], [`Unit`], [`Lookup`]: a value with its unit, timestamp and origin, and whether
//!   it is fresh, stale or absent. **A missing value is never 0.**
//! - [`SensorStore`]: the in-memory latest-reading table with per-namespace staleness.
//! - [`expr`]: the expression language (`max`, `max_avail`, `first`, `ema`, `gate`, …), with
//!   strict and `_avail` semantics and unit checks. A missing result is [`Missing`], never a
//!   number.
//! - [`registry`]: named expressions (`[sensors.<name>]`), evaluated in dependency order into
//!   the `sensor.*` namespace.
//! - [`samples`]: typed telemetry samples (GPU, host, BMC, hwmon, fans) as hostd produces them,
//!   and their mapping to readings.
//! - [`feed`]: the telemetry feed API (snapshot + subscribe, merge patches, SSE framing) that
//!   Glance, the admin UI, Home Assistant and alerts consume.
//!
//! Pure: no IO. Only `tokio::sync` is used, for the feed's subscription channel.

pub mod expr;
pub mod feed;
pub mod id;
pub mod reading;
pub mod registry;
pub mod samples;

pub use expr::{Expr, ExprError, Missing, Value};
pub use id::{IdError, Namespace, SensorId};
pub use reading::{Lookup, Reading, SensorCatalog, SensorSource, SensorStore, Unit};
pub use registry::{RegistryError, SensorRegistry};

/// Milliseconds since the Unix epoch. Readings cross process boundaries (hostd → supervisor),
/// so time is wall-clock milliseconds rather than `Instant`.
pub type TimestampMs = i64;

/// The current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> TimestampMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
