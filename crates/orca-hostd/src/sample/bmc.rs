//! BMC readings from `ipmitool sdr` (read-only; ports `bmcstat.py`).
//!
//! ```text
//! CPU1 Temp        | 45 degrees C      | ok
//! FAN1             | 1400 RPM          | ok
//! FAN5             | no reading        | ns
//! 12V              | 12.19 Volts       | ok
//! ```
//!
//! The Supermicro fan-mode GET (`raw 0x30 0x45 0x00`) is a read; mode SET is never sent here
//! (writes belong to the `ipmi` fan backend and need the limits file to allow them).

use orca_sensors::samples::{BmcSample, SdrEntry};
use orca_sensors::{TimestampMs, Unit};

/// Parses `ipmitool sdr` output.
pub fn parse_sdr(out: &str) -> Vec<SdrEntry> {
    out.lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('|').map(str::trim).collect();
            if parts.len() < 3 || parts[0].is_empty() {
                return None;
            }
            let (value, unit_text) = match parts[1].split_once(' ') {
                Some((v, u)) => match v.parse::<f64>() {
                    Ok(x) if x.is_finite() => (Some(x), u.trim().to_string()),
                    _ => (None, String::new()),
                },
                None => (parts[1].parse::<f64>().ok().filter(|x| x.is_finite()), String::new()),
            };
            // Discrete sensors read "0x00": keep the name with no value.
            let value = if parts[1].starts_with("0x") { None } else { value };
            Some(SdrEntry { name: parts[0].to_string(), value, unit: Unit::from_text(&unit_text), unit_text, status: parts[2].to_string() })
        })
        .collect()
}

/// Parses the `raw 0x30 0x45 0x00` reply (` 02`) into the mode byte.
pub fn parse_fan_mode(out: &str) -> Option<u8> {
    u8::from_str_radix(out.split_whitespace().next()?, 16).ok()
}

async fn run(cmd: &[String], extra: &[&str]) -> Result<String, String> {
    let (prog, pre) = cmd.split_first().ok_or("no ipmitool command")?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(prog).args(pre).args(extra).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "ipmitool timed out".to_string())?
    .map_err(|e| format!("ipmitool: {e}"))?;
    if !out.status.success() {
        return Err(format!("ipmitool exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reads the SDR and (optionally) the fan mode. `ipmitool` is the command prefix.
pub async fn read(ipmitool: &[String], fan_mode: bool, ts: TimestampMs) -> Result<BmcSample, String> {
    let sdr = parse_sdr(&run(ipmitool, &["sdr"]).await?);
    let fan_mode = if fan_mode { run(ipmitool, &["raw", "0x30", "0x45", "0x00"]).await.ok().and_then(|s| parse_fan_mode(&s)) } else { None };
    Ok(BmcSample { ts, sdr, fan_mode })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
CPU1 Temp        | 45 degrees C      | ok
CPU2 Temp        | 47 degrees C      | ok
MB_10G Temp      | 52 degrees C      | ok
FAN1             | 1400 RPM          | ok
FAN5             | no reading        | ns
12V              | 12.19 Volts       | ok
PS1 Status       | 0x01              | ok
";

    #[test]
    fn parses_sdr() {
        let s = parse_sdr(FIXTURE);
        assert_eq!(s.len(), 7);
        assert_eq!(s[0].value, Some(45.0));
        assert_eq!(s[0].unit, Unit::Celsius);
        assert_eq!(s[3].unit, Unit::Rpm);
        assert_eq!(s[4].value, None);
        assert_eq!(s[4].status, "ns");
        assert_eq!(s[5].unit, Unit::Volts);
        assert_eq!(s[6].value, None);
        let b = BmcSample { ts: 1, sdr: s, fan_mode: parse_fan_mode(" 02\n") };
        assert_eq!(b.fan_mode, Some(2));
        let ids: Vec<String> = b.readings().iter().map(|(i, _)| i.to_string()).collect();
        assert!(ids.contains(&"bmc.\"CPU1 Temp\"".into()));
        assert!(!ids.iter().any(|i| i.contains("FAN5")), "no reading is absent, not 0");
    }
}
