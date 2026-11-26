//! The profiles shim answered from the control plane's admin backend (🟨C's
//! [`ControlBackend`]), plus the listener that serves it (ROUTER-CONFIG.md 8.4).
//!
//! The JSON keeps `profiles_api.py`'s shapes (`/live`, `/cards`, `/jobs`, `/jobs/<id>`,
//! `/plan`, `POST /switch`, `POST /cards/X/offline|online`), recorded read-only from the live
//! API. Profiles (`/profiles`, `/compose`, `/profiles/save|delete`) are config edits now: the
//! shim answers 503/501 and points at the admin UI.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::callers::CallerResolver;
use super::profiles_shim::{ShimBackend, handle};
use crate::control_api::ControlBackend;
use crate::control_api::model::{Job, StateView};
use crate::guards::Caller;

pub struct ControlShim {
    pub backend: Arc<dyn ControlBackend>,
    pub server: String,
}

fn iso_ms(ms: i64) -> Value {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .unwrap_or(Value::Null)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A job in layoutctl's record shape.
pub fn job_record(j: &Job) -> Value {
    let mut t = 0.0;
    let steps: Vec<Value> = j
        .steps
        .iter()
        .map(|s| {
            t += s.seconds;
            json!({"t": (t * 10.0).round() / 10.0, "msg": s.title})
        })
        .collect();
    json!({"id": j.id, "layout": j.target, "from": j.from, "profile": null,
        "requestedBy": j.requested_by, "state": j.state, "started": iso_ms(j.started),
        "ended": j.ended.map(iso_ms), "seconds": j.ended.map(|e| (e - j.started) as f64 / 1000.0),
        "error": j.error, "steps": steps})
}

/// A `history.jsonl` line.
pub fn history_line(j: &Job) -> Value {
    let r = job_record(j);
    json!({"id": r["id"], "layout": r["layout"], "from": r["from"], "profile": null,
        "requestedBy": r["requestedBy"], "state": r["state"], "seconds": r["seconds"],
        "estimateSeconds": null, "error": r["error"], "started": r["started"], "ended": r["ended"]})
}

fn api(e: crate::control_api::ApiError) -> (u16, Value) {
    (e.status, json!({"error": e.message}))
}

impl ControlShim {
    fn st(&self) -> StateView {
        self.backend.state()
    }

    fn job_now(st: &StateView) -> Value {
        match &st.job {
            Some(j) => json!({"id": j.id, "layout": j.target, "profile": null,
                "state": j.state, "requestedBy": null, "created": iso_ms(j.started)}),
            None => Value::Null,
        }
    }

    fn guard_on(st: &StateView, id_has: &str) -> bool {
        st.guards.iter().any(|g| g.active && g.id.contains(id_has))
    }

    fn who(caller: &Caller, by: &str) -> String {
        format!(
            "{} ({by})",
            caller.principal.as_deref().unwrap_or("anonymous")
        )
    }
}

#[async_trait::async_trait]
impl ShimBackend for ControlShim {
    fn profiles(&self) -> Option<Value> {
        None
    }

    fn live(&self) -> Value {
        let st = self.st();
        let Some(srv) = st.servers.iter().find(|s| s.id == self.server) else {
            return json!({"layout": null, "cards": {}, "job": null, "checked": now_iso()});
        };
        let mut cards = Map::new();
        let mut offline = Vec::new();
        for c in srv.cards.iter().filter(|c| c.managed) {
            let off = c.admin_state == "offline";
            if off {
                offline.push(c.id.clone());
            }
            let v = match (&c.engine, off) {
                (Some(e), false) => json!({"engine": e.engine, "slots": c.lanes, "model": e.model,
                    "endpoint": null, "healthy": e.healthy, "detail": e.state, "gpu": c.label,
                    "offline": false}),
                _ => json!({"engine": "off", "endpoint": null, "healthy": true,
                    "detail": if off { "offline" } else { "off" }, "gpu": c.label,
                    "offline": off}),
            };
            cards.insert(c.id.clone(), v);
        }
        let healthy = cards
            .values()
            .filter(|v| v["engine"] != "off")
            .all(|v| v["healthy"] == true);
        let leases = self.backend.leases();
        let held: Vec<Value> = leases
            .leases
            .iter()
            .filter(|l| l.kind != "guard")
            .map(|l| json!({"slot": l.slot, "who": l.holder}))
            .collect();
        let layout = srv.active_layout.clone();
        let title = st
            .layouts
            .iter()
            .find(|l| Some(&l.id) == layout.as_ref())
            .map(|l| l.title.clone());
        json!({"layout": layout, "profiles": [], "title": title, "cards": cards,
            "since": null, "healthy": healthy, "leases": held,
            "pigSlots": {"slots": leases.leases.iter().map(|l| l.slot.clone()).collect::<Vec<_>>(),
                         "frozen": leases.frozen, "layout": srv.active_layout},
            "job": Self::job_now(&st), "offline": offline,
            "quietWindow": Self::guard_on(&st, "quiet"), "musicMode": Self::guard_on(&st, "music"),
            "checked": now_iso()})
    }

    fn cards(&self) -> Value {
        let st = self.st();
        let srv = st.servers.iter().find(|s| s.id == self.server);
        let mut out = Map::new();
        for c in srv
            .iter()
            .flat_map(|s| s.cards.iter())
            .filter(|c| c.managed)
        {
            let off = c.admin_state == "offline";
            let e = c.engine.as_ref();
            out.insert(
                c.id.clone(),
                json!({"card": c.id, "gpu": c.label, "offline": off,
                    "engine": if off { "off" } else { e.map_or("off", |e| e.engine.as_str()) },
                    "layoutEngine": e.map(|e| e.engine.clone()),
                    "model": e.map(|e| e.model.clone()), "slots": c.lanes,
                    "healthy": e.is_none_or(|e| e.healthy),
                    "detail": e.map_or(c.admin_state.clone(), |e| e.state.clone()),
                    "since": if off { e.map(|e| iso_ms(e.since)) } else { None }}),
            );
        }
        json!({"layout": srv.and_then(|s| s.active_layout.clone()), "cards": out,
            "job": Self::job_now(&st), "checked": now_iso()})
    }

    fn history(&self, limit: usize) -> Vec<Value> {
        self.backend
            .jobs(limit)
            .iter()
            .filter(|j| j.server == self.server)
            .map(history_line)
            .collect()
    }

    fn job(&self, id: &str) -> Option<Value> {
        self.backend.job(id).map(|j| job_record(&j))
    }

    fn plan(&self, _layout: Option<&str>, _profile: Option<&str>) -> (u16, Value) {
        // the admin API's plan is async; the shim's GET /plan answers from the layout list
        (
            501,
            json!({"error": "plans are in the admin UI now (Router > Layouts > Plan)"}),
        )
    }

    fn ondemand(&self) -> Value {
        json!({"enabled": self.backend.routes().iter().any(|r| r.cold == "on_demand"),
            "see": "/admin/api/state"})
    }

    async fn switch(
        &self,
        layout: Option<&str>,
        profile: Option<&str>,
        requested_by: &str,
        _reason: &str,
        caller: &Caller,
    ) -> (u16, Value) {
        let Some(layout) = layout else {
            return (
                400,
                json!({"error": format!("profiles are layouts now: switch by layout, not profile {profile:?}")}),
            );
        };
        let live = self
            .st()
            .servers
            .iter()
            .find(|s| s.id == self.server)
            .and_then(|s| s.active_layout.clone());
        if live.as_deref() == Some(layout) {
            return (
                200,
                json!({"state": "done", "already": true, "layout": layout, "live": live}),
            );
        }
        match self
            .backend
            .switch(&self.server, layout, &Self::who(caller, requested_by))
            .await
        {
            Ok(j) => (
                202,
                json!({"id": j.id, "state": "queued", "layout": layout, "estimateSeconds": null}),
            ),
            Err(e) if e.status == 409 => {
                (409, json!({"error": e.message, "refusals": [e.message]}))
            }
            Err(e) => api(e),
        }
    }

    async fn card_action(
        &self,
        card: &str,
        online: bool,
        requested_by: &str,
        reason: &str,
        dry_run: bool,
        caller: &Caller,
    ) -> (u16, Value) {
        let action = if online { "online" } else { "offline" };
        if dry_run {
            return (
                200,
                json!({"dryRun": true, "card": card, "action": action, "wouldQueue": true}),
            );
        }
        match self
            .backend
            .set_card(card, action, reason, &Self::who(caller, requested_by))
            .await
        {
            Ok(j) => (
                202,
                json!({"id": j.id, "state": "queued", "card": card, "action": action}),
            ),
            Err(e) => api(e),
        }
    }

    async fn catalog_write(
        &self,
        what: &str,
        _body: &Map<String, Value>,
        _caller: &Caller,
    ) -> (u16, Value) {
        (
            501,
            json!({"error": format!("/{what} is a config edit now: use the admin UI (Router > Layouts)")}),
        )
    }
}

/// What the shim listener serves.
pub struct ShimState {
    pub backend: Arc<dyn ShimBackend>,
    pub callers: Arc<dyn CallerResolver>,
}

async fn serve_one(
    State(s): State<Arc<ShimState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_q = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let (code, v) = handle(
        s.backend.as_ref(),
        s.callers.as_ref(),
        Some(peer.ip()),
        &method,
        &path_q,
        &headers,
        &body,
    )
    .await;
    let status = axum::http::StatusCode::from_u16(code)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(v)).into_response()
}

/// Serve the shim on `listener` until the task is dropped. Production keeps it off until the
/// cutover (the old API owns :8098); tests bind a loopback port.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<ShimState>,
) -> std::io::Result<()> {
    let app = axum::Router::new().fallback(serve_one).with_state(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

#[cfg(test)]
mod shim_backend_tests {
    use super::*;
    use crate::switch::callers::{Resolved, StaticCallers};

    /// The key sets the live profiles API answers with (recorded read-only).
    const CARD_KEYS: &[&str] = &[
        "card",
        "gpu",
        "offline",
        "engine",
        "layoutEngine",
        "model",
        "slots",
        "healthy",
        "detail",
        "since",
    ];
    const LIVE_KEYS: &[&str] = &[
        "layout",
        "profiles",
        "title",
        "cards",
        "since",
        "healthy",
        "leases",
        "pigSlots",
        "job",
        "offline",
        "quietWindow",
        "musicMode",
        "checked",
    ];
    const HISTORY_KEYS: &[&str] = &[
        "id",
        "layout",
        "from",
        "profile",
        "requestedBy",
        "state",
        "seconds",
        "estimateSeconds",
        "error",
        "started",
        "ended",
    ];

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    fn want(k: &[&str]) -> Vec<String> {
        let mut k: Vec<String> = k.iter().map(|s| s.to_string()).collect();
        k.sort();
        k
    }

    #[tokio::test]
    async fn the_shim_listener_over_the_admin_backend() {
        let backend = crate::control_api::demo::DemoControl::new();
        let server = backend.state().servers[0].id.clone();
        let shim = Arc::new(ShimState {
            backend: Arc::new(ControlShim { backend, server }),
            callers: Arc::new(StaticCallers::default().with(
                "svc-switch",
                Resolved {
                    caller: Caller {
                        principal: Some("service:ha#9".into()),
                        grants: vec![],
                    },
                    may_switch: true,
                    may_on_demand: true,
                },
            )),
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(serve(l, shim));
        let c = reqwest::Client::new();
        let get = |p: &str| {
            let c = c.clone();
            let u = format!("{url}{p}");
            async move {
                let r = c.get(u).send().await.unwrap();
                (r.status().as_u16(), r.json::<Value>().await.unwrap())
            }
        };
        assert_eq!(get("/health").await.1["service"], "profiles-api");
        let (code, cards) = get("/cards").await;
        assert_eq!(code, 200);
        for (_, v) in cards["cards"].as_object().unwrap() {
            assert_eq!(keys(v), want(CARD_KEYS));
        }
        assert_eq!(keys(&get("/live").await.1), want(LIVE_KEYS));
        let (_, h) = get("/jobs?limit=5").await;
        let jobs = h["jobs"].as_array().unwrap();
        assert!(!jobs.is_empty());
        for j in jobs {
            assert_eq!(keys(j), want(HISTORY_KEYS));
        }
        let id = jobs[0]["id"].as_str().unwrap();
        let (code, j) = get(&format!("/jobs/{id}")).await;
        assert_eq!(code, 200);
        assert!(j["steps"].is_array());

        // switching: no key 401, a switch key queues a job
        let body = json!({"layout": "dense-x2", "requestedBy": "ha"});
        let r = c
            .post(format!("{url}/switch"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let r = c
            .post(format!("{url}/cards/B/offline"))
            .json(&json!({"dryRun": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let layouts = cards["layout"].clone();
        let other = crate::control_api::demo::DemoControl::new()
            .state()
            .layouts
            .into_iter()
            .find(|l| Some(l.id.as_str()) != layouts.as_str())
            .unwrap()
            .id;
        let r = c
            .post(format!("{url}/switch"))
            .bearer_auth("svc-switch")
            .json(&json!({"layout": other, "requestedBy": "ha"}))
            .send()
            .await
            .unwrap();
        let code = r.status().as_u16();
        let v: Value = r.json().await.unwrap();
        assert!(code == 202 || code == 409, "{code} {v}");
        if code == 202 {
            assert_eq!(v["state"], "queued");
        }
    }
}
