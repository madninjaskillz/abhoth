//! The supervisor's feed as Glance's source: `abhoth-supervisor`'s `GET
//! /api/v1/supervisor/stream` (SSE of [`FeedMessage`]s, SUPERVISOR.md 2.5), mirrored section by
//! section into Glance's own hubs (the full one, and the redacted public one).
//!
//! The mirror follows `seq`: a gap reconnects without `Last-Event-ID` (a fresh snapshot); a
//! dropped connection reconnects with it (the supervisor replays what was missed). While the
//! supervisor cannot be reached, the page keeps the last state and shows why in the
//! `feed_errors` section.

use std::time::Duration;

use orca_sensors::feed::{FeedMessage, apply_merge_patch};
use serde_json::{Map, Value, json};

/// The feed state as last received, and which sections a message changed.
#[derive(Default)]
pub struct Mirror {
    pub state: Map<String, Value>,
    pub seq: Option<u64>,
}

/// What one message did to the mirror.
#[derive(Debug, PartialEq)]
pub enum Applied {
    /// These sections changed (`Null`: removed).
    Sections(Map<String, Value>),
    /// An event, to pass on.
    Event(orca_sensors::feed::FeedEvent),
    /// A message was missed: reconnect for a snapshot.
    Gap,
    /// Already seen (a replay overlapping what we have).
    Stale,
}

impl Mirror {
    pub fn apply(&mut self, msg: FeedMessage) -> Applied {
        match msg {
            FeedMessage::Snapshot(s) => {
                let new = match s.state {
                    Value::Object(m) => m,
                    _ => Map::new(),
                };
                let mut changed = Map::new();
                for k in self.state.keys() {
                    if !new.contains_key(k) {
                        changed.insert(k.clone(), Value::Null);
                    }
                }
                for (k, v) in &new {
                    if self.state.get(k) != Some(v) {
                        changed.insert(k.clone(), v.clone());
                    }
                }
                self.state = new;
                self.seq = Some(s.seq);
                Applied::Sections(changed)
            }
            FeedMessage::Patch { seq, patch, .. } => {
                if let Some(a) = self.step(seq) {
                    return a;
                }
                let mut changed = Map::new();
                if let Value::Object(p) = patch {
                    for (k, pv) in p {
                        if pv.is_null() {
                            self.state.remove(&k);
                            changed.insert(k, Value::Null);
                            continue;
                        }
                        let cur = self.state.entry(k.clone()).or_insert(Value::Null);
                        apply_merge_patch(cur, &pv);
                        changed.insert(k, cur.clone());
                    }
                }
                Applied::Sections(changed)
            }
            FeedMessage::Event { seq, event, .. } => match self.step(seq) {
                Some(a) => a,
                None => Applied::Event(event),
            },
        }
    }

    /// Checks `seq` follows; `None` = apply it.
    fn step(&mut self, seq: u64) -> Option<Applied> {
        match self.seq {
            None => Some(Applied::Gap),
            Some(last) if seq <= last => Some(Applied::Stale),
            Some(last) if seq != last + 1 => Some(Applied::Gap),
            Some(_) => {
                self.seq = Some(seq);
                None
            }
        }
    }
}

/// Splits SSE text into complete frames; the rest stays in `buf`.
pub fn take_frames(buf: &mut String) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        let norm = buf.replace("\r\n", "\n");
        if norm.len() != buf.len() {
            *buf = norm;
        }
        let Some(end) = buf.find("\n\n") else { break };
        let frame: String = buf.drain(..end + 2).collect();
        let (mut event, mut data) = (String::new(), String::new());
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if !data.is_empty() || !event.is_empty() {
            out.push((event, data));
        }
    }
    out
}

/// The stream URL from the configured base (`http://127.0.0.1:9890`, with or without the path).
pub fn stream_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/api/v1/supervisor/stream") {
        b.to_string()
    } else {
        format!("{b}/api/v1/supervisor/stream")
    }
}

impl super::Glance {
    /// Follows the supervisor's stream until stopped.
    pub(super) async fn follow_supervisor(
        self: std::sync::Arc<Self>,
        http: reqwest::Client,
        base: String,
    ) {
        let url = stream_url(&base);
        // The shared client's whole-request timeout would cut the stream; connect quickly.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or(http);
        let mut mirror = Mirror::default();
        let mut resume = true;
        let mut backoff = Duration::from_secs(1);
        loop {
            let err = self.follow_once(&http, &url, &mut mirror, resume).await;
            // A gap starts over from a snapshot; anything else resumes where it left off.
            resume = err != "gap";
            if err != "gap" {
                self.set_feed_error("supervisor feed", Some(&err));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            } else {
                backoff = Duration::from_secs(1);
            }
        }
    }

    async fn follow_once(
        &self,
        http: &reqwest::Client,
        url: &str,
        mirror: &mut Mirror,
        resume: bool,
    ) -> String {
        use futures_util::StreamExt;
        let mut req = http.get(url).header("accept", "text/event-stream");
        if let (true, Some(seq)) = (resume, mirror.seq) {
            req = req.header("last-event-id", seq.to_string());
        } else {
            mirror.seq = None;
        }
        let resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => return format!("HTTP {}", r.status().as_u16()),
            Err(e) => return e.without_url().to_string(),
        };
        let mut body = resp.bytes_stream();
        let mut buf = String::new();
        // The supervisor flushes at least every second while anything changes; 60 s of silence
        // (no keep-alives either) is a dead connection.
        loop {
            let chunk = match tokio::time::timeout(Duration::from_secs(60), body.next()).await {
                Err(_) => return "no data for 60 s".into(),
                Ok(None) => return "the supervisor closed the stream".into(),
                Ok(Some(Err(e))) => return e.without_url().to_string(),
                Ok(Some(Ok(c))) => c,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            for (_, data) in take_frames(&mut buf) {
                let Ok(msg) = serde_json::from_str::<FeedMessage>(&data) else {
                    continue;
                };
                match mirror.apply(msg) {
                    Applied::Sections(changed) => {
                        self.supervisor_calendar.store(
                            mirror.state.contains_key("calendar"),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        if mirror.seq.is_some() {
                            self.set_feed_error("supervisor feed", None);
                        }
                        self.set_sections(&changed);
                    }
                    Applied::Event(e) => self.event(e),
                    Applied::Gap => return "gap".into(),
                    Applied::Stale => {}
                }
            }
        }
    }
}

/// `feed_errors` section entries (the page lists them with the collector warnings).
pub fn feed_errors(cur: Option<Value>, source: &str, error: Option<&str>) -> Value {
    let mut m = match cur {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    match error {
        Some(e) => {
            m.insert(source.into(), json!(e));
        }
        None => {
            m.remove(source);
        }
    }
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_sensors::feed::{FeedEvent, Snapshot};

    fn snap(seq: u64, state: Value) -> FeedMessage {
        FeedMessage::Snapshot(Snapshot { seq, ts: 0, state })
    }
    fn patch(seq: u64, p: Value) -> FeedMessage {
        FeedMessage::Patch {
            seq,
            ts: 0,
            patch: p,
        }
    }

    #[test]
    fn the_mirror_follows_snapshots_patches_and_gaps() {
        let mut m = Mirror::default();
        assert_eq!(
            m.apply(patch(1, json!({}))),
            Applied::Gap,
            "nothing before a snapshot"
        );
        let Applied::Sections(c) = m.apply(snap(
            5,
            json!({"gpus": {"A": {"core_c": 30}}, "host": {"cpu_pct": 1}}),
        )) else {
            panic!()
        };
        assert_eq!(c.len(), 2);
        let Applied::Sections(c) = m.apply(patch(
            6,
            json!({"gpus": {"A": {"core_c": 31}}, "host": null}),
        )) else {
            panic!()
        };
        assert_eq!(c["gpus"], json!({"A": {"core_c": 31}}));
        assert_eq!(c["host"], Value::Null);
        assert!(!m.state.contains_key("host"));
        let ev = FeedMessage::Event {
            seq: 7,
            ts: 0,
            event: FeedEvent {
                kind: "alert.fired".into(),
                severity: Default::default(),
                text: "t".into(),
                data: Value::Null,
            },
        };
        assert!(matches!(m.apply(ev), Applied::Event(_)));
        assert_eq!(
            m.apply(patch(7, json!({}))),
            Applied::Stale,
            "a replayed message is skipped"
        );
        assert_eq!(m.apply(patch(9, json!({}))), Applied::Gap);
        // A new snapshot reports only what changed, and sections that went away.
        let Applied::Sections(c) =
            m.apply(snap(20, json!({"gpus": {"A": {"core_c": 31}}, "fans": {}})))
        else {
            panic!()
        };
        assert_eq!(c.keys().collect::<Vec<_>>(), vec!["fans"]);
    }

    #[test]
    fn frames_split_across_chunks() {
        let mut b = String::from(
            "id: 1\nevent: snapshot\ndata: {\"a\":1}\n\nid: 2\r\nevent: patch\r\ndata: {\"b\"",
        );
        let f = take_frames(&mut b);
        assert_eq!(f, vec![("snapshot".to_string(), "{\"a\":1}".to_string())]);
        b.push_str(":2}\r\n\r\n: keep-alive\n\n");
        let f = take_frames(&mut b);
        assert_eq!(f, vec![("patch".to_string(), "{\"b\":2}".to_string())]);
        assert!(b.is_empty());
    }

    #[test]
    fn urls_and_errors() {
        assert_eq!(
            stream_url("http://127.0.0.1:9890/"),
            "http://127.0.0.1:9890/api/v1/supervisor/stream"
        );
        assert_eq!(
            stream_url("http://h/api/v1/supervisor/stream"),
            "http://h/api/v1/supervisor/stream"
        );
        let e = feed_errors(None, "supervisor feed", Some("down"));
        assert_eq!(e, json!({"supervisor feed": "down"}));
        assert_eq!(feed_errors(Some(e), "supervisor feed", None), Value::Null);
    }
}
