//! Counters for `GET /metrics` (Prometheus text) and `/status`. Plain text formatting, no
//! client library (ROUTER.md 4.1).

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

#[derive(Default)]
struct Inner {
    /// (model, card, status) -> count
    requests: BTreeMap<(String, String, u16), u64>,
    /// card -> (count, sum ttft s, sum duration s)
    timing: BTreeMap<String, (u64, f64, f64)>,
    failovers: u64,
    truncated: u64,
    affinity_hits: u64,
    affinity_moves: u64,
    lease_routed: u64,
    queued_total: u64,
    /// on-demand action -> count
    ondemand: BTreeMap<String, u64>,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    pub fn request(&self, model: &str, card: &str, status: u16, ttft: Option<f64>, total: f64) {
        let mut g = self.inner.lock().unwrap();
        *g.requests
            .entry((model.to_string(), card.to_string(), status))
            .or_default() += 1;
        let t = g.timing.entry(card.to_string()).or_default();
        t.0 += 1;
        t.1 += ttft.unwrap_or(0.0);
        t.2 += total;
    }

    pub fn failover(&self) {
        self.inner.lock().unwrap().failovers += 1;
    }
    pub fn truncated(&self) {
        self.inner.lock().unwrap().truncated += 1;
    }
    pub fn affinity(&self, moved: bool) {
        let mut g = self.inner.lock().unwrap();
        if moved {
            g.affinity_moves += 1;
        } else {
            g.affinity_hits += 1;
        }
    }
    pub fn lease_routed(&self) {
        self.inner.lock().unwrap().lease_routed += 1;
    }
    pub fn queued(&self) {
        self.inner.lock().unwrap().queued_total += 1;
    }
    pub fn ondemand(&self, action: &str) {
        *self
            .inner
            .lock()
            .unwrap()
            .ondemand
            .entry(action.to_string())
            .or_default() += 1;
    }

    /// Prometheus text; `gauges` are (name, labels, value) lines added by the caller.
    pub fn render(&self, gauges: &[(String, String, f64)]) -> String {
        let g = self.inner.lock().unwrap();
        let mut s = String::new();
        s.push_str("# TYPE abhoth_router_requests_total counter\n");
        for ((m, c, st), n) in &g.requests {
            let _ = writeln!(
                s,
                "abhoth_router_requests_total{{model=\"{}\",card=\"{}\",status=\"{st}\"}} {n}",
                esc(m),
                esc(c)
            );
        }
        s.push_str("# TYPE abhoth_router_ttft_seconds_sum counter\n");
        for (c, (n, ttft, total)) in &g.timing {
            let _ = writeln!(
                s,
                "abhoth_router_ttft_seconds_sum{{card=\"{}\"}} {ttft:.3}",
                esc(c)
            );
            let _ = writeln!(
                s,
                "abhoth_router_duration_seconds_sum{{card=\"{}\"}} {total:.3}",
                esc(c)
            );
            let _ = writeln!(
                s,
                "abhoth_router_timed_requests_total{{card=\"{}\"}} {n}",
                esc(c)
            );
        }
        let _ = writeln!(s, "abhoth_router_failovers_total {}", g.failovers);
        let _ = writeln!(s, "abhoth_router_truncated_total {}", g.truncated);
        let _ = writeln!(s, "abhoth_router_affinity_hits_total {}", g.affinity_hits);
        let _ = writeln!(s, "abhoth_router_affinity_moves_total {}", g.affinity_moves);
        let _ = writeln!(s, "abhoth_router_lease_routed_total {}", g.lease_routed);
        let _ = writeln!(s, "abhoth_router_queued_total {}", g.queued_total);
        for (a, n) in &g.ondemand {
            let _ = writeln!(
                s,
                "abhoth_router_ondemand_decisions_total{{action=\"{}\"}} {n}",
                esc(a)
            );
        }
        for (name, labels, v) in gauges {
            let _ = writeln!(s, "{name}{labels} {v}");
        }
        s
    }

    pub fn summary(&self) -> serde_json::Value {
        let g = self.inner.lock().unwrap();
        let total: u64 = g.requests.values().sum();
        let errors: u64 = g
            .requests
            .iter()
            .filter(|((_, _, st), _)| *st >= 400)
            .map(|(_, n)| n)
            .sum();
        serde_json::json!({
            "requests": total, "errors": errors, "failovers": g.failovers, "truncated": g.truncated,
            "affinityHits": g.affinity_hits, "affinityMoves": g.affinity_moves,
            "leaseRouted": g.lease_routed, "queued": g.queued_total, "onDemand": g.ondemand,
        })
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}
