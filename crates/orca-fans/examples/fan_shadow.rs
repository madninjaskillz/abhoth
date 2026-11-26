//! Fan shadow mode from a recording (or a live, read-only stream): replays nvidia-smi samples
//! through the supervisor's fan config, sends nothing, and prints what hostd would push next
//! to what the current pusher pushed (seen on the ESP's SSE), then a summary, the predicted vs
//! reported device duty, and the read-only diff of the device's curve numbers against config.
//!
//!     cargo run -p orca-fans --example fan_shadow -- <recording.jsonl | -> [config.toml] [--quiet]
//!
//! The recording is JSON lines `{"t", "src": "smi" | "sse", "line"}`: `smi` lines are the
//! pusher's nvidia-smi query, `sse` lines are the `data:` payloads of the ESP's `/events`
//! (GET only). With `-` the stream is read from stdin as it arrives.

use std::io::BufRead;

use orca_fans::config::FansConfig;
use orca_fans::controller::FansPolicy;
use orca_fans::esp::{DeviceSettings, DeviceSync, parse_state_event};
use orca_fans::limits::FanLimits;
use orca_fans::replay::ShadowSession;

const CARDS: [(&str, &str); 4] = [
    ("01:00.0", "B"),
    ("21:00.0", "A"),
    ("44:00.0", "D"),
    ("E1:00.0", "C"),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quiet = args.iter().any(|a| a == "--quiet");
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(src) = pos.first() else {
        eprintln!("usage: fan_shadow <recording.jsonl | -> [config.toml] [--quiet]");
        std::process::exit(2);
    };
    let config = match pos.get(1) {
        Some(p) => std::fs::read_to_string(p).expect("config"),
        None => include_str!("pusher-parity.toml").to_string(),
    };
    let mut s = ShadowSession::new(&config, &CARDS).expect("config");
    let reader: Box<dyn BufRead> = if src.as_str() == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        Box::new(std::io::BufReader::new(
            std::fs::File::open(src.as_str()).expect("recording"),
        ))
    };
    let mut last_t = 0.0;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let t = v["t"].as_f64().unwrap_or(last_t);
        last_t = t;
        let body = v["line"].as_str().unwrap_or_default();
        match v["src"].as_str() {
            Some("smi") => {
                for p in s.smi(t, body) {
                    if !quiet {
                        println!(
                            "{:9.3} fan{} hostd  T={} L={} ({})",
                            p.t,
                            p.channel,
                            p.push.temp,
                            p.push.load.as_deref().unwrap_or("-"),
                            p.push.reason.as_str()
                        );
                    }
                }
            }
            Some("sse") => {
                if let Some((name, val)) = parse_state_event(body) {
                    if !quiet && name.starts_with("GPU ") && name.ends_with(" Temp") {
                        println!(
                            "{t:9.3} {name:<10} pusher T={}",
                            val.as_f64()
                                .map(|x| format!("{x:.1}"))
                                .unwrap_or("-".into())
                        );
                    }
                    s.sse(t, &name, val);
                }
            }
            _ => {}
        }
    }
    s.finish(last_t);
    let settings = s
        .esp
        .settings()
        .unwrap_or(DeviceSettings::FIRMWARE_DEFAULTS);
    println!("\n== shadow summary ({last_t:.0} s) ==");
    println!(
        "fan  hostd  pusher  compared  |dT| mean/max  |dL| mean/max  duty pts  |dDuty| mean/max"
    );
    for r in s.log.compare(&settings, 30.0) {
        println!(
            "{:>3}  {:>5}  {:>6}  {:>8}  {:>6.2}/{:<5.1}  {:>6.2}/{:<5.1}  {:>8}  {:>6.2}/{:<5.1}",
            r.channel,
            r.ours,
            r.pusher,
            r.compared,
            r.temp_mean_abs,
            r.temp_max_abs,
            r.load_mean_abs,
            r.load_max_abs,
            r.duty_compared,
            r.duty_mean_abs,
            r.duty_max_abs
        );
    }
    // The read-only diff phase of sync_device_settings: report, never write.
    let cfg = FansConfig::from_toml(&config).expect("config");
    let policy = FansPolicy::compile(&cfg, &FanLimits::default(), &|_| true).expect("config");
    match (s.esp.settings(), policy.fans.first()) {
        (Some(dev), Some(f)) => match DeviceSettings::from_policy(&f.policy) {
            Ok(want) => {
                let d = DeviceSync::new(Default::default()).diff(&dev, &want);
                if d.is_empty() {
                    println!(
                        "device curve numbers: match config (read-only diff, nothing written)"
                    );
                }
                for x in d {
                    println!(
                        "device curve numbers: {} device={} config={} (not written)",
                        x.entity, x.device, x.config
                    );
                }
            }
            Err(e) => println!("device curve numbers: config cannot own them: {e}"),
        },
        _ => println!("device curve numbers: not seen on SSE"),
    }
}
