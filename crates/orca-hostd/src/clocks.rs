//! The clocks/power loop: `orca_gpuctl::Controller` on its own thread.
//!
//! The controller and the driver (`GpuControl`) are synchronous, so they live on a plain thread
//! that receives [`ClockMsg`]s. GPU readings come from hostd's telemetry sampler (one
//! `nvidia-smi` reader for telemetry, fans and clocks); the thread feeds them to
//! `Controller::sample` and runs `tick` at least once a second. Every driver command goes
//! through `LimitedControl`, so nothing outside the limits file reaches the hardware.
//!
//! The lowered-cards file is rewritten whenever the set of cards hostd holds low changes.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use orca_gpuctl::controller::CardStatus;
use orca_gpuctl::controller::AssertOutcome;
use orca_gpuctl::{AssertReason, CardPowerConfig, ClockEvent, Controller, GpuControl, GpuReading, LimitedControl, Limits, Mode, WakeSource};
use orca_sensors::samples::GpuSample;
use tokio::sync::oneshot;

use crate::files::{Paths, write_lowered};

/// A card for the controller: (label, uuid, config).
pub type CardSpec = (String, String, CardPowerConfig);

pub enum ClockMsg {
    /// A new policy: rebuild the controller and apply `serving` on every card.
    Configure(Vec<CardSpec>),
    /// GPU samples from the telemetry sampler.
    Samples(Vec<GpuSample>),
    Wake { card: String, source: WakeSource, detail: String },
    Hold { card: String, reason: String, ttl: Duration, reply: oneshot::Sender<Result<(), String>> },
    Release { card: String, reason: String },
    Cap { card: String, cap: Option<u32>, ttl: Duration, reply: oneshot::Sender<Result<(), String>> },
    Assert { card: String, reason: AssertReason, reply: oneshot::Sender<Option<AssertOutcome>> },
    Status { reply: oneshot::Sender<Vec<CardStatus>> },
    /// Shutdown: every card back to serving (no card left low), then the thread ends.
    Shutdown { reply: oneshot::Sender<()> },
}

/// Converts a telemetry sample to the controller's reading.
pub fn to_reading(s: &GpuSample) -> GpuReading {
    let u = |v: Option<f64>| v.filter(|x| x.is_finite() && *x >= 0.0).map(|x| x.round() as u32);
    GpuReading {
        uuid: s.uuid.clone(),
        util_pct: u(s.util_pct),
        temp_c: u(s.core_c),
        mem_temp_c: u(s.mem_temp_c),
        sm_mhz: u(s.sm_mhz),
        max_sm_mhz: u(s.max_sm_mhz),
        app_sm_mhz: u(s.app_sm_mhz),
        app_mem_mhz: u(s.app_mem_mhz),
        power_limit_w: s.power_limit_w,
        power_draw_w: s.power_w,
        persistence: s.persistence,
        event_reasons: s.throttle_reasons,
    }
}

/// Starts the loop. `events` receives every controller event (for logs, alerts, the feed).
pub fn spawn(
    gpu: Box<dyn GpuControl + Send>,
    limits: Limits,
    mode: Mode,
    paths: Option<Paths>,
    events: tokio::sync::mpsc::UnboundedSender<ClockEvent>,
) -> (Sender<ClockMsg>, JoinHandle<()>) {
    let (tx, rx) = channel();
    let h = std::thread::Builder::new()
        .name("hostd-clocks".into())
        .spawn(move || run(gpu, limits, mode, paths, rx, events))
        .expect("spawn clocks thread");
    (tx, h)
}

fn run(
    mut gpu: Box<dyn GpuControl + Send>,
    limits: Limits,
    mode: Mode,
    paths: Option<Paths>,
    rx: Receiver<ClockMsg>,
    events: tokio::sync::mpsc::UnboundedSender<ClockEvent>,
) {
    let t0 = Instant::now();
    let mut ctl: Option<Controller> = None;
    let mut lowered: Vec<String> = Vec::new();
    loop {
        let msg = rx.recv_timeout(Duration::from_secs(1));
        let now = t0.elapsed();
        let mut g = LimitedControl { limits: &limits, inner: &mut *gpu };
        let mut out: Vec<ClockEvent> = Vec::new();
        let mut stop = None;
        match msg {
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(c) = ctl.as_mut() {
                    out.extend(c.set_mode(&mut g, Mode::Off, now));
                }
                stop = Some(None);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(c) = ctl.as_mut() {
                    out.extend(c.tick(&mut g, now));
                }
            }
            Ok(m) => match m {
                ClockMsg::Configure(cards) => {
                    // A previous controller may hold cards low: bring them back first.
                    if let Some(c) = ctl.as_mut() {
                        out.extend(c.set_mode(&mut g, Mode::Off, now));
                    }
                    let mut c = Controller::new(cards, mode, now);
                    out.extend(c.start(&mut g, now));
                    ctl = Some(c);
                }
                ClockMsg::Samples(samples) => {
                    if let Some(c) = ctl.as_mut() {
                        let ids: Vec<(String, String)> = c.cards().map(|s| (s.id.clone(), s.uuid.clone())).collect();
                        for (id, uuid) in ids {
                            let r = samples.iter().find(|s| s.uuid == uuid).map(to_reading);
                            out.extend(c.sample(&mut g, &id, now, r));
                        }
                        out.extend(c.tick(&mut g, now));
                    }
                }
                ClockMsg::Wake { card, source, detail } => {
                    if let Some(c) = ctl.as_mut() {
                        out.extend(c.wake(&mut g, &card, source, &detail, now));
                    }
                }
                ClockMsg::Hold { card, reason, ttl, reply } => {
                    let r = match ctl.as_mut() {
                        Some(c) => c.hold(&mut g, &card, &reason, ttl, now).map(|e| out.extend(e)),
                        None => Err("no clock policy".into()),
                    };
                    let _ = reply.send(r);
                }
                ClockMsg::Release { card, reason } => {
                    if let Some(c) = ctl.as_mut() {
                        c.release(&card, &reason);
                    }
                }
                ClockMsg::Cap { card, cap, ttl, reply } => {
                    let r = match ctl.as_mut() {
                        Some(c) => c.override_cap(&mut g, &card, cap, ttl, Some(&limits), now).map(|e| out.extend(e)),
                        None => Err("no clock policy".into()),
                    };
                    let _ = reply.send(r);
                }
                ClockMsg::Assert { card, reason, reply } => {
                    let r = match ctl.as_mut() {
                        Some(c) => {
                            let (o, e) = c.assert(&mut g, &card, reason, now);
                            out.extend(e);
                            o
                        }
                        None => None,
                    };
                    let _ = reply.send(r);
                }
                ClockMsg::Status { reply } => {
                    let _ = reply.send(ctl.as_ref().map(|c| c.status(now)).unwrap_or_default());
                }
                ClockMsg::Shutdown { reply } => {
                    if let Some(c) = ctl.as_mut() {
                        out.extend(c.set_mode(&mut g, Mode::Off, now));
                    }
                    stop = Some(Some(reply));
                }
            },
        }
        for e in out {
            let _ = events.send(e);
        }
        let now_low = ctl.as_ref().map(Controller::lowered_uuids).unwrap_or_default();
        if now_low != lowered {
            lowered = now_low;
            if let Some(p) = &paths
                && let Err(e) = write_lowered(p, &lowered)
            {
                eprintln!("hostd: writing the lowered-cards file: {e}");
            }
        }
        if let Some(reply) = stop {
            if let Some(r) = reply {
                let _ = r.send(());
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_gpuctl::{FakeNvml, GpuCommand};

    const LIMITS: &str = r#"
[gpus."GPU-a"]
label = "A"
actions = ["persistence", "power_limit", "lock_clocks", "reset_clocks"]
clock_mhz = [135, 1597]
power_limit_w = [250, 350]
"#;

    fn sample(uuid: &str, util: f64) -> GpuSample {
        GpuSample {
            uuid: uuid.into(),
            util_pct: Some(util),
            core_c: Some(40.0),
            sm_mhz: Some(1597.0),
            max_sm_mhz: Some(1597.0),
            power_limit_w: Some(350.0),
            persistence: Some(true),
            ..Default::default()
        }
    }

    /// A fake driver shared with the test through a mutex.
    struct Shared(std::sync::Arc<std::sync::Mutex<FakeNvml>>);
    impl GpuControl for Shared {
        fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), orca_gpuctl::nvml::GpuError> {
            self.0.lock().unwrap().apply(uuid, cmd)
        }
        fn read(&mut self, uuid: &str) -> Result<GpuReading, orca_gpuctl::nvml::GpuError> {
            self.0.lock().unwrap().read(uuid)
        }
    }

    #[tokio::test]
    async fn configure_assert_status_shutdown_with_fake_nvml() {
        let fake = std::sync::Arc::new(std::sync::Mutex::new(FakeNvml::default().with_card("GPU-a", 1597)));
        let limits = Limits::parse(LIMITS).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { limits: dir.path().join("l"), state_dir: dir.path().join("s"), run_dir: dir.path().join("r") };
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, h) = spawn(Box::new(Shared(fake.clone())), limits, Mode::On, Some(paths), etx);
        let cfg: CardPowerConfig = toml::from_str("persistence = true\npower_limit_w = 350\nserving = \"unlocked\"").unwrap();
        tx.send(ClockMsg::Configure(vec![("A".into(), "GPU-a".into(), cfg)])).unwrap();
        tx.send(ClockMsg::Samples(vec![sample("GPU-a", 50.0)])).unwrap();
        let (r, rr) = oneshot::channel();
        tx.send(ClockMsg::Assert { card: "A".into(), reason: AssertReason::EngineReady, reply: r }).unwrap();
        let o = rr.await.unwrap().expect("assert outcome");
        assert_eq!(o.card, "A");
        let (r, rr) = oneshot::channel();
        tx.send(ClockMsg::Status { reply: r }).unwrap();
        let st = rr.await.unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].serving, "unlocked");
        // A command outside the limits never reaches the driver: cap 2000 > 1597.
        let (r, rr) = oneshot::channel();
        tx.send(ClockMsg::Cap { card: "A".into(), cap: Some(2000), ttl: Duration::from_secs(60), reply: r }).unwrap();
        assert!(rr.await.unwrap().is_err());
        let (r, rr) = oneshot::channel();
        tx.send(ClockMsg::Shutdown { reply: r }).unwrap();
        rr.await.unwrap();
        h.join().unwrap();
        let mut n = 0;
        while erx.try_recv().is_ok() {
            n += 1;
        }
        assert!(n > 0, "the controller reported what it applied");
        assert!(fake.lock().unwrap().log.iter().all(|(u, _)| u == "GPU-a"));
    }

    #[test]
    fn reading_conversion() {
        let r = to_reading(&GpuSample { uuid: "u".into(), util_pct: Some(12.6), mem_temp_c: None, ..Default::default() });
        assert_eq!(r.util_pct, Some(13));
        assert_eq!(r.mem_temp_c, None);
    }
}
