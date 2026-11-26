//! Sensor ids: dotted source paths (SUPERVISOR.md 4.1).
//!
//! A segment is either plain (`[A-Za-z0-9_]` plus `-` when it is followed by a letter or `_`,
//! so `esp.gpu-fans` is one segment but `gpu.A.core-5` is a subtraction) or quoted
//! (`bmc."CPU1 Temp"`, with `\"` and `\\` escapes). A `*` segment makes the id a pattern
//! (`gpu.*.hbm`), which only expression functions over several arguments accept.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The first segment of a sensor id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Namespace {
    /// `gpu.<card>.<metric>`: hostd's GPU sampler (NVML / nvidia-smi).
    Gpu,
    /// `bmc.<sdr name>`: `ipmitool sdr`.
    Bmc,
    /// `hwmon.<chip>.<label>`: sysfs hwmon.
    Hwmon,
    /// `esp.<device>.<entity>`: ESPHome SSE.
    Esp,
    /// `ha.<entity_id>`: Home Assistant states.
    Ha,
    /// `mqtt.<alias>`: an MQTT topic + JSON path from config.
    Mqtt,
    /// `host.<metric>`: hostd `/proc`.
    Host,
    /// `engine.<id>.<metric>`: engine metrics from the telemetry hub.
    Engine,
    /// `fan.<id>.<metric>`: fan backend readings (duty, rpm, failsafe), whatever the backend.
    Fan,
    /// `sensor.<name>`: a virtual sensor, a named expression.
    Sensor,
}

impl Namespace {
    pub const ALL: [Namespace; 10] = [
        Namespace::Gpu,
        Namespace::Bmc,
        Namespace::Hwmon,
        Namespace::Esp,
        Namespace::Ha,
        Namespace::Mqtt,
        Namespace::Host,
        Namespace::Engine,
        Namespace::Fan,
        Namespace::Sensor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Namespace::Gpu => "gpu",
            Namespace::Bmc => "bmc",
            Namespace::Hwmon => "hwmon",
            Namespace::Esp => "esp",
            Namespace::Ha => "ha",
            Namespace::Mqtt => "mqtt",
            Namespace::Host => "host",
            Namespace::Engine => "engine",
            Namespace::Fan => "fan",
            Namespace::Sensor => "sensor",
        }
    }

    pub fn parse(s: &str) -> Option<Namespace> {
        Namespace::ALL.into_iter().find(|n| n.as_str() == s)
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("empty sensor id")]
    Empty,
    #[error(
        "unknown sensor namespace `{0}` (expected gpu, bmc, hwmon, esp, ha, mqtt, host, engine, fan or sensor)"
    )]
    UnknownNamespace(String),
    #[error("sensor id `{0}` needs at least one segment after the namespace")]
    TooShort(String),
    #[error("bad sensor id `{text}` at byte {at}: {why}")]
    Syntax {
        text: String,
        at: usize,
        why: &'static str,
    },
}

/// A sensor id or pattern. Ordered and hashable; serialises as its canonical text.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SensorId {
    ns: Namespace,
    /// Segments after the namespace; `*` is a wildcard segment.
    rest: Vec<String>,
}

impl SensorId {
    /// Builds an id from a namespace and segments (no parsing, no quoting needed).
    pub fn new<I, S>(ns: Namespace, rest: I) -> Result<SensorId, IdError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let rest: Vec<String> = rest.into_iter().map(Into::into).collect();
        if rest.is_empty() || rest.iter().any(String::is_empty) {
            return Err(IdError::TooShort(ns.to_string()));
        }
        Ok(SensorId { ns, rest })
    }

    /// Shorthand for ids built from code with known-good parts. Panics on an empty part.
    pub fn of(ns: Namespace, rest: &[&str]) -> SensorId {
        SensorId::new(ns, rest.iter().copied()).expect("valid sensor id parts")
    }

    /// Parses `gpu.A.core`, `bmc."CPU1 Temp"`, `gpu.*.hbm`.
    pub fn parse(text: &str) -> Result<SensorId, IdError> {
        let (id, used) = parse_path(text, 0)?;
        if used != text.len() {
            return Err(IdError::Syntax {
                text: text.into(),
                at: used,
                why: "trailing characters",
            });
        }
        Ok(id)
    }

    pub fn namespace(&self) -> Namespace {
        self.ns
    }

    /// The segments after the namespace.
    pub fn segments(&self) -> &[String] {
        &self.rest
    }

    /// True if any segment is `*`.
    pub fn is_pattern(&self) -> bool {
        self.rest.iter().any(|s| s == "*")
    }

    /// Does this concrete id match `pattern` (segment-wise, `*` matches one segment)?
    pub fn matches(&self, pattern: &SensorId) -> bool {
        self.ns == pattern.ns
            && self.rest.len() == pattern.rest.len()
            && self
                .rest
                .iter()
                .zip(&pattern.rest)
                .all(|(s, p)| p == "*" || s == p)
    }

    /// The last segment (the metric for `gpu.A.core`).
    pub fn leaf(&self) -> &str {
        self.rest.last().map(String::as_str).unwrap_or("")
    }

    /// A new id with one more segment.
    pub fn child(&self, seg: impl Into<String>) -> SensorId {
        let mut rest = self.rest.clone();
        rest.push(seg.into());
        SensorId { ns: self.ns, rest }
    }
}

pub(crate) fn is_plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Parses a path starting at byte `start`; returns the id and the byte offset after it.
/// Used by the expression lexer too.
pub(crate) fn parse_path(text: &str, start: usize) -> Result<(SensorId, usize), IdError> {
    let bytes = text.as_bytes();
    let err = |at: usize, why: &'static str| IdError::Syntax {
        text: text.into(),
        at,
        why,
    };
    let mut segs: Vec<String> = Vec::new();
    let mut i = start;
    loop {
        // One segment.
        if i >= bytes.len() {
            return Err(if segs.is_empty() {
                IdError::Empty
            } else {
                err(i, "expected a segment after `.`")
            });
        }
        let c = bytes[i] as char;
        if c == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                let Some(ch) = text[i..].chars().next() else {
                    return Err(err(i, "unterminated quoted segment"));
                };
                i += ch.len_utf8();
                match ch {
                    '"' => break,
                    '\\' => {
                        let Some(e) = text[i..].chars().next() else {
                            return Err(err(i, "unterminated escape"));
                        };
                        i += e.len_utf8();
                        s.push(e);
                    }
                    _ => s.push(ch),
                }
            }
            if s.is_empty() {
                return Err(err(i, "empty quoted segment"));
            }
            segs.push(s);
        } else if c == '*' && !segs.is_empty() {
            segs.push("*".into());
            i += 1;
        } else if is_plain_char(c) {
            let s0 = i;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                let dash_in_name = ch == '-'
                    && i + 1 < bytes.len()
                    && ((bytes[i + 1] as char).is_ascii_alphabetic() || bytes[i + 1] == b'_');
                if is_plain_char(ch) || dash_in_name {
                    i += 1;
                } else {
                    break;
                }
            }
            segs.push(text[s0..i].to_string());
        } else {
            return Err(if segs.is_empty() {
                IdError::Empty
            } else {
                err(i, "expected a segment")
            });
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            continue;
        }
        break;
    }
    let first = segs.remove(0);
    let ns = Namespace::parse(&first).ok_or(IdError::UnknownNamespace(first))?;
    if segs.is_empty() {
        return Err(IdError::TooShort(ns.to_string()));
    }
    Ok((SensorId { ns, rest: segs }, i))
}

fn needs_quotes(seg: &str) -> bool {
    if seg == "*" {
        return false;
    }
    let b = seg.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        let ch = c as char;
        if is_plain_char(ch)
            || (ch == '-'
                && i > 0
                && i + 1 < b.len()
                && ((b[i + 1] as char).is_ascii_alphabetic() || b[i + 1] == b'_'))
        {
            continue;
        }
        return true;
    }
    seg.is_empty()
}

impl fmt::Display for SensorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.ns.as_str())?;
        for seg in &self.rest {
            f.write_str(".")?;
            if needs_quotes(seg) {
                f.write_str("\"")?;
                for ch in seg.chars() {
                    if ch == '"' || ch == '\\' {
                        f.write_str("\\")?;
                    }
                    write!(f, "{ch}")?;
                }
                f.write_str("\"")?;
            } else {
                f.write_str(seg)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for SensorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SensorId({self})")
    }
}

impl std::str::FromStr for SensorId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SensorId::parse(s)
    }
}

impl Serialize for SensorId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SensorId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        SensorId::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints() {
        for t in [
            "gpu.A.core",
            "bmc.\"CPU1 Temp\"",
            "esp.gpu-fans.\"Fan 0 RPM\"",
            "ha.sensor.ecoflow_ac_out_power",
            "gpu.*.hbm",
            "hwmon.k10temp.Tctl",
            "bmc.\"a\\\"b\"",
        ] {
            let id = SensorId::parse(t).unwrap();
            assert_eq!(id.to_string(), t);
            assert_eq!(SensorId::parse(&id.to_string()).unwrap(), id);
        }
        let id = SensorId::parse("esp.gpu-fans.\"Fan 0 RPM\"").unwrap();
        assert_eq!(id.segments(), ["gpu-fans", "Fan 0 RPM"]);
    }

    #[test]
    fn rejects() {
        assert!(matches!(
            SensorId::parse("foo.bar"),
            Err(IdError::UnknownNamespace(_))
        ));
        assert!(matches!(SensorId::parse("gpu"), Err(IdError::TooShort(_))));
        assert!(SensorId::parse("gpu.").is_err());
        assert!(SensorId::parse("gpu.A.core-5").is_err()); // trailing "-5": that is a subtraction
        assert!(SensorId::parse("*.A").is_err());
    }

    #[test]
    fn patterns() {
        let p = SensorId::parse("gpu.*.hbm").unwrap();
        assert!(p.is_pattern());
        assert!(SensorId::parse("gpu.A.hbm").unwrap().matches(&p));
        assert!(!SensorId::parse("gpu.A.core").unwrap().matches(&p));
        assert!(!SensorId::parse("gpu.A.hbm.x").unwrap().matches(&p));
    }

    #[test]
    fn serde_as_text() {
        let id = SensorId::parse("bmc.\"CPU1 Temp\"").unwrap();
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, "\"bmc.\\\"CPU1 Temp\\\"\"");
        assert_eq!(serde_json::from_str::<SensorId>(&j).unwrap(), id);
    }
}
