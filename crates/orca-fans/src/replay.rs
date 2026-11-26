//! Replays nvidia-smi samples through a fan config in (virtual or live) time, on the pusher's
//! schedule, without touching any hardware. Used by the parity tests, the shadow comparison
//! and the `fan_shadow` example (which can also follow a live, read-only recording stream).
//!
//! Samples are the pusher's query: `pci.bus_id, temperature.gpu, temperature.memory,
//! power.draw, enforced.power.limit`. Each sample replaces its card's metrics (a metric the
//! card stops reporting becomes absent), then the `[sensors.*]` are evaluated. On each loop
//! tick (first at 1.5 s, then every 0.25 s, as the pusher) the sensors are evaluated again and
//! the fan loop runs.

use std::collections::BTreeMap;

use orca_sensors::samples::GpuSample;
use orca_sensors::{SensorId, SensorRegistry, SensorStore};

use crate::config::FansConfig;
use crate::controller::{Action, FanLoop, FansPolicy};
use crate::inputs::ExprInputs;
use crate::limits::FanLimits;
use crate::push::Push;

/// The pusher's main-loop period and start delay.
pub const TICK: f64 = 0.25;
pub const START: f64 = 1.5;

/// One would-be push: channel, the push, and the time.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub t: f64,
    pub fan: usize,
    pub channel: u32,
    pub push: Push,
}

pub struct Replay {
    registry: SensorRegistry,
    store: SensorStore,
    inputs: ExprInputs,
    pub fan_loop: FanLoop,
    cards: BTreeMap<String, String>,
    next_tick: f64,
}

/// Parses the pusher's nvidia-smi CSV line into (bus `bb:dd.f` upper-case, sample).
pub fn parse_smi(line: &str, ts_ms: i64) -> Option<(String, GpuSample)> {
    let p: Vec<&str> = line.split(',').map(str::trim).collect();
    if p.len() != 5 {
        return None;
    }
    let bus = p[0]
        .trim_start_matches("00000000:")
        .trim_start_matches("0000:")
        .to_ascii_uppercase();
    let num = |s: &str| s.parse::<f64>().ok().filter(|v| v.is_finite());
    let core = num(p[1])?;
    Some((
        bus.clone(),
        GpuSample {
            bus: bus.to_ascii_lowercase(),
            ts: ts_ms,
            core_c: Some(core),
            mem_temp_c: num(p[2]),
            power_w: num(p[3]),
            power_limit_w: num(p[4]),
            ..Default::default()
        },
    ))
}

impl Replay {
    /// `config` is a whole supervisor.toml (its `[sensors.*]` and fan tables are used);
    /// `cards` maps bus ids (`01:00.0`) to card labels (`B`).
    pub fn new(config: &str, cards: &[(&str, &str)]) -> Result<Replay, String> {
        let doc: toml::Table = toml::from_str(config).map_err(|e| e.to_string())?;
        let mut sensors: Vec<(String, String)> = Vec::new();
        if let Some(t) = doc.get("sensors").and_then(|s| s.as_table()) {
            for (k, v) in t {
                let e = v
                    .get("expr")
                    .and_then(|e| e.as_str())
                    .ok_or_else(|| format!("sensors.{k}: expr is required"))?;
                sensors.push((k.clone(), e.to_string()));
            }
        }
        let registry = SensorRegistry::new(sensors.iter().map(|(a, b)| (a.as_str(), b.as_str())))
            .map_err(|e| e.to_string())?;
        let cfg = FansConfig::from_toml(config).map_err(|e| e.to_string())?;
        let policy = FansPolicy::compile(&cfg, &FanLimits::default(), &|_| true)
            .map_err(|e| e.to_string())?;
        let inputs = ExprInputs::new(&policy).map_err(|(w, e)| format!("{w}: {e}"))?;
        Ok(Replay {
            registry,
            store: SensorStore::new(),
            inputs,
            fan_loop: FanLoop::new(&policy),
            cards: cards
                .iter()
                .map(|(b, c)| (b.to_ascii_uppercase(), c.to_string()))
                .collect(),
            next_tick: START,
        })
    }

    /// Feeds one nvidia-smi line that arrived at `t` seconds. Unknown buses are ignored.
    pub fn sample(&mut self, t: f64, line: &str) {
        let ts = (t * 1000.0).round() as i64;
        let Some((bus, s)) = parse_smi(line, ts) else {
            return;
        };
        let Some(card) = self.cards.get(&bus).cloned() else {
            return;
        };
        for m in ["core", "hbm", "power_w", "power_limit_w", "power_pct"] {
            if let Ok(id) = SensorId::parse(&format!("gpu.{card}.{m}")) {
                self.store.remove(&id);
            }
        }
        self.store.extend(s.readings(&card));
        self.registry.evaluate(&mut self.store, ts);
    }

    /// Runs the loop ticks up to `t` (including `t` when `inclusive`). Call it with
    /// `inclusive = false` before feeding a sample that arrived at `t`: a tick at exactly the
    /// arrival time sees the sample, as in the pusher. `ok(t)` says whether a push at that time
    /// would have succeeded (failure injection). Returns the would-be pushes.
    pub fn run_until(&mut self, t: f64, inclusive: bool, ok: &dyn Fn(f64) -> bool) -> Vec<Sent> {
        let mut out = Vec::new();
        while self.next_tick < t || (inclusive && self.next_tick == t) {
            let now = self.next_tick;
            let now_ms = (now * 1000.0).round() as i64;
            self.registry.evaluate(&mut self.store, now_ms);
            let (inputs, store) = (&mut self.inputs, &self.store);
            let st = self
                .fan_loop
                .step(now, &mut |i, _| inputs.eval(i, store, now_ms));
            for s in st {
                if let Some(Action::Push { fan, push }) = s.action {
                    let channel = self.fan_loop.spec(fan).channel.index().unwrap_or(u32::MAX);
                    let good = ok(now);
                    self.fan_loop.push_result(fan, &push, good, now);
                    if good {
                        out.push(Sent {
                            t: now,
                            fan,
                            channel,
                            push,
                        });
                    }
                }
            }
            self.next_tick += TICK;
        }
        out
    }

    /// The next tick time.
    pub fn next_tick(&self) -> f64 {
        self.next_tick
    }
}

/// Shadow mode over a recording or a live read-only stream: hostd's would-be pushes on one
/// side, the pusher's pushes and the device state from the ESP's SSE on the other. Nothing
/// is ever sent.
pub struct ShadowSession {
    pub replay: Replay,
    pub log: crate::shadow::ShadowLog,
    pub esp: crate::esp::EspState,
}

impl ShadowSession {
    pub fn new(config: &str, cards: &[(&str, &str)]) -> Result<ShadowSession, String> {
        Ok(ShadowSession {
            replay: Replay::new(config, cards)?,
            log: Default::default(),
            esp: Default::default(),
        })
    }

    /// An nvidia-smi line at `t`. Returns hostd's would-be pushes up to `t`.
    pub fn smi(&mut self, t: f64, line: &str) -> Vec<Sent> {
        let sent = self.replay.run_until(t, false, &|_| true);
        self.record(&sent);
        self.replay.sample(t, line);
        sent
    }

    /// An ESP entity state at `t` (from SSE).
    pub fn sse(&mut self, t: f64, name: &str, value: crate::esp::EntityValue) {
        if let Some(v) = value.as_f64() {
            self.log.sse_state(t, name, v);
        }
        self.esp.apply(name.to_string(), value, t);
    }

    /// Runs the remaining ticks up to `t`.
    pub fn finish(&mut self, t: f64) -> Vec<Sent> {
        let sent = self.replay.run_until(t, true, &|_| true);
        self.record(&sent);
        sent
    }

    fn record(&mut self, sent: &[Sent]) {
        for s in sent {
            self.log
                .our_push(s.t, s.channel, &s.push.temp, s.push.load.as_deref());
        }
    }
}
