//! The hostd client: one connection (the local socket or the forced-command relay's
//! stdin/stdout), `hello`, `telemetry.subscribe`, the policy, a status poll, and requests from
//! the rest of the supervisor. Reconnects with back-off; the hub shows `health.hostd` as
//! `ok` / `down` meanwhile. hostd keeps running its last-known-good policy whatever happens
//! here (it never depends on the supervisor).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use orca_hostd::protocol::{Envelope, FromHostd, PROTOCOL_VERSION, PolicyDoc, Rates, TelemetryBatch, ToHostd, read_frame, write_frame};
use orca_sensors::feed::{FeedEvent, Severity, TelemetrySink, sections};
use orca_sensors::{Namespace, Reading, SensorId, Unit};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

/// How to reach hostd.
#[derive(Debug, Clone)]
pub enum Endpoint {
    Socket(std::path::PathBuf),
    Command(Vec<String>),
}

type Pending = HashMap<u64, oneshot::Sender<FromHostd>>;

/// A cloneable handle: sends a request and waits for its reply.
#[derive(Clone)]
pub struct HostdClient {
    tx: mpsc::Sender<(ToHostd, oneshot::Sender<FromHostd>)>,
}

impl HostdClient {
    pub async fn call(&self, m: ToHostd) -> Result<FromHostd, String> {
        let (tx, rx) = oneshot::channel();
        self.tx.send((m, tx)).await.map_err(|_| "hostd client stopped".to_string())?;
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| "hostd did not answer in 10 s".to_string())?
            .map_err(|_| "hostd is not connected".to_string())
    }
}

/// Starts the client. `policy`: sent after every (re)connect when set.
pub fn spawn(ep: Endpoint, sink: Arc<dyn TelemetrySink>, policy: Option<PolicyDoc>) -> HostdClient {
    let (tx, mut rx) = mpsc::channel::<(ToHostd, oneshot::Sender<FromHostd>)>(64);
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            sink.set_section(sections::HEALTH, json!({"hostd": {"status": "connecting"}}));
            let r = match &ep {
                Endpoint::Socket(p) => connect_socket(p).await,
                Endpoint::Command(c) => connect_command(c),
            };
            match r {
                Ok((r, w, child)) => {
                    backoff = Duration::from_secs(1);
                    let why = session(r, w, sink.as_ref(), policy.as_ref(), &mut rx).await;
                    drop(child);
                    if why.is_none() {
                        return; // every handle dropped
                    }
                    sink.event(FeedEvent { kind: "hostd.disconnected".into(), severity: Severity::Warning, text: why.unwrap_or_default(), data: Value::Null });
                }
                Err(e) => {
                    sink.set_section(sections::HEALTH, json!({"hostd": {"status": "down", "error": e}}));
                }
            }
            sink.set_section(sections::HEALTH, json!({"hostd": {"status": "down"}}));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
    HostdClient { tx }
}

type Conn = (Box<dyn AsyncBufRead + Unpin + Send>, Box<dyn AsyncWrite + Unpin + Send>, Option<tokio::process::Child>);

#[cfg(unix)]
async fn connect_socket(p: &std::path::Path) -> Result<Conn, String> {
    let s = tokio::net::UnixStream::connect(p).await.map_err(|e| format!("{}: {e}", p.display()))?;
    let (r, w) = s.into_split();
    Ok((Box::new(tokio::io::BufReader::new(r)), Box::new(w), None))
}

#[cfg(not(unix))]
async fn connect_socket(p: &std::path::Path) -> Result<Conn, String> {
    Err(format!("{}: unix sockets need a unix host", p.display()))
}

fn connect_command(c: &[String]) -> Result<Conn, String> {
    let (prog, args) = c.split_first().ok_or("hostd_command is empty")?;
    let mut child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{prog}: {e}"))?;
    let r = child.stdout.take().ok_or("no stdout")?;
    let w = child.stdin.take().ok_or("no stdin")?;
    Ok((Box::new(tokio::io::BufReader::new(r)), Box::new(w), Some(child)))
}

/// One connection. Returns `None` when every client handle is gone (stop), else why it ended.
async fn session(
    mut r: Box<dyn AsyncBufRead + Unpin + Send>,
    mut w: Box<dyn AsyncWrite + Unpin + Send>,
    sink: &dyn TelemetrySink,
    policy: Option<&PolicyDoc>,
    rx: &mut mpsc::Receiver<(ToHostd, oneshot::Sender<FromHostd>)>,
) -> Option<String> {
    let mut pending: Pending = HashMap::new();
    let mut next = 0u64;
    let send = |m: ToHostd, next: &mut u64| {
        *next += 1;
        Envelope::new(Some(*next), m)
    };
    let hello = send(ToHostd::Hello { proto: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").into(), client: "supervisor".into() }, &mut next);
    if write_frame(&mut w, &hello).await.is_err() {
        return Some("write failed".into());
    }
    if let Some(p) = policy {
        let e = send(ToHostd::PolicyPut(p.clone()), &mut next);
        if write_frame(&mut w, &e).await.is_err() {
            return Some("write failed".into());
        }
    }
    let e = send(ToHostd::TelemetrySubscribe(Rates::default()), &mut next);
    if write_frame(&mut w, &e).await.is_err() {
        return Some("write failed".into());
    }
    let status_id = std::sync::atomic::AtomicU64::new(0);
    let mut status_tick = tokio::time::interval(Duration::from_secs(5));
    let mut buf = Vec::new();
    let mut ids = Ingest::default();
    loop {
        tokio::select! {
            f = read_frame::<_, FromHostd>(&mut r, &mut buf) => {
                let env = match f {
                    Ok(Some(e)) => e,
                    Ok(None) => return Some("hostd closed the connection".into()),
                    Err(e) => return Some(format!("bad frame: {e}")),
                };
                if let Some(id) = env.id {
                    if id == status_id.load(std::sync::atomic::Ordering::Relaxed) {
                        if let FromHostd::Status(st) = &env.msg {
                            sink.set_section(sections::CLOCKS, serde_json::to_value(&st.clocks).unwrap_or_default());
                            sink.set_section(sections::HEALTH, json!({"hostd": {"status": "ok", "version": st.version, "policy_rev": st.policy_rev, "limits_hash": st.limits_hash, "holds": st.holds, "overrides": st.overrides}}));
                        }
                    } else if let Some(tx) = pending.remove(&id) {
                        let _ = tx.send(env.msg);
                    } else {
                        match env.msg {
                            FromHostd::Hello { .. } => sink.set_section(sections::HEALTH, json!({"hostd": {"status": "ok"}})),
                            FromHostd::PolicyRefused { rev, reasons } => sink.event(FeedEvent { kind: "hostd.policy_refused".into(), severity: Severity::Critical, text: format!("hostd refused policy rev {rev}"), data: json!(reasons) }),
                            FromHostd::Error { message, .. } => sink.event(FeedEvent { kind: "hostd.error".into(), severity: Severity::Warning, text: message, data: Value::Null }),
                            _ => {}
                        }
                    }
                    continue;
                }
                match env.msg {
                    FromHostd::Telemetry(b) => ids.apply(sink, &b),
                    FromHostd::Event(e) => sink.event(FeedEvent {
                        kind: format!("hostd.{}", e.kind),
                        severity: e.severity,
                        text: e.text,
                        data: e.data,
                    }),
                    _ => {}
                }
            }
            req = rx.recv() => {
                let (m, tx) = req?;
                let e = send(m, &mut next);
                pending.insert(next, tx);
                if write_frame(&mut w, &e).await.is_err() {
                    return Some("write failed".into());
                }
            }
            _ = status_tick.tick() => {
                let e = send(ToHostd::Status, &mut next);
                status_id.store(next, std::sync::atomic::Ordering::Relaxed);
                if write_frame(&mut w, &e).await.is_err() {
                    return Some("write failed".into());
                }
            }
        }
    }
}

/// Puts telemetry batches into the hub. A card's GPU readings and the `sensor.*` values are
/// replaced per batch: what a batch no longer carries is removed (missing, never stale).
#[derive(Default)]
pub struct Ingest {
    last: BTreeSet<SensorId>,
}

impl Ingest {
    pub fn apply(&mut self, sink: &dyn TelemetrySink, b: &TelemetryBatch) {
        let mut r = orca_hostd::sample::batch_readings(b);
        for (name, v) in &b.sensors {
            r.push((SensorId::of(Namespace::Sensor, &[name.as_str()]), Reading::new(*v, Unit::Unknown, b.ts, "hostd")));
        }
        let now: BTreeSet<SensorId> = r
            .iter()
            .map(|(i, _)| i.clone())
            .filter(|i| matches!(i.namespace(), Namespace::Gpu | Namespace::Sensor))
            .collect();
        let gone: Vec<SensorId> = self
            .last
            .iter()
            .filter(|i| !now.contains(*i))
            // Only for cards in this batch (GPU sampling every tick) and for sensor.*.
            .filter(|i| i.namespace() == Namespace::Sensor || i.segments().first().is_some_and(|c| b.gpus.contains_key(c)))
            .cloned()
            .collect();
        if !gone.is_empty() {
            sink.remove_readings(&gone);
        }
        self.last = now;
        sink.put_readings(&r);
        if !b.gpus.is_empty() {
            sink.set_section(sections::GPUS, serde_json::to_value(&b.gpus).unwrap_or_default());
        }
        if let Some(h) = &b.host {
            sink.set_section(sections::HOST, serde_json::to_value(h).unwrap_or_default());
        }
        if let Some(h) = &b.hwmon {
            sink.set_section(sections::HWMON, serde_json::to_value(h).unwrap_or_default());
        }
        if let Some(h) = &b.bmc {
            sink.set_section(sections::BMC, serde_json::to_value(h).unwrap_or_default());
        }
        if !b.fans.is_empty() {
            let m: BTreeMap<&str, Value> = b.fans.iter().map(|f| (f.fan.as_str(), serde_json::to_value(f).unwrap_or_default())).collect();
            sink.set_section(sections::FANS, serde_json::to_value(m).unwrap_or_default());
        }
        if let Some(f) = &b.facts {
            sink.set_section(sections::FACTS, serde_json::to_value(f).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HubConfig, TelemetryHub};
    use orca_sensors::feed::TelemetryFeed;

    #[tokio::test]
    async fn talks_to_a_hostd_and_feeds_the_hub() {
        let hub = TelemetryHub::new(HubConfig::default());
        // A fake hostd on a duplex pipe.
        let (a, b) = tokio::io::duplex(1 << 20);
        let (hr, mut hw) = tokio::io::split(b);
        let fake = tokio::spawn(async move {
            let mut hr = tokio::io::BufReader::new(hr);
            let mut buf = Vec::new();
            let mut sent_batch = false;
            while let Ok(Some(env)) = read_frame::<_, ToHostd>(&mut hr, &mut buf).await {
                let reply = match env.msg {
                    ToHostd::Hello { .. } => FromHostd::Hello { proto: 1, version: "t".into(), limits_hash: "h".into(), policy_rev: None },
                    ToHostd::TelemetrySubscribe(_) => FromHostd::Ack,
                    ToHostd::Status => FromHostd::Status(Default::default()),
                    ToHostd::Wake { .. } => FromHostd::Ack,
                    _ => FromHostd::Ack,
                };
                write_frame(&mut hw, &Envelope::new(env.id, reply)).await.unwrap();
                if !sent_batch {
                    sent_batch = true;
                    let mut bt = TelemetryBatch { ts: 1_000, ..Default::default() };
                    bt.sensors.insert("ctrl_A".into(), 55.0);
                    write_frame(&mut hw, &Envelope::new(None, FromHostd::Telemetry(Box::new(bt)))).await.unwrap();
                }
            }
        });
        let (ar, aw) = tokio::io::split(a);
        let (tx, mut rx) = mpsc::channel(4);
        let client = HostdClient { tx };
        let h2 = hub.clone();
        let s = tokio::spawn(async move { session(Box::new(tokio::io::BufReader::new(ar)), Box::new(aw), h2.as_ref(), None, &mut rx).await });
        assert!(matches!(client.call(ToHostd::Wake { card: "A".into() }).await.unwrap(), FromHostd::Ack));
        for _ in 0..50 {
            hub.flush(2_000);
            if hub.snapshot().state["sensors"].get("sensor.ctrl_A").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(hub.snapshot().state["sensors"]["sensor.ctrl_A"]["v"], 55.0);
        drop(client);
        assert_eq!(s.await.unwrap(), None);
        fake.abort();
    }
}
