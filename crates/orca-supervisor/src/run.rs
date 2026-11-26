//! `abhoth supervisor run|check --config FILE`: wires the hub, the hostd client, the router
//! poller and controls, Home Assistant (MQTT + REST reads) and the feed HTTP endpoints.

use std::sync::Arc;
use std::time::Duration;

use orca_homeassistant::feed::CardConfig;
use orca_homeassistant::model::ServerInfo;
use orca_homeassistant::mqtt::{HaMqttParams, MqttHandle};
use orca_sensors::feed::{FeedMessage, Resume, TelemetryFeed, TelemetrySink};
use orca_sensors::{Namespace, Reading, SensorId, Unit, now_ms};
use tokio::sync::watch;

use crate::config::Config;
use crate::hostd::{self, Endpoint};
use crate::router::{Controls, Router};
use crate::{HubConfig, TelemetryHub};

const USAGE: &str = "usage: abhoth supervisor <run|check> --config FILE\n";

pub async fn main(args: &[String]) -> i32 {
    let mut it = args.iter();
    let cmd = it.next().cloned().unwrap_or_default();
    let mut cfg_path = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => cfg_path = it.next().cloned(),
            _ => {
                eprint!("{USAGE}");
                return 2;
            }
        }
    }
    let Some(p) = cfg_path else {
        eprint!("{USAGE}");
        return 2;
    };
    let cfg = match std::fs::read_to_string(&p)
        .map_err(|e| format!("{p}: {e}"))
        .and_then(|t| Config::parse(&t))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("abhoth supervisor: {e}");
            return 1;
        }
    };
    let problems = cfg.check();
    for n in &cfg.not_wired {
        eprintln!("note: [{n}] is not wired in this build; ignored");
    }
    if !problems.is_empty() {
        for e in &problems {
            eprintln!("problem: {e}");
        }
        return 1;
    }
    match cmd.as_str() {
        "check" => {
            println!("ok");
            0
        }
        "run" => match run(cfg).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("abhoth supervisor: {e}");
                1
            }
        },
        _ => {
            eprint!("{USAGE}");
            2
        }
    }
}

pub async fn run(cfg: Config) -> Result<(), String> {
    let hub = TelemetryHub::new(HubConfig::default());
    hub.spawn_flusher(Duration::from_millis(500));
    let sink: Arc<dyn TelemetrySink> = hub.clone();
    let ep = match (&cfg.supervisor.hostd_socket, &cfg.supervisor.hostd_command) {
        (Some(s), _) => Endpoint::Socket(s.clone()),
        (_, Some(c)) => Endpoint::Command(c.clone()),
        _ => return Err("no hostd endpoint".into()),
    };
    let policy = if cfg.supervisor.push_policy {
        Some(cfg.policy(now_ms() as u64 / 1000)?)
    } else {
        None
    };
    let hostd = hostd::spawn(ep, sink.clone(), policy);
    let router = match (&cfg.supervisor.router_url, &cfg.supervisor.router_key) {
        (Some(u), Some(k)) => {
            let r = Arc::new(Router::new(u, k.resolve()?, cfg.router_server()));
            r.spawn_poller(sink.clone(), Duration::from_secs(5));
            Some(r)
        }
        _ => None,
    };
    let controls = Arc::new(Controls {
        router,
        hostd: hostd.clone(),
    });
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    // The HA connection's publisher, for the alerts' `mqtt` sink.
    let mut ha_handle: Option<MqttHandle> = None;
    if let Some(m) = cfg.mqtt.clone().filter(|m| m.enabled) {
        let server = ServerInfo {
            id: cfg.supervisor.server.clone(),
            name: cfg.supervisor.name.clone(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        };
        let cards: Vec<CardConfig> = cfg
            .cards
            .iter()
            .map(|(id, cap)| CardConfig {
                id: id.clone(),
                allow_cap: *cap,
            })
            .collect();
        let (snap_tx, snap_rx) = watch::channel(Default::default());
        let feed: Arc<dyn TelemetryFeed> = hub.clone();
        tokio::spawn(orca_homeassistant::feed::follow(
            feed, server, cards, snap_tx,
        ));
        let (handle, outbound) = MqttHandle::channel(64);
        ha_handle = Some(handle);
        let params = HaMqttParams {
            cfg: m,
            sw_version: env!("CARGO_PKG_VERSION").into(),
            snapshots: snap_rx,
            control: controls.clone(),
            outbound,
            shutdown: stop_rx.clone(),
        };
        tasks.push(tokio::spawn(async move {
            if let Err(e) = orca_homeassistant::mqtt::run(params).await {
                tracing::error!("mqtt: {e:#}");
            }
        }));
    }
    if let Some(h) = cfg
        .homeassistant
        .clone()
        .filter(|h| h.enabled && !h.reads.is_empty())
    {
        let rest = orca_homeassistant::rest::HaRest::from_config(&h)
            .map_err(|e| format!("homeassistant: {e:#}"))?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(orca_homeassistant::rest::poll_reads(rest, h, tx));
        let s2 = sink.clone();
        tokio::spawn(async move {
            while let Some(round) = rx.recv().await {
                let ts = now_ms();
                let mut put = Vec::new();
                let mut gone = Vec::new();
                for r in round {
                    let id = SensorId::of(Namespace::Ha, &[r.name.as_str()]);
                    match r.value {
                        Some(v) => put.push((
                            id,
                            Reading::new(
                                v,
                                r.unit
                                    .as_deref()
                                    .map(Unit::from_text)
                                    .unwrap_or(Unit::Unknown),
                                ts,
                                "ha",
                            ),
                        )),
                        None => gone.push(id),
                    }
                }
                s2.put_readings(&put);
                s2.remove_readings(&gone);
            }
        });
    }
    if let Some(a) = cfg.alerts.clone().filter(|a| a.enabled) {
        let mut extra: Vec<Arc<dyn orca_alerts::sinks::Sink>> = Vec::new();
        if let Some(h) = &ha_handle {
            extra.push(Arc::new(orca_alerts::sinks::MqttSink {
                server: cfg.supervisor.server.clone(),
                publisher: Arc::new(h.clone()),
            }));
        }
        for i in crate::alerts::spawn(a, hub.clone(), cfg.supervisor.server.clone(), extra) {
            tracing::warn!("alerts: {i}");
        }
    }
    if let Some(addr) = &cfg.supervisor.listen {
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("{addr}: {e}"))?;
        tasks.push(tokio::spawn(serve_feed(l, hub.clone())));
    }
    tracing::info!("supervisor running for {}", cfg.supervisor.server);
    wait_for_signal().await;
    let _ = stop_tx.send(true);
    // MQTT publishes `offline` on shutdown; give it a moment.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        for t in tasks.iter_mut().take(1) {
            let _ = t.await;
        }
    })
    .await;
    Ok(())
}

/// `GET /api/v1/supervisor/snapshot` and `GET /api/v1/supervisor/stream` (SSE with
/// `Last-Event-ID` replay).
async fn serve_feed(l: tokio::net::TcpListener, hub: Arc<TelemetryHub>) {
    use axum::Router as A;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;

    async fn snapshot(State(h): State<Arc<TelemetryHub>>) -> Response {
        axum::Json(h.snapshot()).into_response()
    }
    async fn stream(State(h): State<Arc<TelemetryHub>>, headers: HeaderMap) -> Response {
        let last = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let sub = h.subscribe(last);
        let first: Vec<FeedMessage> = match sub.start {
            Resume::Snapshot(s) => vec![FeedMessage::Snapshot(s)],
            Resume::Replay(v) => v,
        };
        let s = futures_util::stream::unfold(
            (first.into_iter(), sub.rx),
            |(mut q, mut rx)| async move {
                if let Some(m) = q.next() {
                    return Some((Ok::<_, std::convert::Infallible>(m.sse()), (q, rx)));
                }
                // A lagging client is cut; EventSource reconnects with Last-Event-ID.
                let m = rx.recv().await.ok()?;
                Some((Ok(m.sse()), (q, rx)))
            },
        );
        (
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(s),
        )
            .into_response()
    }
    let app = A::new()
        .route("/api/v1/supervisor/snapshot", get(snapshot))
        .route("/api/v1/supervisor/stream", get(stream))
        .with_state(hub);
    if let Err(e) = axum::serve(l, app).await {
        tracing::error!("feed server: {e}");
    }
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
