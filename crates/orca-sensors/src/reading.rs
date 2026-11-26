//! Readings, units, availability and the latest-reading store (SUPERVISOR.md 4.1).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::{Namespace, SensorId, TimestampMs};

/// The unit of a reading or expression value.
///
/// Units are checked, not converted: `+`, `-`, `max`, `min`, `avg`, `first` and `clamp` need
/// compatible units (equal, or one side [`Unit::None`] / [`Unit::Unknown`]). A mismatch is a
/// validation error when units are known up front, and a [`crate::Missing::UnitMismatch`] at run
/// time, never a silently wrong number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Dimensionless: number literals, ratios. Adopts the other operand's unit.
    #[default]
    None,
    /// Not known (for example an MQTT topic with no unit in config). Compatible with anything.
    Unknown,
    Celsius,
    Watts,
    /// 0-100 (utilisation, duty, power as % of the enforced limit).
    Percent,
    Mhz,
    Bytes,
    BytesPerSec,
    Rpm,
    Seconds,
    Volts,
    Amps,
    /// A 0/1 state (failsafe active, stalled).
    Bool,
}

impl Unit {
    /// Units that can meet in `+`, `-`, `max` etc.; returns the resulting unit.
    pub fn combine(self, other: Unit) -> Option<Unit> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (Unit::None, b) | (Unit::Unknown, b) => Some(b),
            (a, Unit::None) | (a, Unit::Unknown) => Some(a),
            _ => None,
        }
    }

    /// Best-effort mapping of unit text from ipmitool, hwmon, HA or MQTT config.
    pub fn from_text(t: &str) -> Unit {
        match t.trim().to_ascii_lowercase().as_str() {
            "" => Unit::None,
            "c" | "°c" | "degrees c" | "celsius" => Unit::Celsius,
            "w" | "watts" => Unit::Watts,
            "%" | "percent" => Unit::Percent,
            "mhz" => Unit::Mhz,
            "b" | "bytes" => Unit::Bytes,
            "b/s" | "bytes/s" => Unit::BytesPerSec,
            "rpm" => Unit::Rpm,
            "s" | "seconds" => Unit::Seconds,
            "v" | "volts" => Unit::Volts,
            "a" | "amps" => Unit::Amps,
            _ => Unit::Unknown,
        }
    }
}

/// One reading: `(value, unit, timestamp, source)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    #[serde(rename = "v")]
    pub value: f64,
    #[serde(rename = "u", default)]
    pub unit: Unit,
    /// When the value was measured (ms since the epoch).
    pub ts: TimestampMs,
    /// Where it came from, e.g. `hostd/nvidia-smi`, `esp/sse`, `expr`. Informational.
    #[serde(rename = "src", default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

impl Reading {
    pub fn new(value: f64, unit: Unit, ts: TimestampMs, source: impl Into<String>) -> Reading {
        Reading {
            value,
            unit,
            ts,
            source: source.into(),
        }
    }
}

/// The result of looking a sensor up at a given time.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// Present and younger than its stale limit.
    Fresh(Reading),
    /// Present but too old. Strict functions treat it as missing; `age()` still sees it.
    Stale { reading: Reading, age_ms: i64 },
    /// Never seen, or removed (the card does not report that metric).
    Absent,
}

impl Lookup {
    pub fn fresh(&self) -> Option<&Reading> {
        match self {
            Lookup::Fresh(r) => Some(r),
            _ => None,
        }
    }
}

/// Anything expressions can read from.
pub trait SensorSource {
    /// Looks one concrete id up as of `now`.
    fn lookup(&self, id: &SensorId, now: TimestampMs) -> Lookup;
    /// All known concrete ids matching a pattern (`gpu.*.hbm`), in a stable order.
    fn matching(&self, pattern: &SensorId) -> Vec<SensorId>;
}

/// What validation knows about the sensors a server has (SUPERVISOR.md 4.2: "validation rejects
/// expressions whose sources do not exist on that server").
pub trait SensorCatalog {
    fn exists(&self, id: &SensorId) -> bool;
    /// The unit, if known up front.
    fn unit(&self, id: &SensorId) -> Option<Unit>;
    /// Sources that are often absent (e.g. `hbm` on a card without the sensor), so a strict
    /// function over them deserves a warning.
    fn often_absent(&self, _id: &SensorId) -> bool {
        false
    }
    /// Concrete ids for a pattern.
    fn expand(&self, pattern: &SensorId) -> Vec<SensorId>;
}

/// Default stale limits per namespace (ms). A source config may override them.
pub fn default_stale_ms(ns: Namespace) -> i64 {
    match ns {
        Namespace::Gpu => 3_000, // the pusher's SAMPLE_STALE
        Namespace::Hwmon => 10_000,
        Namespace::Host => 15_000,
        Namespace::Bmc => 30_000,
        Namespace::Esp => 30_000,
        Namespace::Fan => 30_000,
        Namespace::Engine => 15_000,
        Namespace::Sensor => 10_000,
        Namespace::Ha | Namespace::Mqtt => 120_000,
    }
}

/// The latest reading per sensor, with staleness rules. Implements [`SensorSource`] and
/// [`SensorCatalog`] (a sensor "exists" once it has been seen).
#[derive(Debug, Clone, Default)]
pub struct SensorStore {
    readings: BTreeMap<SensorId, Reading>,
    stale_ns: HashMap<Namespace, i64>,
    stale_id: HashMap<SensorId, i64>,
}

impl SensorStore {
    pub fn new() -> SensorStore {
        SensorStore::default()
    }

    /// Overrides the stale limit for a whole namespace.
    pub fn set_stale_namespace(&mut self, ns: Namespace, ms: i64) {
        self.stale_ns.insert(ns, ms);
    }

    /// Overrides the stale limit for one sensor.
    pub fn set_stale(&mut self, id: SensorId, ms: i64) {
        self.stale_id.insert(id, ms);
    }

    pub fn stale_ms(&self, id: &SensorId) -> i64 {
        self.stale_id
            .get(id)
            .or_else(|| self.stale_ns.get(&id.namespace()))
            .copied()
            .unwrap_or_else(|| default_stale_ms(id.namespace()))
    }

    /// Stores a reading. Non-finite values are not stored: the sensor becomes absent instead
    /// (a NaN is not a reading, and it must never turn into 0 downstream).
    pub fn put(&mut self, id: SensorId, reading: Reading) {
        if reading.value.is_finite() {
            self.readings.insert(id, reading);
        } else {
            self.readings.remove(&id);
        }
    }

    /// Makes a sensor absent (the source said the metric is gone).
    pub fn remove(&mut self, id: &SensorId) {
        self.readings.remove(id);
    }

    pub fn get(&self, id: &SensorId) -> Option<&Reading> {
        self.readings.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SensorId, &Reading)> {
        self.readings.iter()
    }

    pub fn len(&self) -> usize {
        self.readings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.readings.is_empty()
    }

    /// Stores many readings.
    pub fn extend(&mut self, it: impl IntoIterator<Item = (SensorId, Reading)>) {
        for (id, r) in it {
            self.put(id, r);
        }
    }
}

impl SensorSource for SensorStore {
    fn lookup(&self, id: &SensorId, now: TimestampMs) -> Lookup {
        match self.readings.get(id) {
            None => Lookup::Absent,
            Some(r) => {
                let age_ms = now - r.ts;
                if age_ms > self.stale_ms(id) {
                    Lookup::Stale {
                        reading: r.clone(),
                        age_ms,
                    }
                } else {
                    Lookup::Fresh(r.clone())
                }
            }
        }
    }

    fn matching(&self, pattern: &SensorId) -> Vec<SensorId> {
        self.readings
            .keys()
            .filter(|k| k.matches(pattern))
            .cloned()
            .collect()
    }
}

impl SensorCatalog for SensorStore {
    fn exists(&self, id: &SensorId) -> bool {
        self.readings.contains_key(id)
    }
    fn unit(&self, id: &SensorId) -> Option<Unit> {
        self.readings.get(id).map(|r| r.unit)
    }
    fn expand(&self, pattern: &SensorId) -> Vec<SensorId> {
        self.matching(pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staleness() {
        let mut s = SensorStore::new();
        let id = SensorId::parse("gpu.A.core").unwrap();
        s.put(id.clone(), Reading::new(50.0, Unit::Celsius, 1_000, "t"));
        assert!(matches!(s.lookup(&id, 3_000), Lookup::Fresh(_)));
        assert!(matches!(
            s.lookup(&id, 4_001),
            Lookup::Stale { age_ms: 3_001, .. }
        ));
        s.set_stale(id.clone(), 10_000);
        assert!(matches!(s.lookup(&id, 4_001), Lookup::Fresh(_)));
        assert_eq!(
            s.lookup(&SensorId::parse("gpu.A.hbm").unwrap(), 0),
            Lookup::Absent
        );
    }

    #[test]
    fn nan_is_absent_not_zero() {
        let mut s = SensorStore::new();
        let id = SensorId::parse("gpu.D.hbm").unwrap();
        s.put(id.clone(), Reading::new(40.0, Unit::Celsius, 0, "t"));
        s.put(id.clone(), Reading::new(f64::NAN, Unit::Celsius, 1, "t"));
        assert_eq!(s.lookup(&id, 1), Lookup::Absent);
    }

    #[test]
    fn units_combine() {
        assert_eq!(Unit::Celsius.combine(Unit::None), Some(Unit::Celsius));
        assert_eq!(Unit::Unknown.combine(Unit::Watts), Some(Unit::Watts));
        assert_eq!(Unit::Celsius.combine(Unit::Watts), None);
        assert_eq!(Unit::from_text("degrees C"), Unit::Celsius);
    }
}
