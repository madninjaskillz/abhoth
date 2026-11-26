//! Shadow mode (SUPERVISOR.md 5.4 step 1): compute what hostd would send, send nothing, and
//! log it next to what the current pusher sends, as read from the ESP's SSE stream.
//!
//! [`ShadowLog`] collects both sides as timed events; [`ShadowLog::compare`] lines them up:
//!
//! - for every pusher push, the value hostd would have had on the device at that moment
//!   (its last would-be push), and the difference;
//! - the device duty predicted from hostd's would-be pushes (the firmware model in
//!   [`crate::esp::FirmwareFan`]) against the duty the ESP reports.
//!
//! The two sides sample the GPUs independently (two NVML/nvidia-smi readers), so pushes do not
//! land at the same instants; differences of a few tenths of a degree are expected.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::esp::{DeviceSettings, FirmwareFan};

/// One push, from either side.
#[derive(Debug, Clone, PartialEq)]
pub struct PushEvent {
    pub t: f64,
    pub channel: u32,
    pub temp: f64,
    pub load: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ShadowLog {
    pub ours: Vec<PushEvent>,
    /// Pusher pushes seen over SSE (`GPU N Temp` states; the `GPU N Power` state that came
    /// just before is attached as the load).
    pub pusher: Vec<PushEvent>,
    /// `Fan N Duty` states seen over SSE: (t, channel, duty %).
    pub device_duty: Vec<(f64, u32, f64)>,
    last_power: BTreeMap<u32, f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelReport {
    pub channel: u32,
    pub ours: usize,
    pub pusher: usize,
    /// Pusher pushes that had a hostd value to compare with.
    pub compared: usize,
    pub temp_mean_abs: f64,
    pub temp_max_abs: f64,
    pub load_mean_abs: f64,
    pub load_max_abs: f64,
    /// Device duty samples compared with the duty predicted from hostd's pushes.
    pub duty_compared: usize,
    pub duty_mean_abs: f64,
    pub duty_max_abs: f64,
}

impl ShadowLog {
    pub fn our_push(&mut self, t: f64, channel: u32, temp: &str, load: Option<&str>) {
        if let Ok(temp) = temp.parse() {
            self.ours.push(PushEvent {
                t,
                channel,
                temp,
                load: load.and_then(|l| l.parse().ok()),
            });
        }
    }

    /// Feeds one SSE entity state. Names are the firmware's (`GPU N Temp`, `GPU N Power`,
    /// `Fan N Duty`).
    pub fn sse_state(&mut self, t: f64, name: &str, value: f64) {
        let parse = |prefix: &str, suffix: &str| -> Option<u32> {
            name.strip_prefix(prefix)?
                .strip_suffix(suffix)?
                .parse()
                .ok()
        };
        if let Some(n) = parse("GPU ", " Power") {
            self.last_power.insert(n, value);
        } else if let Some(n) = parse("GPU ", " Temp") {
            let load = self.last_power.remove(&n);
            self.pusher.push(PushEvent {
                t,
                channel: n,
                temp: value,
                load,
            });
        } else if let Some(n) = parse("Fan ", " Duty") {
            self.device_duty.push((t, n, value));
        }
    }

    pub fn compare(&self, settings: &DeviceSettings, stale_s: f64) -> Vec<ChannelReport> {
        let mut chans: Vec<u32> = self
            .ours
            .iter()
            .chain(&self.pusher)
            .map(|e| e.channel)
            .collect();
        chans.sort();
        chans.dedup();
        chans
            .into_iter()
            .map(|c| {
                let ours: Vec<&PushEvent> = self.ours.iter().filter(|e| e.channel == c).collect();
                let theirs: Vec<&PushEvent> =
                    self.pusher.iter().filter(|e| e.channel == c).collect();
                let mut r = ChannelReport {
                    channel: c,
                    ours: ours.len(),
                    pusher: theirs.len(),
                    ..Default::default()
                };
                let mut last_load: Option<f64> = None;
                let mut nl = 0usize;
                for p in &theirs {
                    let Some(o) = ours.iter().rev().find(|o| o.t <= p.t) else {
                        continue;
                    };
                    if p.t - o.t > stale_s {
                        continue;
                    }
                    let d = (p.temp - o.temp).abs();
                    r.compared += 1;
                    r.temp_mean_abs += d;
                    r.temp_max_abs = r.temp_max_abs.max(d);
                    if let Some(pl) = p.load.or(last_load) {
                        last_load = Some(pl);
                        if let Some(ol) = ours
                            .iter()
                            .rev()
                            .filter(|o| o.t <= p.t)
                            .find_map(|o| o.load)
                        {
                            let dl = (pl - ol).abs();
                            nl += 1;
                            r.load_mean_abs += dl;
                            r.load_max_abs = r.load_max_abs.max(dl);
                        }
                    }
                }
                if r.compared > 0 {
                    r.temp_mean_abs /= r.compared as f64;
                }
                if nl > 0 {
                    r.load_mean_abs /= nl as f64;
                }
                // Predict the device duty from our pushes: run the firmware model with arrivals
                // and the 2 s tick, and sample it where the ESP reported its duty.
                let duties: Vec<(f64, f64)> = self
                    .device_duty
                    .iter()
                    .filter(|(_, n, _)| *n == c)
                    .map(|(t, _, d)| (*t, *d))
                    .collect();
                if let (Some(first), Some(_)) = (ours.first(), duties.first()) {
                    let mut fw = FirmwareFan::default();
                    let mut i = 0;
                    let mut tick = first.t;
                    // Start from the device's own duty so the comparison skips the boot ramp.
                    if let Some((_, d0)) = duties.iter().rev().find(|(t, _)| *t <= first.t) {
                        fw.duty = d0 / 100.0;
                    }
                    for (t, d) in &duties {
                        while tick <= *t || ours.get(i).is_some_and(|o| o.t <= *t) {
                            let next_push = ours.get(i).map(|o| o.t).unwrap_or(f64::INFINITY);
                            if next_push <= tick {
                                let o = ours[i];
                                if let Some(l) = o.load {
                                    fw.set_load(o.t, l, settings, stale_s);
                                }
                                fw.set_temp(o.t, o.temp, settings, stale_s);
                                i += 1;
                            } else {
                                fw.tick(tick, settings, stale_s);
                                tick += 2.0;
                            }
                        }
                        if *t < first.t + 60.0 {
                            continue; // let the model settle (the device ramps down 1 %/s)
                        }
                        let diff = (fw.duty * 100.0 - d).abs();
                        r.duty_compared += 1;
                        r.duty_mean_abs += diff;
                        r.duty_max_abs = r.duty_max_abs.max(diff);
                    }
                    if r.duty_compared > 0 {
                        r.duty_mean_abs /= r.duty_compared as f64;
                    }
                }
                r
            })
            .collect()
    }

    /// The side-by-side log lines: every pusher push with hostd's value at that moment.
    pub fn side_by_side(&self) -> String {
        let mut s = String::new();
        for p in &self.pusher {
            let o = self
                .ours
                .iter()
                .rev()
                .find(|o| o.channel == p.channel && o.t <= p.t);
            let fmt_l = |l: Option<f64>| l.map(|l| format!("{l:.1}")).unwrap_or_else(|| "-".into());
            let _ = writeln!(
                s,
                "{:9.3} fan{} pusher T={:.1} L={}  hostd T={} L={}",
                p.t,
                p.channel,
                p.temp,
                fmt_l(p.load),
                o.map(|o| format!("{:.1}", o.temp))
                    .unwrap_or_else(|| "-".into()),
                fmt_l(o.and_then(|o| o.load)),
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_up_pushes_and_predicts_duty() {
        let mut l = ShadowLog::default();
        for k in 0..100 {
            let t = k as f64;
            l.our_push(t, 0, "50.0", Some("0.0"));
            l.sse_state(t + 0.3, "GPU 0 Power", 0.0);
            l.sse_state(t + 0.3, "GPU 0 Temp", 50.2);
            if k % 5 == 0 {
                // 50 °C on the default curve: 30 + 70 × 5/33 ≈ 40.6 %.
                l.sse_state(t + 0.5, "Fan 0 Duty", 40.6);
            }
        }
        let r = &l.compare(&DeviceSettings::FIRMWARE_DEFAULTS, 30.0)[0];
        assert_eq!((r.ours, r.pusher, r.compared), (100, 100, 100));
        assert!((r.temp_mean_abs - 0.2).abs() < 1e-9);
        assert!(r.duty_compared > 0 && r.duty_max_abs < 0.1, "{r:?}");
        assert!(
            l.side_by_side()
                .lines()
                .next()
                .unwrap()
                .contains("pusher T=50.2 L=0.0  hostd T=50.0 L=0.0")
        );
    }
}
