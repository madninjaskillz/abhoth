//! The stream multiplexer both ends share (13.1): commands with their results, and proxied
//! streams with credit-based flow control (at most [`WINDOW`] unacknowledged bytes per stream and
//! direction). It knows nothing of the WebSocket library underneath: the glue on each side feeds
//! it incoming frames ([`Mux::on_text`], [`Mux::on_binary`]) and writes what comes out of the
//! [`Out`] channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::proto::{CHUNK, Frame, WINDOW, decode_data, encode_data};

/// What the glue writes to the WebSocket.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

/// A stream's response head.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug)]
enum Inbound {
    Data(Bytes),
    End,
    Reset(String),
}

struct Slot {
    tx: mpsc::UnboundedSender<Inbound>,
    credit: Arc<Semaphore>,
    head: Option<oneshot::Sender<Head>>,
}

pub struct Mux {
    out: mpsc::UnboundedSender<Out>,
    streams: Mutex<HashMap<u32, Slot>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_sid: AtomicU32,
    next_cmd: AtomicU64,
    closed: AtomicBool,
    born: Instant,
    /// Milliseconds since `born` when something last arrived.
    heard: AtomicU64,
}

impl Mux {
    pub fn new() -> (Arc<Mux>, mpsc::UnboundedReceiver<Out>) {
        let (out, rx) = mpsc::unbounded_channel();
        let m = Mux {
            out,
            streams: Mutex::default(),
            pending: Mutex::default(),
            next_sid: AtomicU32::new(1),
            next_cmd: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            born: Instant::now(),
            heard: AtomicU64::new(0),
        };
        (Arc::new(m), rx)
    }

    /// Queue a control frame. False once the connection is gone.
    pub fn send(&self, f: &Frame) -> bool {
        !self.is_closed() && self.out.send(Out::Text(f.to_text())).is_ok()
    }

    /// Close the connection with a code (the glue sends the close frame).
    pub fn close(&self, code: u16, reason: &str) {
        let _ = self.out.send(Out::Close(code, reason.into()));
        self.shutdown(reason);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// How long since anything arrived (the keepalive's dead-connection check).
    pub fn silent_for(&self) -> Duration {
        let now = self.born.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.heard.load(Ordering::SeqCst)))
    }

    fn heard_now(&self) {
        self.heard
            .store(self.born.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    /// Streams open now.
    pub fn open_streams(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    /// The connection ended: every open stream and pending command fails.
    pub fn shutdown(&self, why: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        for (_, s) in self.streams.lock().unwrap().drain() {
            s.credit.close();
            let _ = s.tx.send(Inbound::Reset(format!("connection lost: {why}")));
        }
        for (_, p) in self.pending.lock().unwrap().drain() {
            let _ = p.send(Err(format!("connection lost: {why}")));
        }
    }

    // ── commands ─────────────────────────────────────────────────────────────────────────────

    /// Send `cmd` and wait for its `result`.
    pub async fn call(&self, op: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_cmd.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if !self.send(&Frame::Cmd {
            id,
            op: op.into(),
            args,
        }) {
            self.pending.lock().unwrap().remove(&id);
            return Err("the bridge is not connected".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the connection ended".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("no answer to `{op}` in {}s", timeout.as_secs()))
            }
        }
    }

    // ── streams ──────────────────────────────────────────────────────────────────────────────

    fn register(self: &Arc<Self>, sid: u32, head: Option<oneshot::Sender<Head>>) -> Stream {
        let (tx, rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Semaphore::new(WINDOW));
        self.streams.lock().unwrap().insert(
            sid,
            Slot {
                tx,
                credit: credit.clone(),
                head,
            },
        );
        let state: Shared = Arc::default();
        Stream {
            sid,
            tx: StreamTx {
                sid,
                mux: self.clone(),
                credit,
                state: state.clone(),
            },
            rx: StreamRx {
                sid,
                mux: self.clone(),
                rx,
                state,
            },
        }
    }

    /// The router opens a stream to an engine on the bridge. The head arrives on the receiver.
    pub fn open(
        self: &Arc<Self>,
        engine: &str,
        card: &str,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        ws: bool,
    ) -> Result<(Stream, oneshot::Receiver<Head>), String> {
        let sid = self.next_sid.fetch_add(2, Ordering::SeqCst);
        let (htx, hrx) = oneshot::channel();
        let s = self.register(sid, Some(htx));
        if !self.send(&Frame::Open {
            sid,
            engine: engine.into(),
            card: card.into(),
            method: method.into(),
            path: path.into(),
            headers,
            ws,
        }) {
            self.forget(sid);
            return Err("the bridge is not connected".into());
        }
        Ok((s, hrx))
    }

    /// The bridge takes a stream the router opened.
    pub fn accept(self: &Arc<Self>, sid: u32) -> Stream {
        self.register(sid, None)
    }

    /// The bridge answers a stream's head.
    pub fn head(&self, sid: u32, status: u16, headers: Vec<(String, String)>) -> bool {
        self.send(&Frame::Head {
            sid,
            status,
            headers,
        })
    }

    fn forget(&self, sid: u32) {
        self.streams.lock().unwrap().remove(&sid);
    }

    // ── incoming ─────────────────────────────────────────────────────────────────────────────

    /// A control frame arrived. Stream and command bookkeeping is handled here; anything else
    /// (hello, welcome, cmd, open, event, descriptor) is handed back to the caller.
    pub fn on_text(&self, f: Frame) -> Option<Frame> {
        self.heard_now();
        match f {
            Frame::Result {
                id,
                ok,
                data,
                error,
            } => {
                if let Some(p) = self.pending.lock().unwrap().remove(&id) {
                    let _ = p.send(if ok {
                        Ok(data.unwrap_or(Value::Null))
                    } else {
                        Err(error.unwrap_or_else(|| "failed".into()))
                    });
                }
                None
            }
            Frame::Head {
                sid,
                status,
                headers,
            } => {
                let mut g = self.streams.lock().unwrap();
                if let Some(h) = g.get_mut(&sid).and_then(|s| s.head.take()) {
                    let _ = h.send(Head { status, headers });
                }
                None
            }
            Frame::Credit { sid, bytes } => {
                if let Some(s) = self.streams.lock().unwrap().get(&sid) {
                    s.credit.add_permits((bytes as usize).min(WINDOW));
                }
                None
            }
            Frame::End { sid } => {
                if let Some(s) = self.streams.lock().unwrap().get(&sid) {
                    let _ = s.tx.send(Inbound::End);
                }
                None
            }
            Frame::Reset { sid, error } => {
                if let Some(s) = self.streams.lock().unwrap().remove(&sid) {
                    s.credit.close();
                    let _ = s.tx.send(Inbound::Reset(error));
                }
                None
            }
            Frame::Ping { at } => {
                self.send(&Frame::Pong { at });
                None
            }
            Frame::Pong { .. } => None,
            other => Some(other),
        }
    }

    /// A binary frame arrived (stream data).
    pub fn on_binary(&self, b: &[u8]) {
        self.heard_now();
        let Some((sid, payload)) = decode_data(b) else {
            return;
        };
        if let Some(s) = self.streams.lock().unwrap().get(&sid) {
            let _ = s.tx.send(Inbound::Data(Bytes::copy_from_slice(payload)));
        }
    }

    /// Something arrived that is not a frame (a WebSocket ping or pong).
    pub fn on_other(&self) {
        self.heard_now();
    }
}

/// One proxied stream, both directions. [`Stream::split`] gives the halves for pumping both
/// ways at once.
pub struct Stream {
    pub sid: u32,
    pub tx: StreamTx,
    pub rx: StreamRx,
}

impl Stream {
    /// Send body bytes (waits for credit).
    pub async fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.tx.send(data).await
    }
    /// This side has sent everything.
    pub fn end(&mut self) {
        self.tx.end()
    }
    /// Abort the stream (both directions).
    pub fn reset(&mut self, error: &str) {
        self.tx.reset(error)
    }
    /// The next body chunk from the other side; `None` at its end.
    pub async fn recv(&mut self) -> Option<Result<Bytes, String>> {
        self.rx.recv().await
    }
    /// Read the other side's whole body (up to `limit` bytes).
    pub async fn read_all(&mut self, limit: usize) -> Result<Vec<u8>, String> {
        let mut v = Vec::new();
        while let Some(c) = self.recv().await {
            let c = c?;
            if v.len() + c.len() > limit {
                self.reset("body too large");
                return Err("body too large".into());
            }
            v.extend_from_slice(&c);
        }
        Ok(v)
    }
    pub fn split(self) -> (StreamTx, StreamRx) {
        (self.tx, self.rx)
    }
}

/// (sent end, got end, failed)
type Shared = Arc<Mutex<(bool, bool, bool)>>;

/// The sending half of a split [`Stream`].
pub struct StreamTx {
    pub sid: u32,
    mux: Arc<Mux>,
    credit: Arc<Semaphore>,
    state: Shared,
}

impl StreamTx {
    pub async fn send(&self, data: &[u8]) -> Result<(), String> {
        for chunk in data.chunks(CHUNK) {
            let p = self
                .credit
                .acquire_many(chunk.len() as u32)
                .await
                .map_err(|_| "the stream was closed".to_string())?;
            p.forget();
            if self.mux.is_closed()
                || self
                    .mux
                    .out
                    .send(Out::Binary(encode_data(self.sid, chunk)))
                    .is_err()
            {
                return Err("the connection ended".into());
            }
        }
        Ok(())
    }
    /// One WebSocket message as one binary frame (its boundaries matter), whatever its size.
    pub async fn send_message(&self, payload: &[u8]) -> Result<(), String> {
        let n = payload.len().clamp(1, WINDOW);
        let p = self
            .credit
            .acquire_many(n as u32)
            .await
            .map_err(|_| "the stream was closed".to_string())?;
        p.forget();
        if self.mux.is_closed()
            || self
                .mux
                .out
                .send(Out::Binary(encode_data(self.sid, payload)))
                .is_err()
        {
            return Err("the connection ended".into());
        }
        Ok(())
    }
    pub fn end(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.0 && !s.2 {
            s.0 = true;
            self.mux.send(&Frame::End { sid: self.sid });
            if s.1 {
                self.mux.forget(self.sid);
            }
        }
    }
    pub fn reset(&self, error: &str) {
        let mut s = self.state.lock().unwrap();
        if !s.2 {
            s.2 = true;
            self.mux.send(&Frame::Reset {
                sid: self.sid,
                error: error.into(),
            });
            self.mux.forget(self.sid);
        }
    }
}

impl Drop for StreamTx {
    fn drop(&mut self) {
        let ended = {
            let s = self.state.lock().unwrap();
            s.0 || s.2
        };
        if !ended {
            self.reset("dropped");
        }
    }
}

/// The receiving half of a split [`Stream`].
pub struct StreamRx {
    pub sid: u32,
    mux: Arc<Mux>,
    rx: mpsc::UnboundedReceiver<Inbound>,
    state: Shared,
}

impl StreamRx {
    pub async fn recv(&mut self) -> Option<Result<Bytes, String>> {
        {
            let s = self.state.lock().unwrap();
            if s.1 || s.2 {
                return None;
            }
        }
        match self.rx.recv().await {
            Some(Inbound::Data(b)) => {
                self.mux.send(&Frame::Credit {
                    sid: self.sid,
                    bytes: b.len().max(1) as u64,
                });
                Some(Ok(b))
            }
            Some(Inbound::End) => {
                let mut s = self.state.lock().unwrap();
                s.1 = true;
                if s.0 {
                    self.mux.forget(self.sid);
                }
                None
            }
            Some(Inbound::Reset(e)) => {
                self.state.lock().unwrap().2 = true;
                Some(Err(e))
            }
            None => {
                self.state.lock().unwrap().2 = true;
                Some(Err("the connection ended".into()))
            }
        }
    }
}

impl Drop for StreamRx {
    /// Dropping the receiving half after this side has finished sending (a router whose client
    /// went away mid-response) aborts the stream; before that, the sending half decides.
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap();
        if s.0 && !s.1 && !s.2 {
            s.2 = true;
            self.mux.send(&Frame::Reset {
                sid: self.sid,
                error: "dropped".into(),
            });
            self.mux.forget(self.sid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Two muxes wired back to back (what the WebSocket does).
    fn pair() -> (Arc<Mux>, Arc<Mux>) {
        let (a, ar) = Mux::new();
        let (b, br) = Mux::new();
        fn wire(mut from: mpsc::UnboundedReceiver<Out>, to: Arc<Mux>, peer: Arc<Mux>) {
            tokio::spawn(async move {
                while let Some(o) = from.recv().await {
                    match o {
                        Out::Text(t) => {
                            if let Some(f) = to.on_text(Frame::parse(&t).unwrap()) {
                                // the tests' "application": answer commands, echo streams
                                match f {
                                    Frame::Cmd { id, op, args } => {
                                        to.send(&Frame::Result {
                                            id,
                                            ok: op != "fail",
                                            data: Some(json!({"op": op, "args": args})),
                                            error: Some("asked to fail".into()),
                                        });
                                    }
                                    Frame::Open { sid, .. } => {
                                        let mut s = to.accept(sid);
                                        to.head(sid, 200, vec![("x-a".into(), "1".into())]);
                                        tokio::spawn(async move {
                                            let body = s.read_all(1 << 30).await.unwrap();
                                            s.send(&body).await.unwrap();
                                            s.end();
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Out::Binary(b) => to.on_binary(&b),
                        Out::Close(..) => {
                            to.shutdown("closed");
                            peer.shutdown("closed");
                        }
                    }
                }
                to.shutdown("wire ended");
            });
        }
        wire(ar, b.clone(), a.clone());
        wire(br, a.clone(), b.clone());
        (a, b)
    }

    #[tokio::test]
    async fn commands_get_their_results() {
        let (router, _bridge) = pair();
        let v = router
            .call("status", json!({"x": 1}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(v["op"], "status");
        let e = router
            .call("fail", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(e, "asked to fail");
    }

    #[tokio::test]
    async fn a_stream_larger_than_the_window_flows_both_ways() {
        let (router, _bridge) = pair();
        let (mut s, head) = router
            .open("e", "GPU0", "POST", "/x", vec![], false)
            .unwrap();
        let body: Vec<u8> = (0..(3 * WINDOW + 12345)).map(|i| (i % 251) as u8).collect();
        let sender = {
            let body = body.clone();
            async move {
                s.send(&body).await.unwrap();
                s.end();
                s
            }
        };
        // Sending 3+ windows only finishes if the far side reads and gives credit back.
        let mut s = tokio::time::timeout(Duration::from_secs(10), sender)
            .await
            .expect("credit flows");
        let h = head.await.unwrap();
        assert_eq!(h.status, 200);
        let back = s.read_all(1 << 30).await.unwrap();
        assert_eq!(back.len(), body.len());
        assert!(back == body);
        assert_eq!(router.open_streams(), 0);
    }

    #[tokio::test]
    async fn a_lost_connection_fails_streams_and_calls() {
        let (router, bridge) = pair();
        let (mut s, _head) = router.open("e", "GPU0", "GET", "/", vec![], false).unwrap();
        bridge.close(1000, "bye");
        let r = tokio::time::timeout(Duration::from_secs(5), s.recv())
            .await
            .unwrap();
        assert!(matches!(r, Some(Err(_))), "{r:?}");
        let e = router
            .call("status", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            e.contains("not connected") || e.contains("connection"),
            "{e}"
        );
    }
}
