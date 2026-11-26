//! The hardware seam: a closed command set, readings, a recording fake and an `nvidia-smi`
//! adapter. hostd may add an NVML (`nvml-wrapper`) adapter behind the same trait; the
//! `nvidia-smi` one is the fallback and what the dead-man cron uses.
//!
//! Every command addresses a card by UUID, never by index (nvidia-smi index order is not stable).

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Everything this crate can ask the driver to do. There is no free-form command text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GpuCommand {
    /// `-pm 1|0`
    SetPersistence { on: bool },
    /// `-pl W`
    SetPowerLimit { watts: u32 },
    /// `-ac mem,sm`
    SetAppClocks { mem_mhz: u32, sm_mhz: u32 },
    /// `-lgc min,max`
    LockGpuClocks { min_mhz: u32, max_mhz: u32 },
    /// `-rgc`
    ResetGpuClocks,
}

impl GpuCommand {
    /// The equivalent `nvidia-smi` arguments.
    pub fn nvidia_smi_args(&self, uuid: &str) -> Vec<String> {
        let mut a = vec!["-i".to_string(), uuid.to_string()];
        match *self {
            GpuCommand::SetPersistence { on } => {
                a.extend(["-pm".into(), if on { "1" } else { "0" }.into()])
            }
            GpuCommand::SetPowerLimit { watts } => a.extend(["-pl".into(), watts.to_string()]),
            GpuCommand::SetAppClocks { mem_mhz, sm_mhz } => {
                a.extend(["-ac".into(), format!("{mem_mhz},{sm_mhz}")])
            }
            GpuCommand::LockGpuClocks { min_mhz, max_mhz } => {
                a.extend(["-lgc".into(), format!("{min_mhz},{max_mhz}")])
            }
            GpuCommand::ResetGpuClocks => a.push("-rgc".into()),
        }
        a
    }

    /// Short human form for job steps and logs.
    pub fn describe(&self) -> String {
        match *self {
            GpuCommand::SetPersistence { on } => {
                format!("persistence {}", if on { "on" } else { "off" })
            }
            GpuCommand::SetPowerLimit { watts } => format!("PL {watts} W"),
            GpuCommand::SetAppClocks { mem_mhz, sm_mhz } => {
                format!("app clocks {mem_mhz},{sm_mhz}")
            }
            GpuCommand::LockGpuClocks { min_mhz, max_mhz } if min_mhz == max_mhz => {
                format!("pinned {max_mhz}")
            }
            GpuCommand::LockGpuClocks { min_mhz, max_mhz } => format!("locked {min_mhz}-{max_mhz}"),
            GpuCommand::ResetGpuClocks => "unlocked".into(),
        }
    }
}

/// NVML clock event (throttle) reasons, `clocks_event_reasons.active`.
pub mod reasons {
    pub const GPU_IDLE: u64 = 0x1;
    pub const APP_CLOCKS_SETTING: u64 = 0x2;
    pub const SW_POWER_CAP: u64 = 0x4;
    pub const HW_SLOWDOWN: u64 = 0x8;
    pub const SYNC_BOOST: u64 = 0x10;
    pub const SW_THERMAL: u64 = 0x20;
    pub const HW_THERMAL: u64 = 0x40;
    pub const HW_POWER_BRAKE: u64 = 0x80;
    /// Reasons that legitimately keep a busy card below its max: power and thermal limits. A
    /// lock never overrides these (SUPERVISOR.md 6.1).
    pub const EXCUSED: u64 = SW_POWER_CAP | HW_SLOWDOWN | SW_THERMAL | HW_THERMAL | HW_POWER_BRAKE;
}

/// One sample of one card. Fields the driver reports as N/A are `None`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuReading {
    pub uuid: String,
    pub util_pct: Option<u32>,
    pub temp_c: Option<u32>,
    pub mem_temp_c: Option<u32>,
    pub sm_mhz: Option<u32>,
    pub max_sm_mhz: Option<u32>,
    pub app_sm_mhz: Option<u32>,
    pub app_mem_mhz: Option<u32>,
    pub power_limit_w: Option<f64>,
    pub power_draw_w: Option<f64>,
    pub persistence: Option<bool>,
    pub event_reasons: Option<u64>,
}

impl GpuReading {
    /// The hotter of core and HBM, the temperature that decides "cool".
    pub fn hottest_c(&self) -> Option<u32> {
        match (self.temp_c, self.mem_temp_c) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuError(pub String);

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GpuError {}

/// The driver. Implemented by the NVML / `nvidia-smi` adapters and by [`FakeNvml`].
pub trait GpuControl {
    fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), GpuError>;
    fn read(&mut self, uuid: &str) -> Result<GpuReading, GpuError>;
}

impl<T: GpuControl + ?Sized> GpuControl for &mut T {
    fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), GpuError> {
        (**self).apply(uuid, cmd)
    }
    fn read(&mut self, uuid: &str) -> Result<GpuReading, GpuError> {
        (**self).read(uuid)
    }
}

/// A fake driver: records every command, keeps a per-card reading that tests edit, and can be
/// told to fail. Applying commands updates the reading the way the driver would (lock / reset
/// move the "ceiling" the fake clock settles at).
#[derive(Debug, Default)]
pub struct FakeNvml {
    pub log: Vec<(String, GpuCommand)>,
    pub cards: BTreeMap<String, FakeCard>,
    /// Commands that fail (by UUID, or "*" for every card).
    pub fail: Vec<(String, GpuCommand)>,
    pub fail_reads: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FakeCard {
    pub reading: GpuReading,
    pub lock: Option<(u32, u32)>,
}

impl FakeNvml {
    pub fn with_card(mut self, uuid: &str, max_sm: u32) -> Self {
        self.cards.insert(
            uuid.to_string(),
            FakeCard {
                reading: GpuReading {
                    uuid: uuid.into(),
                    util_pct: Some(0),
                    temp_c: Some(30),
                    sm_mhz: Some(135),
                    max_sm_mhz: Some(max_sm),
                    persistence: Some(true),
                    event_reasons: Some(reasons::GPU_IDLE),
                    ..Default::default()
                },
                lock: None,
            },
        );
        self
    }

    pub fn card(&mut self, uuid: &str) -> &mut FakeCard {
        self.cards.get_mut(uuid).expect("fake card")
    }

    /// Commands sent to one card, in order.
    pub fn sent(&self, uuid: &str) -> Vec<GpuCommand> {
        self.log
            .iter()
            .filter(|(u, _)| u == uuid)
            .map(|(_, c)| *c)
            .collect()
    }

    pub fn clear(&mut self) {
        self.log.clear();
    }

    /// Simulates load: utilisation and a clock the card would run at, bounded by any lock.
    pub fn load(&mut self, uuid: &str, util: u32, natural_mhz: u32, temp_c: u32) {
        let c = self.card(uuid);
        let ceil = c.lock.map(|(_, hi)| hi).unwrap_or(u32::MAX);
        let floor = c.lock.map(|(lo, _)| lo).unwrap_or(0);
        c.reading.util_pct = Some(util);
        c.reading.temp_c = Some(temp_c);
        c.reading.sm_mhz = Some(natural_mhz.min(ceil).max(floor));
        c.reading.event_reasons = Some(if util == 0 { reasons::GPU_IDLE } else { 0 });
    }
}

impl GpuControl for FakeNvml {
    fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), GpuError> {
        self.log.push((uuid.to_string(), cmd));
        if self
            .fail
            .iter()
            .any(|(u, c)| (u == uuid || u == "*") && *c == cmd)
        {
            return Err(GpuError(format!("fake failure: {}", cmd.describe())));
        }
        let Some(c) = self.cards.get_mut(uuid) else {
            return Err(GpuError(format!("no such GPU {uuid}")));
        };
        match cmd {
            GpuCommand::SetPersistence { on } => c.reading.persistence = Some(on),
            GpuCommand::SetPowerLimit { watts } => c.reading.power_limit_w = Some(f64::from(watts)),
            GpuCommand::SetAppClocks { mem_mhz, sm_mhz } => {
                c.reading.app_mem_mhz = Some(mem_mhz);
                c.reading.app_sm_mhz = Some(sm_mhz);
            }
            GpuCommand::LockGpuClocks { min_mhz, max_mhz } => {
                c.lock = Some((min_mhz, max_mhz));
                if let Some(s) = c.reading.sm_mhz {
                    c.reading.sm_mhz = Some(s.clamp(min_mhz, max_mhz));
                }
            }
            GpuCommand::ResetGpuClocks => c.lock = None,
        }
        Ok(())
    }

    fn read(&mut self, uuid: &str) -> Result<GpuReading, GpuError> {
        if self.fail_reads {
            return Err(GpuError("fake read failure".into()));
        }
        self.cards
            .get(uuid)
            .map(|c| c.reading.clone())
            .ok_or_else(|| GpuError(format!("no such GPU {uuid}")))
    }
}

/// The `nvidia-smi` adapter.
#[derive(Debug, Clone)]
pub struct NvidiaSmi {
    pub program: String,
}

impl Default for NvidiaSmi {
    fn default() -> Self {
        NvidiaSmi {
            program: "nvidia-smi".into(),
        }
    }
}

/// The query fields, in the order [`parse_query_line`] expects.
pub const QUERY_FIELDS: &str = "uuid,utilization.gpu,temperature.gpu,temperature.memory,clocks.sm,clocks.max.sm,\
clocks.applications.graphics,clocks.applications.memory,power.limit,power.draw,persistence_mode,clocks_event_reasons.active";

impl NvidiaSmi {
    fn run(&self, args: &[String]) -> Result<String, GpuError> {
        let out = Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| GpuError(format!("{}: {e}", self.program)))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            Ok(text)
        } else {
            Err(GpuError(format!(
                "{} {}: {}",
                self.program,
                args.join(" "),
                text.trim()
            )))
        }
    }

    /// Read every card in one call.
    pub fn read_all(&self) -> Result<Vec<GpuReading>, GpuError> {
        let out = self.run(&[
            format!("--query-gpu={QUERY_FIELDS}"),
            "--format=csv,noheader,nounits".into(),
        ])?;
        out.lines()
            .filter(|l| l.trim().starts_with("GPU-"))
            .map(parse_query_line)
            .collect()
    }
}

impl GpuControl for NvidiaSmi {
    fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), GpuError> {
        self.run(&cmd.nvidia_smi_args(uuid)).map(|_| ())
    }

    fn read(&mut self, uuid: &str) -> Result<GpuReading, GpuError> {
        let out = self.run(&[
            "-i".into(),
            uuid.into(),
            format!("--query-gpu={QUERY_FIELDS}"),
            "--format=csv,noheader,nounits".into(),
        ])?;
        parse_query_line(
            out.lines()
                .find(|l| l.trim().starts_with("GPU-"))
                .unwrap_or(""),
        )
    }
}

/// Parses one `--format=csv,noheader,nounits` line of [`QUERY_FIELDS`].
pub fn parse_query_line(line: &str) -> Result<GpuReading, GpuError> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() != 12 || !f[0].starts_with("GPU-") {
        return Err(GpuError(format!("unexpected nvidia-smi line: {line:?}")));
    }
    let na = |s: &str| s.is_empty() || s.starts_with('[') || s.eq_ignore_ascii_case("n/a");
    let u = |s: &str| {
        if na(s) {
            None
        } else {
            s.parse::<f64>().ok().map(|v| v.round() as u32)
        }
    };
    let w = |s: &str| if na(s) { None } else { s.parse::<f64>().ok() };
    Ok(GpuReading {
        uuid: f[0].to_string(),
        util_pct: u(f[1]),
        temp_c: u(f[2]),
        mem_temp_c: u(f[3]),
        sm_mhz: u(f[4]),
        max_sm_mhz: u(f[5]),
        app_sm_mhz: u(f[6]),
        app_mem_mhz: u(f[7]),
        power_limit_w: w(f[8]),
        power_draw_w: w(f[9]),
        persistence: match f[10] {
            "Enabled" => Some(true),
            "Disabled" => Some(false),
            _ => None,
        },
        event_reasons: f[11]
            .strip_prefix("0x")
            .and_then(|h| u64::from_str_radix(h, 16).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smi_args() {
        let u = "GPU-1";
        assert_eq!(
            GpuCommand::LockGpuClocks {
                min_mhz: 135,
                max_mhz: 135
            }
            .nvidia_smi_args(u),
            ["-i", u, "-lgc", "135,135"]
        );
        assert_eq!(
            GpuCommand::ResetGpuClocks.nvidia_smi_args(u),
            ["-i", u, "-rgc"]
        );
        assert_eq!(
            GpuCommand::SetAppClocks {
                mem_mhz: 877,
                sm_mhz: 1380
            }
            .nvidia_smi_args(u),
            ["-i", u, "-ac", "877,1380"]
        );
        assert_eq!(
            GpuCommand::SetPersistence { on: true }.nvidia_smi_args(u),
            ["-i", u, "-pm", "1"]
        );
        assert_eq!(
            GpuCommand::SetPowerLimit { watts: 350 }.nvidia_smi_args(u),
            ["-i", u, "-pl", "350"]
        );
    }

    #[test]
    fn parses_deepthought_style_line() {
        // Shape of the 2026-11-26 survey of card A (V100-SXM3), placeholder UUID.
        let r = parse_query_line(
            "GPU-aaaa, 0, 25, 27, 135, 1597, 1380, 958, 350.00, 40.81, Enabled, 0x0000000000000001",
        )
        .unwrap();
        assert_eq!(r.max_sm_mhz, Some(1597));
        assert_eq!(r.app_sm_mhz, Some(1380));
        assert_eq!(r.power_limit_w, Some(350.0));
        assert_eq!(r.persistence, Some(true));
        assert_eq!(r.event_reasons, Some(reasons::GPU_IDLE));
        assert_eq!(r.hottest_c(), Some(27));
        // The Quadro has no HBM sensor.
        let q = parse_query_line(
            "GPU-dddd, 0, 30, [N/A], 135, 772, 772, 3005, 120.00, 10.1, Enabled, 0x1",
        )
        .unwrap();
        assert_eq!(q.mem_temp_c, None);
        assert_eq!(q.hottest_c(), Some(30));
        assert!(parse_query_line("garbage").is_err());
    }

    #[test]
    fn fake_records_and_bounds_clock() {
        let mut f = FakeNvml::default().with_card("GPU-a", 1597);
        f.apply(
            "GPU-a",
            GpuCommand::LockGpuClocks {
                min_mhz: 135,
                max_mhz: 1400,
            },
        )
        .unwrap();
        f.load("GPU-a", 100, 1597, 50);
        assert_eq!(f.read("GPU-a").unwrap().sm_mhz, Some(1400));
        f.apply("GPU-a", GpuCommand::ResetGpuClocks).unwrap();
        f.load("GPU-a", 100, 1597, 50);
        assert_eq!(f.read("GPU-a").unwrap().sm_mhz, Some(1597));
        assert_eq!(f.sent("GPU-a").len(), 2);
        assert!(f.apply("GPU-x", GpuCommand::ResetGpuClocks).is_err());
    }
}
