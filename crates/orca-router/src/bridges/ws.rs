//! `GET /bridge/ws` (13.1): a bridge dials in with its token, says `hello` with its descriptor,
//! gets `welcome`, then stays connected (pings every 20 s; 45 s of silence ends it).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use orca_bridge::mux::{Mux, Out};
use orca_bridge::proto::{
    CLOSE_BAD_TOKEN, CLOSE_BAD_VERSION, CLOSE_REFUSED, Frame, PING_SECONDS, PROTOCOL,
    SILENCE_SECONDS,
};

use super::{Hub, tokens};

/// Answer the upgrade. A bad token still upgrades, then closes with 4401 (the protocol's way).
pub async fn upgrade(hub: Option<Arc<Hub>>, req: Request) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_default();
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string());
    let (mut parts, _) = req.into_parts();
    let up = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let Some(hub) = hub else {
        return crate::proxy::error(503, "this router takes no bridges", "bridge_error", None);
    };
    let bridge = token
        .as_deref()
        .and_then(|t| tokens::verify(&hub.state_dir, t));
    up.max_message_size(64 << 20)
        .on_upgrade(move |socket| session(hub, socket, bridge, peer))
}

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

async fn session(hub: Arc<Hub>, mut socket: WebSocket, bridge: Option<String>, peer: String) {
    let Some(id) = bridge else {
        tracing::warn!("bridge: a connection from {peer} with no valid token was refused");
        close(socket, CLOSE_BAD_TOKEN, "bad token").await;
        return;
    };
    // The first frame must be hello, within 15 s.
    let hello = match tokio::time::timeout(Duration::from_secs(15), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => Frame::parse(&t).ok(),
        _ => None,
    };
    let Some(Frame::Hello {
        v,
        bridge,
        version,
        descriptor,
    }) = hello
    else {
        close(socket, CLOSE_REFUSED, "the first frame must be hello").await;
        return;
    };
    if v != PROTOCOL {
        close(
            socket,
            CLOSE_BAD_VERSION,
            &format!("this router speaks v{PROTOCOL}"),
        )
        .await;
        return;
    }
    if bridge != id {
        tracing::warn!("bridge: {peer} said it is `{bridge}` with the token of `{id}`");
        close(socket, CLOSE_BAD_TOKEN, "the token is for another bridge").await;
        return;
    }
    let refused = hub.refuse(&id, &descriptor);
    if !refused.is_empty() {
        tracing::warn!("bridge {id}: descriptor refused: {}", refused.join("; "));
        close(socket, CLOSE_REFUSED, &refused.join("; ")).await;
        return;
    }

    let (mux, mut out) = Mux::new();
    let conn_no = hub.upsert(&id, descriptor, &version, &peer, Some(mux.clone()));
    let settings = hub.settings(&id);
    mux.send(&Frame::Welcome {
        v: PROTOCOL,
        server: id.clone(),
        settings,
    });
    tracing::info!("bridge {id} connected from {peer} (abhoth {version})");

    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(o) = out.recv().await {
            let m = match o {
                Out::Text(t) => Message::Text(t.into()),
                Out::Binary(b) => Message::Binary(b.into()),
                Out::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let mut ping = tokio::time::interval(Duration::from_secs(PING_SECONDS));
    ping.tick().await;
    let why = loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let f = match Frame::parse(&t) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!("bridge {id}: {e}");
                            continue;
                        }
                    };
                    match mux.on_text(f) {
                        Some(Frame::Descriptor { descriptor }) => {
                            let refused = hub.refuse(&id, &descriptor);
                            if refused.is_empty() {
                                hub.upsert(&id, descriptor, "", &peer, None);
                            } else {
                                tracing::warn!("bridge {id}: new descriptor refused: {}", refused.join("; "));
                            }
                        }
                        Some(Frame::Event { kind, data }) => hub.event(&id, &kind, data),
                        _ => hub.touch(&id),
                    }
                }
                Some(Ok(Message::Binary(b))) => mux.on_binary(&b),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    mux.on_other();
                    hub.touch(&id);
                }
                Some(Ok(Message::Close(c))) => break format!("closed ({})", c.map_or(1005, |c| c.code)),
                Some(Err(e)) => break e.to_string(),
                None => break "the connection ended".into(),
            },
            _ = ping.tick() => {
                if mux.is_closed() {
                    break "replaced or closed".into();
                }
                if mux.silent_for() > Duration::from_secs(SILENCE_SECONDS) {
                    mux.close(1001, "silent");
                    break format!("no word for {SILENCE_SECONDS}s");
                }
                mux.send(&Frame::Ping { at: chrono::Utc::now().timestamp_millis() as u64 });
            }
        }
    };
    mux.shutdown(&why);
    hub.offline(&id, conn_no);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    tracing::info!("bridge {id} disconnected: {why}");
}

/// Is this request the bridge endpoint (`/bridge/ws`, or `/migo/ws` under Migo's name)?
pub fn is_bridge_path(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::GET && orca_bridge::proto::is_ws_path(path)
}
