//! Inference profiles (`ProfileEndpoints`): the catalog for the pickers, the profile builder
//! (compose, save, delete), the server-wide switch with its plan and job, and each project's
//! profile and agent mode. Only mapped when a profiles API is configured.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use orca_core::live::types;
use orca_core::profiles::ProfileService;
use orca_core::profiles::agent_modes;
use serde_json::{Map, Value, json};

use crate::audit::Caller;
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/profiles", get(catalog))
        .route("/api/profiles/compose", post(compose))
        .route("/api/profiles/save", post(save))
        .route("/api/profiles/delete", post(delete))
        .route("/api/profiles/plan", get(plan))
        .route("/api/profiles/switch", post(switch))
        .route("/api/profiles/jobs/{id}", get(job))
        .route("/api/projects/{pid}/profile", post(project_profile))
        .route("/api/projects/{pid}/mode", post(project_mode))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}

fn caller(addr: SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: None,
    }
}

fn service(s: &AppState) -> Result<&Arc<ProfileService>, Response> {
    s.profiles
        .as_ref()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No profiles API is configured."))
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn iso(t: Option<std::time::SystemTime>) -> Option<String> {
    let d = t?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(crate::util::iso_from_unix(
        d.as_secs() as i64,
        d.subsec_nanos(),
    ))
}

async fn catalog(State(s): State<Arc<AppState>>) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (catalog, error) = svc.get_catalog().await;
    // A project that chose a profile by an old name shows (and keeps) its new name.
    let mut choices = Map::new();
    let mut modes = Map::new();
    for p in s.projects.list() {
        let id = p.id().to_string();
        let choice = s
            .projects
            .profiles
            .get(&id)
            .filter(|c| !c.is_empty())
            .map(|c| {
                catalog
                    .as_ref()
                    .and_then(|cat| cat.resolve(Some(&c)))
                    .unwrap_or(c)
            });
        choices.insert(id.clone(), to_value(&choice));
        let mode = s.projects.modes.get(&id).filter(|m| !m.is_empty());
        modes.insert(id, to_value(&mode));
    }
    let profiles = catalog.as_ref().map(|cat| {
        let mut m = Map::new();
        for (name, p) in &cat.profiles {
            let bridge = agent_modes::is_bridge(Some(p));
            m.insert(
                name.clone(),
                json!({
                    "title": p.title,
                    "description": p.description,
                    "layout": p.layout,
                    "client": p.client,
                    "summary": p.summary,
                    "hidden": p.hidden,
                    "defaultMode": if bridge { None } else { Some(agent_modes::default_of(Some(p))) },
                    "bridge": bridge,
                    "fits": cat.fits(p),
                    "restore": cat.restore_note(p),
                    "mode": p.mode,
                    "cards": p.cards,
                    "source": p.source,
                }),
            );
        }
        Value::Object(m)
    });
    axum::Json(json!({
        "available": catalog.is_some(),
        "error": error,
        "offline": catalog.as_ref().is_some_and(|c| c.from_cache),
        "fetchedAt": catalog.as_ref().and_then(|c| iso(c.fetched_at)),
        "layouts": catalog.as_ref().map(|c| to_value(&c.layouts)),
        "engines": catalog.as_ref().map(|c| to_value(&c.engines)),
        "engineProfiles": catalog.as_ref().map(|c| to_value(&c.engine_profiles)),
        "models": catalog.as_ref().map(|c| to_value(&c.models)),
        "profiles": profiles,
        "live": catalog.as_ref().map(|c| to_value(&c.live)),
        "aliases": catalog.as_ref().map(|c| to_value(&c.aliases)),
        "layoutAliases": catalog.as_ref().map(|c| to_value(&c.layout_aliases)),
        "allModes": agent_modes::ALL,
        "projects": choices,
        "projectModes": modes,
    }))
    .into_response()
}

async fn compose(State(s): State<Arc<AppState>>, axum::Json(body): axum::Json<Value>) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let cards = &body["cards"];
    if !cards.is_object() {
        return err(StatusCode::BAD_REQUEST, "No cards.");
    }
    match svc.compose(cards).await {
        Ok(p) => axum::Json(json!({"profile": p})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

async fn save(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let name = body["name"].as_str().unwrap_or("");
    if !body["cards"].is_object() || name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "A name and the cards.");
    }
    match svc
        .save_profile(
            name,
            &body["cards"],
            body["mode"].as_str(),
            body["title"].as_str(),
            body["description"].as_str(),
            body["overwrite"].as_bool().unwrap_or(false),
        )
        .await
    {
        Ok(saved) => {
            s.audit.write(
                Some(&caller(addr)),
                "profile.save",
                json!({"profile": saved}),
            );
            axum::Json(json!({"profile": saved})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

async fn delete(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(name) = body["profile"].as_str().filter(|n| !n.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "Which profile?");
    };
    match svc.delete_profile(name).await {
        Ok(()) => {
            s.audit.write(
                Some(&caller(addr)),
                "profile.delete",
                json!({"profile": name}),
            );
            axum::Json(json!({"deleted": name})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

async fn plan(
    State(s): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let profile = q.get("profile").cloned().unwrap_or_default();
    match svc.plan(&profile).await {
        Ok(p) => axum::Json(p).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn switch(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(name) = body["profile"].as_str().filter(|n| !n.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "Which profile?");
    };
    match svc
        .request_switch(name, "openorca web UI", Some("web UI server layout"))
        .await
    {
        Ok(job) => {
            s.audit.write(
                Some(&caller(addr)),
                "profile.switch",
                json!({"profile": name, "state": job.state, "job": job.id}),
            );
            let status = if job.state.as_deref() == Some("refused") {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, axum::Json(job)).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn job(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let svc = match service(&s) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match svc.get_job(&id).await {
        Ok(Some(j)) => axum::Json(j).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn project_profile(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let svc = match service(&s) {
        Ok(x) => x.clone(),
        Err(r) => return r,
    };
    let Some(rt) = s.projects.get(&pid) else {
        return err(StatusCode::NOT_FOUND, "No such project.");
    };
    if rt.read_only {
        return err(
            StatusCode::BAD_REQUEST,
            "This is a bench run: it can be watched, not driven.",
        );
    }
    let mut name = body["profile"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    if let Some(n) = &name {
        let (catalog, _) = svc.get_catalog().await;
        match catalog.and_then(|c| c.resolve(Some(n))) {
            Some(key) => name = Some(key),
            None => return err(StatusCode::BAD_REQUEST, &format!("Unknown profile {n}.")),
        }
    }
    let result = rt.session.set_profile(name.as_deref()).await;
    if result.ok {
        s.projects.profiles.set(&pid, name.as_deref().unwrap_or(""));
        let text = match &name {
            None => "Profile cleared: the session restarts on the config's own settings with your next prompt.".to_string(),
            Some(n) => format!("Profile {n}: the session restarts with it on your next prompt, on the same conversation."),
        };
        rt.session
            .bus()
            .publish(types::NOTICE, json!({"level": "info", "text": text}));
    }
    s.audit.write(
        Some(&caller(addr)),
        "profile.project",
        json!({"project": pid, "profile": name, "ok": result.ok}),
    );
    let status = if result.ok {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    (
        status,
        axum::Json(json!({"ok": result.ok, "profile": name, "message": result.message})),
    )
        .into_response()
}

async fn project_mode(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let Some(rt) = s.projects.get(&pid) else {
        return err(StatusCode::NOT_FOUND, "No such project.");
    };
    if rt.read_only {
        return err(
            StatusCode::BAD_REQUEST,
            "This is a bench run: it can be watched, not driven.",
        );
    }
    let mut mode: Option<&'static str> = None;
    if let Some(m) = body["mode"].as_str().filter(|m| !m.is_empty()) {
        match agent_modes::normalize(Some(m)) {
            Some(n) => mode = Some(n),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("Unknown mode {m} (one of {}).", agent_modes::ALL.join(", ")),
                );
            }
        }
    }
    let result = rt.session.set_mode(mode).await;
    if result.ok {
        s.projects.modes.set(&pid, mode.unwrap_or(""));
        let text = match mode {
            None => "Mode: the profile's own. The session restarts with it on your next prompt, on the same conversation.".to_string(),
            Some(m) => format!("Mode {m}: the session restarts in it on your next prompt, on the same conversation."),
        };
        rt.session
            .bus()
            .publish(types::NOTICE, json!({"level": "info", "text": text}));
    }
    s.audit.write(
        Some(&caller(addr)),
        "mode.project",
        json!({"project": pid, "mode": mode, "ok": result.ok}),
    );
    let status = if result.ok {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    (
        status,
        axum::Json(json!({"ok": result.ok, "mode": mode, "message": result.message})),
    )
        .into_response()
}
