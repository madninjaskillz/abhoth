//! hwmon inputs from sysfs (`/sys/class/hwmon/hwmon*/`).
//!
//! Chips are named by their `name` file; when several share a name (two `k10temp`, three
//! `nvme`) they become `k10temp0`, `k10temp1`, … in hwmon index order, so ids are stable across
//! reads. Labels come from `*_label`, else the input stem (`temp1`).

use std::collections::BTreeMap;
use std::path::Path;

use orca_sensors::samples::{HwmonReading, HwmonSample};
use orca_sensors::{TimestampMs, Unit};

/// Input kind from a file stem (`temp1` → °C ÷ 1000).
fn kind(stem: &str) -> Option<(Unit, f64)> {
    let k = stem.trim_end_matches(|c: char| c.is_ascii_digit());
    Some(match k {
        "temp" => (Unit::Celsius, 1000.0),
        "fan" => (Unit::Rpm, 1.0),
        "in" => (Unit::Volts, 1000.0),
        "curr" => (Unit::Amps, 1000.0),
        "power" => (Unit::Watts, 1_000_000.0),
        _ => return None,
    })
}

fn index_of(name: &str) -> u32 {
    name.trim_start_matches("hwmon").parse().unwrap_or(u32::MAX)
}

/// Reads every hwmon device under `class_dir` (normally `/sys/class/hwmon`).
pub fn read(class_dir: &Path, ts: TimestampMs) -> HwmonSample {
    let mut devs: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(class_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("hwmon"))
                .map(|e| (index_of(&e.file_name().to_string_lossy()), e.path()))
                .collect()
        })
        .unwrap_or_default();
    devs.sort();
    let named: Vec<(String, std::path::PathBuf)> = devs
        .into_iter()
        .map(|(_, p)| {
            let n = std::fs::read_to_string(p.join("name"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".into());
            (n, p)
        })
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (n, _) in &named {
        *counts.entry(n.as_str()).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut inputs = Vec::new();
    for (n, p) in &named {
        let chip = if counts[n.as_str()] > 1 {
            let k = seen.entry(n.clone()).or_default();
            *k += 1;
            format!("{n}{}", *k - 1)
        } else {
            n.clone()
        };
        let Ok(rd) = std::fs::read_dir(p) else {
            continue;
        };
        let mut files: Vec<String> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        for f in files {
            let Some(stem) = f.strip_suffix("_input") else {
                continue;
            };
            let Some((unit, div)) = kind(stem) else {
                continue;
            };
            let Some(raw) = std::fs::read_to_string(p.join(&f))
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
            else {
                continue; // unreadable input (sensor asleep): absent, not 0
            };
            let label = std::fs::read_to_string(p.join(format!("{stem}_label")))
                .map(|s| s.trim().to_string())
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.to_string());
            inputs.push(HwmonReading {
                chip: chip.clone(),
                label,
                value: raw / div,
                unit,
            });
        }
    }
    HwmonSample { ts, inputs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(root: &Path, idx: u32, name: &str, files: &[(&str, &str)]) {
        let d = root.join(format!("hwmon{idx}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        for (f, v) in files {
            std::fs::write(d.join(f), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn fake_sysfs() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        dev(
            r,
            0,
            "nvme",
            &[("temp1_input", "38850"), ("temp1_label", "Composite")],
        );
        dev(
            r,
            1,
            "nvme",
            &[("temp1_input", "40850"), ("temp1_label", "Composite")],
        );
        dev(
            r,
            3,
            "k10temp",
            &[
                ("temp1_input", "52125"),
                ("temp1_label", "Tctl"),
                ("temp3_input", "48000"),
                ("temp3_label", "Tccd1"),
            ],
        );
        dev(
            r,
            10,
            "nct6779",
            &[
                ("fan2_input", "812"),
                ("in0_input", "1032"),
                ("power1_input", "12500000"),
                ("pwm2", "128"),
            ],
        );
        dev(r, 5, "drivetemp", &[("temp1_input", "31000")]);
        dev(r, 6, "bad", &[("temp1_input", "xx")]);
        let s = read(r, 1);
        let get = |chip: &str, label: &str| {
            s.inputs
                .iter()
                .find(|i| i.chip == chip && i.label == label)
                .map(|i| (i.value, i.unit))
        };
        assert_eq!(get("nvme0", "Composite"), Some((38.85, Unit::Celsius)));
        assert_eq!(get("nvme1", "Composite"), Some((40.85, Unit::Celsius)));
        assert_eq!(get("k10temp", "Tctl"), Some((52.125, Unit::Celsius)));
        assert_eq!(get("drivetemp", "temp1"), Some((31.0, Unit::Celsius)));
        assert_eq!(get("nct6779", "fan2"), Some((812.0, Unit::Rpm)));
        assert_eq!(get("nct6779", "in0"), Some((1.032, Unit::Volts)));
        assert_eq!(get("nct6779", "power1"), Some((12.5, Unit::Watts)));
        assert!(s.inputs.iter().all(|i| i.chip != "bad"));
        let ids: Vec<String> = s.readings().iter().map(|(i, _)| i.to_string()).collect();
        assert!(ids.contains(&"hwmon.k10temp.Tctl".to_string()));
        assert!(read(&r.join("nope"), 0).inputs.is_empty());
    }
}
