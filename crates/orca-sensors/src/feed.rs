//! The telemetry feed API (SUPERVISOR.md 2.5, D-SV11): what Glance, the admin UI, Home
//! Assistant, alerts and fans consume.
//!
//! - The state is one JSON object of **sections** ([`sections`]); each subsystem owns its
//!   section and replaces it whole. **A missing value is an absent key, never `null` or 0**
//!   (`null` would collide with merge-patch deletion, and D-SV4 forbids zeros).
//! - [`Snapshot`]: the full state at a sequence number (`GET /api/v1/supervisor/snapshot`).
//! - [`FeedMessage`]: what a subscriber receives: `snapshot` on connect, `patch` (RFC 7396 JSON
//!   merge patch, at most once a second) and `event` (immediate).
//! - [`TelemetryFeed`]: the trait the hub implements (`orca-supervisor`), so consumers can be
//!   tested against a fake.
//! - [`merge_patch_diff`] / [`apply_merge_patch`]: the patch maths, for Rust clients.
//! - [`FeedMessage::sse`]: SSE framing (`id:` = seq, so `Last-Event-ID` resumes). The same
//!   JSON goes over a websocket unchanged if one is ever needed.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast;

use crate::TimestampMs;

/// Section names in the feed state. A section is owned by one producer.
pub mod sections {
    /// `{ "<sensor id>": {"v":…, "u":…, "ts":…}, … }`: every sensor reading, including
    /// `sensor.*` (virtual) ones. Missing sensors are absent keys.
    pub const SENSORS: &str = "sensors";
    /// `{ "<card>": GpuSample, … }` keyed by config label (A, B, …).
    pub const GPUS: &str = "gpus";
    /// `HostSample`.
    pub const HOST: &str = "host";
    /// `BmcSample`.
    pub const BMC: &str = "bmc";
    /// `HwmonSample`.
    pub const HWMON: &str = "hwmon";
    /// `{ "<fan id>": FanSample + policy state, … }` (orca-fans).
    pub const FANS: &str = "fans";
    /// `{ "<card>": clocks/power/idle state, … }` (orca-gpuctl).
    pub const CLOCKS: &str = "clocks";
    /// Engines, layouts, slots (router control plane).
    pub const ENGINES: &str = "engines";
    pub const LEASES: &str = "leases";
    /// Active and upcoming calendar entries (orca-calendar).
    pub const CALENDAR: &str = "calendar";
    /// Open alerts (orca-alerts).
    pub const ALERTS: &str = "alerts";
    /// Host facts: containers, GPU processes, zpools, OOM kills (hostd).
    pub const FACTS: &str = "facts";
    /// Per-source health: last sample time, last error (hostd, ESP, HA, MQTT).
    pub const HEALTH: &str = "health";
}

/// The full state at `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub ts: TimestampMs,
    /// A JSON object: section name → section value.
    pub state: Value,
}

/// Severity of a feed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

/// A discrete event: a switch step, an alert firing or resolving, a calendar transition, a
/// fan failsafe. Delivered immediately, not batched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedEvent {
    /// Dotted kind: `alert.fired`, `alert.resolved`, `calendar.start`, `fan.failsafe`,
    /// `switch.step`, `hostd.policy`, ….
    pub kind: String,
    #[serde(default)]
    pub severity: Severity,
    /// A short human line for the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// One message on the feed. `seq` increases by one per message, across all kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedMessage {
    Snapshot(Snapshot),
    Patch {
        seq: u64,
        ts: TimestampMs,
        /// RFC 7396 merge patch against the previous state.
        patch: Value,
    },
    Event {
        seq: u64,
        ts: TimestampMs,
        event: FeedEvent,
    },
}

impl FeedMessage {
    pub fn seq(&self) -> u64 {
        match self {
            FeedMessage::Snapshot(s) => s.seq,
            FeedMessage::Patch { seq, .. } | FeedMessage::Event { seq, .. } => *seq,
        }
    }

    /// The SSE event name: `snapshot`, `patch` or `event`.
    pub fn sse_name(&self) -> &'static str {
        match self {
            FeedMessage::Snapshot(_) => "snapshot",
            FeedMessage::Patch { .. } => "patch",
            FeedMessage::Event { .. } => "event",
        }
    }

    /// One SSE frame: `id: <seq>\nevent: <name>\ndata: <json>\n\n`. The JSON has no newlines.
    pub fn sse(&self) -> String {
        let data = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.seq(),
            self.sse_name(),
            data
        )
    }
}

/// How a subscriber starts.
#[derive(Debug)]
pub enum Resume {
    /// Start from this snapshot, then read the receiver.
    Snapshot(Snapshot),
    /// The `Last-Event-ID` was recent enough: replay these messages, then read the receiver.
    Replay(Vec<FeedMessage>),
}

/// A live subscription: where to start, and the stream of later messages. A lagged receiver
/// (`RecvError::Lagged`) must resubscribe and start from a fresh snapshot.
#[derive(Debug)]
pub struct Subscription {
    pub start: Resume,
    pub rx: broadcast::Receiver<FeedMessage>,
}

/// The telemetry feed. Implemented by the supervisor's telemetry hub; fakes implement it in
/// consumer tests.
pub trait TelemetryFeed: Send + Sync {
    /// The current full state.
    fn snapshot(&self) -> Snapshot;
    /// Subscribes atomically: no message between the snapshot and the receiver is lost.
    /// With `last_seq` (`Last-Event-ID`), replays from there if still retained.
    fn subscribe(&self, last_seq: Option<u64>) -> Subscription;
}

/// The producer side, for subsystems that publish into the hub.
pub trait TelemetrySink: Send + Sync {
    /// Replaces a section. Published in the next patch (≤ 1 s).
    fn set_section(&self, name: &str, value: Value);
    /// Merges sensor readings into the `sensors` section and the sensor history.
    fn put_readings(&self, readings: &[(crate::SensorId, crate::Reading)]);
    /// Removes sensors (they became absent).
    fn remove_readings(&self, ids: &[crate::SensorId]);
    /// Publishes an event immediately.
    fn event(&self, event: FeedEvent);
}

/// RFC 7396: the merge patch that turns `old` into `new`, or `None` if they are equal.
/// `null` values must not appear in `new` (they could not be represented).
pub fn merge_patch_diff(old: &Value, new: &Value) -> Option<Value> {
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let mut patch = Map::new();
            for (k, ov) in o {
                match n.get(k) {
                    None => {
                        patch.insert(k.clone(), Value::Null);
                    }
                    Some(nv) => {
                        if let Some(p) = merge_patch_diff(ov, nv) {
                            patch.insert(k.clone(), p);
                        }
                    }
                }
            }
            for (k, nv) in n {
                if !o.contains_key(k) {
                    patch.insert(k.clone(), nv.clone());
                }
            }
            if patch.is_empty() {
                None
            } else {
                Some(Value::Object(patch))
            }
        }
        (a, b) if a == b => None,
        // An object turning into a scalar (or back) must replace, not merge.
        (_, b) => Some(b.clone()),
    }
}

/// RFC 7396: applies a merge patch in place.
pub fn apply_merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let t = target.as_object_mut().expect("object");
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    apply_merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_then_apply_roundtrips() {
        let cases = [
            (json!({}), json!({"a": 1})),
            (
                json!({"a": 1, "b": {"c": 2, "d": 3}}),
                json!({"a": 1, "b": {"c": 5}}),
            ),
            (json!({"a": {"x": 1}}), json!({"a": 7})),
            (json!({"a": 7}), json!({"a": {"x": 1}})),
            (json!({"a": [1, 2]}), json!({"a": [1, 3]})),
            (json!({"s": {"gpu.A.core": {"v": 50}}}), json!({"s": {}})),
        ];
        for (old, new) in cases {
            let mut t = old.clone();
            if let Some(p) = merge_patch_diff(&old, &new) {
                apply_merge_patch(&mut t, &p);
            }
            assert_eq!(t, new, "{old} → {new}");
        }
        assert_eq!(merge_patch_diff(&json!({"a": 1}), &json!({"a": 1})), None);
    }

    #[test]
    fn sse_frame() {
        let m = FeedMessage::Event {
            seq: 9,
            ts: 1,
            event: FeedEvent {
                kind: "fan.failsafe".into(),
                severity: Severity::Critical,
                text: "fan0".into(),
                data: Value::Null,
            },
        };
        let s = m.sse();
        assert!(s.starts_with("id: 9\nevent: event\ndata: {\"type\":\"event\""));
        assert!(s.ends_with("\n\n"));
        let data = s.lines().nth(2).unwrap().strip_prefix("data: ").unwrap();
        assert_eq!(serde_json::from_str::<FeedMessage>(data).unwrap(), m);
    }
}
