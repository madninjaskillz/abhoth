//! Shadow mode against a real 20-minute recording (2026-11-26, read-only: nvidia-smi on the
//! host and GET /events on the ESP while the pusher ran). hostd's would-be pushes are compared
//! with the pusher's pushes seen over SSE, and the device duty predicted from hostd's pushes
//! with the duty the ESP reported. Nothing is sent.

use std::path::Path;

use orca_fans::esp::{DeviceSettings, DeviceSync, EntityValue};
use orca_fans::replay::ShadowSession;
use serde_json::Value;

const CARDS: [(&str, &str); 4] = [
    ("01:00.0", "B"),
    ("21:00.0", "A"),
    ("44:00.0", "D"),
    ("E1:00.0", "C"),
];

fn lines(name: &str) -> Vec<Value> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/parity")
        .join(name);
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn shadow_against_the_live_pusher() {
    let mut s = ShadowSession::new(include_str!("../examples/pusher-parity.toml"), &CARDS).unwrap();
    // Merge the two recorded streams by time.
    let mut ev: Vec<(f64, Value, bool)> = lines("recorded.jsonl")
        .into_iter()
        .map(|v| (v["t"].as_f64().unwrap(), v, true))
        .collect();
    ev.extend(
        lines("recorded.sse.jsonl")
            .into_iter()
            .map(|v| (v["t"].as_f64().unwrap(), v, false)),
    );
    ev.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut end = 0.0;
    for (t, v, smi) in ev {
        end = t;
        if smi {
            s.smi(t, v["line"].as_str().unwrap());
        } else {
            let val = match &v["value"] {
                Value::Bool(b) => EntityValue::Bool(*b),
                Value::Number(n) => EntityValue::Number(n.as_f64().unwrap()),
                // ESPHome sends number entities' values as strings ("60").
                Value::String(x) => x
                    .parse()
                    .map(EntityValue::Number)
                    .unwrap_or(EntityValue::None),
                _ => EntityValue::None,
            };
            s.sse(t, v["name"].as_str().unwrap(), val);
        }
    }
    s.finish(end);
    let settings = s.esp.settings().expect("device settings seen on SSE");
    // Read-only diff phase: the device holds the numbers the parity config says (which are
    // not the firmware defaults nor SUPERVISOR.md 3.2's: they were retuned on 2026-11-25).
    assert_ne!(settings, DeviceSettings::FIRMWARE_DEFAULTS);
    let want = DeviceSettings::from_policy(&s.replay.fan_loop.spec(0).policy).unwrap();
    assert!(
        DeviceSync::new(Default::default())
            .diff(&settings, &want)
            .is_empty()
    );
    let report = s.log.compare(&settings, 30.0);
    assert_eq!(report.len(), 3);
    for r in &report {
        eprintln!("{r:?}");
        // Both sides send on the 10 s heartbeat at idle: about the same number of pushes.
        assert!(
            (r.ours as f64 - r.pusher as f64).abs() <= 0.1 * r.pusher as f64 + 3.0,
            "{r:?}"
        );
        assert!(r.compared as f64 >= 0.9 * r.pusher as f64, "{r:?}");
        // Two independent nvidia-smi readers, the same EMA: well under the 1 °C send threshold.
        assert!(r.temp_mean_abs < 0.5 && r.temp_max_abs <= 1.5, "{r:?}");
        // The device duty predicted from hostd's pushes matches what the ESP reported.
        assert!(r.duty_compared > 100 && r.duty_mean_abs < 2.0, "{r:?}");
    }
    assert_eq!(s.esp.flag("Failsafe Active"), Some(false));
}
