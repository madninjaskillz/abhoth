//! GPU telemetry from `nvidia-smi --query-gpu` (the pusher's source, SUPERVISOR.md 5.4).
//!
//! One query covers every card. Values the card does not report (`N/A`, `[N/A]`,
//! `[Not Supported]`) become `None`, never 0. A one-shot query is used per tick; the stream
//! form (`-lms 500`) uses the same parser.

use orca_sensors::TimestampMs;
use orca_sensors::samples::GpuSample;

/// The query fields, in order. Keep in sync with [`parse_line`].
pub const QUERY_FIELDS: &[&str] = &[
    "index",
    "uuid",
    "pci.bus_id",
    "name",
    "temperature.gpu",
    "temperature.memory",
    "power.draw",
    "enforced.power.limit",
    "clocks.sm",
    "clocks.mem",
    "clocks.max.sm",
    "utilization.gpu",
    "utilization.memory",
    "memory.used",
    "memory.total",
    "fan.speed",
    "pstate",
    "clocks_event_reasons.active",
    "persistence_mode",
    "clocks.applications.graphics",
    "clocks.applications.memory",
];

/// The `nvidia-smi` arguments for one query of every card.
pub fn query_args() -> Vec<String> {
    vec![format!("--query-gpu={}", QUERY_FIELDS.join(",")), "--format=csv,noheader,nounits".into()]
}

fn absent(s: &str) -> bool {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    s.is_empty() || s == "n/a" || s == "not supported" || s == "unknown" || s.starts_with("gpu requires reset")
}

fn num(s: &str) -> Option<f64> {
    if absent(s) {
        return None;
    }
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `00000000:21:00.0` → `21:00.0` (lower-case).
pub fn short_bus(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    match s.split_once(':') {
        Some((dom, rest)) if dom.len() > 4 && rest.contains(':') => rest.to_string(),
        _ => s,
    }
}

/// Parses one CSV line of [`QUERY_FIELDS`].
pub fn parse_line(line: &str, ts: TimestampMs) -> Result<GpuSample, String> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() != QUERY_FIELDS.len() {
        return Err(format!("expected {} fields, got {}: {line:?}", QUERY_FIELDS.len(), f.len()));
    }
    let index = f[0].parse::<u32>().map_err(|_| format!("bad index {:?}", f[0]))?;
    if !f[1].starts_with("GPU-") {
        return Err(format!("bad uuid in {line:?}"));
    }
    Ok(GpuSample {
        index,
        uuid: f[1].to_string(),
        bus: short_bus(f[2]),
        name: f[3].to_string(),
        ts,
        core_c: num(f[4]),
        mem_temp_c: num(f[5]),
        hotspot_c: None,
        power_w: num(f[6]),
        power_limit_w: num(f[7]),
        sm_mhz: num(f[8]),
        mem_mhz: num(f[9]),
        max_sm_mhz: num(f[10]),
        util_pct: num(f[11]),
        mem_util_pct: num(f[12]),
        vram_used_mib: num(f[13]),
        vram_total_mib: num(f[14]),
        fan_pct: num(f[15]),
        pstate: (!absent(f[16])).then(|| f[16].to_string()),
        throttle_reasons: (!absent(f[17])).then(|| u64::from_str_radix(f[17].trim_start_matches("0x"), 16).ok()).flatten(),
        persistence: match f[18].to_ascii_lowercase().as_str() {
            "enabled" => Some(true),
            "disabled" => Some(false),
            _ => None,
        },
        app_sm_mhz: num(f[19]),
        app_mem_mhz: num(f[20]),
    })
}

/// Parses a whole query output. Bad lines are reported, good ones kept.
pub fn parse_output(out: &str, ts: TimestampMs) -> (Vec<GpuSample>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errs = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        match parse_line(line, ts) {
            Ok(s) => ok.push(s),
            Err(e) => errs.push(e),
        }
    }
    (ok, errs)
}

/// Runs one query. `nvidia_smi` is the binary (or a wrapper such as `sudo -n nvidia-smi`).
pub async fn query(nvidia_smi: &[String]) -> Result<Vec<GpuSample>, String> {
    let (prog, pre) = nvidia_smi.split_first().ok_or("no nvidia-smi command")?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(prog).args(pre).args(query_args()).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "nvidia-smi timed out after 10 s".to_string())?
    .map_err(|e| format!("nvidia-smi: {e}"))?;
    if !out.status.success() {
        return Err(format!("nvidia-smi exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    let (samples, errs) = parse_output(&String::from_utf8_lossy(&out.stdout), orca_sensors::now_ms());
    if samples.is_empty() && !errs.is_empty() {
        return Err(errs.join("; "));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from a 4-card host (driver 570), UUIDs replaced.
    pub const FIXTURE: &str = "\
0, GPU-00000000-0000-0000-0000-00000000000b, 00000000:01:00.0, Tesla V100-PCIE-32GB, 25, 24, 24.56, 250.00, 135, 877, 1380, 0, 0, 32256, 32768, [N/A], P0, 0x0000000000000000, Enabled, 1380, 877
1, GPU-00000000-0000-0000-0000-00000000000a, 00000000:21:00.0, Tesla V100-SXM3-32GB, 25, 22, 50.80, 350.00, 135, 958, 1597, 0, 0, 32275, 32768, [N/A], P0, 0x0000000000000000, Enabled, 1380, 958
2, GPU-00000000-0000-0000-0000-00000000000d, 00000000:44:00.0, Quadro M4000, 30, N/A, 10.69, 120.00, 135, 324, 772, 0, 0, 4733, 8192, 46, P8, 0x0000000000000001, Enabled, 772, 3005
3, GPU-00000000-0000-0000-0000-00000000000c, 00000000:E1:00.0, Tesla V100-SXM2-16GB, 25, 24, 17.90, 300.00, 135, 877, 1530, 0, 0, 11091, 16384, [N/A], P0, 0x0000000000000000, Enabled, 1312, 877
";

    #[test]
    fn parses_fixture() {
        let (s, errs) = parse_output(FIXTURE, 7);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(s.len(), 4);
        let a = &s[1];
        assert_eq!(a.bus, "21:00.0");
        assert_eq!(a.mem_temp_c, Some(22.0));
        assert_eq!(a.max_sm_mhz, Some(1597.0));
        assert_eq!(a.fan_pct, None);
        assert_eq!(a.persistence, Some(true));
        let d = &s[2];
        assert_eq!(d.name, "Quadro M4000");
        assert_eq!(d.mem_temp_c, None, "M4000 has no memory sensor: absent, not 0");
        assert_eq!(d.fan_pct, Some(46.0));
        assert_eq!(d.throttle_reasons, Some(1));
        assert_eq!(s[3].bus, "e1:00.0");
        assert_eq!(s[3].app_sm_mhz, Some(1312.0));
        assert_eq!(s[0].app_mem_mhz, Some(877.0));
        // Readings: no hbm for D.
        let r = d.readings("D");
        assert!(r.iter().all(|(id, _)| id.leaf() != "hbm"));
        assert!(r.iter().any(|(id, _)| id.leaf() == "fan_pct"));
    }

    #[test]
    fn bad_lines() {
        let (s, errs) = parse_output("garbage\n0, GPU-x, 0:1:0.0, n\n", 0);
        assert!(s.is_empty());
        assert_eq!(errs.len(), 2);
        assert!(absent("[Not Supported]") && absent(" N/A ") && !absent("0"));
    }
}
