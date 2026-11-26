//! The telemetry hub (SUPERVISOR.md 2.5): merges hostd samples, engine metrics, leases, the
//! calendar and external readings into one state, keeps sparkline history, and publishes the
//! feed ([`TelemetryFeed`]): a snapshot on connect, merge patches at most once per flush
//! (the flusher runs every second) and events immediately.
//!
//! Ordering: every message is sent under the state lock, so `seq` order is send order and
//! [`TelemetryHub::subscribe`] is atomic (nothing falls between the snapshot and the stream).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use orca_sensors::feed::{
    FeedEvent, FeedMessage, Resume, Snapshot, Subscription, TelemetryFeed, TelemetrySink,
    merge_patch_diff, sections,
};
use orca_sensors::{Reading, SensorId, TimestampMs, now_ms};
use serde_json::{Map, Value};
use tokio::sync::broadcast;

/// A sparkline resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// 10 minutes at 2.5 s (as the dash today).
    Fine,
    /// 24 hours at 1 minute.
    Coarse,
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    /// Messages kept for `Last-Event-ID` replay.
    pub retain: usize,
    /// Broadcast channel capacity; a slower subscriber lags and must resubscribe.
    pub channel: usize,
    pub fine_step_ms: i64,
    pub fine_len: usize,
    pub coarse_step_ms: i64,
    pub coarse_len: usize,
}

impl Default for HubConfig {
    fn default() -> HubConfig {
        HubConfig {
            retain: 512,
            channel: 1024,
            fine_step_ms: 2_500,
            fine_len: 240,
            coarse_step_ms: 60_000,
            coarse_len: 1_440,
        }
    }
}

/// A fixed-step ring of `(bucket start ms, value)`; the last value in a bucket wins.
#[derive(Debug, Clone)]
struct Ring {
    step: i64,
    cap: usize,
    buf: VecDeque<(TimestampMs, f64)>,
}

impl Ring {
    fn new(step: i64, cap: usize) -> Ring {
        Ring {
            step,
            cap,
            buf: VecDeque::new(),
        }
    }

    fn push(&mut self, ts: TimestampMs, v: f64) {
        let b = ts.div_euclid(self.step) * self.step;
        match self.buf.back_mut() {
            Some((last, val)) if *last == b => *val = v,
            Some((last, _)) if *last > b => {} // out of order: ignore
            _ => {
                self.buf.push_back((b, v));
                while self.buf.len() > self.cap {
                    self.buf.pop_front();
                }
            }
        }
    }
}

#[derive(Debug)]
struct Inner {
    seq: u64,
    /// The state as last published (patches are diffs against it).
    published: Value,
    /// The state being built.
    current: Value,
    retained: VecDeque<FeedMessage>,
    history: HashMap<SensorId, (Ring, Ring)>,
}

/// The hub. Cheap to share as `Arc<TelemetryHub>`.
pub struct TelemetryHub {
    cfg: HubConfig,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<FeedMessage>,
}

impl std::fmt::Debug for TelemetryHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHub").finish_non_exhaustive()
    }
}

impl TelemetryHub {
    pub fn new(cfg: HubConfig) -> Arc<TelemetryHub> {
        let (tx, _) = broadcast::channel(cfg.channel.max(16));
        let empty = Value::Object(Map::new());
        Arc::new(TelemetryHub {
            cfg,
            inner: Mutex::new(Inner {
                seq: 0,
                published: empty.clone(),
                current: empty,
                retained: VecDeque::new(),
                history: HashMap::new(),
            }),
            tx,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves consistent JSON; keep serving.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, inner: &mut Inner, msg: FeedMessage) {
        inner.retained.push_back(msg.clone());
        while inner.retained.len() > self.cfg.retain {
            inner.retained.pop_front();
        }
        // No receivers is fine.
        let _ = self.tx.send(msg);
    }

    /// Publishes a patch if the state changed since the last flush. Returns its seq.
    pub fn flush(&self, now: TimestampMs) -> Option<u64> {
        let mut inner = self.lock();
        let patch = merge_patch_diff(&inner.published, &inner.current)?;
        inner.published = inner.current.clone();
        inner.seq += 1;
        let seq = inner.seq;
        self.send(
            &mut inner,
            FeedMessage::Patch {
                seq,
                ts: now,
                patch,
            },
        );
        Some(seq)
    }

    /// Runs [`TelemetryHub::flush`] every `every` (1 s for the feed) until the hub is dropped.
    pub fn spawn_flusher(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                let Some(hub) = weak.upgrade() else { break };
                hub.flush(now_ms());
            }
        })
    }

    /// Sparkline points for one sensor, oldest first.
    pub fn history(&self, id: &SensorId, res: Resolution) -> Vec<(TimestampMs, f64)> {
        let inner = self.lock();
        inner
            .history
            .get(id)
            .map(|(f, c)| match res {
                Resolution::Fine => f.buf.iter().copied().collect(),
                Resolution::Coarse => c.buf.iter().copied().collect(),
            })
            .unwrap_or_default()
    }

    /// The current (unflushed) value of a section.
    pub fn section(&self, name: &str) -> Option<Value> {
        self.lock().current.get(name).cloned()
    }

    fn sensors_mut(current: &mut Value) -> &mut Map<String, Value> {
        let obj = current.as_object_mut().expect("state is an object");
        let s = obj
            .entry(sections::SENSORS)
            .or_insert_with(|| Value::Object(Map::new()));
        if !s.is_object() {
            *s = Value::Object(Map::new());
        }
        s.as_object_mut().expect("object")
    }
}

/// Removes nulls recursively: a `null` in state would read as "delete" in a merge patch.
/// Missing values are absent keys (D-SV4).
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        v => v,
    }
}

impl TelemetrySink for TelemetryHub {
    fn set_section(&self, name: &str, value: Value) {
        let mut inner = self.lock();
        let obj = inner.current.as_object_mut().expect("state is an object");
        if value.is_null() {
            obj.remove(name);
        } else {
            obj.insert(name.to_string(), strip_nulls(value));
        }
    }

    fn put_readings(&self, readings: &[(SensorId, Reading)]) {
        let (fine_step, fine_len, coarse_step, coarse_len) = (
            self.cfg.fine_step_ms,
            self.cfg.fine_len,
            self.cfg.coarse_step_ms,
            self.cfg.coarse_len,
        );
        let mut inner = self.lock();
        let inner = &mut *inner;
        let sensors = TelemetryHub::sensors_mut(&mut inner.current);
        for (id, r) in readings {
            if !r.value.is_finite() {
                sensors.remove(&id.to_string());
                continue;
            }
            let v = serde_json::to_value(r).unwrap_or(Value::Null);
            sensors.insert(id.to_string(), v);
            let (f, c) = inner.history.entry(id.clone()).or_insert_with(|| {
                (
                    Ring::new(fine_step, fine_len),
                    Ring::new(coarse_step, coarse_len),
                )
            });
            f.push(r.ts, r.value);
            c.push(r.ts, r.value);
        }
    }

    fn remove_readings(&self, ids: &[SensorId]) {
        let mut inner = self.lock();
        let sensors = TelemetryHub::sensors_mut(&mut inner.current);
        for id in ids {
            sensors.remove(&id.to_string());
        }
    }

    fn event(&self, event: FeedEvent) {
        let mut inner = self.lock();
        inner.seq += 1;
        let seq = inner.seq;
        self.send(
            &mut inner,
            FeedMessage::Event {
                seq,
                ts: now_ms(),
                event,
            },
        );
    }
}

impl TelemetryFeed for TelemetryHub {
    fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            seq: inner.seq,
            ts: now_ms(),
            state: inner.published.clone(),
        }
    }

    fn subscribe(&self, last_seq: Option<u64>) -> Subscription {
        let inner = self.lock();
        let rx = self.tx.subscribe();
        if let Some(last) = last_seq {
            // Replay only if every message after `last` is still retained.
            let oldest = inner.retained.front().map(FeedMessage::seq);
            if last <= inner.seq && (last == inner.seq || oldest.is_some_and(|o| o <= last + 1)) {
                let replay = inner
                    .retained
                    .iter()
                    .filter(|m| m.seq() > last)
                    .cloned()
                    .collect();
                return Subscription {
                    start: Resume::Replay(replay),
                    rx,
                };
            }
        }
        let snap = Snapshot {
            seq: inner.seq,
            ts: now_ms(),
            state: inner.published.clone(),
        };
        Subscription {
            start: Resume::Snapshot(snap),
            rx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_sensors::Unit;
    use orca_sensors::feed::apply_merge_patch;
    use serde_json::json;

    fn id(s: &str) -> SensorId {
        SensorId::parse(s).unwrap()
    }

    #[test]
    fn snapshot_plus_patches_reconstruct_state() {
        let hub = TelemetryHub::new(HubConfig::default());
        let sub = hub.subscribe(None);
        let Resume::Snapshot(snap) = sub.start else {
            panic!()
        };
        let mut rx = sub.rx;
        let mut client = snap.state;

        hub.put_readings(&[(
            id("gpu.A.core"),
            Reading::new(50.0, Unit::Celsius, 1_000, "t"),
        )]);
        hub.set_section("host", json!({"cpu_pct": 3.0, "gone": null}));
        assert!(hub.flush(1_000).is_some());
        assert!(hub.flush(1_001).is_none(), "no change, no patch");
        hub.remove_readings(&[id("gpu.A.core")]);
        hub.put_readings(&[(
            id("gpu.B.core"),
            Reading::new(f64::NAN, Unit::Celsius, 2_000, "t"),
        )]);
        hub.flush(2_000);

        while let Ok(m) = rx.try_recv() {
            if let FeedMessage::Patch { patch, .. } = m {
                apply_merge_patch(&mut client, &patch);
            }
        }
        assert_eq!(client, hub.snapshot().state);
        assert_eq!(client, json!({"sensors": {}, "host": {"cpu_pct": 3.0}}));
    }

    #[test]
    fn events_are_immediate_and_resume_works() {
        let hub = TelemetryHub::new(HubConfig {
            retain: 3,
            ..Default::default()
        });
        hub.event(FeedEvent {
            kind: "a".into(),
            severity: Default::default(),
            text: String::new(),
            data: Value::Null,
        });
        hub.set_section("x", json!(1));
        hub.flush(1);
        hub.event(FeedEvent {
            kind: "b".into(),
            severity: Default::default(),
            text: String::new(),
            data: Value::Null,
        });
        // seq: 1 event, 2 patch, 3 event
        match hub.subscribe(Some(1)).start {
            Resume::Replay(v) => {
                assert_eq!(v.iter().map(FeedMessage::seq).collect::<Vec<_>>(), [2, 3])
            }
            r => panic!("{r:?}"),
        }
        match hub.subscribe(Some(3)).start {
            Resume::Replay(v) => assert!(v.is_empty()),
            r => panic!("{r:?}"),
        }
        hub.set_section("x", json!(2));
        hub.flush(2);
        hub.set_section("x", json!(3));
        hub.flush(3);
        // seq 1 is gone from the 3-message window: snapshot instead.
        assert!(matches!(hub.subscribe(Some(1)).start, Resume::Snapshot(_)));
        // A seq from the future (hub restarted): snapshot.
        assert!(matches!(hub.subscribe(Some(99)).start, Resume::Snapshot(_)));
    }

    #[test]
    fn history_rings() {
        let hub = TelemetryHub::new(HubConfig {
            fine_len: 3,
            ..Default::default()
        });
        let core = id("gpu.A.core");
        for k in 0..10 {
            hub.put_readings(&[(
                core.clone(),
                Reading::new(k as f64, Unit::Celsius, k * 1_000, "t"),
            )]);
        }
        let fine = hub.history(&core, Resolution::Fine);
        assert_eq!(fine, vec![(2_500, 4.0), (5_000, 7.0), (7_500, 9.0)]);
        assert_eq!(hub.history(&core, Resolution::Coarse), vec![(0, 9.0)]);
    }
}
