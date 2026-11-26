//! The fan loop in hostd: `orca_fans::FanLoop` over hostd's sensor store.
//!
//! - Every tick (500 ms) each fan's `temp` / `load` expressions are evaluated against the
//!   store, the loop decides, and the actions go to the backends. Missing input is never 0:
//!   device fans get nothing pushed (the device fails safe), host fans go to their safe state.
//! - Backends run in `act` only if the backend config says `act` **and** the limits file lets
//!   that kind write; otherwise they are wrapped in `Shadow` (computed, logged, never written).
//! - ESPHome: one `/events` reader per device (GET only) feeds `esp.<backend>.<entity>`
//!   readings and the fan readings (`fan.<id>.rpm|duty|…`).
//! - Watchdog: a separate OS thread puts host-curve fans into their safe state if the loop has
//!   not ticked for 5 s (a stuck runtime cannot stop it).
//! - Shutdown: every backend's `release` (hardware auto / device curve).
//!
//! Wired kinds: `esphome` and `fake`. A fan on any other kind is refused at `policy.put` until
//! its backend is wired here (the kinds exist in orca-fans).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use orca_fans::backend::esphome::EspHome;
use orca_fans::backend::fake::Fake;
use orca_fans::backend::{BackendError, Caps, ChannelReading, FanBackend, Shadow};
use orca_fans::config::{BackendConfig, BackendKind, Channel, FanMode, RunMode, SecretRef};
use orca_fans::controller::{Action, FanLoop, FansPolicy, execute};
use orca_fans::esp::{EntityValue, EspState};
use orca_fans::inputs::ExprInputs;
use orca_sensors::samples::FanSample;
use orca_sensors::{Namespace, Reading, SensorId, SensorStore, Unit, now_ms};

use crate::limits::Limits;

/// Backend kinds hostd can open today.
pub fn wired(kind: BackendKind) -> bool {
    matches!(kind, BackendKind::Esphome | BackendKind::Fake)
}

pub fn kind_name(kind: BackendKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// hostd's checks on the `fans` part of a policy (on top of orca-fans' own validation).
pub fn validate(p: &FansPolicy, limits: &Limits) -> Result<(), Vec<String>> {
    let mut why = Vec::new();
    if let Err(e) = p.validate(&limits.fans, &|k| limits.backend_writes(&kind_name(k))) {
        why.push(e);
    }
    for f in &p.fans {
        match p.backends.get(&f.backend) {
            Some(b) if !wired(b.kind) => why.push(format!(
                "fans.{}: backend kind {} is not wired into hostd yet",
                f.id,
                kind_name(b.kind)
            )),
            _ => {}
        }
    }
    if let Err(e) = ExprInputs::new(p) {
        why.push(format!("{}: {}", e.0, e.1));
    }
    if why.is_empty() { Ok(()) } else { Err(why) }
}

/// Resolves `secret:<name>` to a file `secrets_dir/<name>` (root-only files on the apps pool).
pub fn resolve_secret(r: &SecretRef, secrets_dir: &std::path::Path) -> Result<String, String> {
    r.resolve(&|name| {
        Some(SecretRef::File(
            secrets_dir.join(name).display().to_string(),
        ))
    })
}

/// An `Arc<EspHome>` as a backend, so the SSE reader and the loop share one device state.
struct ArcEsp(Arc<EspHome>);

#[async_trait]
impl FanBackend for ArcEsp {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn kind(&self) -> BackendKind {
        self.0.kind()
    }
    fn caps(&self) -> Caps {
        self.0.caps()
    }
    async fn push_inputs(
        &self,
        ch: &Channel,
        temp: &str,
        load: Option<&str>,
    ) -> Result<(), BackendError> {
        self.0.push_inputs(ch, temp, load).await
    }
    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        self.0.set_duty(ch, duty).await
    }
    async fn set_setting(&self, e: &str, v: f64) -> Result<(), BackendError> {
        self.0.set_setting(e, v).await
    }
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        self.0.read().await
    }
    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        self.0.fail_safe(ch).await
    }
    async fn release(&self) -> Result<(), BackendError> {
        self.0.release().await
    }
}

/// The opened backends of one policy.
pub struct Opened {
    pub backends: BTreeMap<String, Arc<dyn FanBackend>>,
    /// ESPHome devices by backend id, for the SSE readers and `esp.*` readings.
    pub esp: BTreeMap<String, Arc<EspHome>>,
    pub shadow: Vec<String>,
}

/// Opens every backend the policy's fans use.
pub fn open(
    p: &FansPolicy,
    limits: &Limits,
    secrets_dir: &std::path::Path,
) -> Result<Opened, Vec<String>> {
    let mut o = Opened {
        backends: BTreeMap::new(),
        esp: BTreeMap::new(),
        shadow: Vec::new(),
    };
    let mut why = Vec::new();
    for (id, b) in &p.backends {
        let used: Vec<u32> = p
            .fans
            .iter()
            .filter(|f| &f.backend == id)
            .filter_map(|f| f.channel.index())
            .collect();
        if !p.fans.iter().any(|f| &f.backend == id) {
            continue;
        }
        let act = b.mode == RunMode::Act && limits.backend_writes(&kind_name(b.kind));
        let inner: Result<Arc<dyn FanBackend>, String> = match b.kind {
            BackendKind::Esphome => open_esp(id, b, used, secrets_dir).map(|e| {
                o.esp.insert(id.clone(), e.clone());
                Arc::new(ArcEsp(e)) as Arc<dyn FanBackend>
            }),
            BackendKind::Fake => Ok(Arc::new(Fake::new(id, FanMode::DeviceCurve))),
            k => Err(format!(
                "backend {id}: kind {} is not wired into hostd yet",
                kind_name(k)
            )),
        };
        match inner {
            Err(e) => why.push(e),
            Ok(b) if act => {
                o.backends.insert(id.clone(), b);
            }
            Ok(b) => {
                o.shadow.push(id.clone());
                o.backends
                    .insert(id.clone(), Arc::new(Shadow::new(Box::new(ArcDyn(b)))));
            }
        }
    }
    if why.is_empty() { Ok(o) } else { Err(why) }
}

/// `Arc<dyn FanBackend>` boxed for `Shadow`.
struct ArcDyn(Arc<dyn FanBackend>);

#[async_trait]
impl FanBackend for ArcDyn {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn kind(&self) -> BackendKind {
        self.0.kind()
    }
    fn caps(&self) -> Caps {
        self.0.caps()
    }
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        self.0.read().await
    }
    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        self.0.fail_safe(ch).await
    }
    async fn release(&self) -> Result<(), BackendError> {
        self.0.release().await
    }
}

fn open_esp(
    id: &str,
    b: &BackendConfig,
    channels: Vec<u32>,
    secrets_dir: &std::path::Path,
) -> Result<Arc<EspHome>, String> {
    let url = b
        .url
        .as_deref()
        .ok_or(format!("backend {id}: url is missing"))?;
    let (user, password) = match &b.auth {
        Some(a) => (
            a.user.clone(),
            resolve_secret(&a.password, secrets_dir).map_err(|e| format!("backend {id}: {e}"))?,
        ),
        None => (String::new(), String::new()),
    };
    Ok(Arc::new(EspHome::new(
        id,
        url,
        &user,
        &password,
        b.names.clone(),
        channels,
    )))
}

/// `esp.<backend>.<entity>` readings from an ESPHome device's SSE state.
pub fn esp_readings(backend: &str, st: &EspState, ts: i64) -> Vec<(SensorId, Reading)> {
    st.entities
        .iter()
        .filter_map(|(name, (v, _))| {
            let (value, unit) = match v {
                EntityValue::Bool(b) => (if *b { 1.0 } else { 0.0 }, Unit::Bool),
                other => (other.as_f64()?, Unit::Unknown),
            };
            Some((
                SensorId::of(Namespace::Esp, &[backend, name]),
                Reading::new(value, unit, ts, "esp/sse"),
            ))
        })
        .collect()
}

/// The running fan loop's shared bits.
pub struct FanRuntime {
    /// Last loop tick (ms since the epoch), for the watchdog.
    pub last_tick: Arc<AtomicI64>,
    /// Latest per-fan samples for telemetry.
    pub samples: Arc<Mutex<Vec<FanSample>>>,
    pub task: tokio::task::JoinHandle<()>,
    pub opened: Arc<Opened>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    fan_ids: Vec<String>,
    /// (fan, duty or None to clear, ttl s), applied by the loop on its next tick.
    overrides: tokio::sync::mpsc::UnboundedSender<(String, Option<f64>, f64)>,
}

impl FanRuntime {
    /// Starts the loop, the SSE readers and the watchdog thread.
    pub fn start(
        policy: &FansPolicy,
        opened: Opened,
        store: Arc<Mutex<SensorStore>>,
        tick: Duration,
    ) -> FanRuntime {
        let opened = Arc::new(opened);
        let last_tick = Arc::new(AtomicI64::new(now_ms()));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lp = FanLoop::new(policy);
        let mut inputs = ExprInputs::new(policy).expect("validated");
        let fan_ids: Vec<String> = policy.fans.iter().map(|f| f.id.clone()).collect();
        let (overrides, mut ovr_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Option<f64>, f64)>();
        for (id, e) in &opened.esp {
            let (e, id, stop) = (e.clone(), id.clone(), stop.clone());
            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                while !stop.load(Ordering::Relaxed) {
                    match e.run_events().await {
                        Ok(()) => backoff = Duration::from_secs(1),
                        Err(err) => {
                            eprintln!("hostd: esp {id} events: {err}");
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                    tokio::time::sleep(backoff).await;
                }
            });
        }
        let (o2, lt, sm, st2) = (
            opened.clone(),
            last_tick.clone(),
            samples.clone(),
            stop.clone(),
        );
        let task = tokio::spawn(async move {
            let mut t = tokio::time::interval(tick);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while !st2.load(Ordering::Relaxed) {
                t.tick().await;
                let now_ms_ = now_ms();
                let now_s = now_ms_ as f64 / 1000.0;
                while let Ok((fan, duty, ttl)) = ovr_rx.try_recv() {
                    // Clear = an override that has already expired.
                    let r = match duty {
                        Some(d) => lp.set_override(&fan, d, now_s, ttl),
                        None => lp.set_override(&fan, 0.0, now_s, 0.0),
                    };
                    if let Err(e) = r {
                        eprintln!("hostd: fan override {fan}: {e}");
                    }
                }
                // ESP state into the store first, so expressions over esp.* see it.
                {
                    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, e) in &o2.esp {
                        let st = e.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        s.extend(esp_readings(id, &st, now_ms_));
                    }
                }
                let mut temps: Vec<Option<f64>> = Vec::new();
                let statuses = {
                    let s = store.lock().unwrap_or_else(|e| e.into_inner());
                    lp.step(now_s, &mut |i, _| {
                        let inp = inputs.eval(i, &*s, now_ms_);
                        if temps.len() <= i {
                            temps.resize(i + 1, None);
                        }
                        temps[i] = inp.temp.map(|t| t.value);
                        inp
                    })
                };
                let mut out = Vec::with_capacity(statuses.len());
                for (i, st) in statuses.iter().enumerate() {
                    if let Some(a) = &st.action {
                        let r = execute(&lp, a, &o2.backends).await;
                        if let Action::Push { fan, push } = a {
                            lp.push_result(*fan, push, r.is_ok(), now_s);
                        }
                        if let Err(e) = r {
                            eprintln!("hostd: fan {}: {e}", st.fan);
                        }
                    }
                    let spec = lp.spec(i);
                    let ch = o2
                        .esp
                        .get(&spec.backend)
                        .zip(spec.channel.index())
                        .map(|(e, n)| {
                            e.state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .channel(&e.names, n)
                        });
                    out.push(FanSample {
                        fan: st.fan.clone(),
                        ts: now_ms_,
                        duty_pct: ch.and_then(|c| c.duty),
                        rpm: ch.and_then(|c| c.rpm),
                        ff_duty_pct: ch.and_then(|c| c.ff_duty),
                        failsafe: ch.and_then(|c| c.failsafe),
                        stalled: ch.and_then(|c| c.stalled),
                        control_temp_c: temps.get(i).copied().flatten(),
                        wanted_duty_pct: Some(st.target.duty),
                    });
                }
                {
                    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                    for f in &out {
                        s.extend(f.readings());
                    }
                }
                *sm.lock().unwrap_or_else(|e| e.into_inner()) = out;
                lt.store(now_ms(), Ordering::Relaxed);
            }
        });
        spawn_watchdog(opened.clone(), policy, last_tick.clone(), stop.clone());
        FanRuntime {
            last_tick,
            samples,
            task,
            opened,
            stop,
            fan_ids,
            overrides,
        }
    }

    /// A duty override for one fan (TTL ≤ 1 h; floors and the critical temperature still
    /// apply), or `None` to clear it.
    pub fn set_override(&self, fan: &str, duty: Option<f64>, ttl_s: f64) -> Result<(), String> {
        if !self.fan_ids.iter().any(|f| f == fan) {
            return Err(format!("unknown fan {fan:?}"));
        }
        if duty.is_some() && !(1.0..=orca_fans::limits::MAX_OVERRIDE_TTL_S).contains(&ttl_s) {
            return Err("override ttl must be 1-3600 s".into());
        }
        self.overrides
            .send((fan.to_string(), duty, ttl_s))
            .map_err(|_| "fan loop stopped".to_string())
    }

    /// Stops the loop and hands every backend back to the hardware.
    pub async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
        for (id, b) in &self.opened.backends {
            if let Err(e) = b.release().await {
                eprintln!("hostd: releasing fan backend {id}: {e}");
            }
        }
    }
}

/// If the loop stops ticking for 5 s, put every host-curve fan into its safe state, from an OS
/// thread with its own runtime (works even if the main runtime is wedged).
fn spawn_watchdog(
    opened: Arc<Opened>,
    policy: &FansPolicy,
    last: Arc<AtomicI64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let host: Vec<(String, Channel)> = policy
        .fans
        .iter()
        .filter(|f| f.mode == FanMode::HostCurve)
        .map(|f| (f.backend.clone(), f.channel.clone()))
        .collect();
    if host.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("hostd-fan-watchdog".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut tripped = false;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let stalled = now_ms() - last.load(Ordering::Relaxed) > 5_000;
                if stalled && !tripped {
                    eprintln!(
                        "hostd: fan loop stalled for 5 s: host-curve fans to their safe state"
                    );
                    for (b, ch) in &host {
                        if let Some(be) = opened.backends.get(b) {
                            let _ = rt.block_on(be.fail_safe(ch));
                        }
                    }
                }
                tripped = stalled;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_fans::config::FansConfig;

    fn limits(write: &str) -> Limits {
        Limits::parse(&format!("[backends]\nwrite = [{write}]\n")).unwrap()
    }

    fn policy(kind: &str, mode: &str) -> FansPolicy {
        let cfg = FansConfig::from_toml(&format!(
            r#"
[fan_backends.dev]
kind = "{kind}"
mode = "{mode}"
{url}
[fans.duct0]
backend = "dev"
channel = 0
temp = "first(gpu.B.hbm, gpu.B.core)"
load = "gpu.B.power_pct"
curve = [[60, 30], [82, 100]]
"#,
            url = match kind {
                "esphome" => "url = \"http://esp.invalid\"",
                "liquidctl" => "match = \"Kraken\"",
                _ => "",
            }
        ))
        .unwrap();
        FansPolicy::compile(&cfg, &orca_fans::FanLimits::default(), &|_| true).unwrap()
    }

    #[test]
    fn validation_and_shadow_selection() {
        assert!(validate(&policy("fake", "act"), &limits("\"fake\"")).is_ok());
        let e = validate(&policy("liquidctl", "act"), &limits("")).unwrap_err();
        assert!(e.iter().any(|m| m.contains("not wired")), "{e:?}");
        let d = tempfile::tempdir().unwrap();
        // act in config but the limits do not let the kind write: shadow.
        let o = open(&policy("fake", "act"), &limits(""), d.path()).unwrap();
        assert_eq!(o.shadow, vec!["dev".to_string()]);
        let o = open(&policy("fake", "act"), &limits("\"fake\""), d.path()).unwrap();
        assert!(o.shadow.is_empty());
        let o = open(&policy("fake", "shadow"), &limits("\"fake\""), d.path()).unwrap();
        assert_eq!(o.shadow.len(), 1);
    }

    #[test]
    fn esp_state_to_readings() {
        let mut st = EspState::default();
        st.apply("Fan 0 RPM".into(), EntityValue::Number(1200.0), 1.0);
        st.apply("Failsafe Active".into(), EntityValue::Bool(false), 1.0);
        let r = esp_readings("gpu-fans", &st, 5);
        let ids: Vec<String> = r.iter().map(|(i, _)| i.to_string()).collect();
        assert!(
            ids.contains(&"esp.gpu-fans.\"Fan 0 RPM\"".to_string()),
            "{ids:?}"
        );
        assert!(
            r.iter()
                .any(|(_, x)| x.unit == Unit::Bool && x.value == 0.0)
        );
    }

    #[tokio::test]
    async fn loop_pushes_from_the_store_and_misses_nothing_as_zero() {
        let p = policy("fake", "act");
        let d = tempfile::tempdir().unwrap();
        let o = open(&p, &limits("\"fake\""), d.path()).unwrap();
        let store = Arc::new(Mutex::new(SensorStore::new()));
        {
            let mut s = store.lock().unwrap();
            let t = now_ms();
            s.put(
                SensorId::parse("gpu.B.core").unwrap(),
                Reading::new(70.0, Unit::Celsius, t, "t"),
            );
            s.put(
                SensorId::parse("gpu.B.power_pct").unwrap(),
                Reading::new(50.0, Unit::Percent, t, "t"),
            );
        }
        let rt = FanRuntime::start(&p, o, store.clone(), Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let s = rt.samples.lock().unwrap().clone();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].control_temp_c, Some(70.0));
        assert!(
            store
                .lock()
                .unwrap()
                .get(&SensorId::parse("fan.duct0.wanted_duty").unwrap())
                .is_some()
        );
        rt.stop().await;
    }
}
