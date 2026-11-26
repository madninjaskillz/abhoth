//! The daemon: shared state, the message handler, the telemetry loop and the socket server.
//!
//! hostd survives supervisor restarts by design: the supervisor is just a client on the
//! socket. A dropped connection changes nothing; holds and overrides run out by their TTL;
//! the policy in force stays (and is on disk as last-known-good for hostd's own restarts).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_gpuctl::{AssertReason, ClockEvent, WakeSource};
use orca_sensors::feed::Severity;
use orca_sensors::{SensorRegistry, SensorSource as _, SensorStore, TimestampMs, now_ms};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot};

use crate::clocks::ClockMsg;
use crate::files::{self, Paths};
use crate::limits::LoadedLimits;
use crate::policy;
use crate::protocol::{
    AssertReport, Envelope, ErrorCode, FromHostd, HostdEvent, HostdStatus, LoopHealth, MAX_FRAME, Override, PROTOCOL_VERSION,
    PolicyDoc, Target, TelemetryBatch, ToHostd,
};
use crate::sample::{Collector, Want, batch_readings};

/// State shared by the loops and connections.
pub struct Shared {
    pub version: String,
    pub started: TimestampMs,
    pub limits: LoadedLimits,
    pub paths: Option<Paths>,
    pub policy: Mutex<Option<PolicyDoc>>,
    pub notes: Mutex<Vec<String>>,
    pub registry: Mutex<SensorRegistry>,
    pub store: Arc<Mutex<SensorStore>>,
    /// The running fan loop, if the policy has fans.
    pub fans: tokio::sync::Mutex<Option<crate::fans::FanRuntime>>,
    /// Where `secret:<name>` files live (root-only).
    pub secrets_dir: std::path::PathBuf,
    /// Fan loop tick.
    pub fan_tick: Duration,
    /// UUID → card label (from the policy), for telemetry.
    pub labels: Mutex<BTreeMap<String, String>>,
    pub health: Mutex<BTreeMap<String, LoopHealth>>,
    pub clocks: std::sync::mpsc::Sender<ClockMsg>,
    pub telemetry: broadcast::Sender<Arc<TelemetryBatch>>,
    pub events: broadcast::Sender<HostdEvent>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(
        version: impl Into<String>,
        limits: LoadedLimits,
        paths: Option<Paths>,
        clocks: std::sync::mpsc::Sender<ClockMsg>,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            version: version.into(),
            started: now_ms(),
            limits,
            secrets_dir: paths_secrets(&paths),
            paths,
            policy: Mutex::new(None),
            notes: Mutex::new(Vec::new()),
            registry: Mutex::new(SensorRegistry::default()),
            store: Arc::new(Mutex::new(SensorStore::new())),
            fans: tokio::sync::Mutex::new(None),
            fan_tick: Duration::from_millis(500),
            labels: Mutex::new(BTreeMap::new()),
            health: Mutex::new(BTreeMap::new()),
            clocks,
            telemetry: broadcast::channel(64).0,
            events: broadcast::channel(256).0,
        })
    }

    pub fn event(&self, kind: &str, severity: Severity, text: String, data: Value) {
        let e = HostdEvent { ts: now_ms(), kind: kind.into(), severity, text, data };
        eprintln!("hostd: {} {}", e.kind, e.text);
        let _ = self.events.send(e);
    }

    pub fn policy_rev(&self) -> Option<u64> {
        lock(&self.policy).as_ref().map(|p| p.rev)
    }

    /// Validates and applies a policy. `persist` is false when re-applying the one from disk.
    pub async fn apply_policy(&self, doc: PolicyDoc, persist: bool) -> Result<u64, Vec<String>> {
        let v = policy::validate(doc, &self.limits.limits, if persist { self.policy_rev() } else { None })?;
        let rev = v.doc.rev;
        // Open the fan backends before anything changes, so a bad secret refuses the policy.
        let opened = if v.doc.fans.fans.is_empty() {
            None
        } else {
            Some(crate::fans::open(&v.doc.fans, &self.limits.limits, &self.secrets_dir)?)
        };
        if persist && let Some(p) = &self.paths {
            files::save_policy(p, &v.doc).map_err(|e| vec![format!("saving the policy: {e}")])?;
        }
        *lock(&self.registry) = v.registry;
        *lock(&self.labels) = v.labels;
        *lock(&self.notes) = v.notes;
        let _ = self.clocks.send(ClockMsg::Configure(v.cards));
        {
            let mut f = self.fans.lock().await;
            if let Some(old) = f.take() {
                old.stop().await;
            }
            if let Some(o) = opened {
                if !o.shadow.is_empty() {
                    lock(&self.notes).push(format!("fan backends in shadow: {}", o.shadow.join(", ")));
                }
                *f = Some(crate::fans::FanRuntime::start(&v.doc.fans, o, self.store.clone(), self.fan_tick));
            }
        }
        *lock(&self.policy) = Some(v.doc);
        self.event("policy.applied", Severity::Info, format!("policy rev {rev} in force"), Value::Null);
        Ok(rev)
    }

    /// Handles one message. Returns the reply and whether the connection now wants telemetry.
    pub async fn handle(&self, msg: ToHostd) -> (FromHostd, bool) {
        let err = |code, m: String| (FromHostd::Error { code, message: m }, false);
        match msg {
            ToHostd::Hello { proto, .. } => {
                if proto != PROTOCOL_VERSION {
                    return err(ErrorCode::Version, format!("hostd speaks protocol {PROTOCOL_VERSION}, not {proto}"));
                }
                (
                    FromHostd::Hello {
                        proto: PROTOCOL_VERSION,
                        version: self.version.clone(),
                        limits_hash: self.limits.hash.clone(),
                        policy_rev: self.policy_rev(),
                    },
                    false,
                )
            }
            ToHostd::TelemetrySubscribe(_) => (FromHostd::Ack, true),
            ToHostd::TelemetryUnsubscribe => (FromHostd::Ack, false),
            ToHostd::PolicyPut(doc) => {
                let rev = doc.rev;
                match self.apply_policy(doc, true).await {
                    Ok(rev) => (FromHostd::PolicyApplied { rev }, false),
                    Err(reasons) => {
                        self.event("policy.refused", Severity::Warning, reasons.join("; "), Value::Null);
                        (FromHostd::PolicyRefused { rev, reasons }, false)
                    }
                }
            }
            ToHostd::Wake { card } => {
                let _ = self.clocks.send(ClockMsg::Wake { card, source: WakeSource::Router, detail: "router admission".into() });
                (FromHostd::Ack, false)
            }
            ToHostd::Hold { target, ttl_s, reason } => {
                if let Err(e) = self.limits.limits.check_ttl(Duration::from_secs(ttl_s)) {
                    return err(ErrorCode::Forbidden, e);
                }
                let (tx, rx) = oneshot::channel();
                let card = target_id(&target);
                let _ = self.clocks.send(ClockMsg::Hold { card, reason, ttl: Duration::from_secs(ttl_s), reply: tx });
                match rx.await {
                    Ok(Ok(())) => (FromHostd::Ack, false),
                    Ok(Err(e)) => err(ErrorCode::Forbidden, e),
                    Err(_) => err(ErrorCode::Unavailable, "clocks loop not running".into()),
                }
            }
            ToHostd::Release { target, reason } => {
                let card = target_id(&target);
                let reasons: Vec<(String, String)> = match reason {
                    Some(r) => vec![(card, r)],
                    // Every hold on the target: holds are keyed by reason.
                    None => {
                        let (tx, rx) = oneshot::channel();
                        let _ = self.clocks.send(ClockMsg::Status { reply: tx });
                        rx.await
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|c| card == "all" || c.card == card)
                            .flat_map(|c| c.holds.into_iter().map(move |h| (c.card.clone(), h)))
                            .collect()
                    }
                };
                for (card, reason) in reasons {
                    let _ = self.clocks.send(ClockMsg::Release { card, reason });
                }
                (FromHostd::Ack, false)
            }
            ToHostd::Override(o) => match o {
                Override::FanDuty { fan, duty, ttl_s } => match self.fans.lock().await.as_ref() {
                    None => err(ErrorCode::NotFound, format!("no fan {fan:?}: the policy has no fans")),
                    Some(f) => match f.set_override(&fan, Some(duty), ttl_s as f64) {
                        Ok(()) => (FromHostd::Ack, false),
                        Err(e) => err(ErrorCode::Forbidden, e),
                    },
                },
                Override::ClockCap { card, mhz, ttl_s } => self.cap(card, Some(mhz), ttl_s).await,
                Override::Clear { card: Some(card), .. } => self.cap(card, None, self.limits.limits.gpu.max_ttl.as_secs()).await,
                Override::Clear { fan: Some(fan), .. } => match self.fans.lock().await.as_ref() {
                    Some(f) => match f.set_override(&fan, None, 0.0) {
                        Ok(()) => (FromHostd::Ack, false),
                        Err(e) => err(ErrorCode::NotFound, e),
                    },
                    None => err(ErrorCode::NotFound, format!("no fan {fan:?}")),
                },
                Override::Clear { .. } => (FromHostd::Ack, false),
            },
            ToHostd::Assert { card } => {
                let (tx, rx) = oneshot::channel();
                let _ = self.clocks.send(ClockMsg::Assert { card: card.clone(), reason: AssertReason::EngineReady, reply: tx });
                match rx.await {
                    Ok(Some(o)) => {
                        let mut actions: Vec<String> = o.applied.iter().map(|c| c.describe()).collect();
                        actions.extend(o.mismatches.iter().cloned());
                        (FromHostd::AssertResult(AssertReport { card: o.card, ok: o.ok, actions, summary: o.summary }), false)
                    }
                    Ok(None) => err(ErrorCode::NotFound, format!("card {card} has no power policy")),
                    Err(_) => err(ErrorCode::Unavailable, "clocks loop not running".into()),
                }
            }
            ToHostd::Status => (FromHostd::Status(self.status().await), false),
        }
    }

    async fn cap(&self, card: String, mhz: Option<u32>, ttl_s: u64) -> (FromHostd, bool) {
        let (tx, rx) = oneshot::channel();
        let _ = self.clocks.send(ClockMsg::Cap { card, cap: mhz, ttl: Duration::from_secs(ttl_s), reply: tx });
        match rx.await {
            Ok(Ok(())) => (FromHostd::Ack, false),
            Ok(Err(e)) => (FromHostd::Error { code: ErrorCode::Forbidden, message: e }, false),
            Err(_) => (FromHostd::Error { code: ErrorCode::Unavailable, message: "clocks loop not running".into() }, false),
        }
    }

    pub async fn status(&self) -> HostdStatus {
        let (tx, rx) = oneshot::channel();
        let _ = self.clocks.send(ClockMsg::Status { reply: tx });
        let cards = tokio::time::timeout(Duration::from_secs(5), rx).await.ok().and_then(Result::ok).unwrap_or_default();
        let mut holds = Vec::new();
        let mut overrides = Vec::new();
        for c in &cards {
            for h in &c.holds {
                holds.push(format!("{}: {h}", c.card));
            }
            if let Some(cap) = c.cap_mhz {
                overrides.push(format!("{}: clock cap {cap} MHz", c.card));
            }
        }
        let mut loops: Vec<LoopHealth> = lock(&self.health).values().cloned().collect();
        for n in lock(&self.notes).iter() {
            loops.push(LoopHealth { name: "note".into(), ok: true, last_ok: None, last_error: Some(n.clone()) });
        }
        HostdStatus {
            version: self.version.clone(),
            started: self.started,
            limits_hash: self.limits.hash.clone(),
            policy_rev: self.policy_rev(),
            loops,
            holds,
            overrides,
        }
    }

    /// Records collector health.
    fn note_health(&self, c: &Collector) {
        let mut h = lock(&self.health);
        for src in ["gpu", "host", "hwmon", "bmc"] {
            let last_ok = c.last_ok.get(src).copied();
            let last_error = c.errors.get(src).cloned();
            if last_ok.is_none() && last_error.is_none() {
                continue;
            }
            h.insert(src.into(), LoopHealth { name: src.into(), ok: last_error.is_none(), last_ok, last_error });
        }
    }
}

/// Every 5 s: the cards' compute processes (a change wakes the card, gpu-power "procs") and the
/// sniff port map (their listening ports plus `idle.sniff_ports` from the policy).
pub async fn discovery_loop(shared: Arc<Shared>, smi: Vec<String>, proc_root: std::path::PathBuf, map: Arc<Mutex<crate::sniff::PortMap>>) {
    let mut last: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut first = true;
    loop {
        let mut cmd = tokio::process::Command::new(&smi[0]);
        cmd.args(&smi[1..]).args(["--query-compute-apps=pid,gpu_uuid", "--format=csv,noheader,nounits"]).kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await;
        if let Ok(Ok(o)) = out
            && o.status.success()
        {
            let labels = lock(&shared.labels).clone();
            let mut pids: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            for (uuid, mut ps) in crate::sniff::parse_compute_apps(&String::from_utf8_lossy(&o.stdout)) {
                if let Some(l) = labels.get(&uuid) {
                    ps.sort_unstable();
                    pids.insert(l.clone(), ps);
                }
            }
            if !first {
                for (card, ps) in &pids {
                    if last.get(card) != Some(ps) {
                        let _ = shared.clocks.send(ClockMsg::Wake { card: card.clone(), source: orca_gpuctl::WakeSource::Procs, detail: format!("compute processes {ps:?}") });
                    }
                }
            }
            first = false;
            let extra: BTreeMap<String, Vec<u16>> = lock(&shared.policy)
                .as_ref()
                .map(|p| p.power.iter().filter_map(|(l, c)| c.idle().map(|i| (l.clone(), i.sniff_ports.clone()))).collect())
                .unwrap_or_default();
            let pr = proc_root.clone();
            let p2 = pids.clone();
            if let Ok(m) = tokio::task::spawn_blocking(move || crate::sniff::discover(&pr, &p2, &extra)).await {
                *lock(&map) = m;
            }
            last = pids;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Puts a batch into the store. A new GPU sample replaces that card's metrics, so a value that
/// turned N/A (HBM on some cards) becomes absent instead of staying fresh.
pub fn ingest(store: &mut SensorStore, batch: &TelemetryBatch) {
    for label in batch.gpus.keys() {
        let old = store.matching(&orca_sensors::SensorId::of(orca_sensors::Namespace::Gpu, &[label.as_str(), "*"]));
        for id in old {
            store.remove(&id);
        }
    }
    store.extend(batch_readings(batch));
}

fn paths_secrets(p: &Option<Paths>) -> std::path::PathBuf {
    p.as_ref().map(|p| p.secrets_dir()).unwrap_or_default()
}

/// Stops the fan loop and releases its backends (shutdown).
pub async fn stop_fans(shared: &Shared) {
    if let Some(f) = shared.fans.lock().await.take() {
        f.stop().await;
    }
}

fn target_id(t: &Target) -> String {
    match t {
        Target::All => "all".into(),
        Target::Card(c) => c.clone(),
    }
}

/// Parses one request line: the typed envelope, or rc-core's one-shot relay form
/// `{"method": "...", "params": {...}}` (hostctl `Op::Hostd`).
pub fn parse_request(line: &[u8]) -> Result<Envelope<ToHostd>, (Option<u64>, String)> {
    let v: Value = serde_json::from_slice(line).map_err(|e| (None, e.to_string()))?;
    let id = v.get("id").and_then(Value::as_u64);
    let v = match (v.get("method").and_then(Value::as_str), v.get("type")) {
        (Some(m), None) => {
            let mut obj = v.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
            obj.insert("type".into(), Value::String(m.to_string()));
            if let Some(i) = id {
                obj.insert("id".into(), i.into());
            }
            Value::Object(obj)
        }
        _ => v,
    };
    serde_json::from_value(v).map_err(|e| (id, e.to_string()))
}

/// Serves one connection until it closes. Generic over the stream so tests use a pipe.
pub async fn serve_conn<R, W>(shared: Arc<Shared>, mut r: R, mut w: W)
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    use tokio::io::AsyncBufReadExt as _;
    use tokio::io::AsyncReadExt as _;
    let mut tel: Option<broadcast::Receiver<Arc<TelemetryBatch>>> = None;
    let mut ev: Option<broadcast::Receiver<HostdEvent>> = None;
    let mut buf = Vec::new();
    loop {
        // `buf` keeps a partly read line across wake-ups for telemetry (a cancelled
        // read_until leaves what it read in `buf`); it is cleared once a whole line is handled.
        let room = (MAX_FRAME + 1).saturating_sub(buf.len()).max(1) as u64;
        let mut lim = (&mut r).take(room);
        let out: Option<Envelope<FromHostd>> = tokio::select! {
            n = lim.read_until(b'\n', &mut buf) => {
                match n {
                    Ok(0) | Err(_) => return,
                    Ok(_) if buf.len() > MAX_FRAME => {
                        let _ = write_env(&mut w, &Envelope::new(None, FromHostd::Error { code: ErrorCode::BadRequest, message: "frame too long".into() })).await;
                        return;
                    }
                    Ok(_) if buf.last() != Some(&b'\n') => continue,
                    Ok(_) => {
                        let line = buf.trim_ascii().to_vec();
                        buf.clear();
                        if line.is_empty() { continue; }
                        match parse_request(&line) {
                            Ok(env) => {
                                let sub = matches!(env.msg, ToHostd::TelemetrySubscribe(_));
                                let unsub = matches!(env.msg, ToHostd::TelemetryUnsubscribe);
                                let (reply, _) = shared.handle(env.msg).await;
                                if sub && tel.is_none() {
                                    tel = Some(shared.telemetry.subscribe());
                                    ev = Some(shared.events.subscribe());
                                }
                                if unsub {
                                    tel = None;
                                    ev = None;
                                }
                                Some(Envelope::new(env.id, reply))
                            }
                            Err((id, e)) => Some(Envelope::new(id, FromHostd::Error { code: ErrorCode::BadRequest, message: e })),
                        }
                    }
                }
            }
            b = recv_opt(&mut tel) => b.map(|b| Envelope::new(None, FromHostd::Telemetry(Box::new((*b).clone())))),
            e = recv_opt(&mut ev) => e.map(|e| Envelope::new(None, FromHostd::Event(e))),
        };
        if let Some(env) = out
            && write_env(&mut w, &env).await.is_err()
        {
            return;
        }
    }
}

/// Receives from an optional broadcast receiver; a lagged receiver skips ahead; `None` waits
/// forever (the select! branch never fires).
async fn recv_opt<T: Clone>(rx: &mut Option<broadcast::Receiver<T>>) -> Option<T> {
    match rx {
        None => std::future::pending().await,
        Some(r) => loop {
            match r.recv().await {
                Ok(v) => return Some(v),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
    }
}

async fn write_env<W: AsyncWrite + Unpin>(w: &mut W, env: &Envelope<FromHostd>) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(env).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

/// Sampling intervals for the telemetry loop.
#[derive(Debug, Clone)]
pub struct Intervals {
    pub gpu: Duration,
    pub host: Duration,
    pub hwmon: Duration,
    pub bmc: Duration,
}

impl Default for Intervals {
    fn default() -> Intervals {
        Intervals {
            gpu: Duration::from_millis(500),
            host: Duration::from_secs(5),
            hwmon: Duration::from_secs(2),
            bmc: Duration::from_secs(10),
        }
    }
}

/// The telemetry loop: samples, evaluates virtual sensors, feeds the clocks loop, publishes
/// batches, writes the heartbeat and pings the systemd watchdog. Runs until cancelled.
pub async fn telemetry_loop(shared: Arc<Shared>, mut collector: Collector, iv: Intervals, sd: Option<SdNotify>) {
    let start = tokio::time::Instant::now();
    let mut last: BTreeMap<&'static str, tokio::time::Instant> = BTreeMap::new();
    let mut last_beat = start - Duration::from_secs(60);
    let mut t = tokio::time::interval(iv.gpu);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        let now = tokio::time::Instant::now();
        let mut due = |k: &'static str, every: Duration| {
            let d = last.get(k).is_none_or(|l| now.duration_since(*l) >= every);
            if d {
                last.insert(k, now);
            }
            d
        };
        let want = Want { gpu: true, host: due("host", iv.host), hwmon: due("hwmon", iv.hwmon), bmc: due("bmc", iv.bmc) };
        collector.labels = lock(&shared.labels).clone();
        let mut batch = collector.collect(want).await;
        shared.note_health(&collector);
        if want.gpu && collector.errors.contains_key("gpu") {
            // Never act blind: the clocks loop sees "unreadable" and returns cards to serving.
            let _ = shared.clocks.send(ClockMsg::Samples(Vec::new()));
        } else if !batch.gpus.is_empty() {
            let _ = shared.clocks.send(ClockMsg::Samples(batch.gpus.values().cloned().collect()));
        }
        if let Some(f) = shared.fans.lock().await.as_ref() {
            batch.fans = f.samples.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        {
            let mut store = lock(&shared.store);
            ingest(&mut store, &batch);
            let results = lock(&shared.registry).evaluate(&mut store, batch.ts);
            for r in results {
                if let Ok(v) = r.result {
                    batch.sensors.insert(r.name, v.v);
                }
            }
        }
        let _ = shared.telemetry.send(Arc::new(batch));
        if now.duration_since(last_beat) >= Duration::from_secs(5) {
            last_beat = now;
            if let Some(p) = &shared.paths
                && let Err(e) = files::write_heartbeat(p, now_ms())
            {
                eprintln!("hostd: heartbeat: {e}");
            }
            if let Some(s) = &sd {
                s.notify("WATCHDOG=1");
            }
        }
    }
}

/// Logs controller events and republishes them as hostd events.
pub async fn clock_events(shared: Arc<Shared>, mut rx: tokio::sync::mpsc::UnboundedReceiver<ClockEvent>) {
    while let Some(e) = rx.recv().await {
        let (kind, sev, text) = match &e {
            ClockEvent::Lowered { card, lock_mhz, .. } => ("clock.lowered", Severity::Info, format!("{card} idle: locked at {lock_mhz} MHz")),
            ClockEvent::Woke { card, source, detail, .. } => ("clock.woke", Severity::Info, format!("{card} woke ({}: {detail})", source.as_str())),
            ClockEvent::Applied { card, cmd } => ("clock.applied", Severity::Info, format!("{card}: {}", cmd.describe())),
            ClockEvent::CapSet { card, cap_mhz, ttl_s } => ("clock.cap", Severity::Info, format!("{card}: cap {cap_mhz:?} for {ttl_s} s")),
            ClockEvent::CapExpired { card } => ("clock.cap_expired", Severity::Info, format!("{card}: cap expired")),
            ClockEvent::Asserted(o) => ("clock.asserted", if o.ok { Severity::Info } else { Severity::Warning }, o.summary.clone()),
            ClockEvent::Alert(a) => ("clock.alert", Severity::Warning, format!("{}: {}", a.card, a.message)),
        };
        shared.event(kind, sev, text, serde_json::to_value(&e).unwrap_or(Value::Null));
    }
}

/// systemd notify (`READY=1`, `WATCHDOG=1`) over `$NOTIFY_SOCKET`, without libsystemd.
#[derive(Debug, Clone)]
pub struct SdNotify {
    #[cfg_attr(not(unix), allow(dead_code))]
    path: String,
}

impl SdNotify {
    pub fn from_env() -> Option<SdNotify> {
        let p = std::env::var("NOTIFY_SOCKET").ok()?;
        // Abstract sockets (`@…`) need a Linux-only API; plain paths cover systemd's default.
        (!p.is_empty() && !p.starts_with('@')).then_some(SdNotify { path: p })
    }

    pub fn notify(&self, msg: &str) {
        #[cfg(unix)]
        {
            if let Ok(s) = std::os::unix::net::UnixDatagram::unbound() {
                let _ = s.send_to(msg.as_bytes(), &self.path);
            }
        }
        #[cfg(not(unix))]
        let _ = msg;
    }
}

#[cfg(test)]
mod tests;
