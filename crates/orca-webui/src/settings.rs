//! The settings page (the web's `/config`), drawn from `ConfigSchema`:
//! * `/api/settings`: the server's defaults, config.json in its OpenOrca home;
//! * `/api/projects/{pid}/settings`: that project's running session, as /config in its
//!   terminal would.
//!
//! The global page uses `orca_core::config::schema` (ported `ConfigSchema`) on config.json in
//! the host's OpenOrca home; a caller can still put its own [`SettingsSchema`] in its place.

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

use orca_core::config::schema::{self, ProfileOverlay};
use orca_core::config::{ConfigManager, OrcaConfig};

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
    let result = match state.options.settings.clone() {
        Some(schema) => schema.describe_file(home(&state).as_deref()).await,
        None => describe_home(&state).await,
    };
    match result {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e})),
        )
            .into_response(),
    }
}

/// `ConfigSchema.Describe` of config.json in the OpenOrca home, with the default profile's keys
/// marked (sessions that start with it get its values; they stay editable here, since sessions
/// without a profile use the file's).
async fn describe_home(state: &AppState) -> Result<Value, String> {
    let home = home(state);
    let mut manager = ConfigManager::new(home.as_deref());
    // A broken or missing file describes the defaults, as the C# loader falls back.
    let _ = manager.load();
    let overlay = default_profile(state, &manager.config).await;
    let mut d = schema::describe(&manager.config, overlay.as_ref());
    d["configFile"] = json!(manager.file_path().to_string_lossy());
    Ok(d)
}

async fn default_profile(state: &AppState, config: &OrcaConfig) -> Option<ProfileOverlay> {
    let name = config
        .profiles
        .active
        .as_deref()
        .filter(|n| !n.is_empty())?;
    let service = state.profiles.as_ref()?;
    let (catalog, _) = service.get_catalog().await;
    let profile = catalog?.find(name)?.clone();
    if profile.client.is_empty() {
        return None;
    }
    Some(ProfileOverlay::new(
        name,
        profile.client.keys().cloned().collect(),
    ))
}

/// `ConfigSchema.Apply` to config.json in the OpenOrca home, saved with the writer /config uses.
fn apply_home(state: &AppState, req: &SettingRequest) -> SettingChange {
    let home = home(state);
    let mut manager = ConfigManager::new(home.as_deref());
    let _ = manager.load();
    let change = schema::apply(
        &mut manager.config,
        &req.key,
        req.value.as_ref(),
        req.reset,
        None,
    );
    let mut change = from_core(change);
    if change.ok
        && let Err(e) = manager.save()
    {
        change = SettingChange::failed(&req.key, &e.to_string());
    }
    change
}

/// The core schema's change as the web UI reports it.
pub fn from_core(c: schema::SettingChange) -> SettingChange {
    SettingChange {
        ok: c.ok,
        key: c.key,
        error: c.error,
        old: c.old,
        new: c.new,
        live: c.live,
    }
}

async fn put_global(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::Json(req): axum::Json<SettingRequest>,
) -> Response {
    let _gate = GLOBAL_GATE.lock().await;
    let change = match state.options.settings.clone() {
        Some(schema) => {
            schema
                .apply_file(
                    home(&state).as_deref(),
                    &req.key,
                    req.value.clone(),
                    req.reset,
                )
                .await
        }
        None => apply_home(&state, &req),
    };
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
