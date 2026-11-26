//! The telemetry samplers: GPUs (`nvidia-smi`), host `/proc`, hwmon sysfs, BMC (`ipmitool`).
//!
//! Each has a pure parser tested against recorded fixtures (the fake sources) and a thin
//! reader. [`Collector`] ties them together into [`TelemetryBatch`]es.

pub mod bmc;
pub mod gpu;
pub mod host;
pub mod hwmon;

use std::collections::BTreeMap;
use std::path::PathBuf;

use orca_sensors::{Reading, SensorId, TimestampMs};

use crate::protocol::TelemetryBatch;

/// Where the samplers read from. Defaults are the real host paths.
#[derive(Debug, Clone)]
pub struct Sources {
    /// `nvidia-smi` command (a prefix such as `sudo -n nvidia-smi` is allowed).
    pub nvidia_smi: Vec<String>,
    /// `ipmitool` command; empty disables the BMC reader.
    pub ipmitool: Vec<String>,
    /// Read the Supermicro fan mode (a raw GET).
    pub bmc_fan_mode: bool,
    /// Root for `/proc` (normally `/`).
    pub proc_root: PathBuf,
    /// `/sys/class/hwmon`.
    pub hwmon_dir: PathBuf,
}

impl Default for Sources {
    fn default() -> Sources {
        Sources {
            nvidia_smi: vec!["nvidia-smi".into()],
            ipmitool: vec!["ipmitool".into()],
            bmc_fan_mode: true,
            proc_root: PathBuf::from("/"),
            hwmon_dir: PathBuf::from("/sys/class/hwmon"),
        }
    }
}

/// Which parts to sample in one call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Want {
    pub gpu: bool,
    pub host: bool,
    pub hwmon: bool,
    pub bmc: bool,
}

impl Want {
    pub const ALL: Want = Want {
        gpu: true,
        host: true,
        hwmon: true,
        bmc: true,
    };
}

/// The sampling state (host counters) and the last error per source.
#[derive(Debug)]
pub struct Collector {
    pub sources: Sources,
    host: host::HostSampler,
    /// UUID → card label, from the policy.
    pub labels: BTreeMap<String, String>,
    pub errors: BTreeMap<&'static str, String>,
    pub last_ok: BTreeMap<&'static str, TimestampMs>,
}

impl Collector {
    pub fn new(sources: Sources) -> Collector {
        let host = host::HostSampler::new(sources.proc_root.clone());
        Collector {
            sources,
            host,
            labels: BTreeMap::new(),
            errors: BTreeMap::new(),
            last_ok: BTreeMap::new(),
        }
    }

    fn note(&mut self, src: &'static str, r: Result<(), String>, ts: TimestampMs) {
        match r {
            Ok(()) => {
                self.errors.remove(src);
                self.last_ok.insert(src, ts);
            }
            Err(e) => {
                self.errors.insert(src, e);
            }
        }
    }

    /// The label a GPU is reported under: its policy label, else its UUID.
    pub fn label_for(&self, uuid: &str) -> String {
        self.labels
            .get(uuid)
            .cloned()
            .unwrap_or_else(|| uuid.to_string())
    }

    /// Samples the wanted parts. Failures are recorded in `errors` and leave the part absent.
    pub async fn collect(&mut self, want: Want) -> TelemetryBatch {
        let ts = orca_sensors::now_ms();
        let mut b = TelemetryBatch {
            ts,
            ..Default::default()
        };
        if want.gpu {
            let r = gpu::query(&self.sources.nvidia_smi).await;
            let r = r.map(|v| {
                for s in v {
                    b.gpus.insert(self.label_for(&s.uuid), s);
                }
            });
            self.note("gpu", r, ts);
        }
        if want.host {
            let r = self.host.sample(ts).map(|h| b.host = Some(h));
            self.note("host", r, ts);
        }
        if want.hwmon {
            b.hwmon = Some(hwmon::read(&self.sources.hwmon_dir, ts));
            self.note("hwmon", Ok(()), ts);
        }
        if want.bmc && !self.sources.ipmitool.is_empty() {
            let r = bmc::read(&self.sources.ipmitool, self.sources.bmc_fan_mode, ts)
                .await
                .map(|s| b.bmc = Some(s));
            self.note("bmc", r, ts);
        }
        b
    }
}

/// Every sensor reading in a batch (`gpu.*`, `host.*`, `hwmon.*`, `bmc.*`, `fan.*`).
pub fn batch_readings(b: &TelemetryBatch) -> Vec<(SensorId, Reading)> {
    let mut out = Vec::new();
    for (label, g) in &b.gpus {
        out.extend(g.readings(label));
    }
    if let Some(h) = &b.host {
        out.extend(h.readings());
    }
    if let Some(h) = &b.hwmon {
        out.extend(h.readings());
    }
    if let Some(m) = &b.bmc {
        out.extend(m.readings());
    }
    for f in &b.fans {
        out.extend(f.readings());
    }
    out
}
