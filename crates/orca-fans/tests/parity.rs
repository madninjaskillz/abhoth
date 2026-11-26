//! Pusher parity (SUPERVISOR.md 5.4): recorded and synthetic nvidia-smi traces are replayed
//! through the supervisor's fan config, and the pushes are compared with what the real
//! `push-gpu-fans.py` posted for the same trace (`fixtures/parity/*.pusher.jsonl`, made by
//! `parity/pusher_replay.py`, which runs the unchanged pusher in virtual time).
//!
//! The config below is today's pusher written as supervisor config: HBM with core fallback,
//! EMA τ 4 s, the 52-64 °C load gate, the 1 °C / 10 % / 1 s / 10 s send rules, the 3 s stale
//! input, and FAN_MAP as `temp`/`load` expressions per fan.

use std::path::Path;

use orca_fans::replay::Replay;
use serde_json::Value;

const COMMON: &str = r#"
[sensors.ctrl_A]
expr = "ema(first(gpu.A.hbm, gpu.A.core), 4s)"
[sensors.ctrl_B]
expr = "ema(first(gpu.B.hbm, gpu.B.core), 4s)"
[sensors.ctrl_C]
expr = "ema(first(gpu.C.hbm, gpu.C.core), 4s)"
[sensors.ctrl_D]
expr = "ema(first(gpu.D.hbm, gpu.D.core), 4s)"

[fan_backends.esp-gpu-fans]
kind      = "esphome"
url       = "http://esp-gpu-fans.lan"
transport = "web_server"
auth      = { kind = "digest", user = "fans", password = "secret:esp_fans_password" }
names     = { temp = "GPU {n} Temp", load = "GPU {n} Power" }
device_stale_seconds = 30
sync_device_settings = false
mode      = "shadow"

[fan_defaults]
mode         = "device_curve"
load_gate    = { lo = 52, hi = 64 }
curve        = [[60, 30], [82, 100]]
feed_forward = { lo = 50, hi = 100, max_duty = 60 }
min_duty     = 30
max_temp     = 85
ramp_down    = "2%/2s"
stale_input  = "3s"
send         = { temp_delta = 1.0, load_delta = 10, min_gap = "1s", heartbeat = "10s" }
alerts       = { temp_warn = 75, stalled = true, failsafe = true }
"#;

/// FAN_MAP="01:00.0 21:00.0 E1:00.0" (today's env file).
const MAP_DEFAULT: &str = r#"
[fans.duct0]
backend = "esp-gpu-fans"
channel = 0
temp    = "sensor.ctrl_B"
load    = "gpu.B.power_pct"
[fans.duct1]
backend = "esp-gpu-fans"
channel = 1
temp    = "sensor.ctrl_A"
load    = "gpu.A.power_pct"
[fans.duct2]
backend = "esp-gpu-fans"
channel = 2
temp    = "sensor.ctrl_C"
load    = "gpu.C.power_pct"
"#;

/// FAN_MAP="01:00.0+21:00.0 max E1:00.0": a strict pair and the hottest card in the box.
const MAP_VARIANTS: &str = r#"
[fans.duct0]
backend = "esp-gpu-fans"
channel = 0
temp    = "max(sensor.ctrl_B, sensor.ctrl_A)"
load    = "max_avail(gpu.B.power_pct, gpu.A.power_pct)"
[fans.duct1]
backend = "esp-gpu-fans"
channel = 1
temp    = "max_avail(sensor.ctrl_A, sensor.ctrl_B, sensor.ctrl_C, sensor.ctrl_D)"
load    = "max_avail(gpu.A.power_pct, gpu.B.power_pct, gpu.C.power_pct, gpu.D.power_pct)"
[fans.duct2]
backend = "esp-gpu-fans"
channel = 2
temp    = "sensor.ctrl_C"
load    = "gpu.C.power_pct"
"#;

const CARDS: [(&str, &str); 4] = [
    ("01:00.0", "B"),
    ("21:00.0", "A"),
    ("44:00.0", "D"),
    ("E1:00.0", "C"),
];

struct Trace {
    lines: Vec<(f64, String)>,
    fails: Vec<(f64, f64)>,
}

fn load_trace(name: &str) -> Trace {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/parity")
        .join(name);
    let mut t = Trace {
        lines: Vec::new(),
        fails: Vec::new(),
    };
    for l in std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let v: Value = serde_json::from_str(l).unwrap();
        if let Some(line) = v.get("line") {
            t.lines
                .push((v["t"].as_f64().unwrap(), line.as_str().unwrap().to_string()));
        } else if let Some(f) = v.get("fail") {
            t.fails
                .push((f[0].as_f64().unwrap(), f[1].as_f64().unwrap()));
        }
    }
    t
}

fn load_golden(name: &str) -> Vec<(f64, String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/parity")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (
                v["t"].as_f64().unwrap(),
                v["entity"].as_str().unwrap().into(),
                v["value"].as_str().unwrap().into(),
            )
        })
        .collect()
}

/// Replays a trace through the supervisor config on the pusher's schedule (see
/// [`orca_fans::replay`]), with the trace's POST failure windows.
fn replay(config: &str, trace: &Trace) -> Vec<(f64, String, String)> {
    let mut r = Replay::new(config, &CARDS).unwrap();
    let ok = |t: f64| !trace.fails.iter().any(|(a, b)| *a <= t && t <= *b);
    let mut sent = Vec::new();
    for (t, line) in &trace.lines {
        sent.extend(r.run_until(*t, false, &ok));
        r.sample(*t, line);
    }
    let end = trace.lines.last().map(|l| l.0).unwrap_or(0.0) + 15.0;
    sent.extend(r.run_until(end, true, &ok));
    let mut out = Vec::new();
    for s in sent {
        if let Some(l) = &s.push.load {
            out.push((s.t, format!("GPU {} Power", s.channel), l.clone()));
        }
        out.push((s.t, format!("GPU {} Temp", s.channel), s.push.temp.clone()));
    }
    out
}

fn check(trace: &str, map: &str) {
    let t = load_trace(&format!("{trace}.jsonl"));
    let golden = load_golden(&format!("{trace}.pusher.jsonl"));
    let ours = replay(&format!("{COMMON}{map}"), &t);
    let first_diff = ours
        .iter()
        .zip(&golden)
        .position(|(a, b)| (a.0 - b.0).abs() > 1e-6 || a.1 != b.1 || a.2 != b.2);
    if let Some(i) =
        first_diff.or((ours.len() != golden.len()).then(|| ours.len().min(golden.len())))
    {
        let lo = i.saturating_sub(3);
        panic!(
            "{trace}: {} pushes vs the pusher's {}; first difference at #{i}:\n ours:   {:?}\n pusher: {:?}",
            ours.len(),
            golden.len(),
            &ours[lo..(i + 3).min(ours.len())],
            &golden[lo..(i + 3).min(golden.len())]
        );
    }
    assert!(!golden.is_empty());
}

#[test]
fn parity_recorded_trace() {
    check("recorded", MAP_DEFAULT);
}

#[test]
fn parity_heat_ramp() {
    check("heat_ramp", MAP_DEFAULT);
}

#[test]
fn parity_faults() {
    check("faults", MAP_DEFAULT);
}

#[test]
fn parity_fan_map_variants() {
    check("fanmap", MAP_VARIANTS);
}
