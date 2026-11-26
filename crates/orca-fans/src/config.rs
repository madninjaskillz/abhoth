//! The fan tables of `supervisor.toml` (ROUTER-CONFIG.md 4.11, SUPERVISOR.md 5.1-5.2):
//! `[fan_backends.*]`, `[fan_defaults]`, `[fans.*]` and `[fan_profiles.*]`.
//!
//! Parsing is strict (`deny_unknown_fields`) so a typo cannot silently drop a safety field.
//! [`FansConfig::resolve`] merges `[fan_defaults]` into each fan and validates the result.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::limits::{FanLimits, MAX_OVERRIDE_TTL_S};

/// Every fan table of one config file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FansConfig {
    #[serde(default)]
    pub fan_backends: BTreeMap<String, BackendConfig>,
    #[serde(default)]
    pub fan_defaults: FanFields,
    #[serde(default)]
    pub fans: BTreeMap<String, FanTable>,
    #[serde(default)]
    pub fan_profiles: BTreeMap<String, ProfileConfig>,
}

impl FansConfig {
    /// Parses the fan tables out of a whole `supervisor.toml`, ignoring every other table.
    pub fn from_toml(text: &str) -> Result<FansConfig, ConfigError> {
        let mut doc: toml::Table =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let mut only = toml::Table::new();
        for key in ["fan_backends", "fan_defaults", "fans", "fan_profiles"] {
            if let Some(v) = doc.remove(key) {
                only.insert(key.to_string(), v);
            }
        }
        toml::Value::Table(only)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("fan config: {0}")]
    Parse(String),
    #[error("fan config: {0}")]
    Invalid(String),
}

/// `kind = "…"` of a `[fan_backends.*]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Esphome,
    Hwmon,
    Ipmi,
    Liquidctl,
    Nvml,
    Windows,
    Fake,
}

impl BackendKind {
    /// The control mode the hardware supports (SUPERVISOR.md 5.1).
    pub fn native_mode(self) -> FanMode {
        match self {
            BackendKind::Esphome | BackendKind::Windows => FanMode::DeviceCurve,
            BackendKind::Liquidctl | BackendKind::Hwmon | BackendKind::Ipmi | BackendKind::Nvml => {
                FanMode::HostCurve
            }
            BackendKind::Fake => FanMode::DeviceCurve,
        }
    }
}

/// `mode = "act" | "shadow"` on a backend: shadow computes and logs, never writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Act,
    #[default]
    Shadow,
}

/// What a host-curve backend does when hostd exits or its inputs go stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnExit {
    /// Hand the fan back to the hardware's own automatic mode.
    #[default]
    Auto,
    /// Full speed.
    Full,
}

/// A secret: `"secret:<name>"` (the `[secrets]` table), `{ file = … }` or `{ env = … }`.
/// A plain string that is not a `secret:` reference is refused, so a password can never sit
/// in the config file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    Named(String),
    File(String),
    Env(String),
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            T {
                file: Option<String>,
                env: Option<String>,
            },
        }
        match Raw::deserialize(d)? {
            Raw::S(s) => match s.strip_prefix("secret:") {
                Some(n) if !n.is_empty() => Ok(SecretRef::Named(n.to_string())),
                _ => Err(serde::de::Error::custom(
                    "a secret must be \"secret:<name>\", { file = … } or { env = … }, never the value itself",
                )),
            },
            Raw::T {
                file: Some(f),
                env: None,
            } => Ok(SecretRef::File(f)),
            Raw::T {
                file: None,
                env: Some(e),
            } => Ok(SecretRef::Env(e)),
            Raw::T { .. } => Err(serde::de::Error::custom(
                "a secret table needs exactly one of file or env",
            )),
        }
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            SecretRef::Named(n) => s.serialize_str(&format!("secret:{n}")),
            SecretRef::File(f) => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("file", f)?;
                m.end()
            }
            SecretRef::Env(e) => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("env", e)?;
                m.end()
            }
        }
    }
}

impl SecretRef {
    /// Resolves the value. `named` looks up `secret:<name>` references (the `[secrets]` table).
    pub fn resolve(&self, named: &dyn Fn(&str) -> Option<SecretRef>) -> Result<String, String> {
        match self {
            SecretRef::Named(n) => match named(n) {
                Some(SecretRef::Named(_)) | None => Err(format!("secret {n:?} is not defined")),
                Some(r) => r.resolve(named),
            },
            SecretRef::File(f) => std::fs::read_to_string(f)
                .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
                .map_err(|e| format!("secret file {f}: {e}")),
            SecretRef::Env(e) => std::env::var(e).map_err(|_| format!("secret env {e} is not set")),
        }
    }
}

/// A value that may be given inline or via a secret reference (user names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaybeSecret {
    Plain(String),
    Secret(SecretRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestAuth {
    /// Always `"digest"` (ESPHome's `web_server` auth type).
    pub kind: String,
    pub user: String,
    pub password: SecretRef,
}

/// Entity names on an ESPHome device; `{n}` is replaced by the channel number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EspNames {
    #[serde(default = "default_temp_name")]
    pub temp: String,
    #[serde(default = "default_load_name")]
    pub load: String,
    #[serde(default = "default_rpm_name")]
    pub rpm: String,
    #[serde(default = "default_duty_name")]
    pub duty: String,
    #[serde(default = "default_ff_name")]
    pub ff_duty: String,
    #[serde(default = "default_stalled_name")]
    pub stalled: String,
    #[serde(default = "default_failsafe_name")]
    pub failsafe: String,
}

fn default_temp_name() -> String {
    "GPU {n} Temp".into()
}
fn default_load_name() -> String {
    "GPU {n} Power".into()
}
fn default_rpm_name() -> String {
    "Fan {n} RPM".into()
}
fn default_duty_name() -> String {
    "Fan {n} Duty".into()
}
fn default_ff_name() -> String {
    "Fan {n} FF Duty".into()
}
fn default_stalled_name() -> String {
    "Fan {n} Stalled".into()
}
fn default_failsafe_name() -> String {
    "Failsafe Active".into()
}

impl Default for EspNames {
    fn default() -> Self {
        EspNames {
            temp: default_temp_name(),
            load: default_load_name(),
            rpm: default_rpm_name(),
            duty: default_duty_name(),
            ff_duty: default_ff_name(),
            stalled: default_stalled_name(),
            failsafe: default_failsafe_name(),
        }
    }
}

impl EspNames {
    pub fn for_channel(pattern: &str, n: u32) -> String {
        pattern.replace("{n}", &n.to_string())
    }
}

/// `sync_device_settings`: whether config owns the device's curve numbers
/// (SUPERVISOR.md 15 Q3). `false`/`"off"`: never touched. `"diff"`: read and report the
/// differences only. `"write"`: write them, but only after a complete read-only diff in this
/// run (see [`crate::esp::DeviceSync`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncDeviceSettings {
    #[default]
    Off,
    Diff,
    Write,
}

impl Serialize for SyncDeviceSettings {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            SyncDeviceSettings::Off => "off",
            SyncDeviceSettings::Diff => "diff",
            SyncDeviceSettings::Write => "write",
        })
    }
}

impl<'de> Deserialize<'de> for SyncDeviceSettings {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            B(bool),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::B(false) => Ok(SyncDeviceSettings::Off),
            Raw::B(true) => Err(serde::de::Error::custom(
                "sync_device_settings = true is ambiguous: use \"diff\" (read-only) first, then \"write\"",
            )),
            Raw::S(s) => match s.as_str() {
                "off" => Ok(SyncDeviceSettings::Off),
                "diff" => Ok(SyncDeviceSettings::Diff),
                "write" => Ok(SyncDeviceSettings::Write),
                o => Err(serde::de::Error::custom(format!(
                    "sync_device_settings: unknown value {o:?}"
                ))),
            },
        }
    }
}

/// One `[fan_backends.<id>]` table. Fields that do not apply to the kind are refused by
/// [`FansConfig::resolve`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub kind: BackendKind,
    #[serde(default)]
    pub mode: RunMode,
    // esphome
    pub url: Option<String>,
    pub transport: Option<String>,
    pub auth: Option<DigestAuth>,
    #[serde(default)]
    pub names: EspNames,
    pub device_stale_seconds: Option<f64>,
    #[serde(default)]
    pub sync_device_settings: SyncDeviceSettings,
    // hwmon
    /// The hwmon chip `name` (e.g. `nct6798`); hwmonN numbers are not stable.
    pub chip: Option<String>,
    pub on_exit: Option<OnExit>,
    // ipmi
    /// IPMI writes; refused unless the hostd limits file allows them too.
    #[serde(default)]
    pub write: bool,
    pub restore_mode: Option<String>,
    // liquidctl
    #[serde(rename = "match")]
    pub match_: Option<String>,
    // nvml
    pub gpu: Option<String>,
    // windows
    pub sensor_file: Option<String>,
}

/// `{ by = "temp", lo, hi }`: load is scaled to 0 below `lo`, linearly to full at `hi`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadGate {
    #[serde(default)]
    pub by: Option<GateBy>,
    pub lo: f64,
    pub hi: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateBy {
    Temp,
}

/// `{ lo, hi, max_duty }`: load ≤ lo → min_duty, linear to max_duty at hi.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedForward {
    pub lo: f64,
    pub hi: f64,
    pub max_duty: f64,
}

/// A partial feed-forward, for profiles (`feed_forward = { max_duty = 60 }`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedForwardPatch {
    pub lo: Option<f64>,
    pub hi: Option<f64>,
    pub max_duty: Option<f64>,
}

/// `send = { temp_delta, load_delta, min_gap, heartbeat }` for device backends.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendPolicy {
    #[serde(default = "d_temp_delta")]
    pub temp_delta: f64,
    #[serde(default = "d_load_delta")]
    pub load_delta: f64,
    #[serde(
        default = "d_min_gap",
        deserialize_with = "de_duration",
        serialize_with = "ser_duration"
    )]
    pub min_gap: Duration,
    #[serde(
        default = "d_heartbeat",
        deserialize_with = "de_duration",
        serialize_with = "ser_duration"
    )]
    pub heartbeat: Duration,
    /// After a failed push, wait this long before retrying that fan (pusher RETRY).
    #[serde(
        default = "d_retry",
        deserialize_with = "de_duration",
        serialize_with = "ser_duration"
    )]
    pub retry: Duration,
}

fn d_temp_delta() -> f64 {
    1.0
}
fn d_load_delta() -> f64 {
    10.0
}
fn d_min_gap() -> Duration {
    Duration::from_secs(1)
}
fn d_heartbeat() -> Duration {
    Duration::from_secs(10)
}
fn d_retry() -> Duration {
    Duration::from_secs(5)
}

impl Default for SendPolicy {
    fn default() -> Self {
        SendPolicy {
            temp_delta: d_temp_delta(),
            load_delta: d_load_delta(),
            min_gap: d_min_gap(),
            heartbeat: d_heartbeat(),
            retry: d_retry(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertPolicy {
    pub temp_warn: Option<f64>,
    #[serde(default = "yes")]
    pub stalled: bool,
    #[serde(default = "yes")]
    pub failsafe: bool,
    #[serde(default = "yes")]
    pub stale: bool,
}

fn yes() -> bool {
    true
}

impl Default for AlertPolicy {
    fn default() -> Self {
        AlertPolicy {
            temp_warn: None,
            stalled: true,
            failsafe: true,
            stale: true,
        }
    }
}

/// `"2%/2s"`: the largest duty decrease per interval. Ramp-up is always immediate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RampDown {
    pub pct: f64,
    pub per: Duration,
}

impl RampDown {
    pub fn parse(s: &str) -> Result<RampDown, String> {
        let (p, t) = s
            .split_once('/')
            .ok_or_else(|| format!("ramp_down {s:?}: expected \"<pct>%/<duration>\""))?;
        let pct: f64 = p
            .trim()
            .strip_suffix('%')
            .ok_or_else(|| format!("ramp_down {s:?}: the step needs a %"))?
            .trim()
            .parse()
            .map_err(|_| format!("ramp_down {s:?}: bad percentage"))?;
        let per = parse_duration(t.trim())?;
        if !(pct > 0.0 && pct <= 100.0) || per.is_zero() {
            return Err(format!(
                "ramp_down {s:?}: the step must be 0-100 % over a non-zero time"
            ));
        }
        Ok(RampDown { pct, per })
    }

    /// Percent per second.
    pub fn rate(&self) -> f64 {
        self.pct / self.per.as_secs_f64()
    }
}

impl Serialize for RampDown {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}%/{}s", self.pct, self.per.as_secs_f64()))
    }
}

impl<'de> Deserialize<'de> for RampDown {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        RampDown::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// `ms`, `s`, `m`, `h` suffixes; a bare number is seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad duration {s:?}"))?;
    if !(v.is_finite() && v >= 0.0) {
        return Err(format!("bad duration {s:?}"));
    }
    Ok(Duration::from_secs_f64(v * mult))
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        S(String),
        N(f64),
    }
    match Raw::deserialize(d)? {
        Raw::S(s) => parse_duration(&s).map_err(serde::de::Error::custom),
        Raw::N(n) if n.is_finite() && n >= 0.0 => Ok(Duration::from_secs_f64(n)),
        Raw::N(_) => Err(serde::de::Error::custom("bad duration")),
    }
}

fn ser_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

fn ser_opt_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_f64(d.as_secs_f64()),
        None => s.serialize_none(),
    }
}

fn de_opt_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    de_duration(d).map(Some)
}

/// The fan policy fields, shared by `[fan_defaults]` and `[fans.*]` (all optional there).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanFields {
    pub mode: Option<FanMode>,
    pub temp: Option<String>,
    pub load: Option<String>,
    pub load_gate: Option<LoadGate>,
    pub curve: Option<Vec<[f64; 2]>>,
    pub feed_forward: Option<FeedForward>,
    pub min_duty: Option<f64>,
    pub max_temp: Option<f64>,
    pub ramp_down: Option<RampDown>,
    /// Input age after which a host-curve fan fails safe (SUPERVISOR.md 5.2 `stale`).
    #[serde(
        default,
        deserialize_with = "de_opt_duration",
        serialize_with = "ser_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub stale: Option<Duration>,
    /// Device backends: an input sample older than this means "send nothing" (pusher 3 s).
    #[serde(
        default,
        deserialize_with = "de_opt_duration",
        serialize_with = "ser_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub stale_input: Option<Duration>,
    pub send: Option<SendPolicy>,
    pub profile: Option<String>,
    pub alerts: Option<AlertPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    DeviceCurve,
    HostCurve,
}

/// One `[fans.<id>]` table: `backend`, `channel` and the [`FanFields`]. Deserialised by hand
/// because serde's `flatten` would silently disable `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq)]
pub struct FanTable {
    pub backend: String,
    /// Channel on the backend: an ESP fan number, a hwmon pwm index, an IPMI zone, a
    /// liquidctl channel name or an NVML fan index.
    pub channel: Channel,
    pub fields: FanFields,
}

impl<'de> Deserialize<'de> for FanTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut t = toml::Table::deserialize(d)?;
        let backend = match t.remove("backend") {
            Some(toml::Value::String(s)) => s,
            Some(_) => return Err(D::Error::custom("backend must be a string")),
            None => return Err(D::Error::missing_field("backend")),
        };
        let channel = match t.remove("channel") {
            Some(toml::Value::Integer(i)) if (0..=u32::MAX as i64).contains(&i) => {
                Channel::Index(i as u32)
            }
            Some(toml::Value::String(s)) => Channel::Name(s),
            Some(_) => return Err(D::Error::custom("channel must be a number or a name")),
            None => return Err(D::Error::missing_field("channel")),
        };
        let fields: FanFields = toml::Value::Table(t)
            .try_into()
            .map_err(|e: toml::de::Error| D::Error::custom(e.message()))?;
        Ok(FanTable {
            backend,
            channel,
            fields,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Channel {
    Index(u32),
    Name(String),
}

impl Channel {
    pub fn index(&self) -> Option<u32> {
        match self {
            Channel::Index(i) => Some(*i),
            Channel::Name(n) => n.parse().ok(),
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Channel::Index(i) => write!(f, "{i}"),
            Channel::Name(n) => f.write_str(n),
        }
    }
}

/// One `[fan_profiles.<id>]` table. A profile can lower the curve and the feed-forward,
/// or set a fixed `duty` (the `full` profile, with a TTL). It can never change `max_temp`
/// or `min_duty` (D-SV5): those fields do not exist here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub curve: Option<Vec<[f64; 2]>>,
    pub feed_forward: Option<FeedForwardPatch>,
    pub duty: Option<f64>,
    #[serde(
        default,
        deserialize_with = "de_opt_duration",
        serialize_with = "ser_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub ttl: Option<Duration>,
}

/// A fan after `[fan_defaults]` is merged in and validated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanSpec {
    pub id: String,
    pub backend: String,
    pub channel: Channel,
    pub mode: FanMode,
    /// Expression → control temperature (°C). Evaluated by orca-sensors.
    pub temp: String,
    /// Expression → load (%), optional.
    pub load: Option<String>,
    pub policy: Policy,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub stale: Duration,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub stale_input: Duration,
    pub send: SendPolicy,
    pub profile: Option<String>,
    pub alerts: AlertPolicy,
}

/// The numbers of one fan's policy (what [`crate::policy`] evaluates).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub load_gate: Option<LoadGate>,
    pub curve: Curve,
    pub feed_forward: Option<FeedForward>,
    pub min_duty: f64,
    pub max_temp: f64,
    pub ramp_down: RampDown,
}

/// Piecewise-linear temperature → duty, flat outside the first and last points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Curve(pub Vec<[f64; 2]>);

impl Curve {
    pub fn validate(points: &[[f64; 2]]) -> Result<Curve, String> {
        if points.is_empty() {
            return Err("a curve needs at least one point".into());
        }
        for w in points.windows(2) {
            if w[1][0] <= w[0][0] {
                return Err("curve temperatures must strictly increase".into());
            }
            if w[1][1] < w[0][1] {
                return Err("curve duties must not decrease as the temperature rises".into());
            }
        }
        for p in points {
            if !p[0].is_finite() || !(0.0..=100.0).contains(&p[1]) {
                return Err("curve duties must be 0-100 %".into());
            }
        }
        Ok(Curve(points.to_vec()))
    }
}

impl FansConfig {
    /// Merges defaults, validates every fan against its backend and the hostd limits, and
    /// returns the fans in id order.
    pub fn resolve(
        &self,
        limits: &FanLimits,
        may_write: &dyn Fn(BackendKind) -> bool,
    ) -> Result<Vec<FanSpec>, ConfigError> {
        let bad = |m: String| ConfigError::Invalid(m);
        for (id, b) in &self.fan_backends {
            validate_backend(id, b, may_write).map_err(bad)?;
        }
        for (id, p) in &self.fan_profiles {
            if let Some(c) = &p.curve {
                Curve::validate(c).map_err(|e| bad(format!("fan_profiles.{id}: {e}")))?;
            }
            if let Some(d) = p.duty {
                if !(0.0..=100.0).contains(&d) {
                    return Err(bad(format!("fan_profiles.{id}: duty must be 0-100")));
                }
                match p.ttl {
                    Some(t) if t.as_secs_f64() <= MAX_OVERRIDE_TTL_S => {}
                    _ => {
                        return Err(bad(format!(
                            "fan_profiles.{id}: a fixed duty needs a ttl of at most {MAX_OVERRIDE_TTL_S} s"
                        )));
                    }
                }
            }
        }
        let d = &self.fan_defaults;
        let mut out = Vec::new();
        for (id, t) in &self.fans {
            let f = &t.fields;
            let ctx = |m: &str| bad(format!("fans.{id}: {m}"));
            let backend = self
                .fan_backends
                .get(&t.backend)
                .ok_or_else(|| ctx(&format!("backend {:?} is not defined", t.backend)))?;
            let mode = f.mode.or(d.mode).unwrap_or(backend.kind.native_mode());
            if mode == FanMode::DeviceCurve && backend.kind.native_mode() != FanMode::DeviceCurve {
                return Err(ctx(&format!(
                    "backend kind {:?} has no device curve: use mode = \"host_curve\"",
                    backend.kind
                )));
            }
            if mode == FanMode::HostCurve && backend.kind == BackendKind::Esphome {
                return Err(ctx(
                    "the ESPHome firmware runs its own curve: mode must be \"device_curve\"",
                ));
            }
            let temp = f
                .temp
                .clone()
                .or(d.temp.clone())
                .ok_or_else(|| ctx("temp is required"))?;
            let load = f.load.clone().or(d.load.clone());
            let curve_pts = f
                .curve
                .clone()
                .or(d.curve.clone())
                .ok_or_else(|| ctx("curve is required"))?;
            let curve = Curve::validate(&curve_pts).map_err(|e| ctx(&e))?;
            let min_duty = f.min_duty.or(d.min_duty).unwrap_or(limits.min_duty);
            let max_temp = f.max_temp.or(d.max_temp).unwrap_or(limits.max_control_temp);
            let ramp_down = f.ramp_down.or(d.ramp_down).unwrap_or(RampDown {
                pct: 2.0,
                per: Duration::from_secs(2),
            });
            let feed_forward = f.feed_forward.or(d.feed_forward);
            let load_gate = f.load_gate.or(d.load_gate);
            if let Some(ff) = feed_forward
                && (ff.hi < ff.lo || !(0.0..=100.0).contains(&ff.max_duty))
            {
                return Err(ctx("feed_forward needs lo ≤ hi and max_duty 0-100"));
            }
            if let Some(g) = load_gate
                && g.hi < g.lo
            {
                return Err(ctx("load_gate needs lo ≤ hi"));
            }
            if load_gate.is_some() && load.is_none() {
                return Err(ctx("load_gate without a load expression"));
            }
            let stale = f.stale.or(d.stale).unwrap_or(Duration::from_secs(30));
            let stale_input = f
                .stale_input
                .or(d.stale_input)
                .unwrap_or(Duration::from_secs(3));
            let send = f.send.or(d.send).unwrap_or_default();
            let profile = f.profile.clone().or(d.profile.clone());
            if let Some(p) = &profile
                && !self.fan_profiles.contains_key(p)
            {
                return Err(ctx(&format!("profile {p:?} is not defined")));
            }
            let alerts = f.alerts.or(d.alerts).unwrap_or_default();
            let spec = FanSpec {
                id: id.clone(),
                backend: t.backend.clone(),
                channel: t.channel.clone(),
                mode,
                temp,
                load,
                policy: Policy {
                    load_gate,
                    curve,
                    feed_forward,
                    min_duty,
                    max_temp,
                    ramp_down,
                },
                stale,
                stale_input,
                send,
                profile,
                alerts,
            };
            limits
                .check_fan(&spec, backend, may_write)
                .map_err(|e| ctx(&e))?;
            out.push(spec);
        }
        Ok(out)
    }
}

fn validate_backend(
    id: &str,
    b: &BackendConfig,
    may_write: &dyn Fn(BackendKind) -> bool,
) -> Result<(), String> {
    let ctx = |m: &str| format!("fan_backends.{id}: {m}");
    let only = |name: &str, present: bool, kinds: &[BackendKind]| -> Result<(), String> {
        if present && !kinds.contains(&b.kind) {
            Err(ctx(&format!("{name} does not apply to kind {:?}", b.kind)))
        } else {
            Ok(())
        }
    };
    use BackendKind::*;
    if b.kind == Windows {
        return Err(ctx(
            "the windows backend is design only (SUPERVISOR.md 5.3, 15 Q16)",
        ));
    }
    only("url", b.url.is_some(), &[Esphome, Windows])?;
    only("transport", b.transport.is_some(), &[Esphome])?;
    only("auth", b.auth.is_some(), &[Esphome])?;
    only(
        "device_stale_seconds",
        b.device_stale_seconds.is_some(),
        &[Esphome],
    )?;
    only(
        "sync_device_settings",
        b.sync_device_settings != SyncDeviceSettings::Off,
        &[Esphome],
    )?;
    only("chip", b.chip.is_some(), &[Hwmon])?;
    only("on_exit", b.on_exit.is_some(), &[Hwmon, Nvml, Liquidctl])?;
    only("write", b.write, &[Ipmi])?;
    only("restore_mode", b.restore_mode.is_some(), &[Ipmi])?;
    only("match", b.match_.is_some(), &[Liquidctl])?;
    only("gpu", b.gpu.is_some(), &[Nvml])?;
    only("sensor_file", b.sensor_file.is_some(), &[Windows])?;
    match b.kind {
        Esphome => {
            if b.url.is_none() {
                return Err(ctx("url is required"));
            }
            match b.transport.as_deref() {
                None | Some("web_server") => {}
                Some("native") => {
                    return Err(ctx(
                        "transport \"native\" (the encrypted API) is not built yet; use \"web_server\"",
                    ));
                }
                Some(o) => return Err(ctx(&format!("unknown transport {o:?}"))),
            }
            if let Some(a) = &b.auth
                && a.kind != "digest"
            {
                return Err(ctx("auth.kind must be \"digest\""));
            }
            if let Some(s) = b.device_stale_seconds
                && !(1.0..=120.0).contains(&s)
            {
                return Err(ctx("device_stale_seconds must be 1-120"));
            }
        }
        Hwmon => {
            if b.chip.is_none() {
                return Err(ctx("chip is required (the hwmon `name`)"));
            }
        }
        Ipmi => {
            if b.write && !may_write(Ipmi) {
                return Err(ctx(
                    "write = true, but the hostd limits file does not allow IPMI writes",
                ));
            }
            if let Some(m) = &b.restore_mode {
                crate::backend::ipmi::parse_mode(m).map_err(|e| ctx(&e))?;
            }
        }
        Liquidctl => {
            if b.match_.is_none() {
                return Err(ctx("match is required (a liquidctl --match string)"));
            }
        }
        Nvml => {
            if b.gpu.is_none() {
                return Err(ctx("gpu is required (a card label or bus id)"));
            }
        }
        Windows | Fake => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_ramps() {
        assert_eq!(parse_duration("4s").unwrap(), Duration::from_secs(4));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("-1s").is_err());
        let r = RampDown::parse("2%/2s").unwrap();
        assert_eq!(r.rate(), 1.0);
        assert!(RampDown::parse("2/2s").is_err());
        assert!(RampDown::parse("0%/2s").is_err());
    }

    #[test]
    fn secrets_are_never_inline() {
        #[derive(Deserialize)]
        struct T {
            #[allow(dead_code)]
            p: SecretRef,
        }
        assert!(toml::from_str::<T>("p = \"hunter2\"").is_err());
        assert!(toml::from_str::<T>("p = \"secret:esp\"").is_ok());
        assert!(toml::from_str::<T>("p = { file = \"/run/x\" }").is_ok());
        assert!(toml::from_str::<T>("p = { file = \"/a\", env = \"B\" }").is_err());
    }

    #[test]
    fn sync_true_is_refused() {
        #[derive(Deserialize)]
        struct T {
            #[allow(dead_code)]
            s: SyncDeviceSettings,
        }
        assert!(toml::from_str::<T>("s = true").is_err());
        assert!(toml::from_str::<T>("s = false").is_ok());
        assert!(toml::from_str::<T>("s = \"diff\"").is_ok());
    }
}
