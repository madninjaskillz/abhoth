//! End to end against an in-process broker (rumqttd): discovery, availability and LWT,
//! retained-message policy, stale sweep, reconnect, and command round-trips through fake
//! guards.

mod common;

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use common::{Call, FakeControl};
use orca_homeassistant::commands::FanMode;
use orca_homeassistant::mqtt::{HaMqttParams, MqttHandle, run};
use orca_homeassistant::notify::{AlertNotice, Severity};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start_broker(port: u16) {
    let conn = rumqttd::ConnectionSettings {
        connection_timeout_ms: 5000,
        max_payload_size: 1024 * 1024,
        max_inflight_count: 2000,
        auth: None,
        external_auth: None,
        dynamic_filters: true,
    };
    let server = rumqttd::ServerSettings {
        name: "v4".into(),
        listen: SocketAddr::from(([127, 0, 0, 1], port)),
        tls: None,
        next_connection_delay_ms: 1,
        connections: conn,
    };
    let config = rumqttd::Config {
        id: 0,
        router: rumqttd::RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 1000,
            max_segment_size: 16 * 1024 * 1024,
            max_segment_count: 10,
            ..Default::default()
        },
        v4: Some(HashMap::from([("1".to_string(), server)])),
        ..Default::default()
    };
    std::thread::spawn(move || {
        let mut broker = rumqttd::Broker::new(config);
        let _ = broker.start();
    });
}

/// A plain client that records everything it sees.
struct Observer {
    client: AsyncClient,
    rx: mpsc::UnboundedReceiver<(String, Vec<u8>, bool)>,
}

impl Observer {
    async fn connect(port: u16, id: &str, filter: &str) -> Self {
        let mut o = MqttOptions::new(id, "127.0.0.1", port);
        o.set_max_packet_size(1 << 20, 1 << 20);
        let (client, mut el) = AsyncClient::new(o, 100);
        let (tx, rx) = mpsc::unbounded_channel();
        client.subscribe(filter, QoS::AtLeastOnce).await.unwrap();
        tokio::spawn(async move {
            loop {
                match el.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        if tx.send((p.topic.clone(), p.payload.to_vec(), p.retain)).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });
        Self { client, rx }
    }

    /// Waits for a message on `topic` matching `pred`.
    async fn wait(&mut self, topic: &str, pred: impl Fn(&[u8], bool) -> bool) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut seen: Vec<String> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some((t, p, r))) if t == topic && pred(&p, r) => return p,
                Ok(Some((t, _, _))) => {
                    if !seen.contains(&t) {
                        seen.push(t)
                    }
                }
                _ => panic!("timed out waiting for {topic}; saw {} topics: {:?}", seen.len(), &seen[..seen.len().min(40)]),
            }
        }
    }

    /// Retained messages currently held for `filter` (a fresh subscription, collected briefly).
    async fn retained(port: u16, filter: &str) -> HashMap<String, Vec<u8>> {
        let mut o = Observer::connect(port, &format!("probe-{}", rand_id()), filter).await;
        let mut out = HashMap::new();
        // A fixed window: live messages keep arriving on busy topics, so never wait on silence.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
        while let Ok(Some((t, p, r))) = tokio::time::timeout_at(deadline, o.rx.recv()).await {
            if r {
                out.insert(t, p);
            }
        }
        let _ = o.client.disconnect().await;
        out
    }
}

fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed) + std::process::id() as u64 * 1000
}

fn json(p: &[u8]) -> Value {
    serde_json::from_slice(p).unwrap()
}

async fn wait_calls(control: &FakeControl, n: usize) -> Vec<Call> {
    for _ in 0..100 {
        let c = control.calls.lock().unwrap().clone();
        if c.len() >= n {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected {n} control calls, got {:?}", control.calls.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end() {
    let port = free_port();
    let snap0 = common::deepthought();
    let (snap_tx, snap_rx) = watch::channel(snap0.clone());
    let (handle, outbound) = MqttHandle::channel(8);
    let (stop_tx, stop_rx) = watch::channel(false);
    let control = Arc::new(FakeControl::default());
    control.leased_cards.lock().unwrap().push("A".into());

    start_broker(port);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut obs = Observer::connect(port, "observer", "#").await;
    // Seed what a previous run / ha-bridge left behind.
    let seed = Observer::connect(port, "seed", "seed/none").await;
    let stale = "homeassistant/sensor/abhoth_deepthought_engine_gone_state/config";
    seed.client.publish(stale, QoS::AtLeastOnce, true, r#"{"name":"gone"}"#).await.unwrap();
    seed.client.publish("abhoth/deepthought/set/card_b_online", QoS::AtLeastOnce, true, "OFF").await.unwrap();
    seed.client.publish("homeassistant/select/deepthought_maxclock/config", QoS::AtLeastOnce, true, r#"{"name":"GPU Max Clock"}"#).await.unwrap();
    let mut want: Vec<&str> = vec![stale, "abhoth/deepthought/set/card_b_online", "homeassistant/select/deepthought_maxclock/config"];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !want.is_empty() {
        match tokio::time::timeout_at(deadline, obs.rx.recv()).await {
            Ok(Some((t, _, _))) => want.retain(|w| *w != t),
            _ => panic!("seeds never arrived: {want:?}"),
        }
    }
    for topic in [stale, "abhoth/deepthought/set/card_b_online", "homeassistant/select/deepthought_maxclock/config"] {
        assert!(Observer::retained(port, topic).await.contains_key(topic), "seeded {topic} is retained");
    }

    let task = tokio::spawn(run(HaMqttParams {
        cfg: common::mqtt_config(port, true),
        sw_version: "0.16.6".into(),
        snapshots: snap_rx,
        control: control.clone(),
        outbound,
        shutdown: stop_rx,
    }));
    // Availability comes up, discovery and state are published.
    obs.wait("abhoth/deepthought/availability", |p, _| p == b"online").await;
    let cfg = obs.wait("homeassistant/sensor/deepthought_gpu_temp/config", |p, _| !p.is_empty()).await;
    assert_eq!(json(&cfg)["state_topic"], "abhoth/deepthought/card/b/state");
    let st = obs.wait("abhoth/deepthought/card/b/state", |_, _| true).await;
    assert_eq!(json(&st)["core_temp"], 26.0);

    // Retained-message policy: after the sweep, the stale config, the retained command and
    // the retired ha-bridge select are gone; ours are retained. (rumqttd forwards at most
    // `max_inflight_count` retained messages per subscription, so probe topic by topic.)
    tokio::time::sleep(Duration::from_secs(4)).await;
    let held = |topic: &'static str| async move { Observer::retained(port, topic).await.remove(topic) };
    assert_eq!(held(stale).await, None, "stale discovery config swept");
    assert_eq!(held("abhoth/deepthought/set/card_b_online").await, None, "retained command cleared");
    assert_eq!(held("homeassistant/select/deepthought_maxclock/config").await, None, "retired maxclock cleared");
    assert_eq!(held("abhoth/deepthought/availability").await.as_deref(), Some(&b"online"[..]));
    assert!(held("homeassistant/sensor/abhoth_deepthought_card_c_core_temp/config").await.is_some());
    assert!(held("abhoth/deepthought/state").await.is_some());
    assert_eq!(held("abhoth/deepthought/result").await, None, "results are not retained");
    // The retained command was never acted on.
    let calls = control.calls.lock().unwrap().clone();
    assert!(calls.is_empty(), "{calls:?}");

    // ── commands through the fake guards ──
    let sender = obs.client.clone();
    let cmd = |topic: &str, payload: &str| {
        let c = sender.clone();
        let (t, p) = (format!("abhoth/deepthought/set/{topic}"), payload.to_string());
        async move { c.publish(t, QoS::AtLeastOnce, false, p).await.unwrap() }
    };
    // Card offline: accepted by the guard, as the MQTT principal.
    cmd("card_b_online", "OFF").await;
    let r = json(&obs.wait("abhoth/deepthought/result", |_, _| true).await);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(wait_calls(&control, 1).await[0], Call::CardOnline("mqtt:deepthought".into(), "B".into(), false));
    // Card A holds a lease: refused by the guard, and the refusal reaches HA.
    cmd("card_a_online", "OFF").await;
    let r = json(&obs.wait("abhoth/deepthought/result", |_, _| true).await);
    assert_eq!(r["ok"], false);
    assert_eq!(r["message"], "card A holds a lease");
    // Clock cap on pinned card A: refused before reaching the port.
    cmd("card_a_clock_cap", "1300").await;
    let r = json(&obs.wait("abhoth/deepthought/result", |_, _| true).await);
    assert!(r["message"].as_str().unwrap().contains("pinned"), "{r}");
    cmd("card_b_clock_cap", "1400").await;
    obs.wait("abhoth/deepthought/result", |p, _| json(p)["command"] == "clock_cap").await;
    // Select a layout, then apply: the switch uses the selected layout. The bench guard
    // refuses the first try.
    cmd("layout_target", "flash-x2").await;
    obs.wait("abhoth/deepthought/layout_target", |p, _| p == b"flash-x2").await;
    *control.bench.lock().unwrap() = true;
    cmd("layout_apply", "apply").await;
    let r = json(&obs.wait("abhoth/deepthought/result", |p, _| json(p)["command"] == "layout_apply").await);
    assert_eq!(r["ok"], false);
    assert!(r["message"].as_str().unwrap().contains("bench"));
    *control.bench.lock().unwrap() = false;
    cmd("layout_apply", "apply").await;
    let r = json(&obs.wait("abhoth/deepthought/result", |p, _| json(p)["command"] == "layout_apply").await);
    assert_eq!(r["ok"], true);
    assert_eq!(r["job"], "job-1");
    // A forced apply is not available from HA, and unknown commands do nothing.
    cmd("layout_apply", r#"{"layout":"flash-x2","force":true}"#).await;
    obs.wait("abhoth/deepthought/result", |p, _| json(p)["ok"] == false).await;
    cmd("fan_mode", "full").await;
    obs.wait("abhoth/deepthought/result", |p, _| json(p)["command"] == "fan_mode").await;
    let calls = wait_calls(&control, 6).await;
    assert_eq!(
        calls[1..],
        [
            Call::CardOnline("mqtt:deepthought".into(), "A".into(), false),
            Call::ClockCap("mqtt:deepthought".into(), "B".into(), Some(1400), 3600),
            Call::Switch("mqtt:deepthought".into(), "flash-x2".into()),
            Call::Switch("mqtt:deepthought".into(), "flash-x2".into()),
            Call::Fan("mqtt:deepthought".into(), FanMode::Full),
        ]
    );
    // The last command is visible in the server state attributes.
    let st = json(&obs.wait("abhoth/deepthought/state", |p, _| json(p)["last_command"]["command"] == "fan_mode").await);
    assert_eq!(st["last_command"]["ok"], true);

    // ── snapshot changes: an engine disappears → its discovery is cleared; the layout
    // changes → the select follows it ──
    let mut snap1 = snap0.clone();
    snap1.engines.retain(|e| e.id != "comfyui");
    snap1.control.layout = Some("dense-stock-x2".into());
    snap_tx.send(snap1).unwrap();
    obs.wait("homeassistant/sensor/abhoth_deepthought_engine_comfyui_state/config", |p, _| p.is_empty()).await;
    obs.wait("abhoth/deepthought/layout_target", |p, _| p == b"dense-stock-x2").await;

    // ── alerts (the mqtt sink) ──
    handle
        .alert(AlertNotice {
            identity: "hot_card/A".into(),
            rule: "hot_card".into(),
            severity: Severity::Critical,
            title: "GPU A at 86°C".into(),
            message: "core 86°C for 60 s".into(),
            resolved: false,
            children: vec![],
            url: None,
            at: "2026-11-26T10:00:00Z".into(),
        })
        .await
        .unwrap();
    let ev = json(&obs.wait("abhoth/deepthought/alert_event", |_, _| true).await);
    assert_eq!(ev["event_type"], "critical");
    let held = Observer::retained(port, "abhoth/deepthought/alert/#").await;
    assert_eq!(json(&held["abhoth/deepthought/alert/hot_card"])["state"], "open");
    // The orca-alerts MqttSink path: a raw publish on an alert topic, which also fires the
    // event entity; anything outside the alert topics is refused.
    handle.publish("abhoth/deepthought/set/card_c_online", "OFF", false).await.unwrap();
    let body = r#"{"state":"resolved","identity":"hot_card/A","rule":"hot_card","severity":"critical","title":"t","message":"m","at":1}"#;
    handle.publish("abhoth/deepthought/alert/hot_card", body, true).await.unwrap();
    let ev = json(&obs.wait("abhoth/deepthought/alert_event", |p, _| json(p)["event_type"] == "resolved").await);
    assert_eq!(ev["identity"], "hot_card/A");
    assert!(!control.calls.lock().unwrap().iter().any(|c| matches!(c, Call::CardOnline(_, card, _) if card == "C")));

    // ── orderly stop publishes offline ──
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
    let held = Observer::retained(port, "abhoth/deepthought/availability").await;
    assert_eq!(held.get("abhoth/deepthought/availability").map(Vec::as_slice), Some(&b"offline"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_then_last_will_marks_offline_on_crash() {
    let port = free_port();
    let (_snap_tx, snap_rx) = watch::channel(common::deepthought());
    let (_handle, outbound) = MqttHandle::channel(1);
    let (_stop_tx, stop_rx) = watch::channel(false);
    // The publisher starts before the broker exists: it must keep retrying, then connect.
    let task = tokio::spawn(run(HaMqttParams {
        cfg: common::mqtt_config(port, false),
        sw_version: "0.16.6".into(),
        snapshots: snap_rx,
        control: Arc::new(FakeControl::default()),
        outbound,
        shutdown: stop_rx,
    }));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    start_broker(port);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut obs = Observer::connect(port, "observer-lwt", "abhoth/deepthought/availability").await;
    obs.wait("abhoth/deepthought/availability", |p, _| p == b"online").await;
    // A crash: the task and its connection vanish without a DISCONNECT.
    task.abort();
    let _ = task.await;
    obs.wait("abhoth/deepthought/availability", |p, _| p == b"offline").await;
}
