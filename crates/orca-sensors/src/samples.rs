//! Typed telemetry samples, as hostd produces them (SUPERVISOR.md 2.4 `telemetry.subscribe`),
//! and their mapping to sensor readings (4.1).
//!
//! Every metric is an `Option`: **a metric a card or host does not report is `None` (absent),
//! never 0.** The M4000 has no `mem_temp_c`; Volta and Maxwell have no hotspot.
//!
//! These types live here (not in `orca-hostd`) so `orca-fans`, `orca-gpuctl`, the supervisor's
//! telemetry hub, Glance and Home Assistant can all use them without depending on the daemon.

use serde::{Deserialize, Serialize};

use crate::reading::{Reading, Unit};
use crate::{Namespace, SensorId, TimestampMs};

/// One GPU at one instant. Units are in the field names.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GpuSample {
    /// Driver index (`nvidia-smi -i`). Can change across reboots; `uuid` is the identity.
    pub index: u32,
    pub uuid: String,
    /// PCI bus id, normalised to `bb:dd.f` lower-case (`21:00.0`).
    pub bus: String,
    pub name: String,
    pub ts: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_c: Option<f64>,
    /// HBM / memory temperature where the card reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotspot_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
    /// The enforced power limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sm_mhz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mhz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sm_mhz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_util_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_used_mib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_total_mib: Option<f64>,
    /// Only cards with their own fan (the M4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_pct: Option<f64>,
    /// `P0`…`P12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pstate: Option<String>,
    /// `clocks_event_reasons.active` bitmask (throttle reasons), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_reasons: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<bool>,
}

impl GpuSample {
    /// Load as the pusher computes it: draw / enforced limit × 100.
    pub fn power_pct(&self) -> Option<f64> {
        match (self.power_w, self.power_limit_w) {
            (Some(p), Some(l)) if l > 0.0 => Some(p / l * 100.0),
            _ => None,
        }
    }

    /// Readings under `gpu.<card>.*`, where `card` is the config label (A, B, …). Absent
    /// metrics produce no reading.
    pub fn readings(&self, card: &str) -> Vec<(SensorId, Reading)> {
        let base = SensorId::of(Namespace::Gpu, &[card]);
        let src = "hostd/gpu";
        let mib = 1024.0 * 1024.0;
        let fields: [(&str, Option<f64>, Unit); 14] = [
            ("core", self.core_c, Unit::Celsius),
            ("hbm", self.mem_temp_c, Unit::Celsius),
            ("hotspot", self.hotspot_c, Unit::Celsius),
            ("power_w", self.power_w, Unit::Watts),
            ("power_limit_w", self.power_limit_w, Unit::Watts),
            ("power_pct", self.power_pct(), Unit::Percent),
            ("sm_mhz", self.sm_mhz, Unit::Mhz),
            ("mem_mhz", self.mem_mhz, Unit::Mhz),
            ("util", self.util_pct, Unit::Percent),
            ("mem_util", self.mem_util_pct, Unit::Percent),
            ("mem_used", self.vram_used_mib.map(|v| v * mib), Unit::Bytes),
            (
                "mem_total",
                self.vram_total_mib.map(|v| v * mib),
                Unit::Bytes,
            ),
            ("fan_pct", self.fan_pct, Unit::Percent),
            ("max_sm_mhz", self.max_sm_mhz, Unit::Mhz),
        ];
        fields
            .into_iter()
            .filter_map(|(k, v, u)| {
                v.filter(|x| x.is_finite())
                    .map(|v| (base.child(k), Reading::new(v, u, self.ts, src)))
            })
            .collect()
    }
}

/// Host memory, with the ZFS ARC split out (the dash's used / ARC / consumed / free).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MemSample {
    pub total: u64,
    /// `MemFree`.
    pub free: u64,
    /// `MemAvailable` (the kernel's estimate; on ZFS hosts it does not count the ARC).
    pub available: u64,
    /// ZFS ARC size (`/proc/spl/kstat/zfs/arcstats` `size`), 0 without ZFS.
    pub arc: u64,
    /// ARC target maximum (`c_max`), if ZFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_max: Option<u64>,
    /// Buffers + page cache + reclaimable slab.
    pub cache: u64,
    /// Used by processes and the kernel, excluding ARC and cache:
    /// `total − free − cache − arc` (floored at 0).
    pub used: u64,
    /// Everything not free: `total − free` (`used + arc + cache`).
    pub consumed: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// One network interface.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NetSample {
    pub iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Rates since the previous sample; absent on the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_bps: Option<f64>,
}

/// Host CPU, load, memory and network.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HostSample {
    pub ts: TimestampMs,
    /// Busy % over all CPUs since the previous sample; absent on the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    pub cpu_count: u32,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub uptime_s: f64,
    pub mem: MemSample,
    #[serde(default)]
    pub net: Vec<NetSample>,
}

impl HostSample {
    /// `host.cpu`, `host.load1`, `host.mem_*`, `host.arc`, `host.net_rx` / `host.net_tx` (sum of
    /// the listed interfaces) and `host.net.<iface>.rx|tx`.
    pub fn readings(&self) -> Vec<(SensorId, Reading)> {
        let h = |k: &str| SensorId::of(Namespace::Host, &[k]);
        let src = "hostd/proc";
        let mut out = Vec::new();
        let mut put =
            |id: SensorId, v: f64, u: Unit| out.push((id, Reading::new(v, u, self.ts, src)));
        if let Some(c) = self.cpu_pct {
            put(h("cpu"), c, Unit::Percent);
        }
        put(h("load1"), self.load1, Unit::None);
        put(h("load5"), self.load5, Unit::None);
        put(h("load15"), self.load15, Unit::None);
        let m = &self.mem;
        put(h("mem_total"), m.total as f64, Unit::Bytes);
        put(h("mem_free"), m.free as f64, Unit::Bytes);
        put(h("mem_avail"), m.available as f64, Unit::Bytes);
        put(h("mem_used"), m.used as f64, Unit::Bytes);
        put(h("mem_consumed"), m.consumed as f64, Unit::Bytes);
        put(h("mem_cache"), m.cache as f64, Unit::Bytes);
        put(h("arc"), m.arc as f64, Unit::Bytes);
        let (mut rx, mut tx, mut any) = (0.0, 0.0, false);
        for n in &self.net {
            if let (Some(r), Some(t)) = (n.rx_bps, n.tx_bps) {
                put(
                    SensorId::of(Namespace::Host, &["net", &n.iface, "rx"]),
                    r,
                    Unit::BytesPerSec,
                );
                put(
                    SensorId::of(Namespace::Host, &["net", &n.iface, "tx"]),
                    t,
                    Unit::BytesPerSec,
                );
                rx += r;
                tx += t;
                any = true;
            }
        }
        if any {
            put(h("net_rx"), rx, Unit::BytesPerSec);
            put(h("net_tx"), tx, Unit::BytesPerSec);
        }
        out
    }
}

/// One `ipmitool sdr` row.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SdrEntry {
    pub name: String,
    /// Absent for `no reading` / `disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default)]
    pub unit: Unit,
    /// The raw unit text (`degrees C`, `RPM`, `Volts`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_text: String,
    /// `ok`, `nc`, `cr`, `nr`, `ns`.
    pub status: String,
}

/// The BMC: sensor data records and the fan mode.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BmcSample {
    pub ts: TimestampMs,
    pub sdr: Vec<SdrEntry>,
    /// Supermicro fan mode (Standard 0, Full 1, Optimal 2, HeavyIO 4), if read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_mode: Option<u8>,
}

impl BmcSample {
    pub fn readings(&self) -> Vec<(SensorId, Reading)> {
        self.sdr
            .iter()
            .filter_map(|e| {
                let v = e.value?;
                Some((
                    SensorId::of(Namespace::Bmc, &[&e.name]),
                    Reading::new(v, e.unit, self.ts, "hostd/ipmi"),
                ))
            })
            .collect()
    }
}

/// One hwmon input (`temp*_input`, `fan*_input`, `in*_input`, `power*_input`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HwmonReading {
    /// `name` of the hwmon device (`k10temp`, `nvme`); disambiguated with a suffix when several
    /// devices share a name (`nvme0`, `nvme1`).
    pub chip: String,
    /// The `*_label`, or the input's stem (`temp1`) when there is none.
    pub label: String,
    pub value: f64,
    pub unit: Unit,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HwmonSample {
    pub ts: TimestampMs,
    pub inputs: Vec<HwmonReading>,
}

impl HwmonSample {
    pub fn readings(&self) -> Vec<(SensorId, Reading)> {
        self.inputs
            .iter()
            .filter(|r| r.value.is_finite())
            .map(|r| {
                (
                    SensorId::of(Namespace::Hwmon, &[&r.chip, &r.label]),
                    Reading::new(r.value, r.unit, self.ts, "hostd/hwmon"),
                )
            })
            .collect()
    }
}

/// One fan as a backend reports it (whatever the backend: ESP, hwmon, IPMI, NVML).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FanSample {
    /// The config id (`duct0`).
    pub fan: String,
    pub ts: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<f64>,
    /// The feed-forward duty floor the device applies (ESP "FF duty").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ff_duty_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failsafe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<bool>,
    /// The control temperature hostd last pushed / used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_temp_c: Option<f64>,
    /// What the policy wants (both modes compute it, 5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wanted_duty_pct: Option<f64>,
}

impl FanSample {
    pub fn readings(&self) -> Vec<(SensorId, Reading)> {
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        let fields: [(&str, Option<f64>, Unit); 7] = [
            ("duty", self.duty_pct, Unit::Percent),
            ("rpm", self.rpm, Unit::Rpm),
            ("ff_duty", self.ff_duty_pct, Unit::Percent),
            ("failsafe", self.failsafe.map(b), Unit::Bool),
            ("stalled", self.stalled.map(b), Unit::Bool),
            ("control_temp", self.control_temp_c, Unit::Celsius),
            ("wanted_duty", self.wanted_duty_pct, Unit::Percent),
        ];
        let base = SensorId::of(Namespace::Fan, &[&self.fan]);
        fields
            .into_iter()
            .filter_map(|(k, v, u)| {
                v.map(|v| (base.child(k), Reading::new(v, u, self.ts, "hostd/fan")))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_absent_metrics_make_no_reading() {
        let s = GpuSample {
            uuid: "GPU-x".into(),
            core_c: Some(40.0),
            power_w: Some(100.0),
            power_limit_w: Some(250.0),
            ts: 5,
            ..Default::default()
        };
        let r = s.readings("D");
        let ids: Vec<String> = r.iter().map(|(i, _)| i.to_string()).collect();
        assert_eq!(
            ids,
            [
                "gpu.D.core",
                "gpu.D.power_w",
                "gpu.D.power_limit_w",
                "gpu.D.power_pct"
            ]
        );
        assert_eq!(r[3].1.value, 40.0);
    }

    #[test]
    fn host_and_bmc_readings() {
        let h = HostSample {
            ts: 1,
            cpu_pct: Some(12.5),
            net: vec![
                NetSample {
                    iface: "eth0".into(),
                    rx_bps: Some(10.0),
                    tx_bps: Some(1.0),
                    ..Default::default()
                },
                NetSample {
                    iface: "eth1".into(),
                    rx_bps: Some(5.0),
                    tx_bps: Some(2.0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = h.readings();
        let get = |k: &str| {
            r.iter()
                .find(|(i, _)| i.to_string() == k)
                .map(|(_, r)| r.value)
        };
        assert_eq!(get("host.cpu"), Some(12.5));
        assert_eq!(get("host.net_rx"), Some(15.0));
        assert_eq!(get("host.net.eth1.tx"), Some(2.0));
        let b = BmcSample {
            ts: 1,
            sdr: vec![
                SdrEntry {
                    name: "CPU1 Temp".into(),
                    value: Some(45.0),
                    unit: Unit::Celsius,
                    unit_text: "degrees C".into(),
                    status: "ok".into(),
                },
                SdrEntry {
                    name: "FAN5".into(),
                    value: None,
                    unit: Unit::Rpm,
                    unit_text: String::new(),
                    status: "ns".into(),
                },
            ],
            fan_mode: Some(2),
        };
        let r = b.readings();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0.to_string(), "bmc.\"CPU1 Temp\"");
    }
}
