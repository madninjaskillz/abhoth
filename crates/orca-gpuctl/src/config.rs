//! `[cards.X.power]` (SUPERVISOR.md 6.1).
//!
//! ```toml
//! [cards.A.power]
//! persistence       = true
//! power_limit_w     = 350
//! serving           = "unlocked"           # boost freely (user decision: no -lgc 1597,1597)
//! expect_max_sm_mhz = 1597                 # drift: busy + cool + below this = alert
//! idle              = { after = "60s", lock_mhz = 135, wake = ["router", "sniff", "util"] }
//! reassert          = ["boot", "engine_ready", "every:5m"]
//! on_mismatch       = "fix"
//! allow_cap         = false                # the HA/UI clock cap is not offered for A
//! ```
//!
//! `serving` kinds: `"unlocked"`, `{ unlocked = { cap = 1500 } }`, `{ cap = 1500 }` (the same),
//! `{ pin = 1597 }` and `{ range = [lo, hi] }`. `idle` is the idle-down policy, or `false`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The default floor of a `-lgc lo,hi` window (the V100's lowest clock).
pub const DEFAULT_FLOOR_MHZ: u32 = 135;
/// The longest any hold or override may last (SUPERVISOR.md 2.3, "Overrides expire").
pub const MAX_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardPowerConfig {
    /// `nvidia-smi -pm 1` at boot and on assert. Without it the driver drops `-lgc` locks.
    #[serde(default)]
    pub persistence: bool,
    /// `-pl`, in watts.
    #[serde(default)]
    pub power_limit_w: Option<u32>,
    /// `[mem, sm]` for `-ac` (card B: `[877, 1380]`).
    #[serde(default)]
    pub app_clocks: Option<[u32; 2]>,
    /// Clocks while an engine on the card is Ready or busy.
    #[serde(default)]
    pub serving: Serving,
    /// Idle clock-down, or `false`.
    #[serde(default)]
    pub idle: IdleSetting,
    /// When the policy is re-applied and checked.
    #[serde(default = "default_reassert")]
    pub reassert: Vec<Reassert>,
    /// What a periodic check does with a mismatch. Default: `fix` for `pin`, `warn` otherwise.
    #[serde(default)]
    pub on_mismatch: Option<OnMismatch>,
    /// Whether the per-card clock cap (the old HA "GPU Max Clock") may apply. Default: false for
    /// `pin`, true otherwise. Card A sets it to false explicitly.
    #[serde(default)]
    pub allow_cap: Option<bool>,
    /// The clock a busy, cool, unthrottled card is expected to reach (A: 1597). Without it the
    /// card's own `clocks.max.sm` is used.
    #[serde(default)]
    pub expect_max_sm_mhz: Option<u32>,
    /// The bottom of a `-lgc lo,hi` window for caps and ranges.
    #[serde(default)]
    pub floor_mhz: Option<u32>,
    /// Power and idle control on a card that layouts do not manage (card C).
    #[serde(default)]
    pub manage_power: Option<bool>,
    #[serde(default)]
    pub drift: DriftConfig,
}

impl CardPowerConfig {
    pub fn floor(&self) -> u32 {
        self.floor_mhz.unwrap_or(DEFAULT_FLOOR_MHZ)
    }

    pub fn on_mismatch(&self) -> OnMismatch {
        self.on_mismatch.unwrap_or(match self.serving {
            Serving::Pin(_) => OnMismatch::Fix,
            _ => OnMismatch::Warn,
        })
    }

    /// Whether the clock cap is offered for this card (UI and HA).
    pub fn cap_offered(&self) -> bool {
        self.allow_cap
            .unwrap_or(!matches!(self.serving, Serving::Pin(_)))
    }

    pub fn idle(&self) -> Option<&IdleConfig> {
        match &self.idle {
            IdleSetting::Off => None,
            IdleSetting::On(c) => Some(c),
        }
    }

    pub fn periodic(&self) -> Option<Duration> {
        self.reassert.iter().find_map(|r| match r {
            Reassert::Every(d) => Some(*d),
            _ => None,
        })
    }

    pub fn reasserts_on(&self, r: &Reassert) -> bool {
        self.reassert.iter().any(|x| x == r)
    }

    /// Static checks that do not need the limits file.
    pub fn check(&self) -> Result<(), String> {
        match self.serving {
            Serving::Range(lo, hi) if lo > hi => {
                return Err(format!("serving range {lo}..{hi} is backwards"));
            }
            Serving::Unlocked { cap: Some(c) } | Serving::Pin(c) if c < self.floor() => {
                return Err(format!(
                    "serving clock {c} MHz is below the floor {} MHz",
                    self.floor()
                ));
            }
            _ => {}
        }
        if let Some(i) = self.idle() {
            if i.after.is_zero() {
                return Err("idle.after must be more than 0".into());
            }
            if i.wake.is_empty() {
                return Err("idle.wake needs at least one source".into());
            }
        }
        Ok(())
    }
}

fn default_reassert() -> Vec<Reassert> {
    vec![
        Reassert::Boot,
        Reassert::EngineReady,
        Reassert::Every(Duration::from_secs(300)),
    ]
}

/// The serving clock policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ServingRepr", into = "ServingRepr")]
pub enum Serving {
    /// No lock (`-rgc`); boost allowed up to the hardware max, or to `cap` (`-lgc floor,cap`).
    Unlocked { cap: Option<u32> },
    /// `-lgc n,n`.
    Pin(u32),
    /// `-lgc lo,hi`.
    Range(u32, u32),
}

impl Default for Serving {
    fn default() -> Self {
        Serving::Unlocked { cap: None }
    }
}

impl Serving {
    pub fn kind(&self) -> &'static str {
        match self {
            Serving::Unlocked { .. } => "unlocked",
            Serving::Pin(_) => "pin",
            Serving::Range(..) => "range",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ServingRepr {
    Word(String),
    Table(ServingTable),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServingTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    range: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unlocked: Option<UnlockedTable>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnlockedTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cap: Option<u32>,
}

impl TryFrom<ServingRepr> for Serving {
    type Error = String;
    fn try_from(r: ServingRepr) -> Result<Self, String> {
        match r {
            ServingRepr::Word(w) if w == "unlocked" => Ok(Serving::Unlocked { cap: None }),
            ServingRepr::Word(w) => Err(format!(
                "unknown serving policy {w:?} (unlocked, pin, range, cap)"
            )),
            ServingRepr::Table(t) => {
                let set = [
                    t.pin.is_some(),
                    t.range.is_some(),
                    t.cap.is_some(),
                    t.unlocked.is_some(),
                ];
                if set.iter().filter(|x| **x).count() != 1 {
                    return Err("serving needs exactly one of pin, range, cap, unlocked".into());
                }
                Ok(if let Some(p) = t.pin {
                    Serving::Pin(p)
                } else if let Some([lo, hi]) = t.range {
                    Serving::Range(lo, hi)
                } else if let Some(c) = t.cap {
                    Serving::Unlocked { cap: Some(c) }
                } else {
                    Serving::Unlocked {
                        cap: t.unlocked.and_then(|u| u.cap),
                    }
                })
            }
        }
    }
}

impl From<Serving> for ServingRepr {
    fn from(s: Serving) -> Self {
        match s {
            Serving::Unlocked { cap: None } => ServingRepr::Word("unlocked".into()),
            Serving::Unlocked { cap: Some(c) } => ServingRepr::Table(ServingTable {
                cap: Some(c),
                ..Default::default()
            }),
            Serving::Pin(p) => ServingRepr::Table(ServingTable {
                pin: Some(p),
                ..Default::default()
            }),
            Serving::Range(lo, hi) => ServingRepr::Table(ServingTable {
                range: Some([lo, hi]),
                ..Default::default()
            }),
        }
    }
}

/// `idle = false` or `idle = { … }`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IdleSetting {
    #[default]
    #[serde(with = "false_only")]
    Off,
    On(IdleConfig),
}

mod false_only {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(false)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        match bool::deserialize(d)? {
            false => Ok(()),
            true => Err(serde::de::Error::custom(
                "idle = true needs a table: idle = { after = \"60s\", … }",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleConfig {
    #[serde(with = "dur", default = "default_after")]
    pub after: Duration,
    #[serde(default = "default_lock")]
    pub lock_mhz: u32,
    /// Utilisation at or below this is "no work" (gpu-power `UTIL_IDLE`).
    #[serde(default)]
    pub util_at_most: u32,
    #[serde(default = "default_wake")]
    pub wake: Vec<WakeSource>,
    /// Ports the sniffer watches for this card, on top of its engines' ports.
    #[serde(default)]
    pub sniff_ports: Vec<u16>,
}

fn default_after() -> Duration {
    Duration::from_secs(60)
}
fn default_lock() -> u32 {
    DEFAULT_FLOOR_MHZ
}
fn default_wake() -> Vec<WakeSource> {
    vec![WakeSource::Router, WakeSource::Sniff, WakeSource::Util]
}

/// What woke a card. `router`, `sniff` and `util` are configurable per card; the others are
/// always honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeSource {
    Router,
    Sniff,
    Util,
    /// The compute-process list changed (gpu-power "procs").
    Procs,
    /// `POST /admin/api/cards/X/wake`.
    Admin,
    Hold,
    Assert,
    /// NVML could not be read while low: back to serving, never act blind.
    Error,
}

impl WakeSource {
    pub fn configurable(self) -> bool {
        matches!(
            self,
            WakeSource::Router | WakeSource::Sniff | WakeSource::Util
        )
    }
    pub fn as_str(self) -> &'static str {
        match self {
            WakeSource::Router => "router",
            WakeSource::Sniff => "sniff",
            WakeSource::Util => "util",
            WakeSource::Procs => "procs",
            WakeSource::Admin => "admin",
            WakeSource::Hold => "hold",
            WakeSource::Assert => "assert",
            WakeSource::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMismatch {
    Fix,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Reassert {
    Boot,
    EngineReady,
    DriverReload,
    Every(Duration),
}

impl TryFrom<String> for Reassert {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Ok(match s.as_str() {
            "boot" => Reassert::Boot,
            "engine_ready" => Reassert::EngineReady,
            "driver_reload" => Reassert::DriverReload,
            x => match x.strip_prefix("every:") {
                Some(d) => Reassert::Every(parse_duration(d)?),
                None => {
                    return Err(format!(
                        "unknown reassert {x:?} (boot, engine_ready, every:5m, driver_reload)"
                    ));
                }
            },
        })
    }
}

impl From<Reassert> for String {
    fn from(r: Reassert) -> String {
        match r {
            Reassert::Boot => "boot".into(),
            Reassert::EngineReady => "engine_ready".into(),
            Reassert::DriverReload => "driver_reload".into(),
            Reassert::Every(d) => format!("every:{}", format_duration(d)),
        }
    }
}

/// Drift detection: a card that is busy and cool but runs below its expected max clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftConfig {
    #[serde(default = "d_busy")]
    pub busy_util_at_least: u32,
    #[serde(default = "d_cool")]
    pub cool_below_c: u32,
    #[serde(default = "d_tol")]
    pub tolerance_mhz: u32,
    #[serde(with = "dur", default = "d_for")]
    pub sustained: Duration,
}

impl Default for DriftConfig {
    fn default() -> Self {
        DriftConfig {
            busy_util_at_least: d_busy(),
            cool_below_c: d_cool(),
            tolerance_mhz: d_tol(),
            sustained: d_for(),
        }
    }
}
fn d_busy() -> u32 {
    50
}
fn d_cool() -> u32 {
    70
}
fn d_tol() -> u32 {
    45
}
fn d_for() -> Duration {
    Duration::from_secs(30)
}

/// `"60s"`, `"5m"`, `"1h"`, `"500ms"` or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    Ok(match unit {
        "" | "s" => Duration::from_secs(n),
        "ms" => Duration::from_millis(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return Err(format!("bad duration unit in {s:?} (ms, s, m, h)")),
    })
}

pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if ms.is_multiple_of(3_600_000) && ms > 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) && ms > 0 {
        format!("{}m", ms / 60_000)
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

mod dur {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format_duration(*d))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        super::parse_duration(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<CardPowerConfig, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }

    #[test]
    fn card_a_unlocked_boost() {
        let c = parse(
            r#"
            persistence = true
            power_limit_w = 350
            serving = "unlocked"
            expect_max_sm_mhz = 1597
            idle = { after = "60s", lock_mhz = 135, wake = ["router", "sniff", "util"] }
            reassert = ["boot", "engine_ready", "every:5m"]
            on_mismatch = "fix"
            allow_cap = false
            "#,
        )
        .unwrap();
        assert_eq!(c.serving, Serving::Unlocked { cap: None });
        assert!(!c.cap_offered());
        assert_eq!(c.periodic(), Some(Duration::from_secs(300)));
        assert_eq!(c.idle().unwrap().lock_mhz, 135);
        assert_eq!(c.on_mismatch(), OnMismatch::Fix);
        c.check().unwrap();
    }

    #[test]
    fn serving_kinds() {
        let s = |t: &str| parse(&format!("serving = {t}")).map(|c| c.serving);
        assert_eq!(s(r#""unlocked""#).unwrap(), Serving::Unlocked { cap: None });
        assert_eq!(s("{ pin = 1597 }").unwrap(), Serving::Pin(1597));
        assert_eq!(
            s("{ range = [1200, 1500] }").unwrap(),
            Serving::Range(1200, 1500)
        );
        assert_eq!(
            s("{ cap = 1400 }").unwrap(),
            Serving::Unlocked { cap: Some(1400) }
        );
        assert_eq!(
            s("{ unlocked = { cap = 1500 } }").unwrap(),
            Serving::Unlocked { cap: Some(1500) }
        );
        assert!(s(r#""boost""#).is_err());
        assert!(s("{ pin = 1, cap = 2 }").is_err());
        assert!(s("{ turbo = 1 }").is_err());
    }

    #[test]
    fn defaults_and_idle_false() {
        let c = parse("idle = false").unwrap();
        assert!(c.idle().is_none());
        assert_eq!(c.serving, Serving::Unlocked { cap: None });
        assert!(c.cap_offered());
        assert_eq!(c.on_mismatch(), OnMismatch::Warn);
        assert!(parse("idle = true").is_err());
        let p = parse("serving = { pin = 1597 }").unwrap();
        assert!(!p.cap_offered());
        assert_eq!(p.on_mismatch(), OnMismatch::Fix);
        assert!(parse("bogus = 1").is_err());
    }

    #[test]
    fn checks() {
        assert!(
            parse("serving = { range = [1500, 1200] }")
                .unwrap()
                .check()
                .is_err()
        );
        assert!(parse("serving = { pin = 100 }").unwrap().check().is_err());
        assert!(parse("idle = { wake = [] }").unwrap().check().is_err());
        assert!(parse("reassert = [\"every:soon\"]").is_err());
    }

    #[test]
    fn round_trip() {
        let c = parse(
            r#"serving = { cap = 1400 }
            idle = { after = "90s", sniff_ports = [8188] }
            reassert = ["every:5m", "engine_ready"]"#,
        )
        .unwrap();
        let back: CardPowerConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
        assert!(parse_duration("5d").is_err());
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
    }
}
