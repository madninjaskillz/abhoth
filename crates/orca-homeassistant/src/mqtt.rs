//! The MQTT runtime: connection, LWT availability, reconnects, discovery and state
//! publishing, the retained-message policy and command handling (SUPERVISOR.md 11.1).
//!
//! Retained-message policy:
//! - **retained:** discovery configs, device state JSON, availability (server and card), the
//!   layout select's state, and the per-rule alert JSON (10.3);
//! - **never retained:** command results, alert events;
//! - **cleared (retained empty):** discovery configs we no longer publish (also ones a previous
//!   run left behind), configured retired topics, ha-bridge's topics at cutover, and any
//!   retained message found on a command topic (it is ignored, never acted on).
//!
//! `MqttOptions` is never logged: its `Debug` would include the password.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use indexmap::IndexMap;
use rumqttc::{AsyncClient, Event, EventLoop, LastWill, MqttOptions, Packet, QoS, Transport};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::commands::{
    self, Command, CommandContext, ControlOutcome, ControlPort, ParseError, ServicePrincipal,
};
use crate::config::MqttConfig;
use crate::discovery::{self, DiscoveryOptions};
use crate::model::HaSnapshot;
use crate::notify::{AlertNotice, event_from_alert_json, mqtt_alert_payload, mqtt_event_payload};
use crate::state::{self, LocalState};
use crate::topics::{LEGACY_TOPICS, Topics};

/// How long after connecting we collect retained discovery configs to sweep stale ones.
const SWEEP_WINDOW: Duration = Duration::from_secs(3);
const MAX_PACKET: usize = 512 * 1024;

/// Builds the client options. Credentials are resolved here from their references.
pub fn mqtt_options(cfg: &MqttConfig, t: &Topics) -> anyhow::Result<MqttOptions> {
    let ep = cfg.endpoint().map_err(anyhow::Error::msg)?;
    let mut o = MqttOptions::new(cfg.client_id.clone(), ep.host, ep.port);
    o.set_keep_alive(Duration::from_secs(cfg.keep_alive_seconds.clamp(5, 600)));
    o.set_clean_session(true);
    o.set_max_packet_size(MAX_PACKET, MAX_PACKET);
    o.set_last_will(LastWill::new(
        t.availability(),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    match (&cfg.username, &cfg.password) {
        (Some(u), Some(p)) => {
            let u = u.resolve().context("mqtt.username")?;
            let p = p.resolve().context("mqtt.password")?;
            o.set_credentials(u.expose(), p.expose());
        }
        (Some(u), None) => {
            o.set_credentials(u.resolve().context("mqtt.username")?.expose(), "");
        }
        (None, Some(_)) => anyhow::bail!("mqtt.password without mqtt.username"),
        (None, None) => {}
    }
    if ep.tls {
        let transport = match &cfg.tls_ca_file {
            Some(path) => {
                let ca = std::fs::read(path).with_context(|| format!("mqtt.tls_ca_file {path}"))?;
                Transport::tls(ca, None, None)
            }
            // The system roots. rumqttc panics if the platform store can't be read; turn that
            // into an error instead of taking the supervisor down.
            None => std::panic::catch_unwind(Transport::tls_with_default_config).map_err(|_| {
                anyhow::anyhow!("could not load the platform's TLS root certificates")
            })?,
        };
        o.set_transport(transport);
    }
    Ok(o)
}

/// Inputs to the runtime.
pub struct HaMqttParams {
    pub cfg: MqttConfig,
    pub sw_version: String,
    /// The hub's view, updated on every hub tick.
    pub snapshots: watch::Receiver<HaSnapshot>,
    pub control: Arc<dyn ControlPort>,
    /// From [`MqttHandle`]s: alerts and alert-topic publishes (the `mqtt` sink).
    pub outbound: mpsc::Receiver<Outbound>,
    /// `true` = publish offline and stop.
    pub shutdown: watch::Receiver<bool>,
}

/// What other subsystems may send through the HA connection.
#[derive(Debug, Clone)]
pub enum Outbound {
    Alert(AlertNotice),
    /// Only under `abhoth/<server>/alert/`; anything else is refused.
    Publish {
        topic: String,
        payload: String,
        retain: bool,
    },
}

/// A cloneable handle for publishing through the HA MQTT connection. It implements the
/// alert engine's `MqttPublish` shape (`publish(topic, payload, retain)`), so orca-alerts'
/// `MqttSink` plugs in with a one-line adapter, without this crate depending on it.
#[derive(Debug, Clone)]
pub struct MqttHandle {
    tx: mpsc::Sender<Outbound>,
}

impl MqttHandle {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Outbound>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    pub async fn publish(&self, topic: &str, payload: &str, retain: bool) -> Result<(), String> {
        self.tx
            .send(Outbound::Publish {
                topic: topic.into(),
                payload: payload.into(),
                retain,
            })
            .await
            .map_err(|_| "the HA MQTT publisher is not running".to_string())
    }

    pub async fn alert(&self, a: AlertNotice) -> Result<(), String> {
        self.tx
            .send(Outbound::Alert(a))
            .await
            .map_err(|_| "the HA MQTT publisher is not running".to_string())
    }
}

enum Inbound {
    Connected,
    Disconnected(String),
    Message {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
}

/// Polls the event loop forever, reconnecting with backoff (1, 2, 4 … 30 s).
async fn drive(mut el: EventLoop, tx: mpsc::Sender<Inbound>) {
    let mut failures = 0u32;
    loop {
        match el.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                failures = 0;
                debug!(code = ?ack.code, "mqtt connected");
                if tx.send(Inbound::Connected).await.is_err() {
                    return;
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let m = Inbound::Message {
                    topic: p.topic.clone(),
                    payload: p.payload.to_vec(),
                    retain: p.retain,
                };
                if tx.send(m).await.is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                failures += 1;
                let wait = Duration::from_secs(2u64.saturating_pow(failures.min(5)).min(30) / 2)
                    .max(Duration::from_secs(1));
                if tx.send(Inbound::Disconnected(e.to_string())).await.is_err() {
                    return;
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
}

struct Runtime {
    cfg: MqttConfig,
    t: Topics,
    opts: DiscoveryOptions,
    who: ServicePrincipal,
    client: AsyncClient,
    control: Arc<dyn ControlPort>,
    local: LocalState,
    published_discovery: IndexMap<String, String>,
    sweep: Option<(Instant, HashSet<String>)>,
    last_layout: Option<String>,
    last_fingerprint: String,
    connected: bool,
    results: mpsc::Sender<(Command, ControlOutcome)>,
}

impl Runtime {
    async fn publish(&self, topic: &str, payload: impl Into<Vec<u8>>, retain: bool) {
        if let Err(e) = self
            .client
            .publish(topic, QoS::AtLeastOnce, retain, payload.into())
            .await
        {
            warn!(topic, error = %e, "mqtt publish queue failed");
        }
    }

    async fn clear(&self, topic: &str) {
        self.publish(topic, Vec::<u8>::new(), true).await;
    }

    async fn on_connect(&mut self, snap: &HaSnapshot) {
        self.connected = true;
        info!(base = %self.t.base, "home assistant mqtt: connected");
        self.publish(&self.t.availability(), "online", true).await;
        if self.cfg.clear_legacy_topics {
            for topic in LEGACY_TOPICS {
                self.clear(topic).await;
            }
        }
        for topic in discovery::retired_topics(&self.t, &self.cfg) {
            self.clear(&topic).await;
        }
        let subs = [
            (self.t.set_filter(), QoS::AtLeastOnce),
            (
                format!("{}/status", self.t.discovery_prefix),
                QoS::AtLeastOnce,
            ),
            (
                format!("{}/+/+/config", self.t.discovery_prefix),
                QoS::AtMostOnce,
            ),
        ];
        for (topic, qos) in subs {
            if let Err(e) = self.client.subscribe(topic.clone(), qos).await {
                warn!(topic, error = %e, "mqtt subscribe failed");
            }
        }
        self.sweep = Some((Instant::now(), HashSet::new()));
        self.published_discovery.clear();
        self.publish_discovery(snap).await;
        self.publish_state(snap).await;
    }

    async fn publish_discovery(&mut self, snap: &HaSnapshot) {
        let msgs = discovery::build(&self.t, &self.opts, snap);
        let next = discovery::index(&msgs);
        for (topic, payload) in &next {
            if self.published_discovery.get(topic) != Some(payload) {
                self.publish(topic, payload.clone(), true).await;
            }
        }
        let gone: Vec<String> = self
            .published_discovery
            .keys()
            .filter(|k| !next.contains_key(*k))
            .cloned()
            .collect();
        for topic in gone {
            self.clear(&topic).await;
        }
        self.published_discovery = next;
    }

    async fn publish_state(&mut self, snap: &HaSnapshot) {
        // The select follows the live layout at start and after every completed switch.
        if snap.control.layout != self.last_layout {
            self.local.layout_target = snap.control.layout.clone();
            self.last_layout = snap.control.layout.clone();
        }
        for m in state::build(&self.t, snap, &self.local) {
            self.publish(&m.topic, m.payload, true).await;
        }
        self.last_fingerprint = state::discrete_fingerprint(snap);
    }

    async fn finish_sweep(&mut self) {
        let Some((_, seen)) = self.sweep.take() else {
            return;
        };
        for topic in seen {
            if !self.published_discovery.contains_key(&topic) {
                info!(topic, "clearing a stale discovery config");
                self.clear(&topic).await;
            }
        }
        let _ = self
            .client
            .unsubscribe(format!("{}/+/+/config", self.t.discovery_prefix))
            .await;
    }

    fn command_ctx<'a>(
        &'a self,
        snap: &'a HaSnapshot,
        cards: &'a [(String, bool)],
    ) -> CommandContext<'a> {
        CommandContext {
            layouts: &snap.control.layouts,
            cards,
            clock_cap_options: &self.cfg.clock_cap_options,
            clock_cap_ttl: Duration::from_secs(self.cfg.clock_cap_ttl_seconds),
        }
    }

    async fn on_message(
        &mut self,
        snap: &HaSnapshot,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    ) {
        // Discovery sweep: remember retained configs we own; ignore everything else there.
        if topic.starts_with(&format!("{}/", self.t.discovery_prefix)) {
            if topic == format!("{}/status", self.t.discovery_prefix) {
                if payload == b"online" {
                    info!("home assistant restarted: republishing discovery and state");
                    self.published_discovery.clear();
                    self.publish_discovery(snap).await;
                    self.publish_state(snap).await;
                }
                return;
            }
            if let Some((_, seen)) = &mut self.sweep
                && retain
                && !payload.is_empty()
                && self.t.owns_discovery_topic(&topic)
            {
                seen.insert(topic);
            }
            return;
        }
        if !self.cfg.controls {
            return;
        }
        let cards: Vec<(String, bool)> = snap
            .cards
            .iter()
            .map(|c| (c.id.clone(), c.cap_offered))
            .collect();
        let parsed = commands::parse(
            &self.t,
            &topic,
            &payload,
            retain,
            &self.command_ctx(snap, &cards),
        );
        match parsed {
            Err(ParseError::NotCommand | ParseError::Empty) => {}
            Err(ParseError::Retained) => {
                warn!(topic, "ignoring a retained command and clearing it");
                self.clear(&topic).await;
            }
            Err(e) => {
                let v = json!({"command": topic.rsplit('/').next(), "ok": false, "message": e.to_string(), "at": now()});
                warn!(topic, error = %e, "refused HA command");
                self.local.last_command = Some(v.clone());
                self.publish(&self.t.result(), v.to_string(), false).await;
                self.publish_state(snap).await;
            }
            Ok(Command::SelectLayout(l)) => {
                self.local.layout_target = Some(l.clone());
                self.publish(&self.t.layout_target(), l, true).await;
            }
            Ok(cmd) => {
                let (port, who, tx) =
                    (self.control.clone(), self.who.clone(), self.results.clone());
                let selected = self.local.layout_target.clone();
                info!(principal = %who.name, command = cmd.name(), "HA command");
                tokio::spawn(async move {
                    if let Some(outcome) =
                        commands::dispatch(port.as_ref(), &who, &cmd, selected.as_deref()).await
                    {
                        let _ = tx.send((cmd, outcome)).await;
                    }
                });
            }
        }
    }

    async fn on_result(&mut self, snap: &HaSnapshot, cmd: Command, outcome: ControlOutcome) {
        let v = commands::result_json(&cmd, &outcome, &now());
        if outcome.ok() {
            info!(
                command = cmd.name(),
                message = outcome.message(),
                "HA command accepted"
            );
        } else {
            warn!(
                command = cmd.name(),
                reason = outcome.message(),
                "HA command refused"
            );
        }
        self.local.last_command = Some(v.clone());
        self.publish(&self.t.result(), v.to_string(), false).await;
        self.publish_state(snap).await;
    }

    /// A publish from another subsystem. Only `abhoth/<server>/alert/<rule>` is allowed; the
    /// "Abhoth alert" event entity fires from the same JSON.
    async fn on_outbound_publish(&self, topic: &str, payload: String, retain: bool) {
        let prefix = format!("{}/alert/", self.t.base);
        let rule = topic.strip_prefix(&prefix).unwrap_or_default();
        if rule.is_empty() || rule.contains(['/', '#', '+']) {
            warn!(
                topic,
                "refusing an outbound publish outside the alert topics"
            );
            return;
        }
        if let Some(ev) = serde_json::from_str(&payload)
            .ok()
            .and_then(|v| event_from_alert_json(&v))
        {
            self.publish(&self.t.alert_event(), ev.to_string(), false)
                .await;
        }
        self.publish(topic, payload, retain).await;
    }

    async fn on_alert(&self, a: &AlertNotice) {
        self.publish(
            &self.t.alert(&a.rule),
            mqtt_alert_payload(a).to_string(),
            true,
        )
        .await;
        self.publish(
            &self.t.alert_event(),
            mqtt_event_payload(a).to_string(),
            false,
        )
        .await;
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Runs the HA MQTT publisher until `shutdown` turns true. Returns early only on a config
/// error; connection problems are retried forever.
pub async fn run(p: HaMqttParams) -> anyhow::Result<()> {
    let HaMqttParams {
        cfg,
        sw_version,
        mut snapshots,
        control,
        mut outbound,
        mut shutdown,
    } = p;
    let problems = cfg.validate();
    anyhow::ensure!(problems.is_empty(), "mqtt config: {}", problems.join("; "));
    let server_id = snapshots.borrow().server.id.clone();
    anyhow::ensure!(!server_id.is_empty(), "the snapshot has no server id");
    let t = Topics::new(&cfg.base_topic, &cfg.discovery_prefix, &server_id);
    let options = mqtt_options(&cfg, &t)?;
    let (client, el) = AsyncClient::new(options, 256);
    let (in_tx, mut inbound) = mpsc::channel(256);
    // Dropped with this future (abort or panic), so the connection closes and the LWT fires.
    let driver = AbortOnDrop(tokio::spawn(drive(el, in_tx)));
    let (res_tx, mut results) = mpsc::channel(32);
    let mut rt = Runtime {
        opts: DiscoveryOptions::from_config(&cfg, &sw_version),
        who: ServicePrincipal::mqtt(&server_id, &cfg.scopes),
        local: LocalState {
            clock_cap_ttl_seconds: cfg.clock_cap_ttl_seconds,
            ..Default::default()
        },
        cfg,
        t,
        client,
        control,
        published_discovery: IndexMap::new(),
        sweep: None,
        last_layout: None,
        last_fingerprint: String::new(),
        connected: false,
        results: res_tx,
    };
    let mut tick = tokio::time::interval(rt.cfg.state_interval());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sweep_tick = tokio::time::interval(Duration::from_millis(500));
    let mut outbound_open = true;
    loop {
        tokio::select! {
            m = inbound.recv() => {
                let Some(m) = m else { break };
                let snap = snapshots.borrow().clone();
                match m {
                    Inbound::Connected => rt.on_connect(&snap).await,
                    Inbound::Disconnected(e) => {
                        if rt.connected {
                            warn!(error = %e, "home assistant mqtt: disconnected; reconnecting");
                        } else {
                            debug!(error = %e, "home assistant mqtt: connect failed; retrying");
                        }
                        rt.connected = false;
                    }
                    Inbound::Message { topic, payload, retain } => rt.on_message(&snap, topic, payload, retain).await,
                }
            }
            r = results.recv() => {
                if let Some((cmd, outcome)) = r {
                    let snap = snapshots.borrow().clone();
                    rt.on_result(&snap, cmd, outcome).await;
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let snap = snapshots.borrow_and_update().clone();
                if rt.connected {
                    rt.publish_discovery(&snap).await;
                    if state::discrete_fingerprint(&snap) != rt.last_fingerprint {
                        rt.publish_state(&snap).await;
                    }
                }
            }
            _ = tick.tick() => {
                if rt.connected {
                    let snap = snapshots.borrow().clone();
                    rt.publish_state(&snap).await;
                    rt.publish(&rt.t.availability(), "online", true).await;
                }
            }
            _ = sweep_tick.tick() => {
                if rt.sweep.as_ref().is_some_and(|(at, _)| at.elapsed() >= SWEEP_WINDOW) {
                    rt.finish_sweep().await;
                }
            }
            o = outbound.recv(), if outbound_open => {
                // Queued by the client while disconnected; sent after the reconnect.
                match o {
                    Some(Outbound::Alert(a)) => rt.on_alert(&a).await,
                    Some(Outbound::Publish { topic, payload, retain }) => rt.on_outbound_publish(&topic, payload, retain).await,
                    None => outbound_open = false,
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
        }
    }
    // Orderly stop: say offline (the LWT covers crashes), then disconnect.
    rt.publish(&rt.t.availability(), "offline", true).await;
    let _ = rt.client.disconnect().await;
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            m = inbound.recv() => if m.is_none() { break },
        }
    }
    drop(driver);
    Ok(())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
