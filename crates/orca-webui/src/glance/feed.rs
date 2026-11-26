//! Glance's SSE endpoint over the supervisor's telemetry feed (SUPERVISOR.md 2.5, D-SV11):
//! [`orca_sensors::feed::TelemetryFeed`] (the hub, or a fake), framed with
//! [`FeedMessage::sse`] (`id:` = seq, so `Last-Event-ID` resumes from the hub's replay).
//!
//! The stream: `snapshot` (or the replayed messages after a reconnect), then `patch` and
//! `event`; a subscriber that falls behind gets a fresh `snapshot`. A `ping` event (no id, not
//! a feed message) goes out when the feed is quiet, so the page can tell quiet from dead.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use futures_util::Stream;
use orca_sensors::feed::{FeedMessage, Resume, TelemetryFeed};
use tokio::sync::broadcast;

/// The `Last-Event-ID` a reconnecting EventSource sends.
pub fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// The frames of one subscriber, as bytes.
pub fn frames(
    feed: Arc<dyn TelemetryFeed>,
    last_seq: Option<u64>,
    ping: Duration,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    struct St {
        feed: Arc<dyn TelemetryFeed>,
        rx: broadcast::Receiver<FeedMessage>,
        queue: std::collections::VecDeque<FeedMessage>,
        last: u64,
        tick: tokio::time::Interval,
    }
    let sub = feed.subscribe(last_seq);
    let mut queue = std::collections::VecDeque::new();
    match sub.start {
        Resume::Snapshot(s) => queue.push_back(FeedMessage::Snapshot(s)),
        Resume::Replay(msgs) => queue.extend(msgs),
    }
    let mut tick = tokio::time::interval(ping);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.reset();
    let st = St {
        feed,
        rx: sub.rx,
        queue,
        last: last_seq.unwrap_or(0),
        tick,
    };
    futures_util::stream::unfold(st, |mut st| async move {
        loop {
            if let Some(m) = st.queue.pop_front() {
                let is_snapshot = matches!(m, FeedMessage::Snapshot(_));
                // Messages already covered by the snapshot or the replay are skipped.
                if !is_snapshot && m.seq() <= st.last {
                    continue;
                }
                st.last = m.seq();
                st.tick.reset();
                return Some((Ok(Bytes::from(m.sse())), st));
            }
            tokio::select! {
                msg = st.rx.recv() => match msg {
                    Ok(m) => st.queue.push_back(m),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Too slow: start over from a fresh snapshot.
                        let sub = st.feed.subscribe(None);
                        st.rx = sub.rx;
                        st.queue.clear();
                        if let Resume::Snapshot(s) = sub.start {
                            st.last = 0;
                            st.queue.push_back(FeedMessage::Snapshot(s));
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = st.tick.tick() => {
                    return Some((Ok(Bytes::from_static(b"event: ping\ndata: \n\n")), st));
                }
            }
        }
    })
}

/// The SSE response.
pub fn sse(feed: Arc<dyn TelemetryFeed>, last_seq: Option<u64>, ping: Duration) -> Response {
    let mut r = Response::new(Body::from_stream(frames(feed, last_seq, ping)));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // nginx-style proxies (NPM) must not buffer the stream.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use orca_sensors::feed::{FeedEvent, TelemetrySink};
    use orca_supervisor::{HubConfig, TelemetryHub};
    use serde_json::json;

    fn parse(frame: &[u8]) -> (String, Option<u64>, String) {
        let text = String::from_utf8_lossy(frame);
        let mut ev = (String::new(), None, String::new());
        for l in text.lines() {
            if let Some(v) = l.strip_prefix("event: ") {
                ev.0 = v.into();
            } else if let Some(v) = l.strip_prefix("id: ") {
                ev.1 = v.parse().ok();
            } else if let Some(v) = l.strip_prefix("data: ") {
                ev.2 = v.into();
            }
        }
        ev
    }

    #[tokio::test]
    async fn snapshot_then_patches_and_events_and_resume() {
        let hub = TelemetryHub::new(HubConfig::default());
        hub.set_section("dash", json!({"time": 1}));
        hub.flush(1);
        let mut s = Box::pin(frames(hub.clone(), None, Duration::from_secs(60)));
        let (name, id, data) = parse(&s.next().await.unwrap().unwrap());
        assert_eq!((name.as_str(), id), ("snapshot", Some(1)));
        assert!(data.contains("\"type\":\"snapshot\""));
        hub.set_section("dash", json!({"time": 2}));
        hub.flush(2);
        hub.event(FeedEvent {
            kind: "switch.step".into(),
            severity: Default::default(),
            text: "x".into(),
            data: serde_json::Value::Null,
        });
        let (name, id, data) = parse(&s.next().await.unwrap().unwrap());
        assert_eq!((name.as_str(), id), ("patch", Some(2)));
        assert!(data.contains("\"patch\":{\"dash\":{\"time\":2}}"));
        let (name, id, _) = parse(&s.next().await.unwrap().unwrap());
        assert_eq!((name.as_str(), id), ("event", Some(3)));
        // A reconnect with Last-Event-ID 2 replays just the event.
        let mut r = Box::pin(frames(hub.clone(), Some(2), Duration::from_secs(60)));
        let (name, id, _) = parse(&r.next().await.unwrap().unwrap());
        assert_eq!((name.as_str(), id), ("event", Some(3)));
    }

    #[tokio::test]
    async fn quiet_feeds_ping() {
        let hub = TelemetryHub::new(HubConfig::default());
        let mut s = Box::pin(frames(hub.clone(), None, Duration::from_millis(50)));
        assert_eq!(parse(&s.next().await.unwrap().unwrap()).0, "snapshot");
        assert_eq!(parse(&s.next().await.unwrap().unwrap()).0, "ping");
    }
}
