//! The router's side of the host daemon (`abhoth hostd`, SUPERVISOR.md 2.4 and 6.2/6.3), for
//! the clock and power controller (`orca-gpuctl`):
//!
//! - **engine ready**: when an engine enters `Ready`, hostd re-asserts the card's clocks,
//!   power limit and persistence (`assert {card, reason: "engine_ready"}`);
//! - **wake**: before the router proxies a request to a card, hostd ends any idle clock-down
//!   on it (`wake {card, source: "router"}`).
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
        let req = self.request(
            server,
            "assert",
            json!({"card": card, "reason": "engine_ready"}),
        );
        match self.exec.run(req, &Cancel::new()).await {
            Ok(o) if o.ok() => Ok(()),
            Ok(o) => Err(format!("hostd assert on {card}: {}", o.summary())),
            Err(e) => Err(format!("hostd assert on {card}: {e}")),
        }
    }

    fn wake(&self, server: &str, card: &str) {
        if !self.wake_due(server, card) {
            return;
        }
        let req = self.request(server, "wake", json!({"card": card, "source": "router"}));
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
        assert!(
            matches!(&reqs[0].op, Op::Hostd { params, .. } if params["reason"] == "engine_ready")
        );
    }
}
