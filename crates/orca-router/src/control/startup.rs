//! Starting the control plane inside `abhoth router serve --control router.toml`
//! (ROUTER-CONFIG.md 13.3 step 3). With `control.enabled = false` nothing here runs and the
//! router is the data plane alone, exactly as before.
//!
//! With it on, per server:
//! - `router.db` (jobs, leases, on-demand switch history), unfinished jobs marked interrupted;
//! - the executors, host and hostd client ([`super::host::for_server`]);
//! - 🟩B's orchestrator over the [`RouterPlane`] (slots, guards, routes), on-demand switching
//!   through it ([`InProcess`]) instead of the profiles API;
//! - 🟨C's admin API with [`OrchActions`] (plan, switch) and its `Access` as the caller resolver
//!   for the on-demand gate and the `:8098` shim;
//! - the hostd wake hook on the request path;
//! - the reconcile loop (adopts what runs; in `act` mode starts missing engines, never stops);
//! - a [`ConfigOverlay`](crate::state::ConfigOverlay) over every loaded `router.json`: the
//!   on-demand settings and names from `router.toml`, and what the cards run now.
//!
//! One managed server is supported for switching (DeepThought); further servers are observed.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;

use super::exec::Cancel;
use super::host::{ExecHost, ServerControl, for_server};
use super::schema::{self, ColdPolicy, ControlConfig, ControlMode};
use super::store::{ActiveLayout, Store};
use crate::access::Access;
use crate::config::{Deployment, OnDemandModel, Relay, RouterConfig};
use crate::control_api::model::{Blocker, Check, Job, LeasesView, Plan, PlanCard, PlanStep};
use crate::control_api::{ApiError, ApiResult, ControlPlane, live};
use crate::guards::{Action, Caller};
use crate::state::AppState;
use crate::switch::callers::CallerResolver;
use crate::switch::ondemand_switch::{InProcess, PickSpec};
use crate::switch::plane::{RouteSink, RouterPlane};
use crate::switch::shim_backend::{ControlShim, ShimState};
use crate::switch::wiring::{self, StoreJobSink};
use crate::switch::{
    CardSpec, HostOps, Orchestrator, Plane, Spec, SwitchRequest, estimate, restart_set, slot_names,
};

type Orch = Orchestrator<Arc<ExecHost>, RouterPlane>;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// What the cards run now, as the last committed switch (or the first observation) left it.
#[derive(Debug, Clone, Default)]
pub struct Running {
    pub layout: Option<String>,
    pub spec: Spec,
    pub ready: Vec<String>,
    /// `spec` came from the host (not a guess): the overlay may rewrite upstreams.
    pub known: bool,
}

/// The data plane's side of a switch: drains withdrawn cards, and on commit records what
/// runs, keeps `router.db`'s active layout, and re-applies the overlay.
pub struct LiveRoutes {
    st: Arc<AppState>,
    store: Arc<Store>,
    server: String,
    running: Arc<Mutex<Running>>,
    /// layoutctl's state dir for the `live.json` / `offline.json` mirror.
    legacy: Option<std::path::PathBuf>,
}

fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Atomic write (temp file + rename in the same directory).
fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("json.router-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

/// layoutctl's `live.json` and `offline.json` for what the router committed. layoutctl's
/// reconcile works from the layout name (it rebuilds the card specs from profiles.json).
pub fn mirror_legacy(dir: &Path, store: &Store, layout: &str, spec: &Spec, slots: &[String]) {
    let live = dir.join("live.json");
    let prev: Value = std::fs::read(&live)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let now = iso_now();
    let cards: serde_json::Map<String, Value> = spec
        .iter()
        .map(|(c, s)| {
            let v = if s.is_off() {
                serde_json::json!({"engine": "off"})
            } else {
                serde_json::json!({"engine": s.engine, "engineProfile": s.profile,
                    "model": s.model})
            };
            (c.clone(), v)
        })
        .collect();
    let offline = store.offline_cards().unwrap_or_default();
    let changed = prev.get("layout").and_then(Value::as_str) != Some(layout);
    let last = if changed {
        serde_json::json!({"job": null, "from": prev.get("layout"), "to": layout, "at": now,
            "by": "abhoth-router"})
    } else {
        prev.get("lastSwitch").cloned().unwrap_or(Value::Null)
    };
    let v = serde_json::json!({
        "layout": layout, "cards": cards,
        "since": if changed { Value::from(now.clone()) } else {
            prev.get("since").cloned().unwrap_or(Value::from(now.clone())) },
        "slots": slots, "offline": offline, "lastSwitch": last, "writer": "abhoth-router"});
    if let Err(e) = write_json(&live, &v) {
        tracing::warn!("legacy mirror {}: {e}", live.display());
    }
    let mut off = serde_json::Map::new();
    for c in &offline {
        let row = store.card_admin(c).ok();
        off.insert(
            c.clone(),
            serde_json::json!({
                "since": row.as_ref().map(|r| r.since / 1000),
                "by": row.as_ref().map(|r| r.by.clone()),
                "reason": row.as_ref().map(|r| r.reason.clone())}),
        );
    }
    let p = dir.join("offline.json");
    if let Err(e) = write_json(&p, &Value::Object(off)) {
        tracing::warn!("legacy mirror {}: {e}", p.display());
    }
}

impl RouteSink for LiveRoutes {
    fn withdraw(&self, cards: &[String]) {
        for c in cards {
            self.st.set_draining(c, true);
        }
    }

    fn commit(&self, layout: Option<&str>, spec: &Spec, ready: &[String]) {
        for c in spec.keys() {
            self.st.set_draining(c, !ready.contains(c));
        }
        {
            let mut r = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if layout.is_some() {
                r.layout = layout.map(String::from);
            }
            r.spec = spec.clone();
            r.ready = ready.to_vec();
            r.known = true;
        }
        if let Some(l) = layout {
            let cards = spec
                .iter()
                .map(|(c, s)| {
                    let v = (!s.is_off()).then(|| schema::CardSpec {
                        engine: s.engine.clone(),
                        profile: s.profile.clone(),
                        model: s.model.clone(),
                    });
                    (c.clone(), v)
                })
                .collect();
            let row = ActiveLayout {
                server: self.server.clone(),
                layout: l.to_string(),
                cards,
                since: now_ms(),
                set_by: "router".into(),
                job_id: None,
            };
            if let Err(e) = self.store.set_active_layout(&row) {
                tracing::warn!("router.db: active layout: {e}");
            }
            if let Some(d) = &self.legacy {
                let mut slots = Vec::new();
                for (c, s) in spec {
                    if !s.is_off() && ready.contains(c) {
                        slots.extend((1..=s.lanes).map(|i| format!("{c}{i}")));
                    }
                }
                mirror_legacy(d, &self.store, l, spec, &slots);
            }
        }
        self.st.reapply_overlay();
    }
}

/// Ask running, idle engines what they are (`/props`, `/v1/models`; never a completion, a
/// load or a switch), when the cache has nothing for their build or it is 6 hours old.
async fn probe_engines(st: Arc<AppState>, c: Arc<ControlConfig>, running: Arc<Mutex<Running>>) {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    loop {
        let spec = running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let snap = st.snapshot();
        let now = chrono::Utc::now().timestamp();
        for (card, s) in &spec.spec {
            if s.is_off() || !spec.ready.contains(card) || st.inflight(card) > 0 {
                continue;
            }
            if !super::caps::needs_probe(&c, &s.engine, now, 6 * 3600) {
                continue;
            }
            let Some(up) = snap.config.upstreams.get(card) else {
                continue;
            };
            let Some(e) = c.engines.get(&s.engine) else {
                continue;
            };
            let how = super::caps::probe_how(&c, &s.engine);
            if let Some(p) = super::caps::probe_engine(
                &st.http,
                &up.url,
                snap.key_for(card),
                how,
                super::caps::stamp(e),
            )
            .await
            {
                tracing::info!(
                    "engine {} on card {card}: probed ({})",
                    s.engine,
                    p.version.as_deref().unwrap_or("no version")
                );
                super::caps::record_probe(&s.engine, p);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    }
}

/// Ask media engines (ComfyUI, Stable Audio) what they are and which files they have (D153):
/// static reads only, never a prompt or a load; a busy ComfyUI is skipped until next time.
async fn probe_media_engines(st: Arc<AppState>, c: Arc<ControlConfig>) {
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    loop {
        for (engine, server, base) in super::caps::media_targets(&c) {
            let Some(e) = c.engines.get(&engine) else {
                continue;
            };
            let how = super::caps::probe_how(&c, &engine);
            match super::caps::probe_media(&st.http, &base, how, super::caps::stamp(e)).await {
                Some(p) => {
                    tracing::info!(
                        "media engine {engine} on {server}: probed ({}, {} folders)",
                        p.version.as_deref().unwrap_or("no version"),
                        p.files.len()
                    );
                    super::caps::record_media_probe(&engine, &server, p);
                }
                None => tracing::debug!("media engine {engine} on {server}: no answer or busy"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    }
}

/// The effective cold policy of a route.
fn cold_of(c: &ControlConfig, r: &schema::Route) -> ColdPolicy {
    r.cold.unwrap_or(c.route_defaults.cold)
}

/// On-demand names from `router.toml`: route name -> the pick (engine, profile, model), with the
/// router's placement per card (D152). A route with `on_demand.pick` is pinned to that engine
/// (and profile) and its `cards`; `layout_from = "auto"` lets the router choose among the
/// model's engines, primary first.
pub fn on_demand_models(c: &ControlConfig) -> IndexMap<String, OnDemandModel> {
    let mut out = IndexMap::new();
    for (name, r) in &c.routes {
        if cold_of(c, r) != ColdPolicy::OnDemand {
            continue;
        }
        let Some(model) = r.targets.iter().find_map(|t| t.model.clone()) else {
            continue;
        };
        let od = r.on_demand.as_ref();
        let auto = od.is_none_or(|o| o.layout_from == "auto");
        let pick = od.and_then(|o| o.pick.as_ref());
        if pick.is_none() && !auto {
            continue;
        }
        let picks = super::inventory::route_picks(c, &model, od);
        let per_card: IndexMap<String, crate::config::CardPick> = picks
            .iter()
            .map(|(card, (engine, profile, rank))| {
                (
                    card.clone(),
                    crate::config::CardPick {
                        engine: engine.clone(),
                        engine_profile: profile.clone(),
                        rank: *rank,
                    },
                )
            })
            .collect();
        let (engine, profile) = match pick {
            Some(pk) => (
                pk.engine.clone(),
                pk.profile.clone().or_else(|| {
                    c.engines
                        .get(&pk.engine)
                        .and_then(|e| e.profiles.keys().next().cloned())
                }),
            ),
            None => match per_card.values().min_by_key(|p| p.rank) {
                Some(p) => (p.engine.clone(), Some(p.engine_profile.clone())),
                // nowhere to run it: still a name (decide() says why), never a switch
                None => (String::new(), None),
            },
        };
        out.insert(
            name.clone(),
            OnDemandModel {
                model,
                engine,
                engine_profile: profile.unwrap_or_default(),
                served_id: String::new(),
                per_card,
            },
        );
    }
    out
}

/// The overlay: what `router.toml` and the running cards change in a loaded `router.json`.
pub fn overlay(c: &ControlConfig, server: &str, run: &Running, rc: &mut RouterConfig) {
    let od = &mut rc.on_demand;
    wiring::apply_on_demand(c, server, od);
    let models = on_demand_models(c);
    od.enabled = !models.is_empty();
    od.dry_run = false;
    // no profiles API and no layoutctl state after the cutover: the router is the source
    od.profiles_api = String::new();
    od.live_file = None;
    od.models = models.clone();
    // names that run nowhere are on-demand (relay) deployments; the relay is never used
    // while on-demand is on (the router answers them), but validation wants one
    for name in models.keys() {
        if !rc.deployments.iter().any(|d| &d.name == name) {
            rc.deployments.push(Deployment {
                name: name.clone(),
                served_id: name.clone(),
                relay: true,
                ..Deployment::default()
            });
        }
    }
    if rc.relay.is_none() && rc.deployments.iter().any(|d| d.relay) {
        rc.relay = Some(Relay {
            url: "http://127.0.0.1:9/".into(),
            key: None,
        });
    }
    if !run.known {
        return;
    }
    for (card, s) in &run.spec {
        let Some(up) = rc.upstreams.get_mut(card) else {
            continue;
        };
        if s.is_off() {
            up.online = false;
            up.catalog_model = None;
            continue;
        }
        up.online = run.ready.contains(card);
        up.catalog_model = Some(s.model.clone());
        let ss = schema::CardSpec {
            engine: s.engine.clone(),
            profile: s.profile.clone(),
            model: s.model.clone(),
        };
        if let Ok(r) = super::resolve::resolve(c, card, &ss) {
            if !up.served_id.is_empty() {
                up.served_id = r.served_id.clone();
            }
            up.url = with_port(&up.url, r.port);
        }
    }
    // Card names follow LiteLLM's: every alias goes to the cards that serve now.
    let serving: Vec<String> = rc
        .upstreams
        .iter()
        .filter(|(_, u)| u.online)
        .map(|(k, _)| k.clone())
        .collect();
    for d in rc.deployments.iter_mut().filter(|d| !d.relay) {
        let keep: Vec<String> = d
            .cards
            .iter()
            .filter(|c| serving.contains(c))
            .cloned()
            .collect();
        if !keep.is_empty() {
            d.cards = keep;
        }
    }
}

/// `url` with its port replaced by `port` (0 = unchanged).
fn with_port(url: &str, port: u16) -> String {
    if port == 0 {
        return url.to_string();
    }
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => hostport,
    };
    format!("{scheme}://{host}:{port}{path}")
}

/// The admin API's actions over the orchestrator.
pub struct OrchActions {
    pub c: Arc<ControlConfig>,
    pub server: String,
    pub orch: Arc<Orch>,
    pub store: Arc<Store>,
    pub running: Arc<Mutex<Running>>,
    pub st: Arc<AppState>,
}

impl OrchActions {
    fn target(&self, server: &str, layout: &str) -> ApiResult<Spec> {
        if server != self.server {
            return Err(ApiError::new(
                404,
                format!("server {server} is not switched here"),
            ));
        }
        let offline = self.store.offline_cards().unwrap_or_default();
        wiring::target_spec(&self.c, server, layout, &offline)
            .ok_or_else(|| ApiError::new(404, format!("no layout {layout}")))
    }

    fn now(&self) -> Running {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn show(s: Option<&CardSpec>) -> String {
    match s {
        Some(s) if !s.is_off() => format!("{}/{}/{}", s.engine, s.profile, s.model),
        _ => "off".into(),
    }
}

#[async_trait::async_trait]
impl live::Actions for OrchActions {
    fn leases(&self) -> LeasesView {
        LeasesView::default()
    }

    async fn plan(&self, server: &str, layout: &str) -> ApiResult<Plan> {
        let target = self.target(server, layout)?;
        let now = self.now();
        let cfg = &self.orch.cfg;
        let restart = restart_set(cfg, &now.spec, &target);
        let est = estimate(cfg, &target, &restart);
        let mut blockers: Vec<Blocker> = self
            .orch
            .plane
            .blocks(Action::Switch, &restart, &Caller::default(), layout)
            .into_iter()
            .map(|b| Blocker {
                kind: "guard".into(),
                id: b.source.clone(),
                reason: b.reason.clone(),
                holder: None,
                retry_seconds: b.retry_seconds,
            })
            .collect();
        for (k, why) in self.orch.plane.held_on(&restart) {
            blockers.push(Blocker {
                kind: "lease".into(),
                id: k,
                reason: why,
                holder: None,
                retry_seconds: 30,
            });
        }
        if self.orch.busy() {
            blockers.push(Blocker {
                kind: "job".into(),
                id: "running".into(),
                reason: "a switch is running".into(),
                holder: None,
                retry_seconds: 30,
            });
        }
        let pre = if restart.is_empty() {
            Ok(())
        } else {
            self.orch.host.preconditions(&target).await
        };
        let preconditions = vec![Check {
            name: "preconditions".into(),
            status: if pre.is_ok() { "pass" } else { "fail" }.into(),
            reason: pre.as_ref().err().cloned(),
        }];
        let cards: Vec<PlanCard> = target
            .iter()
            .map(|(card, t)| {
                let before = now.spec.get(card);
                let action = if restart.contains(card) {
                    if t.is_off() {
                        "stop"
                    } else if before.is_none_or(|b| b.is_off()) {
                        "start"
                    } else {
                        "restart"
                    }
                } else {
                    "keep"
                };
                PlanCard {
                    card: card.clone(),
                    before: show(before),
                    after: show(Some(t)),
                    action: action.into(),
                    reason: String::new(),
                }
            })
            .collect();
        let steps = restart
            .iter()
            .enumerate()
            .map(|(i, c)| PlanStep {
                n: i as u32 + 1,
                title: format!("restart card {c}"),
                detail: show(target.get(c)),
                estimate_seconds: 0,
            })
            .collect();
        let runnable = blockers.is_empty() && pre.is_ok();
        Ok(Plan {
            server: server.into(),
            from: now.layout.clone(),
            to: layout.into(),
            cards,
            steps,
            preconditions,
            blockers,
            slots_before: slot_names(cfg, &now.spec),
            slots_after: slot_names(cfg, &target),
            estimate_seconds: est,
            runnable,
            note: (restart.is_empty() && now.known).then(|| "already live".to_string()),
            ..Plan::default()
        })
    }

    async fn switch(&self, server: &str, layout: &str, who: &str) -> ApiResult<Job> {
        let target = self.target(server, layout)?;
        if self.orch.busy() {
            return Err(ApiError::new(409, "a switch is already running"));
        }
        let now = self.now();
        let req = SwitchRequest {
            layout: layout.into(),
            target,
            active: now.layout.clone(),
            requested_by: who.chars().take(80).collect(),
            reason: format!("admin switch by {who}"),
            caller: Caller {
                principal: Some(who.to_string()),
                grants: Vec::new(),
            },
            action: Action::Switch,
        };
        let orch = self.orch.clone();
        tokio::spawn(async move {
            let r = orch.switch(req).await;
            tracing::info!(job = %r.id, state = %r.state, "admin switch ended");
        });
        Ok(Job {
            id: String::new(),
            server: server.into(),
            kind: "switch".into(),
            from: now.layout,
            target: layout.into(),
            requested_by: who.into(),
            state: "queued".into(),
            started: now_ms(),
            ..Job::default()
        })
    }

    async fn cancel_job(&self, _id: &str, _who: &str) -> ApiResult<Job> {
        Err(ApiError::new(
            501,
            "a running switch is not cancelled; it ends or rolls back on its own",
        ))
    }

    /// A card taken offline (its engine stopped, kept off by every switch and by reconcile) or
    /// back online (the active layout's engine started on it again).
    async fn set_card(&self, card: &str, state: &str, reason: &str, who: &str) -> ApiResult<Job> {
        let want = match state {
            "online" => super::store::CardAdmin::Online,
            "offline" => super::store::CardAdmin::Offline,
            _ => {
                return Err(ApiError::new(
                    400,
                    format!("state {state}: online or offline"),
                ));
            }
        };
        let Some(cd) = self.c.cards.get(card).filter(|x| x.server == self.server) else {
            return Err(ApiError::new(404, format!("unknown card {card}")));
        };
        if !cd.managed {
            return Err(ApiError::new(409, format!("card {card} is not managed")));
        }
        let now = self.now();
        let job = |st: &str, err: Option<String>| Job {
            id: String::new(),
            server: self.server.clone(),
            kind: format!("card-{state}"),
            from: now.layout.clone(),
            target: card.into(),
            requested_by: who.into(),
            state: st.into(),
            started: now_ms(),
            error: err,
            ..Job::default()
        };
        let current = self
            .store
            .card_admin(card)
            .map(|r| r.state)
            .unwrap_or(super::store::CardAdmin::Online);
        let Some(layout) = now.layout.clone() else {
            return Err(ApiError::new(409, "no active layout yet"));
        };
        if self.orch.busy() {
            return Err(ApiError::new(409, "a switch is already running"));
        }
        // What the active layout becomes with this card's new state.
        let mut off = self.store.offline_cards().unwrap_or_default();
        off.retain(|c| c != card);
        if want != super::store::CardAdmin::Online {
            off.push(card.into());
        }
        let target = wiring::target_spec(&self.c, &self.server, &layout, &off)
            .ok_or_else(|| ApiError::new(404, format!("no layout {layout}")))?;
        let restart = restart_set(&self.orch.cfg, &now.spec, &target);
        if current == want && restart.is_empty() {
            return Ok(job("done", None));
        }
        let action = if want == super::store::CardAdmin::Online {
            Action::Online
        } else {
            Action::Offline
        };
        let caller = Caller {
            principal: Some(who.to_string()),
            grants: Vec::new(),
        };
        let mut why: Vec<String> = self
            .orch
            .plane
            .blocks(action, &restart, &caller, &layout)
            .into_iter()
            .map(|b| format!("{}: {}", b.source, b.reason))
            .collect();
        why.extend(
            self.orch
                .plane
                .held_on(&restart)
                .into_iter()
                .map(|(k, w)| format!("lease {k}: {w}")),
        );
        if !why.is_empty() {
            return Err(ApiError::new(409, why.join("; ")));
        }
        self.store
            .set_card_admin(&super::store::CardAdminRow {
                card: card.into(),
                state: want,
                by: who.chars().take(80).collect(),
                reason: reason.chars().take(200).collect(),
                since: now_ms(),
            })
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        if restart.is_empty() {
            return Ok(job("done", None));
        }
        let req = SwitchRequest {
            layout: layout.clone(),
            target,
            active: Some(layout),
            requested_by: who.chars().take(80).collect(),
            reason: format!("card {card} {state} by {who}"),
            caller,
            action,
        };
        let orch = self.orch.clone();
        tokio::spawn(async move {
            let r = orch.switch(req).await;
            tracing::info!(job = %r.id, state = %r.state, "card change ended");
        });
        Ok(job("queued", None))
    }

    async fn release_lease(&self, key: &str, _who: &str) -> ApiResult<()> {
        let mut b = serde_json::Map::new();
        b.insert("key".into(), Value::from(key));
        let (status, v) = self.st.ext.registry.release(&self.st.snapshot(), &b);
        if status < 300 {
            Ok(())
        } else {
            Err(ApiError::new(status, v.to_string()))
        }
    }
}

/// Reconcile's `ensure`: one switch to the active layout through the orchestrator.
struct OrchEnsure {
    c: Arc<ControlConfig>,
    orch: Arc<Orch>,
    store: Arc<Store>,
}

#[async_trait::async_trait]
impl super::reconcile::Ensure for OrchEnsure {
    async fn ensure(&self, server: &str, layout: &str) -> Result<(), String> {
        let offline = self.store.offline_cards().unwrap_or_default();
        let target = wiring::target_spec(&self.c, server, layout, &offline)
            .ok_or_else(|| format!("no layout {layout}"))?;
        let r = self
            .orch
            .switch(SwitchRequest {
                layout: layout.into(),
                target,
                active: Some(layout.into()),
                requested_by: "router-reconcile".into(),
                reason: "reconcile: an engine of the active layout is missing".into(),
                caller: Caller::default(),
                action: Action::Ensure,
            })
            .await;
        if r.state == "done" {
            Ok(())
        } else {
            Err(format!(
                "ensure ended {}: {}",
                r.state,
                r.error.unwrap_or_default()
            ))
        }
    }
}

/// Delegates to the installed control plane's `Access` (the shim's caller resolver).
struct ViaControl(Arc<ControlPlane>);

impl CallerResolver for ViaControl {
    fn resolve<'a>(
        &'a self,
        headers: &'a axum::http::HeaderMap,
        peer: Option<std::net::IpAddr>,
        method: &'a str,
        path: &'a str,
    ) -> crate::switch::callers::ResolveFuture<'a> {
        self.0.access.resolve(headers, peer, method, path)
    }
}

/// `router.db` as found at startup: the active layout (when none is recorded yet) and an
/// adopted instance per card, so the admin API and the `:8098` shim show what runs.
fn seed_store(store: &Store, server: &str, run: &Running) {
    let now = chrono::Utc::now().timestamp();
    let schema_of = |s: &CardSpec| schema::CardSpec {
        engine: s.engine.clone(),
        profile: s.profile.clone(),
        model: s.model.clone(),
    };
    if store.active_layout(server).ok().flatten().is_none()
        && let Some(l) = &run.layout
    {
        let cards = run
            .spec
            .iter()
            .map(|(c, s)| (c.clone(), (!s.is_off()).then(|| schema_of(s))))
            .collect();
        let row = ActiveLayout {
            server: server.into(),
            layout: l.clone(),
            cards,
            since: now_ms(),
            set_by: "router (adopted at start)".into(),
            job_id: None,
        };
        if let Err(e) = store.set_active_layout(&row) {
            tracing::warn!("router.db: active layout: {e}");
        }
    }
    for (card, s) in &run.spec {
        let i = if s.is_off() {
            super::lifecycle::Instance::off(server, card)
        } else {
            super::lifecycle::Instance {
                server: server.into(),
                card: card.clone(),
                engine: s.engine.clone(),
                spec: Some(schema_of(s)),
                state: super::lifecycle::InstanceState::Ready,
                since: now,
                detail: "adopted".into(),
                ..Default::default()
            }
        };
        if let Err(e) = store.put_instance(&i) {
            tracing::warn!("router.db: instance {card}: {e}");
        }
    }
}

/// `{var:x}` from the server's vars.
fn expand_vars(c: &ControlConfig, server: &str, t: &str) -> String {
    let mut s = t.to_string();
    if let Some(sv) = c.servers.get(server) {
        for (k, v) in &sv.vars {
            s = s.replace(&format!("{{var:{k}}}"), v);
        }
    }
    s
}

fn truthy(x: &Value) -> bool {
    match x {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}

/// One reading of a detector guard (`process`, `json_file`, `file_exists`) through the
/// host's builtins: `Some(detail)` when active; `Err` = no reading (the guard goes stale and
/// counts as active).
async fn read_guard(
    c: &ControlConfig,
    server: &str,
    exec: &dyn super::exec::Executor,
    g: &schema::Guard,
) -> Result<Option<String>, String> {
    let t = Duration::from_secs(10);
    let req = |name: &str, args: Value| super::exec::ExecRequest {
        server: server.into(),
        op: super::exec::Op::Builtin {
            name: name.into(),
            args,
        },
        timeout: t,
        root: false,
        stdin: None,
    };
    let cancel = Cancel::new();
    let path = expand_vars(c, server, g.path.as_deref().unwrap_or_default());
    match g.kind {
        schema::GuardKind::FileExists => {
            let o = exec
                .run(
                    req("file-exists", serde_json::json!({"path": path})),
                    &cancel,
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(o.ok().then(|| format!("{path} exists")))
        }
        schema::GuardKind::JsonFile => {
            let o = exec
                .run(req("read-json", serde_json::json!({"path": path})), &cancel)
                .await
                .map_err(|e| e.to_string())?;
            if o.exit_code == Some(1) {
                return Ok(None); // no file: nothing queued
            }
            if !o.ok() {
                return Err(o.summary());
            }
            let v: Value = serde_json::from_str(&o.stdout).map_err(|e| e.to_string())?;
            Ok(g.active_when
                .iter()
                .find(|k| v.get(k.as_str()).is_some_and(truthy))
                .map(|k| format!("{path}: {k}")))
        }
        schema::GuardKind::Process => {
            let Some(m) = &g.matcher else {
                return Ok(None);
            };
            let args = serde_json::to_value(m).map_err(|e| e.to_string())?;
            let o = exec
                .run(req("proc-scan", args), &cancel)
                .await
                .map_err(|e| e.to_string())?;
            if !o.ok() {
                return Err(o.summary());
            }
            let found: Vec<(u32, String)> =
                serde_json::from_str(&o.stdout).map_err(|e| e.to_string())?;
            Ok((!found.is_empty()).then(|| {
                let labels: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
                format!("running: {}", labels.join(", "))
            }))
        }
        _ => Ok(None),
    }
}

/// Feed the detector guards every 15 s (a guard not read for `stale_seconds` counts as
/// active, so a broken channel blocks switches rather than allowing them).
async fn poll_guards(
    c: Arc<ControlConfig>,
    server: String,
    exec: Arc<dyn super::exec::Executor>,
    board: Arc<crate::guards::GuardBoard>,
) {
    loop {
        for (id, g) in &c.guards {
            if g.server.as_deref().is_some_and(|s| s != server)
                || !matches!(
                    g.kind,
                    schema::GuardKind::FileExists
                        | schema::GuardKind::JsonFile
                        | schema::GuardKind::Process
                )
            {
                continue;
            }
            match read_guard(&c, &server, exec.as_ref(), g).await {
                Ok(active) => board.report(id, crate::proxy::now_epoch(), active),
                Err(e) => tracing::warn!("guard {id}: no reading: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

/// What [`start`] set up.
pub struct Started {
    pub message: String,
    /// The `:8098` profiles shim (served when the router is given `--profiles-port`).
    pub shim: Option<Arc<ShimState>>,
}

fn active_of(store: &Store, c: &ControlConfig, server: &str) -> Option<String> {
    store
        .active_layout(server)
        .ok()
        .flatten()
        .map(|a| a.layout)
        .or_else(|| {
            c.servers
                .get(server)
                .and_then(|s| s.state.as_ref())
                .and_then(|s| s.initial_layout.clone())
        })
}

/// Start the control plane from `main` (router.toml). Off: says so and changes nothing.
pub async fn start(st: &Arc<AppState>, main: &Path) -> Result<Started, String> {
    // D152: write the inventory defaults explicitly (backup first; a failure changes nothing).
    let migrated = super::inventory::migrate_file(main);
    if !migrated.is_empty() {
        eprintln!("abhoth router: {migrated}");
    }
    let l = super::load::load(main).map_err(|issues| {
        issues
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let c = Arc::new(l.config);
    // Bridges (ROUTER-MEDIA-PLAN.md 13): `/bridge/ws` and `/admin/api/bridges`, with the control
    // plane on or off.
    {
        let hub = crate::bridges::Hub::new(std::path::Path::new(&c.router.state_dir), c.clone());
        crate::bridges::install(hub.clone());
        let _ = st.bridges.set(hub);
    }
    if !c.control.enabled {
        // The admin pages still work (D151): config forms, admin keys; nothing switches.
        let message = match crate::control_api::install_admin_only(st, main, &c) {
            Ok(m) => m,
            Err(e) => format!("control plane off; admin pages off ({e})"),
        };
        return Ok(Started {
            message,
            shim: None,
        });
    }
    let state_dir = std::path::PathBuf::from(&c.router.state_dir);
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let store = Arc::new(Store::open(&state_dir.join("router.db"))?);
    let local_keys = crate::access::LocalKeys::open(store.clone())?;
    match super::reconcile::recover_jobs(&store) {
        Ok(ids) if !ids.is_empty() => {
            tracing::warn!("router.db: jobs left running by the last process: {ids:?}")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("router.db: recovering jobs: {e}"),
    }
    st.ext.registry.attach_store(store.clone())?;
    st.ext
        .ondemand
        .limiter
        .attach_store(store.clone(), crate::proxy::now_epoch())?;

    // Access (keys) and the admin API come first: the gate and the shim resolve callers by it.
    let token = c
        .access
        .accounts_token
        .as_deref()
        .and_then(|r| crate::control_api::resolve_secret(&c, r));
    if c.access.accounts_token.is_some() && token.is_none() {
        return Err("access.accounts_token: the secret could not be read".into());
    }

    // The switching server: the first one with managed cards.
    let server = c
        .servers
        .keys()
        .find(|s| !c.managed_cards(s).is_empty())
        .cloned()
        .ok_or("no server with managed cards")?;
    let active = {
        let (store, c, server) = (store.clone(), c.clone(), server.clone());
        Arc::new(move || active_of(&store, &c, &server))
    };
    let ServerControl {
        exec, host, wake, ..
    } = for_server(c.clone(), &server, &state_dir, false, active)?;
    st.wake.set(wake).ok();

    // What runs now, from the host (adopted as is: nothing is restarted here).
    let mut running = Running {
        layout: active_of(&store, &c, &server),
        ..Running::default()
    };
    match host.observe().await {
        Ok(o) => {
            running.ready = o
                .spec
                .iter()
                .filter(|(_, s)| !s.is_off())
                .map(|(k, _)| k.clone())
                .collect();
            running.spec = o.spec;
            running.known = true;
            seed_store(&store, &server, &running);
        }
        Err(e) => tracing::warn!("control: first look at {server} failed: {}", e.0),
    }
    let running = Arc::new(Mutex::new(running));

    let ov = {
        let (c, server, running) = (c.clone(), server.clone(), running.clone());
        Arc::new(move |rc: &mut RouterConfig| {
            let r = running.lock().unwrap_or_else(|e| e.into_inner()).clone();
            overlay(&c, &server, &r, rc);
        })
    };
    st.overlay.set(ov).ok();
    st.reapply_overlay();

    let routes = Arc::new(LiveRoutes {
        st: st.clone(),
        store: store.clone(),
        server: server.clone(),
        running: running.clone(),
        legacy: c
            .router
            .legacy_state_dir
            .as_ref()
            .map(std::path::PathBuf::from),
    });
    // The Supervisor's calendar (flag off by default): installed before the board, which adds it.
    let calendar = crate::calendar::start(&c, main, &l.origin, store.clone())?;
    let (board, _cal) = wiring::guard_board(&c, &server);
    let board = Arc::new(board);
    tokio::spawn(poll_guards(
        c.clone(),
        server.clone(),
        exec.clone(),
        board.clone(),
    ));
    let plane = RouterPlane::new(st.clone(), board, routes);
    let jobs = Arc::new(StoreJobSink::new(store.clone(), &server, None));
    let orch = Arc::new(Orchestrator::new(
        wiring::switch_config(&c, &server),
        host.clone(),
        plane,
        jobs,
    ));

    let actions = Arc::new(OrchActions {
        c: c.clone(),
        server: server.clone(),
        orch: orch.clone(),
        store: store.clone(),
        running: running.clone(),
        st: st.clone(),
    });
    let backend = live::ConfigBackend::with_actions(main, &state_dir, store.clone(), actions);
    // D152: inventory, Hugging Face, downloads; engine probes (static reads of idle engines).
    let inv = crate::control::downloads::install(main, &state_dir, backend.clone());
    inv.note_started(&c);
    tokio::spawn(probe_engines(st.clone(), c.clone(), running.clone()));
    tokio::spawn(probe_media_engines(st.clone(), c.clone()));
    // D153 phase 5: standing engines, kept up through each server's docker_api
    // (it does nothing until router.toml has one; a reload that adds one is picked up)
    tokio::spawn(super::standing::run(
        st.clone(),
        c.clone(),
        state_dir.clone(),
    ));
    crate::control_api::bootstrap_hint(&local_keys, &c, main);
    let cp = Arc::new(ControlPlane {
        access: Access::new(c.access.clone(), token)
            .with_local_keys(local_keys)
            .with_sso(&state_dir),
        backend: backend.clone(),
    });
    st.control
        .set(cp.clone())
        .map_err(|_| "the control plane is already installed".to_string())?;

    // On-demand through the orchestrator.
    let pick: PickSpec = {
        let c = c.clone();
        Arc::new(move |m: &OnDemandModel| {
            let e = c.engines.get(&m.engine)?;
            let profile = if m.engine_profile.is_empty() {
                e.profiles.keys().next()?.clone()
            } else {
                m.engine_profile.clone()
            };
            if !e.profiles.contains_key(&profile) || !c.models.contains_key(&m.model) {
                return None;
            }
            Some(wiring::spec_of(
                &c,
                &schema::CardSpec {
                    engine: m.engine.clone(),
                    profile,
                    model: m.model.clone(),
                },
            ))
        })
    };
    let (now_layout, now_spec) = {
        let r = running.lock().unwrap_or_else(|e| e.into_inner());
        (r.layout.clone(), r.spec.clone())
    };
    let switcher = Arc::new(InProcess::new(orch.clone(), pick, now_layout, now_spec));
    st.ext.ondemand.switcher.set(switcher).ok();

    // Reconcile: wait for the host, then every interval (adopt; ensure only in act mode).
    {
        let (c, server, host, store, exec) = (
            c.clone(),
            server.clone(),
            host.clone(),
            store.clone(),
            exec.clone(),
        );
        let ensure = OrchEnsure {
            c: c.clone(),
            orch: orch.clone(),
            store: store.clone(),
        };
        tokio::spawn(async move {
            let cancel = Cancel::new();
            if let Err(e) = super::reconcile::wait_ready(
                &c,
                &server,
                exec.as_ref(),
                Duration::from_secs(10),
                &cancel,
            )
            .await
            {
                tracing::warn!("control: {server} not ready: {e}");
            }
            let boot = format!("boot-{}", now_ms());
            let every = Duration::from_secs(c.control.reconcile.interval_seconds.max(10));
            loop {
                let ens: Option<&dyn super::reconcile::Ensure> =
                    (c.control.mode == ControlMode::Act).then_some(&ensure as _);
                match super::reconcile::reconcile_once(&c, &server, &host, &store, ens, &boot).await
                {
                    Ok(r) if !r.actions.is_empty() => {
                        tracing::info!("reconcile {server}: {}", r.actions.join("; "))
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("reconcile {server}: {e}"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    let shim = Arc::new(ShimState {
        backend: Arc::new(ControlShim {
            backend,
            server: server.clone(),
            config: Some(c.clone()),
        }),
        callers: Arc::new(ViaControl(cp)),
    });
    let n = running.lock().unwrap_or_else(|e| e.into_inner()).spec.len();
    Ok(Started {
        message: format!(
            "control plane on (rev {}, server {server}, {n} cards seen, mode {:?}){calendar}",
            l.rev, c.control.mode
        ),
        shim: Some(shim),
    })
}

/// Same cards and models as at `known` start (tests).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_replaced() {
        assert_eq!(with_port("http://h:8094", 8096), "http://h:8096");
        assert_eq!(with_port("http://h:8094/v1", 9), "http://h:9/v1");
        assert_eq!(with_port("http://h", 80), "http://h:80");
        assert_eq!(with_port("http://h:1", 0), "http://h:1");
    }

    #[test]
    fn running_cards_drive_upstreams_and_aliases() {
        let c = crate::control::validate::tests::sample();
        let server = c.servers.keys().next().unwrap().clone();
        let mut rc = RouterConfig::default();
        for (card, port) in [("A", 8094), ("B", 8096)] {
            rc.upstreams.insert(
                card.into(),
                serde_json::from_value(serde_json::json!({
                    "url": format!("http://h:{port}"), "engine": "ninfer", "lanes": 4
                }))
                .unwrap(),
            );
        }
        rc.deployments.push(Deployment {
            name: "main".into(),
            served_id: "m".into(),
            cards: vec!["A".into(), "B".into()],
            ..Deployment::default()
        });
        let mut spec = Spec::new();
        spec.insert("A".into(), CardSpec::new("ninfer", "p", "m", 4));
        spec.insert("B".into(), CardSpec::off());
        let run = Running {
            layout: Some("L".into()),
            spec,
            ready: vec!["A".into()],
            known: true,
        };
        overlay(&c, &server, &run, &mut rc);
        assert!(!rc.upstreams["B"].online);
        assert_eq!(rc.upstreams["A"].catalog_model.as_deref(), Some("m"));
        assert_eq!(rc.deployments[0].cards, ["A"]);
        assert!(!rc.on_demand.dry_run);
        assert!(rc.on_demand.profiles_api.is_empty());
        crate::config::validate(&rc).unwrap();
    }
}
