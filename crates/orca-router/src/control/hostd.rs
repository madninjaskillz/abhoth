//! The router's side of the host daemon (`abhoth hostd`, SUPERVISOR.md 2.4 and 6.2/6.3), for
//! the clock and power controller (`orca-gpuctl`):
//!
//! - **engine ready**: when an engine enters `Ready`, hostd re-asserts the card's clocks,
//!   power limit and persistence (`{"type":"assert","card":"A"}`, answered `assert.result`);
//! - **wake**: before the router proxies a request to a card, hostd ends any idle clock-down
//!   on it (`{"type":"wake","card":"A"}`);
//! - **hold** / **release**: no idle clock-down while a job needs the card.
//!
//! The messages are 🟪D's `orca-hostd::protocol::ToHostd` (NDJSON envelopes): [`Op::Hostd`]
//! carries the message `type` as `method` and its other fields as `params`; `hostctl serve`
//! frames them (`hello`, then the message) and returns hostd's reply frame, so an `error` or
//! an `assert.result` with `ok: false` comes back as a failure ([`reply_ok`]).
//!
//! Both go through the same channel as every other host call ([`Op::Hostd`] over the
//! executor: in-process for a local server, SSH plus `hostctl serve` relaying to hostd's
//! socket for a remote one). Wakes are on the request path, so they are fire-and-forget and
//! debounced per card: the idle timer is a minute, and one wake every few seconds is enough.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use super::exec::{Cancel, ExecRequest, Executor, Op};

/// What the router tells hostd. Implementations must not block the request path for long.
#[async_trait]
pub trait HostdClient: Send + Sync {
    /// The engine on `card` is Ready: re-assert clocks and power now. Errors are returned so
    /// the job can note them; they never fail a switch.
    async fn engine_ready(&self, server: &str, card: &str) -> Result<(), String>;
    /// A request is about to go to `card`. Returns at once.
    fn wake(&self, server: &str, card: &str);
    /// No idle clock-down on `card` (`None`: every card) for `ttl_s` seconds or until released.
    async fn hold(
        &self,
        _server: &str,
        _card: Option<&str>,
        _ttl_s: u64,
        _reason: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn release(&self, _server: &str, _card: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

/// The protocol's `Target`: a card label or every card.
fn target(card: Option<&str>) -> serde_json::Value {
    match card {
        Some(c) => json!({"card": c}),
        None => json!("all"),
    }
}

/// hostd's reply frame (the relay's stdout) as a result.
pub fn reply_ok(what: &str, stdout: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("{what}: hostd answered something unreadable: {e}"))?;
    match v["type"].as_str() {
        Some("error") => Err(format!(
            "{what}: {} ({})",
            v["message"].as_str().unwrap_or(""),
            v["code"].as_str().unwrap_or("error")
        )),
        Some("assert.result") if v["ok"] == false => Err(format!(
            "{what}: {}",
            v["summary"].as_str().unwrap_or("not ok")
        )),
        Some(_) => Ok(()),
        None => Err(format!("{what}: hostd answered without a type")),
    }
}

/// No host daemon (remote servers without hostd, tests).
pub struct NoHostd;

#[async_trait]
impl HostdClient for NoHostd {
    async fn engine_ready(&self, _server: &str, _card: &str) -> Result<(), String> {
        Ok(())
    }
    fn wake(&self, _server: &str, _card: &str) {}
}

/// hostd over the executors.
pub struct ExecHostd {
    exec: Arc<dyn Executor>,
    /// Minimum gap between two wakes of one card.
    pub wake_every: Duration,
    pub timeout: Duration,
    last_wake: Mutex<BTreeMap<(String, String), Instant>>,
}

impl ExecHostd {
    pub fn new(exec: Arc<dyn Executor>) -> Self {
        Self {
            exec,
            wake_every: Duration::from_secs(5),
            timeout: Duration::from_secs(10),
            last_wake: Mutex::new(BTreeMap::new()),
        }
    }

    fn request(&self, server: &str, method: &str, params: serde_json::Value) -> ExecRequest {
        ExecRequest {
            server: server.into(),
            op: Op::Hostd {
                method: method.into(),
                params,
            },
            timeout: self.timeout,
            root: false,
            stdin: None,
        }
    }

    async fn call(&self, req: ExecRequest, what: &str) -> Result<(), String> {
        match self.exec.run(req, &Cancel::new()).await {
            Ok(o) if o.ok() => reply_ok(what, &o.stdout),
            Ok(o) => Err(format!("{what}: {}", o.summary())),
            Err(e) => Err(format!("{what}: {e}")),
        }
    }

    /// Whether a wake for `card` is due (and mark it sent).
    fn wake_due(&self, server: &str, card: &str) -> bool {
        let mut g = self.last_wake.lock().unwrap_or_else(|e| e.into_inner());
        let k = (server.to_string(), card.to_string());
        if g.get(&k).is_some_and(|t| t.elapsed() < self.wake_every) {
            return false;
        }
        g.insert(k, Instant::now());
        true
    }
}

#[async_trait]
impl HostdClient for ExecHostd {
    async fn engine_ready(&self, server: &str, card: &str) -> Result<(), String> {
        let req = self.request(server, "assert", json!({"card": card}));
        self.call(req, &format!("hostd assert on {card}")).await
    }

    async fn hold(
        &self,
        server: &str,
        card: Option<&str>,
        ttl_s: u64,
        reason: &str,
    ) -> Result<(), String> {
        let req = self.request(
            server,
            "hold",
            json!({"target": target(card), "ttl_s": ttl_s.min(3600), "reason": reason}),
        );
        self.call(req, "hostd hold").await
    }

    async fn release(&self, server: &str, card: Option<&str>) -> Result<(), String> {
        let req = self.request(server, "release", json!({"target": target(card)}));
        self.call(req, "hostd release").await
    }

    fn wake(&self, server: &str, card: &str) {
        if !self.wake_due(server, card) {
            return;
        }
        let req = self.request(server, "wake", json!({"card": card}));
        let exec = self.exec.clone();
        let card = card.to_string();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = exec.run(req, &Cancel::new()).await {
                    tracing::debug!("hostd wake {card}: {e}");
                }
            });
        }
    }
}

/// The data plane's hook: called with a card id right before a request is proxied to it.
pub trait CardWake: Send + Sync {
    fn wake(&self, card: &str);
}

/// Card id -> its server, so the data plane only needs the card.
pub struct WakeByCard {
    pub hostd: Arc<dyn HostdClient>,
    pub server_of: BTreeMap<String, String>,
}

impl CardWake for WakeByCard {
    fn wake(&self, card: &str) {
        if let Some(s) = self.server_of.get(card) {
            self.hostd.wake(s, card);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::exec::FakeExecutor;

    #[tokio::test]
    async fn ready_asserts_and_wakes_are_debounced() {
        let fake = Arc::new(FakeExecutor::default());
        fake.on(
            "assert",
            vec![Ok(crate::control::exec::ExecOutput::ok_with(
                r#"{"id":1,"type":"assert.result","card":"A","ok":true,"summary":"A ok"}"#,
            ))],
        );
        let h = ExecHostd::new(fake.clone());
        h.engine_ready("gpu", "A").await.unwrap();
        let w = WakeByCard {
            hostd: Arc::new(h),
            server_of: [("A".to_string(), "gpu".to_string())].into_iter().collect(),
        };
        w.wake("A");
        w.wake("A");
        w.wake("Z");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let labels = fake.labels();
        assert_eq!(labels.iter().filter(|l| l.as_str() == "assert").count(), 1);
        assert_eq!(
            labels.iter().filter(|l| l.as_str() == "wake").count(),
            1,
            "{labels:?}"
        );
        let reqs = fake.requests();
        assert!(matches!(&reqs[0].op, Op::Hostd { params, .. } if params["card"] == "A"));
    }

    /// What the relay sends is 🟪D's `orca-hostd::protocol::ToHostd` exactly, and its replies
    /// are read back as results.
    #[test]
    fn frames_match_the_hostd_protocol() {
        use crate::control::hostctl::{hostd_frames, hostd_reply};
        use orca_hostd::protocol::{Envelope, ToHostd};
        let msg = |method: &str, params: serde_json::Value| {
            let [hello, m] = hostd_frames(method, &params);
            let h: Envelope<ToHostd> = serde_json::from_value(hello).unwrap();
            assert!(matches!(h.msg, ToHostd::Hello { proto: 1, .. }));
            let e: Envelope<ToHostd> = serde_json::from_value(m).unwrap();
            assert_eq!(e.id, Some(1));
            e.msg
        };
        assert_eq!(
            msg("assert", json!({"card": "A"})),
            ToHostd::Assert { card: "A".into() }
        );
        assert_eq!(
            msg("wake", json!({"card": "B"})),
            ToHostd::Wake { card: "B".into() }
        );
        assert!(matches!(
            msg(
                "hold",
                json!({"target": target(Some("A")), "ttl_s": 60, "reason": "job"})
            ),
            ToHostd::Hold { ttl_s: 60, .. }
        ));
        assert!(matches!(
            msg("release", json!({"target": target(None)})),
            ToHostd::Release { .. }
        ));
        assert_eq!(hostd_reply(r#"{"type":"telemetry","ts":1}"#), None);
        assert_eq!(hostd_reply(r#"{"id":0,"type":"hello","proto":1}"#), None);
        let ack = hostd_reply(r#"{"id":1,"type":"ack"}"#).unwrap();
        assert!(reply_ok("x", &ack).is_ok());
        let refused = hostd_reply(r#"{"id":0,"type":"error","code":"version","message":"old"}"#);
        assert!(reply_ok("x", &refused.unwrap()).is_err());
        let bad = r#"{"id":1,"type":"assert.result","card":"A","ok":false,"summary":"PL"}"#;
        assert!(reply_ok("x", bad).is_err());
    }
}
