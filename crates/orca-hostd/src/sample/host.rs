//! Host CPU, load, memory (with the ZFS ARC split) and network from `/proc`.
//!
//! Parsers are pure functions over file text; [`HostSampler`] reads the files under a root
//! (`/` on the host, a fixture directory in tests) and keeps the previous counters for rates.

use std::path::{Path, PathBuf};

use orca_sensors::TimestampMs;
use orca_sensors::samples::{HostSample, MemSample, NetSample};

/// `/proc/stat` first line → (busy, total) jiffies.
pub fn parse_cpu_times(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is already in user)
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    let total: u64 = v.iter().take(8).sum();
    Some((total - idle, total))
}

/// Number of `cpuN` lines in `/proc/stat`.
pub fn parse_cpu_count(stat: &str) -> u32 {
    stat.lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .count() as u32
}

/// `/proc/loadavg` → (1, 5, 15).
pub fn parse_loadavg(s: &str) -> Option<(f64, f64, f64)> {
    let mut it = s.split_whitespace().map(|x| x.parse::<f64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

fn kv_kib(meminfo: &str, key: &str) -> Option<u64> {
    meminfo.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| {
            v.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|x| x * 1024)
        })?
    })
}

/// `arcstats` (`name type data` rows) → value of `key`.
pub fn arcstat(arcstats: &str, key: &str) -> Option<u64> {
    arcstats.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == key).then(|| it.nth(1)?.parse().ok())?
    })
}

/// `/proc/meminfo` (+ optional arcstats) → the memory split.
///
/// - `cache` = Buffers + Cached + SReclaimable;
/// - `used` = total − free − cache − arc (processes and the kernel; floored at 0);
/// - `consumed` = total − free.
pub fn parse_mem(meminfo: &str, arcstats: Option<&str>) -> Option<MemSample> {
    let total = kv_kib(meminfo, "MemTotal")?;
    let free = kv_kib(meminfo, "MemFree")?;
    let available = kv_kib(meminfo, "MemAvailable").unwrap_or(free);
    let cache = kv_kib(meminfo, "Buffers").unwrap_or(0)
        + kv_kib(meminfo, "Cached").unwrap_or(0)
        + kv_kib(meminfo, "SReclaimable").unwrap_or(0);
    let arc = arcstats.and_then(|a| arcstat(a, "size")).unwrap_or(0);
    let arc_max = arcstats.and_then(|a| arcstat(a, "c_max"));
    let consumed = total.saturating_sub(free);
    let used = consumed.saturating_sub(cache).saturating_sub(arc);
    Some(MemSample {
        total,
        free,
        available,
        arc,
        arc_max,
        cache,
        used,
        consumed,
        swap_total: kv_kib(meminfo, "SwapTotal").unwrap_or(0),
        swap_free: kv_kib(meminfo, "SwapFree").unwrap_or(0),
    })
}

/// `/proc/net/dev` → `(iface, rx_bytes, tx_bytes)`.
pub fn parse_net_dev(s: &str) -> Vec<(String, u64, u64)> {
    s.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let v: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            Some((name.trim().to_string(), *v.first()?, *v.get(8)?))
        })
        .collect()
}

/// Interfaces worth reporting: not loopback, not container veths or bridges.
pub fn interesting_iface(name: &str) -> bool {
    !(name == "lo"
        || name.starts_with("veth")
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("vnet")
        || name.starts_with("kube")
        || name.starts_with("cni")
        || name.starts_with("flannel"))
}

/// Reads host samples, keeping counters between calls for CPU and network rates.
#[derive(Debug)]
pub struct HostSampler {
    root: PathBuf,
    prev_cpu: Option<(u64, u64)>,
    prev_net: Vec<(String, u64, u64)>,
    prev_ts: Option<TimestampMs>,
    /// Only these interfaces, if set; else [`interesting_iface`].
    pub ifaces: Option<Vec<String>>,
}

impl HostSampler {
    pub fn new(root: impl Into<PathBuf>) -> HostSampler {
        HostSampler {
            root: root.into(),
            prev_cpu: None,
            prev_net: Vec::new(),
            prev_ts: None,
            ifaces: None,
        }
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }

    /// One sample at `ts`. Missing files make their fields default; `/proc/meminfo` is required.
    pub fn sample(&mut self, ts: TimestampMs) -> Result<HostSample, String> {
        let stat = self.read("proc/stat").unwrap_or_default();
        let mem = parse_mem(
            &self
                .read("proc/meminfo")
                .ok_or("cannot read /proc/meminfo")?,
            self.read("proc/spl/kstat/zfs/arcstats").as_deref(),
        )
        .ok_or("cannot parse /proc/meminfo")?;
        let (load1, load5, load15) = self
            .read("proc/loadavg")
            .and_then(|s| parse_loadavg(&s))
            .unwrap_or_default();
        let uptime_s = self
            .read("proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);
        let cpu_now = parse_cpu_times(&stat);
        let cpu_pct = match (self.prev_cpu, cpu_now) {
            (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => {
                Some((b1.saturating_sub(b0)) as f64 / (t1 - t0) as f64 * 100.0)
            }
            _ => None,
        };
        self.prev_cpu = cpu_now;
        let dt = self
            .prev_ts
            .map(|p| (ts - p) as f64 / 1000.0)
            .filter(|d| *d > 0.0);
        let net_now: Vec<_> = self
            .read("proc/net/dev")
            .map(|s| parse_net_dev(&s))
            .unwrap_or_default()
            .into_iter()
            .filter(|(n, _, _)| match &self.ifaces {
                Some(list) => list.iter().any(|x| x == n),
                None => interesting_iface(n),
            })
            .collect();
        let net = net_now
            .iter()
            .map(|(n, rx, tx)| {
                let prev = self.prev_net.iter().find(|(p, _, _)| p == n);
                let rate = |now: u64, before: u64| {
                    dt.filter(|_| now >= before)
                        .map(|d| (now - before) as f64 / d)
                };
                NetSample {
                    iface: n.clone(),
                    rx_bytes: *rx,
                    tx_bytes: *tx,
                    rx_bps: prev.and_then(|p| rate(*rx, p.1)),
                    tx_bps: prev.and_then(|p| rate(*tx, p.2)),
                }
            })
            .collect();
        self.prev_net = net_now;
        self.prev_ts = Some(ts);
        Ok(HostSample {
            ts,
            cpu_pct,
            cpu_count: parse_cpu_count(&stat),
            load1,
            load5,
            load15,
            uptime_s,
            mem,
            net,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       1000000 kB\nMemFree:         100000 kB\nMemAvailable:    300000 kB\nBuffers:          10000 kB\nCached:          150000 kB\nSwapCached:            0 kB\nSReclaimable:     40000 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n";
    const ARC: &str = "13 1 0x01 123 33456 1 2\nname                            type data\nhits                            4    1000\nsize                            4    409600000\nc_max                           4    819200000\n";

    #[test]
    fn mem_split() {
        let m = parse_mem(MEMINFO, Some(ARC)).unwrap();
        assert_eq!(m.total, 1_024_000_000);
        assert_eq!(m.arc, 409_600_000);
        assert_eq!(m.cache, 200_000 * 1024);
        assert_eq!(m.consumed, 900_000 * 1024);
        assert_eq!(m.used, 900_000 * 1024 - 200_000 * 1024 - 409_600_000);
        assert_eq!(m.used + m.arc + m.cache + m.free, m.total);
        assert_eq!(m.arc_max, Some(819_200_000));
        let no_zfs = parse_mem(MEMINFO, None).unwrap();
        assert_eq!(no_zfs.arc, 0);
    }

    #[test]
    fn sampler_rates_over_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        let p = |r: &str| dir.path().join(r);
        std::fs::create_dir_all(p("proc/net")).unwrap();
        std::fs::write(p("proc/meminfo"), MEMINFO).unwrap();
        std::fs::write(p("proc/loadavg"), "1.50 1.00 0.50 2/300 1234\n").unwrap();
        std::fs::write(
            p("proc/stat"),
            "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 0 1 8 0 0 0 0\ncpu1 1 0 1 8 0 0 0 0\n",
        )
        .unwrap();
        let net = |rx: u64, tx: u64| {
            format!(
                "Inter-|   Receive\n face |bytes\n    lo: 5 0 0 0 0 0 0 0 5 0 0 0 0 0 0 0\n  eth0: {rx} 0 0 0 0 0 0 0 {tx} 0 0 0 0 0 0 0\nveth1: 9 0 0 0 0 0 0 0 9 0 0 0 0 0 0 0\n"
            )
        };
        std::fs::write(p("proc/net/dev"), net(1000, 500)).unwrap();
        let mut s = HostSampler::new(dir.path());
        let a = s.sample(0).unwrap();
        assert_eq!(a.cpu_pct, None);
        assert_eq!(a.cpu_count, 2);
        assert_eq!(a.load1, 1.5);
        assert_eq!(a.net.len(), 1);
        assert_eq!(a.net[0].rx_bps, None);
        std::fs::write(p("proc/stat"), "cpu  150 0 150 900 0 0 0 0 0 0\n").unwrap();
        std::fs::write(p("proc/net/dev"), net(3000, 1500)).unwrap();
        let b = s.sample(2_000).unwrap();
        assert_eq!(b.cpu_pct, Some(50.0));
        assert_eq!(b.net[0].rx_bps, Some(1000.0));
        assert_eq!(b.net[0].tx_bps, Some(500.0));
    }
}
