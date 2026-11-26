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

/// `run --config FILE` for a caller that restarts the supervisor itself (the single-container
/// mode): reads and checks the config, then runs until SIGTERM. Every failure comes back as
/// one line of text (config problems joined with "; ") instead of a printed exit code.
pub async fn run_file(path: &str) -> Result<(), String> {
    let cfg = std::fs::read_to_string(path)
        .map_err(|e| format!("{path}: {e}"))
        .and_then(|t| Config::parse(&t))?;
    for n in &cfg.not_wired {
        eprintln!("note: [{n}] is not wired in this build; ignored");
    }
    let problems = cfg.check();
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    run(cfg).await
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
            let feed: Arc<dyn TelemetryFeed> = hub.clone();
            let r = Arc::new(
                Router::new(u, k.resolve()?, cfg.router_server())
                    .with_slots(cfg.supervisor.slots_url.clone())
                    .with_feed(feed),
            );
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
        let feed: Arc<dyn TelemetryFeed> = hub.clone();
        // Seeded with a real snapshot (server id set): mqtt::run reads it before `follow`'s
        // first send and refuses an empty server id.
        let (snap_tx, snap_rx) = watch::channel(orca_homeassistant::feed::snapshot_from_state(
            &feed.snapshot().state,
            &server,
            &cards,
        ));
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
    if let Some(l) = cfg.local.clone() {
        crate::local::spawn(l, hub.clone());
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

/// `GET /api/v1/supervisor/history?range=10m|24h&ids=a,b*`: the hub's rings. Without `ids`,
/// the ids that have history. Patterns: exact, `prefix*`, or a whole-segment prefix.
pub fn history_json(hub: &TelemetryHub, query: &str) -> (u16, serde_json::Value) {
    use crate::Resolution;
    let mut range = "10m".to_string();
    let mut ids: Vec<String> = Vec::new();
    for (k, v) in query.split('&').filter_map(|kv| kv.split_once('=')) {
        let v = percent_decode(v);
        match k {
            "range" | "res" => range = v,
            "ids" => ids.extend(
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            ),
            _ => {}
        }
    }
    let res = match range.as_str() {
        "10m" | "fine" => Resolution::Fine,
        "24h" | "coarse" => Resolution::Coarse,
        other => {
            return (
                400,
                serde_json::json!({"error": format!("range {other:?}: use 10m or 24h")}),
            );
        }
    };
    if ids.is_empty() {
        return (
            200,
            serde_json::json!({"range": range, "ids": hub.history_ids()}),
        );
    }
    let series: serde_json::Map<String, serde_json::Value> = hub
        .history_matching(&ids, res, 400)
        .into_iter()
        .map(|(k, pts)| {
            (
                k,
                serde_json::Value::Array(
                    pts.into_iter()
                        .map(|(t, v)| serde_json::json!([t, v]))
                        .collect(),
                ),
            )
        })
        .collect();
    (
        200,
        serde_json::json!({"range": range, "step_ms": hub.step_ms(res), "series": series}),
    )
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(x)) => {
                out.push(x);
                i += 2;
            }
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /api/v1/supervisor/snapshot`, `GET /api/v1/supervisor/stream` (SSE with
/// `Last-Event-ID` replay) and `GET /api/v1/supervisor/history`.
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
    async fn history(State(h): State<Arc<TelemetryHub>>, uri: axum::http::Uri) -> Response {
        let (st, v) = history_json(&h, uri.query().unwrap_or(""));
        (
            axum::http::StatusCode::from_u16(st).unwrap_or(axum::http::StatusCode::OK),
            axum::Json(v),
        )
            .into_response()
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
        .route("/api/v1/supervisor/history", get(history))
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

#[cfg(test)]
mod tests {
    use super::*;
    use orca_sensors::Unit;

    #[test]
    fn history_endpoint() {
        let hub = TelemetryHub::new(HubConfig::default());
        let id = |s: &str| SensorId::parse(s).unwrap();
        hub.put_readings(&[
            (
                id("gpu.A.core"),
                Reading::new(40.0, Unit::Celsius, 1_000, "t"),
            ),
            (id("host.cpu"), Reading::new(3.0, Unit::Percent, 1_000, "t")),
        ]);
        let (st, v) = history_json(&hub, "");
        assert_eq!(st, 200);
        assert_eq!(v["ids"], serde_json::json!(["gpu.A.core", "host.cpu"]));
        let (_, v) = history_json(&hub, "range=24h&ids=gpu.A%2A,nope");
        assert_eq!(v["step_ms"], 60_000);
        assert_eq!(v["series"]["gpu.A.core"], serde_json::json!([[0, 40.0]]));
        assert!(v["series"].get("host.cpu").is_none());
        assert_eq!(history_json(&hub, "range=1w").0, 400);
        assert_eq!(percent_decode("a%2Cb+c%"), "a,b c%");
    }

    /// `run_file` (the single container restarts the supervisor on these): every startup
    /// failure comes back as text, nothing exits the process.
    #[tokio::test]
    async fn run_file_reports_startup_failures() {
        let e = run_file("/nonexistent/supervisor.toml").await.unwrap_err();
        assert!(e.starts_with("/nonexistent/supervisor.toml: "), "{e}");
        let d = std::env::temp_dir().join(format!("sv-runfile-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let cfg = d.join("supervisor.toml");
        std::fs::write(&cfg, "[supervisor]\nserver = \"x\"\nname = \"X\"\n").unwrap();
        let e = run_file(cfg.to_str().unwrap()).await.unwrap_err();
        assert!(e.contains("hostd_socket or hostd_command"), "{e}");
        // the case that crash-looped the container: a secret the supervisor cannot read
        let missing = d.join("no-such-key");
        let fwd = |p: std::path::PathBuf| p.display().to_string().replace('\\', "/");
        std::fs::write(
            &cfg,
            format!(
                "[supervisor]\nserver = \"x\"\nname = \"X\"\nhostd_socket = \"{}\"\n\
                 router_url = \"http://127.0.0.1:1\"\nrouter_key = {{ file = \"{}\" }}\n",
                fwd(d.join("hostd.sock")),
                fwd(missing),
            ),
        )
        .unwrap();
        let e = run_file(cfg.to_str().unwrap()).await.unwrap_err();
        assert!(
            e.contains("secret file") && e.contains("no-such-key"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
