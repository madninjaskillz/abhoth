//! A router-side Wyoming port (D153 phase 5): raw TCP for Home Assistant and other Wyoming
//! clients, passed to the first engine of that kind (`media_api = "wyoming"`) that takes the
//! connection, DeepThought's own first and then bridges' (through `bridges::forward_tcp`). Wyoming has no credentials, so only
//! peers the access settings let in anonymously (the LAN, loopback) or with no access control.
//!
//! Failover is at connection time: a target that refuses (or a bridge that cannot start its
//! engine) is cooled and the next one is tried; once one has taken the connection, bytes flow
//! both ways untouched until either side closes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use super::place::{self, Target, Want};
use crate::bridges::Upstream;
use crate::control::schema::{MediaApi, ModelKind};
use crate::state::AppState;

/// How long one target gets to take the connection.
const CONNECT: Duration = Duration::from_secs(5);

/// `stt=ADDR` / `tts=ADDR` (the CLI's `--wyoming`).
pub fn parse_flag(v: &str) -> Option<(ModelKind, String)> {
    let (k, addr) = v.split_once('=')?;
    let kind = match k {
        "stt" => ModelKind::Stt,
        "tts" => ModelKind::Tts,
        _ => return None,
    };
    (!addr.is_empty()).then(|| (kind, addr.to_string()))
}

/// Accept Wyoming connections on `l` for `kind` until the process ends.
pub async fn serve(l: TcpListener, st: Arc<AppState>, kind: ModelKind) {
    loop {
        let (sock, peer) = match l.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("wyoming {}: accept: {e}", kind.as_str());
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(why) = one(&st, kind, sock, peer).await {
                tracing::info!("wyoming {} from {peer}: {why}", kind.as_str());
            }
        });
    }
}

/// Write a Wyoming `error` event (`text`, `code`) so the client sees why.
async fn refuse(mut sock: TcpStream, code: &str, text: &str) {
    let data = serde_json::json!({"text": text, "code": code}).to_string();
    let head = serde_json::json!({"type": "error", "data_length": data.len()}).to_string();
    let _ = sock.write_all(format!("{head}\n{data}").as_bytes()).await;
    let _ = sock.shutdown().await;
}

async fn allowed(st: &AppState, peer: SocketAddr) -> Result<(), String> {
    let Some(cp) = st.admin_plane() else {
        return Ok(());
    };
    let p = cp
        .access
        .authenticate(&HeaderMap::new(), Some(peer.ip()), "WYOMING", "/wyoming")
        .await
        .map_err(|_| "not allowed".to_string())?;
    if cp.access.allows(&p, crate::access::Perm::Chat) || p.is_admin() {
        Ok(())
    } else {
        Err("not allowed".into())
    }
}

/// The targets for `kind` in the order to try.
pub fn targets(st: &AppState, kind: ModelKind) -> Vec<Target> {
    let Some(hub) = st.bridges.get() else {
        return Vec::new();
    };
    let c = hub.config();
    let cx = super::Cx {
        http: super::client(),
        hub: Some(hub),
        c: &c,
    };
    place::candidates(
        &c,
        &Want {
            kinds: &[kind],
            apis: &[MediaApi::Wyoming],
            model: None,
            server: None,
        },
        &cx.bridge_priority(),
    )
}

async fn one(
    st: &AppState,
    kind: ModelKind,
    mut sock: TcpStream,
    peer: SocketAddr,
) -> Result<(), String> {
    if let Err(why) = allowed(st, peer).await {
        refuse(
            sock,
            "forbidden",
            "this address may not use the router's Wyoming port",
        )
        .await;
        return Err(why);
    }
    let Some(hub) = st.bridges.get().cloned() else {
        refuse(sock, "unavailable", "the router has no control plane").await;
        return Err("no control plane".into());
    };
    let c = hub.config();
    let ts = targets(st, kind);
    let mut failed = Vec::new();
    for t in &ts {
        match crate::bridges::upstream_for(&c, &t.server, &t.engine, &t.card) {
            Some(Upstream::Http(base)) => {
                let addr = base
                    .trim_start_matches("http://")
                    .trim_start_matches("tcp://")
                    .trim_end_matches('/')
                    .to_string();
                match tokio::time::timeout(CONNECT, TcpStream::connect(&addr)).await {
                    Ok(Ok(mut up)) => {
                        let _busy = place::begin(t);
                        tracing::info!("wyoming {} from {peer}: to {}", kind.as_str(), t.key());
                        let _ = up.set_nodelay(true);
                        let _ = sock.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut sock, &mut up).await;
                        return Ok(());
                    }
                    Ok(Err(e)) => failed.push(format!("{}: {e}", t.key())),
                    Err(_) => failed.push(format!("{}: did not answer", t.key())),
                }
            }
            Some(Upstream::Bridge {
                bridge,
                engine,
                card,
            }) => {
                let _busy = place::begin(t);
                // an Err means nothing reached the engine: the next one can be tried
                match crate::bridges::forward_tcp(&hub, &bridge, &engine, &card, &mut sock).await {
                    Ok(()) => {
                        tracing::info!("wyoming {} from {peer}: to {}", kind.as_str(), t.key());
                        return Ok(());
                    }
                    Err(e) => failed.push(format!("{}: {e}", t.key())),
                }
            }
            None => {
                failed.push(format!("{} has no address", t.key()));
                continue;
            }
        }
        place::cool(t);
    }
    let why = if ts.is_empty() {
        format!("no Wyoming engine serves {} here", kind.as_str())
    } else {
        format!(
            "no Wyoming engine for {} took the connection: {}",
            kind.as_str(),
            failed.join("; ")
        )
    };
    refuse(sock, "unavailable", &why).await;
    Err(why)
}
