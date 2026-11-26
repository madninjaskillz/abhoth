//! GPU telemetry (ROUTER-CONFIG.md 6.5): one `nvidia-smi --query-gpu` poll per server, parsed
//! into samples. This is a **sensor source**, not router-private state: the switch logic (VRAM
//! free, clocks after a start), the dash and the supervisor's sensors/fans/alerts all read the
//! same [`GpuSample`]s through a [`TelemetryCache`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The fields polled, in column order.
pub const QUERY_FIELDS: &[&str] = &[
    "uuid",
    "index",
    "name",
    "temperature.gpu",
    "utilization.gpu",
    "utilization.memory",
    "memory.used",
    "memory.total",
    "power.draw",
    "power.limit",
    "clocks.sm",
    "clocks.mem",
    "clocks.max.sm",
    "clocks.applications.graphics",
    "fan.speed",
    "pstate",
    "clocks_throttle_reasons.active",
];

/// A sample older than this is stale (switch decisions do not use it).
pub const STALE_AFTER: Duration = Duration::from_secs(15);

/// One GPU at one moment. `None` = the driver reported `[N/A]` / `[Not Supported]`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GpuSample {
    pub uuid: String,
    pub index: u32,
    pub name: String,
    pub temp_c: Option<f64>,
    pub util_pct: Option<f64>,
    pub mem_util_pct: Option<f64>,
    pub mem_used_mib: Option<u64>,
    pub mem_total_mib: Option<u64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub sm_mhz: Option<u32>,
    pub mem_mhz: Option<u32>,
    pub max_sm_mhz: Option<u32>,
    pub app_sm_mhz: Option<u32>,
    pub fan_pct: Option<f64>,
    pub pstate: Option<String>,
    /// Bitmask of active clock limit reasons.
    pub throttle: Option<u64>,
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

fn int<T: TryFrom<u64>>(s: &str) -> Option<T> {
    num(s)
        .filter(|v| *v >= 0.0)
        .and_then(|v| T::try_from(v.round() as u64).ok())
}

/// Parse `--format=csv,noheader,nounits` output in [`QUERY_FIELDS`] order. Lines that do not
/// have every column are skipped.
pub fn parse(csv: &str) -> Vec<GpuSample> {
    csv.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < QUERY_FIELDS.len() || !f[0].starts_with("GPU-") {
                return None;
            }
            Some(GpuSample {
                uuid: f[0].to_string(),
                index: int(f[1]).unwrap_or(0),
                name: f[2].to_string(),
                temp_c: num(f[3]),
                util_pct: num(f[4]),
                mem_util_pct: num(f[5]),
                mem_used_mib: int(f[6]),
                mem_total_mib: int(f[7]),
                power_w: num(f[8]),
                power_limit_w: num(f[9]),
                sm_mhz: int(f[10]),
                mem_mhz: int(f[11]),
                max_sm_mhz: int(f[12]),
                app_sm_mhz: int(f[13]),
                fan_pct: num(f[14]),
                pstate: Some(f[15].to_string()).filter(|p| p.starts_with('P')),
                throttle: u64::from_str_radix(f[16].trim_start_matches("0x"), 16).ok(),
            })
        })
        .collect()
}

/// Server id -> when it was polled and what it said.
type Polls = BTreeMap<String, (Instant, Vec<GpuSample>)>;

/// The last poll of each server, shared by every reader.
#[derive(Debug, Clone, Default)]
pub struct TelemetryCache {
    inner: Arc<Mutex<Polls>>,
}

impl TelemetryCache {
    pub fn put(&self, server: &str, samples: Vec<GpuSample>) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server.into(), (Instant::now(), samples));
    }
    /// The server's samples, when fresh.
    pub fn fresh(&self, server: &str) -> Option<Vec<GpuSample>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.get(server)
            .filter(|(at, _)| at.elapsed() < STALE_AFTER)
            .map(|(_, s)| s.clone())
    }
    /// One card by UUID (case-insensitive), when fresh.
    pub fn card(&self, server: &str, uuid: &str) -> Option<GpuSample> {
        self.fresh(server)?
            .into_iter()
            .find(|s| s.uuid.eq_ignore_ascii_case(uuid))
    }
    /// Everything, with its age (dash, sensors, MQTT).
    pub fn all(&self) -> BTreeMap<String, (Duration, Vec<GpuSample>)> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .map(|(k, (at, s))| (k.clone(), (at.elapsed(), s.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_poll() {
        let csv = "GPU-aaaa, 0, Tesla V100-SXM2-32GB, 41, 97, 60, 30210, 32768, 251.30, 300.00, 1597, 877, 1597, 1380, [N/A], P0, 0x0000000000000000\n\
                   GPU-bbbb, 1, Tesla V100-SXM2-32GB, 38, 0, 0, 5, 32768, 39.1, 300.00, 135, 877, 1380, 1380, [N/A], P0, 0x0000000000000001\n\
                   garbage\n";
        let s = parse(csv);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].mem_used_mib, Some(30210));
        assert_eq!(s[0].max_sm_mhz, Some(1597));
        assert_eq!(s[0].fan_pct, None);
        assert_eq!(s[1].throttle, Some(1));
        assert_eq!(s[1].pstate.as_deref(), Some("P0"));
        let c = TelemetryCache::default();
        c.put("gpu", s);
        assert_eq!(c.card("gpu", "gpu-BBBB").unwrap().index, 1);
        assert!(c.fresh("other").is_none());
    }
}
