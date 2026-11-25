//! `GET /status` (JSON for the dash) and `GET /metrics` (Prometheus text).

use serde_json::{Map, Value, json};

use crate::state::AppState;

pub fn status(st: &AppState) -> Value {
    let snap = st.snapshot();
    let rt = st.cards();
    let mut cards = Map::new();
    for (c, u) in &snap.config.upstreams {
        let r = rt.get(c).cloned().unwrap_or_default();
        let cooling = r
            .cooling_until
            .map(|t| {
                t.saturating_duration_since(std::time::Instant::now())
                    .as_secs_f64()
            })
            .filter(|s| *s > 0.0);
        cards.insert(
            c.clone(),
            json!({
                "url": u.url, "engine": u.engine, "servedId": u.served_id, "lanes": u.lanes,
                "online": u.online, "catalogModel": u.catalog_model,
                "inflight": r.inflight, "queued": st.ext.admission.queued(c),
                "served": r.served, "failures": r.failures, "coolingSeconds": cooling,
                "draining": r.draining,
                "idleSeconds": r.last_busy.map(|t| t.elapsed().as_secs()),
            }),
        );
    }
    let relay = rt
        .get("relay")
        .map(|r| json!({"inflight": r.inflight, "served": r.served}));
    json!({
        "service": "abhoth-router",
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": st.started.elapsed().as_secs(),
        "rev": snap.config.rev,
        "layout": snap.config.layout,
        "configError": st.config_error.lock().unwrap().clone(),
        "names": crate::server::models(&snap)["data"].as_array().map(|a| a.len()).unwrap_or(0),
        "cards": cards,
        "relay": relay,
        "placement": snap.config.placement,
        "slots": if snap.config.slots.enabled { st.ext.registry.list(&snap) } else { Value::Null },
        "onDemand": {"enabled": snap.config.on_demand.enabled, "dryRun": snap.config.on_demand.dry_run},
        "totals": st.metrics.summary(),
    })
}

pub fn metrics_text(st: &AppState) -> String {
    let snap = st.snapshot();
    let mut gauges = Vec::new();
    for c in snap.config.upstreams.keys() {
        let r = st.card(c);
        let l = format!("{{card=\"{c}\"}}");
        gauges.push((
            "abhoth_router_inflight".to_string(),
            l.clone(),
            r.inflight as f64,
        ));
        gauges.push((
            "abhoth_router_queued".to_string(),
            l.clone(),
            st.ext.admission.queued(c) as f64,
        ));
        gauges.push((
            "abhoth_router_draining".to_string(),
            l,
            if r.draining { 1.0 } else { 0.0 },
        ));
    }
    if snap.config.slots.enabled {
        let held = st.ext.registry.held(&snap);
        gauges.push((
            "abhoth_router_leases".to_string(),
            String::new(),
            held.len() as f64,
        ));
    }
    st.metrics.render(&gauges)
}
