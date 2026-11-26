//! The connection panel's Abhoth key (D149): the key field (save, remove) and key requests
//! (ask the router, show the code, poll until an admin approves; the key is then saved here and
//! never reaches the page). One request per scope (the server's config.json, or a project's
//! session) at a time, kept in memory with its poll secret.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use orca_core::router_mode::{self, KeyRequestStatus};

use super::{Target, bad, load, project_lm, save_changes, session_of};
use crate::audit::Caller;
use crate::server::AppState;

struct Pending {
    id: String,
    code: String,
    poll: String,
    until: Instant,
    key_url: Option<String>,
}

/// By scope and router root (a request is for one router).
static PENDING: LazyLock<Mutex<HashMap<(String, String), Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A waiting key request, as the panel shows it (no secret).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    pub code: String,
    pub expires_in: u64,
    /// Where admins approve it (the router's `keyUrl`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_url: Option<String>,
}

/// The key a scope's pending request is filed under.
pub fn scope_id(pid: Option<&str>) -> String {
    pid.map(|p| format!("project:{p}"))
        .unwrap_or_else(|| "server".into())
}

/// The scope's waiting request for the router at `base_url`, if any.
pub fn pending_view(scope: &str, base_url: &str) -> Option<PendingView> {
    let m = PENDING.lock().unwrap();
    let p = m.get(&(scope.to_string(), router_mode::root_of(base_url)))?;
    let now = Instant::now();
    (p.until > now).then(|| PendingView {
        code: p.code.clone(),
        expires_in: (p.until - now).as_secs(),
        key_url: p.key_url.clone(),
    })
}

/// Whether `key` looks like an Abhoth key (the router refuses anything else).
pub fn check_key(key: &str) -> Result<String, &'static str> {
    let k = key.trim();
    if !router_mode::is_router_key(k) || k.len() < 20 || k.len() > 200 {
        return Err(
            "That is not an Abhoth key: they start with abk_ (Settings > Account > API keys on the router's web UI, or Request a key).",
        );
    }
    Ok(k.to_string())
}

fn caller(
    addr: std::net::SocketAddr,
    p: &Option<axum::Extension<crate::login::Principal>>,
) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: p.as_ref().map(|p| p.username.clone()),
        admin: p.as_ref().is_some_and(|p| p.admin),
        ..Default::default()
    }
}

/// Save (`Some`) or remove (`None`) the key in `target`. The global config also drops
/// `apiKeyEnv`, so the saved key is the one used.
#[allow(clippy::result_large_err)]
async fn store_key(state: &AppState, target: &Target, key: Option<&str>) -> Result<(), Response> {
    let changes = vec![("lmStudio.apiKey".to_string(), json!(key.unwrap_or("")))];
    save_changes(state, target, &changes).await?;
    if matches!(target, Target::Global) {
        let mut m = load(state);
        if m.config.lm_studio.api_key_env.is_some() {
            m.config.lm_studio.api_key_env = None;
            m.save()
                .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }
    router_mode::clear_cache();
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct KeyBody {
    key: String,
}

#[allow(clippy::result_large_err)]
async fn target_of(state: &AppState, pid: Option<String>) -> Result<Target, Response> {
    match pid {
        None => Ok(Target::Global),
        Some(pid) => session_of(state, &pid)
            .map(|s| Target::Project(pid, s))
            .map_err(|(c, w)| bad(c, w)),
    }
}

#[allow(clippy::result_large_err)]
async fn lm_of(
    state: &AppState,
    pid: Option<&str>,
) -> Result<orca_core::config::LmStudioConfig, Response> {
    match pid {
        None => Ok(load(state).config.lm_studio),
        Some(pid) => project_lm(state, pid).await.map_err(|(c, w)| bad(c, w)),
    }
}

async fn do_save(
    state: &AppState,
    pid: Option<String>,
    addr: std::net::SocketAddr,
    p: Option<axum::Extension<crate::login::Principal>>,
    key: Option<String>,
) -> Response {
    let key = match key.as_deref().map(check_key) {
        Some(Err(e)) => return bad(StatusCode::BAD_REQUEST, e),
        Some(Ok(k)) => Some(k),
        None => None,
    };
    let target = match target_of(state, pid.clone()).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    if let Err(r) = store_key(state, &target, key.as_deref()).await {
        return r;
    }
    state.audit.write(
        Some(&caller(addr, &p)),
        "settings.connection.key",
        json!({"project": pid, "key": if key.is_some() { "saved" } else { "removed" }}),
    );
    axum::Json(json!({"ok": true})).into_response()
}

pub(super) async fn save_key(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
    axum::Json(b): axum::Json<KeyBody>,
) -> Response {
    do_save(&state, None, addr, p, Some(b.key)).await
}

pub(super) async fn remove_key(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    do_save(&state, None, addr, p, None).await
}

pub(super) async fn project_save_key(
    State(state): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
    axum::Json(b): axum::Json<KeyBody>,
) -> Response {
    do_save(&state, Some(pid), addr, p, Some(b.key)).await
}

pub(super) async fn project_remove_key(
    State(state): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    do_save(&state, Some(pid), addr, p, None).await
}

/// "Abhoth desktop on Laptop" (the desktop app) or "Abhoth on gpu-host" (a web server).
fn client_name(state: &AppState) -> String {
    let host = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|h| !h.trim().is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this computer".into());
    if state.options.desktop.is_some() {
        format!("Abhoth desktop on {host}")
    } else {
        format!("Abhoth on {host}")
    }
}

async fn do_request(state: &AppState, pid: Option<String>) -> Response {
    let lm = match lm_of(state, pid.as_deref()).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !lm.is_router() {
        return bad(StatusCode::CONFLICT, "Switch to the Abhoth router first.");
    }
    let http = router_mode::probe_client();
    let info = match router_mode::identify(&lm.base_url, &http).await {
        Ok(Some(i)) => i,
        Ok(None) => {
            return bad(
                StatusCode::BAD_GATEWAY,
                "The Base URL is not an Abhoth router.",
            );
        }
        Err(e) => {
            return bad(
                StatusCode::BAD_GATEWAY,
                format!("The router did not answer: {e}"),
            );
        }
    };
    if !info.auth.key_requests {
        return bad(
            StatusCode::CONFLICT,
            "This router does not take key requests: ask its admin for a key (made in its web UI) and paste it here.",
        );
    }
    let t = match router_mode::start_key_request(&lm.base_url, &client_name(state), &http).await {
        Ok(t) => t,
        Err(e) => return bad(StatusCode::BAD_GATEWAY, e),
    };
    let scope = scope_id(pid.as_deref());
    let view = PendingView {
        code: t.code.clone(),
        expires_in: t.expires_in,
        key_url: info.auth.key_url.clone(),
    };
    PENDING.lock().unwrap().insert(
        (scope, router_mode::root_of(&lm.base_url)),
        Pending {
            id: t.id,
            code: t.code,
            poll: t.poll,
            until: Instant::now() + Duration::from_secs(t.expires_in.max(1)),
            key_url: info.auth.key_url,
        },
    );
    axum::Json(json!({"status": "pending", "request": view, "interval": t.interval}))
        .into_response()
}

/// Poll the scope's request once; an approved key is saved and the request forgotten.
async fn do_poll(
    state: &AppState,
    pid: Option<String>,
    addr: std::net::SocketAddr,
    p: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    let scope = scope_id(pid.as_deref());
    let lm = match lm_of(state, pid.as_deref()).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    let slot = (scope.clone(), router_mode::root_of(&lm.base_url));
    let ask = {
        let m = PENDING.lock().unwrap();
        m.get(&slot)
            .map(|x| (x.id.clone(), x.poll.clone(), x.until))
    };
    let Some((id, poll, until)) = ask else {
        return axum::Json(json!({"status": "none"})).into_response();
    };
    if until <= Instant::now() {
        PENDING.lock().unwrap().remove(&slot);
        return axum::Json(json!({"status": "expired"})).into_response();
    }
    let st: KeyRequestStatus = match router_mode::poll_key_request(
        &lm.base_url,
        &id,
        &poll,
        &router_mode::probe_client(),
    )
    .await
    {
        Ok(s) => s,
        // Not reachable for a moment: still pending as far as the page knows.
        Err(e) => {
            return axum::Json(json!({"status": "pending", "warning": e, "request": pending_view(&scope, &lm.base_url)}))
                    .into_response();
        }
    };
    match st.status.as_str() {
        "pending" => {
            axum::Json(json!({"status": "pending", "request": pending_view(&scope, &lm.base_url)}))
                .into_response()
        }
        "approved" => {
            PENDING.lock().unwrap().remove(&slot);
            let Some(key) = st.key.as_deref().and_then(|k| check_key(k).ok()) else {
                return bad(StatusCode::BAD_GATEWAY, "The router sent no usable key.");
            };
            let target = match target_of(state, pid.clone()).await {
                Ok(t) => t,
                Err(r) => return r,
            };
            if let Err(r) = store_key(state, &target, Some(&key)).await {
                return r;
            }
            state.audit.write(
                Some(&caller(addr, &p)),
                "settings.connection.key",
                json!({"project": pid, "key": "saved from an approved key request", "name": st.name}),
            );
            axum::Json(json!({"status": "approved", "name": st.name, "claude": st.claude}))
                .into_response()
        }
        other => {
            PENDING.lock().unwrap().remove(&slot);
            axum::Json(json!({"status": other})).into_response()
        }
    }
}

pub(super) async fn request_key(State(state): State<Arc<AppState>>) -> Response {
    do_request(&state, None).await
}

pub(super) async fn project_request_key(
    State(state): State<Arc<AppState>>,
    Path(pid): Path<String>,
) -> Response {
    do_request(&state, Some(pid)).await
}

pub(super) async fn poll_key(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    do_poll(&state, None, addr, p).await
}

pub(super) async fn project_poll_key(
    State(state): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    p: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    do_poll(&state, Some(pid), addr, p).await
}

fn forget(scope: &str) -> Response {
    PENDING.lock().unwrap().retain(|(s, _), _| s != scope);
    axum::Json(json!({"ok": true})).into_response()
}

pub(super) async fn forget_key_request() -> Response {
    forget(&scope_id(None))
}

pub(super) async fn project_forget_key_request(Path(pid): Path<String>) -> Response {
    forget(&scope_id(Some(&pid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_abhoth_keys_are_saved() {
        assert!(check_key("abk_0123456789abcdefghij").is_ok());
        assert_eq!(
            check_key(" abk_0123456789abcdefghij ").unwrap(),
            "abk_0123456789abcdefghij"
        );
        for k in [
            "",
            "abk_short",
            "sk-ant-0123456789abcdef",
            "lm-studio",
            "abk_has a space in it",
        ] {
            assert!(check_key(k).is_err(), "{k}");
        }
        assert_eq!(scope_id(None), "server");
        assert_eq!(scope_id(Some("p1")), "project:p1");
    }
}
