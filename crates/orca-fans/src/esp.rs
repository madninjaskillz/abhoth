//! The ESP32 fan controller's side: a model of its firmware curve (`gpu-fans.yaml`), the
//! state seen over its SSE `/events` stream, its curve numbers ("device settings"), and the
//! read-only-diff-first sync of those numbers from config (SUPERVISOR.md 15 Q3).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::{EspNames, Policy};

/// The device curve numbers (Appendix A.4), as the firmware names them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceSettings {
    pub curve_low: f64,
    pub curve_high: f64,
    pub min_duty: f64,
    pub ff_low: f64,
    pub ff_high: f64,
    pub ff_max: f64,
    pub critical: f64,
}

/// (entity name, firmware min, firmware max, step) for each setting, in a fixed order.
pub const SETTING_ENTITIES: [(&str, f64, f64); 7] = [
    ("Curve Low Temp", 20.0, 90.0),
    ("Curve High Temp", 30.0, 100.0),
    ("Min Duty", 10.0, 100.0),
    ("FF Low Load", 0.0, 100.0),
    ("FF High Load", 0.0, 150.0),
    ("FF Max Duty", 10.0, 100.0),
    ("Critical Temp", 50.0, 105.0),
];

impl DeviceSettings {
    /// The firmware's `initial_value`s.
    pub const FIRMWARE_DEFAULTS: DeviceSettings = DeviceSettings {
        curve_low: 45.0,
        curve_high: 78.0,
        min_duty: 30.0,
        ff_low: 20.0,
        ff_high: 90.0,
        ff_max: 80.0,
        critical: 85.0,
    };

    pub fn values(&self) -> [f64; 7] {
        [
            self.curve_low,
            self.curve_high,
            self.min_duty,
            self.ff_low,
            self.ff_high,
            self.ff_max,
            self.critical,
        ]
    }

    fn from_values(v: [f64; 7]) -> DeviceSettings {
        DeviceSettings {
            curve_low: v[0],
            curve_high: v[1],
            min_duty: v[2],
            ff_low: v[3],
            ff_high: v[4],
            ff_max: v[5],
            critical: v[6],
        }
    }

    /// What config says the device should hold. The firmware curve is two points: `Min Duty`
    /// at `Curve Low`, 100 % at `Curve High`. A config curve that does not have that shape
    /// cannot be expressed on the device, so it cannot own the numbers.
    pub fn from_policy(p: &Policy) -> Result<DeviceSettings, String> {
        let c = &p.curve.0;
        if c.len() != 2 || c[1][1] != 100.0 || c[0][1] != p.min_duty {
            return Err(format!(
                "the device curve is [[low, min_duty], [high, 100]]; config has {c:?} with min_duty {}",
                p.min_duty
            ));
        }
        let ff = p
            .feed_forward
            .ok_or("the device always runs a feed-forward; config has none")?;
        let s = DeviceSettings {
            curve_low: c[0][0],
            curve_high: c[1][0],
            min_duty: p.min_duty,
            ff_low: ff.lo,
            ff_high: ff.hi,
            ff_max: ff.max_duty,
            critical: p.max_temp,
        };
        for ((name, lo, hi), v) in SETTING_ENTITIES.iter().zip(s.values()) {
            if v < *lo || v > *hi || v.fract() != 0.0 {
                return Err(format!(
                    "{name} = {v} is outside the firmware's range {lo}-{hi} (whole numbers)"
                ));
            }
        }
        Ok(s)
    }
}

/// The ESP firmware's control loop for one fan, reproduced (gpu-fans.yaml `fan_control`).
/// Used by shadow mode to predict the duty the device will apply, and by tests.
#[derive(Debug, Clone)]
pub struct FirmwareFan {
    pub duty: f64, // 0..1
    pub ff: f64,
    seen: Option<f64>,
    pseen: Option<f64>,
    temp: f64,
    load: f64,
}

impl Default for FirmwareFan {
    fn default() -> Self {
        FirmwareFan {
            duty: 1.0,
            ff: 0.0,
            seen: None,
            pseen: None,
            temp: f64::NAN,
            load: f64::NAN,
        }
    }
}

impl FirmwareFan {
    pub fn set_temp(&mut self, now: f64, v: f64, s: &DeviceSettings, stale: f64) {
        self.temp = v;
        self.seen = Some(now);
        self.run(now, false, s, stale);
    }

    pub fn set_load(&mut self, now: f64, v: f64, s: &DeviceSettings, stale: f64) {
        self.load = v;
        self.pseen = Some(now);
        self.run(now, false, s, stale);
    }

    /// The 2 s interval tick.
    pub fn tick(&mut self, now: f64, s: &DeviceSettings, stale: f64) {
        self.run(now, true, s, stale);
    }

    /// True when this fan is in failsafe (no fresh temperature).
    pub fn failsafe(&self, now: f64, stale: f64) -> bool {
        !self.fresh(now, stale)
    }

    fn fresh(&self, now: f64, stale: f64) -> bool {
        matches!(self.seen, Some(t) if now - t < stale) && !self.temp.is_nan()
    }

    fn run(&mut self, now: f64, tick: bool, s: &DeviceSettings, stale: f64) {
        let fresh = self.fresh(now, stale);
        let pfresh =
            fresh && matches!(self.pseen, Some(t) if now - t < stale) && !self.load.is_nan();
        let dmin = s.min_duty / 100.0;
        let fmax = s.ff_max / 100.0;
        let mut ff = 0.0;
        if pfresh {
            ff = if self.load <= s.ff_low {
                dmin
            } else if s.ff_high <= s.ff_low || self.load >= s.ff_high {
                fmax
            } else {
                dmin + (fmax - dmin) * (self.load - s.ff_low) / (s.ff_high - s.ff_low)
            };
        }
        self.ff = ff;
        let mut target = if !fresh {
            1.0
        } else {
            let t = if s.curve_high <= s.curve_low || self.temp >= s.curve_high {
                1.0
            } else if self.temp <= s.curve_low {
                dmin
            } else {
                dmin + (1.0 - dmin) * (self.temp - s.curve_low) / (s.curve_high - s.curve_low)
            };
            f64::max(t, ff)
        };
        if fresh && self.temp >= s.critical {
            target = 1.0;
        }
        let cur = self.duty;
        if target < cur {
            target = if tick {
                f64::max(target, cur - 0.02)
            } else {
                cur
            };
        }
        self.duty = target.clamp(0.0, 1.0);
    }
}

/// One entity state from the SSE stream.
#[derive(Debug, Clone, PartialEq)]
pub enum EntityValue {
    Number(f64),
    Bool(bool),
    Text(String),
    /// The entity reported no value (`NaN`/`null`).
    None,
}

impl EntityValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            EntityValue::Number(n) => Some(*n),
            _ => None,
        }
    }
}

/// Parses one SSE `data:` payload. ESPHome web_server v3 sends `state` events as JSON with
/// `id` (`"number/GPU 0 Temp"`), `name`, `value` and `state`. Other payloads (the `ping`
/// info, log lines) return `None`.
pub fn parse_state_event(data: &str) -> Option<(String, EntityValue)> {
    let v: Value = serde_json::from_str(data).ok()?;
    let id = v.get("id")?.as_str()?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| id.split_once('/').map(|(_, n)| n.to_string()))?;
    let val = match v.get("value") {
        Some(Value::Number(n)) => n
            .as_f64()
            .map(EntityValue::Number)
            .unwrap_or(EntityValue::None),
        Some(Value::Bool(b)) => EntityValue::Bool(*b),
        Some(Value::String(s)) => match s.parse::<f64>() {
            Ok(n) if n.is_finite() => EntityValue::Number(n),
            _ => EntityValue::Text(s.clone()),
        },
        _ => match v.get("state") {
            Some(Value::String(s)) if s.eq_ignore_ascii_case("ON") => EntityValue::Bool(true),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("OFF") => EntityValue::Bool(false),
            _ => EntityValue::None,
        },
    };
    Some((name, val))
}

/// Incremental SSE framing: feed raw lines, get `(event, data)` pairs.
#[derive(Debug, Default)]
pub struct SseParser {
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn line(&mut self, line: &str) -> Option<(String, String)> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if self.data.is_empty() {
                self.event = None;
                return None;
            }
            let ev = self.event.take().unwrap_or_else(|| "message".into());
            let data = std::mem::take(&mut self.data).join("\n");
            return Some((ev, data));
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

/// The latest state of every entity seen over SSE, with the time it arrived.
#[derive(Debug, Clone, Default)]
pub struct EspState {
    pub entities: BTreeMap<String, (EntityValue, f64)>,
}

impl EspState {
    pub fn apply(&mut self, name: String, v: EntityValue, now: f64) {
        self.entities.insert(name, (v, now));
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        self.entities.get(name).and_then(|(v, _)| v.as_f64())
    }

    pub fn flag(&self, name: &str) -> Option<bool> {
        match self.entities.get(name) {
            Some((EntityValue::Bool(b), _)) => Some(*b),
            _ => None,
        }
    }

    /// The device settings, once all seven have been seen.
    pub fn settings(&self) -> Option<DeviceSettings> {
        let mut v = [0.0; 7];
        for (i, (name, _, _)) in SETTING_ENTITIES.iter().enumerate() {
            v[i] = self.number(name)?;
        }
        Some(DeviceSettings::from_values(v))
    }

    /// Telemetry of one fan channel.
    pub fn channel(&self, names: &EspNames, n: u32) -> ChannelState {
        let f = |p: &str| EspNames::for_channel(p, n);
        ChannelState {
            rpm: self.number(&f(&names.rpm)),
            duty: self.number(&f(&names.duty)),
            ff_duty: self.number(&f(&names.ff_duty)),
            stalled: self.flag(&f(&names.stalled)),
            failsafe: self.flag(&names.failsafe),
            pushed_temp: self.number(&f(&names.temp)),
            pushed_load: self.number(&f(&names.load)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChannelState {
    pub rpm: Option<f64>,
    pub duty: Option<f64>,
    pub ff_duty: Option<f64>,
    pub stalled: Option<bool>,
    /// Device-wide: some fan is in failsafe.
    pub failsafe: Option<bool>,
    pub pushed_temp: Option<f64>,
    pub pushed_load: Option<f64>,
}

/// One difference between config and the device.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDiff {
    pub entity: &'static str,
    pub device: f64,
    pub config: f64,
}

/// The read-only diff phase, then (only if allowed) the write plan.
///
/// `write_plan` refuses unless `mode` is `Write` **and** a complete diff has been taken in
/// this run from a full read of the device (all seven numbers). The caller logs the diff;
/// the plan is a list of `(entity, value)` sets for the backend.
#[derive(Debug, Clone)]
pub struct DeviceSync {
    pub mode: crate::config::SyncDeviceSettings,
    diffed: Option<Vec<SettingDiff>>,
}

impl DeviceSync {
    pub fn new(mode: crate::config::SyncDeviceSettings) -> DeviceSync {
        DeviceSync { mode, diffed: None }
    }

    /// Compares the device's numbers with config. Read-only. Returns the differences.
    pub fn diff(&mut self, device: &DeviceSettings, config: &DeviceSettings) -> Vec<SettingDiff> {
        let d: Vec<SettingDiff> = SETTING_ENTITIES
            .iter()
            .zip(device.values().into_iter().zip(config.values()))
            .filter(|(_, (a, b))| (a - b).abs() > 1e-9)
            .map(|((name, _, _), (a, b))| SettingDiff {
                entity: name,
                device: a,
                config: b,
            })
            .collect();
        self.diffed = Some(d.clone());
        d
    }

    pub fn write_plan(&self) -> Result<Vec<(&'static str, f64)>, String> {
        use crate::config::SyncDeviceSettings::*;
        match self.mode {
            Off => Err("sync_device_settings is off".into()),
            Diff => Err("sync_device_settings = \"diff\" is read-only".into()),
            Write => match &self.diffed {
                None => Err("no read-only diff has been taken yet in this run".into()),
                Some(d) => {
                    // Critical Temp (the cap) first, then Min Duty (the floor), then the curve.
                    let mut plan: Vec<(&'static str, f64)> =
                        d.iter().map(|x| (x.entity, x.config)).collect();
                    plan.sort_by_key(|(e, _)| match *e {
                        "Critical Temp" => 0,
                        "Min Duty" => 1,
                        "Curve High Temp" | "FF High Load" => 2,
                        _ => 3,
                    });
                    Ok(plan)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SyncDeviceSettings;

    const S: DeviceSettings = DeviceSettings::FIRMWARE_DEFAULTS;

    #[test]
    fn firmware_boot_failsafe_and_ramp() {
        let mut f = FirmwareFan::default();
        assert_eq!(f.duty, 1.0);
        f.tick(0.0, &S, 30.0);
        assert_eq!(f.duty, 1.0);
        assert!(f.failsafe(0.0, 30.0));
        f.set_load(1.0, 0.0, &S, 30.0);
        f.set_temp(1.0, 40.0, &S, 30.0);
        // An arrival never lowers the duty; ticks lower it 2 % at a time.
        assert_eq!(f.duty, 1.0);
        f.tick(2.0, &S, 30.0);
        assert!((f.duty - 0.98).abs() < 1e-9);
        // Critical overrides everything; an arrival raises at once.
        f.set_temp(3.0, 86.0, &S, 30.0);
        assert_eq!(f.duty, 1.0);
        // Stale: back to 100 %.
        f.set_temp(4.0, 40.0, &S, 30.0);
        for i in 0..10 {
            f.tick(5.0 + 2.0 * i as f64, &S, 30.0);
        }
        assert!(f.duty < 0.85);
        f.tick(40.0, &S, 30.0);
        assert_eq!(f.duty, 1.0);
        assert!(f.failsafe(40.0, 30.0));
    }

    #[test]
    fn sse_framing_and_state() {
        let mut p = SseParser::default();
        assert_eq!(p.line("event: state"), None);
        assert_eq!(p.line(r#"data: {"id":"number/GPU 0 Temp","name":"GPU 0 Temp","value":41.3,"state":"41.3 °C"}"#), None);
        let (ev, data) = p.line("").unwrap();
        assert_eq!(ev, "state");
        let (name, v) = parse_state_event(&data).unwrap();
        assert_eq!(
            (name.as_str(), v),
            ("GPU 0 Temp", EntityValue::Number(41.3))
        );
        let (n, v) = parse_state_event(
            r#"{"id":"binary_sensor/Failsafe Active","name":"Failsafe Active","value":false,"state":"OFF"}"#,
        )
        .unwrap();
        assert_eq!(
            (n.as_str(), v),
            ("Failsafe Active", EntityValue::Bool(false))
        );
        assert!(parse_state_event(r#"{"title":"GPU Fans","uptime":5}"#).is_none());
        let (_, v) = parse_state_event(
            r#"{"id":"number/GPU 2 Power","name":"GPU 2 Power","value":"NaN","state":"NA"}"#,
        )
        .unwrap();
        assert_eq!(v, EntityValue::Text("NaN".into()));
    }

    #[test]
    fn sync_is_read_only_until_diffed_and_write_mode() {
        let mut want = S;
        want.ff_max = 70.0;
        for mode in [SyncDeviceSettings::Off, SyncDeviceSettings::Diff] {
            let mut s = DeviceSync::new(mode);
            assert_eq!(s.diff(&S, &want).len(), 1);
            assert!(s.write_plan().is_err());
        }
        let mut s = DeviceSync::new(SyncDeviceSettings::Write);
        assert!(s.write_plan().is_err(), "no diff yet");
        let d = s.diff(&S, &want);
        assert_eq!(
            d,
            vec![SettingDiff {
                entity: "FF Max Duty",
                device: 80.0,
                config: 70.0
            }]
        );
        assert_eq!(s.write_plan().unwrap(), vec![("FF Max Duty", 70.0)]);
    }
}
