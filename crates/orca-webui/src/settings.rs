//! The settings page (the web's `/config`), drawn from `ConfigSchema`:
//! * `/api/settings`: the server's defaults, config.json in its OpenOrca home;
//! * `/api/projects/{pid}/settings`: that project's running session, as /config in its
//!   terminal would.
//!
//! `ConfigSchema` itself (the describe/apply of /config) belongs to the REPL port; the host takes
//! it as a [`SettingsSchema`] (decision D70). Without one the global page answers 409.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::audit::Caller;
use crate::live::SettingChange;
use crate::server::AppState;

/// `ConfigSchema.Describe` / `Apply` over the config.json of an OpenOrca home.
#[async_trait]
pub trait SettingsSchema: Send + Sync {
    /// The described settings (secrets masked), with `configFile` set.
    async fn describe_file(&self, config_home: Option<&std::path::Path>) -> Result<Value, String>;
    /// Change one setting in config.json and save it.
    async fn apply_file(
        &self,
        config_home: Option<&std::path::Path>,
        key: &str,
        value: Option<Value>,
        reset: bool,
    ) -> SettingChange;
}

#[derive(Deserialize)]
pub struct SettingRequest {
    pub key: String,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub reset: bool,
}

static GLOBAL_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings", get(get_global).put(put_global))
        .route(
            "/api/projects/{pid}/settings",
            get(get_project).put(put_project),
        )
}

fn home(state: &AppState) -> Option<PathBuf> {
    state.options.config_home.clone()
}

async fn get_global(State(state): State<Arc<AppState>>) -> Response {
    let Some(schema) = state.options.settings.clone() else {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": "This server has no settings page."})),
        )
            .into_response();
    };
    match schema.describe_file(home(&state).as_deref()).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e})),
        )
            .into_response(),
    }
}

async fn put_global(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::Json(req): axum::Json<SettingRequest>,
) -> Response {
    let Some(schema) = state.options.settings.clone() else {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": "This server has no settings page."})),
        )
            .into_response();
    };
    let _gate = GLOBAL_GATE.lock().await;
    let change = schema
        .apply_file(
            home(&state).as_deref(),
            &req.key,
            req.value.clone(),
            req.reset,
        )
        .await;
    answer(&state, &caller(addr), "settings.global", None, &req, change)
}

fn caller(addr: std::net::SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: None,
    }
}

async fn get_project(State(state): State<Arc<AppState>>, Path(pid): Path<String>) -> Response {
    let Some(rt) = state.projects.get(&pid) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "No such project."})),
        )
            .into_response();
    };
    match rt.session.get_settings().await {
        Some(d) => axum::Json(d).into_response(),
        None => (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": "This project's session has no settings to show."})),
        )
            .into_response(),
    }
}

async fn put_project(
    State(state): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::Json(req): axum::Json<SettingRequest>,
) -> Response {
    let Some(rt) = state.projects.get(&pid) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "No such project."})),
        )
            .into_response();
    };
    let change = rt
        .session
        .set_setting(&req.key, req.value.clone(), req.reset)
        .await;
    answer(
        &state,
        &caller(addr),
        "settings.project",
        Some(&pid),
        &req,
        change,
    )
}

fn answer(
    state: &AppState,
    caller: &Caller,
    event: &str,
    pid: Option<&str>,
    req: &SettingRequest,
    change: SettingChange,
) -> Response {
    state.audit.write(
        Some(caller),
        event,
        json!({
            "project": pid,
            "key": req.key,
            "reset": req.reset,
            "ok": change.ok,
            "old": change.old,
            "new": change.new,
            "error": change.error,
        }),
    );
    if change.ok {
        axum::Json(json!({"ok": true, "key": change.key, "old": change.old, "new": change.new, "live": change.live}))
            .into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"ok": false, "key": change.key, "error": change.error})),
        )
            .into_response()
    }
}
