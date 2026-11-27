//! The signed-in Glance API: the snapshot and the SSE stream for every user, and the card
//! online/offline actions for admins (`admin_only` covers every non-GET under `/api/glance`).
//! The actions go to the router's admin API (`POST /admin/api/cards/{card}/{online|offline}`,
//! `GET /admin/api/jobs/{id}`) from the server, signed as the acting admin (the router audits
//! them as that user): the old dash POSTed from the browser to an unauthenticated LAN port.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::login::Principal;
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/glance/snapshot", get(snapshot))
        .route("/api/glance/stream", get(stream))
        .route("/api/glance/history", get(history))
        .route("/api/glance/cards/{card}/{action}", post(card_action))
        .route("/api/glance/jobs/{id}", get(job))
}

fn err(code: StatusCode, text: &str) -> Response {
    (code, Json(json!({"error": text}))).into_response()
}

fn glance(state: &AppState) -> Option<&Arc<super::Glance>> {
    state.glance.as_ref()
}

/// `GET /api/glance/snapshot`: the feed's [`orca_sensors::feed::Snapshot`] `{seq, ts, state}`.
async fn snapshot(State(state): State<Arc<AppState>>) -> Response {
    let Some(g) = glance(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut r = Json(g.full_feed().snapshot()).into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    r
}

/// `GET /api/glance/stream`: SSE of feed messages; `Last-Event-ID` resumes.
async fn stream(State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    let Some(g) = glance(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    super::feed::sse(
        g.full_feed(),
        super::feed::last_event_id(&headers),
        g.options.ping,
    )
}

/// `GET /api/glance/history?range=10m&ids=gpu.A.core,…`: the supervisor's sensor history (its
/// `/api/v1/supervisor/history`), so the charts start full instead of empty. Read-only; the
/// query is passed on only when it is plain sensor ids.
async fn history(State(state): State<Arc<AppState>>, uri: axum::http::Uri) -> Response {
    let Some(g) = glance(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(base) = g.options.supervisor.as_deref() else {
        return err(StatusCode::NOT_FOUND, "no supervisor feed");
    };
    let q = uri.query().unwrap_or("");
    if !history_query_ok(q) {
        return err(StatusCode::BAD_REQUEST, "bad history query");
    }
    let url = format!("{}/api/v1/supervisor/history?{q}", supervisor_base(base));
    let got = async {
        let r = state
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let st = r.status().as_u16();
        let v: Value = r.json().await.map_err(|e| e.without_url().to_string())?;
        Ok::<_, String>((st, v))
    }
    .await;
    let mut r = relay(got);
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    r
}

/// The supervisor's base URL from the configured one (with or without the stream path).
pub fn supervisor_base(u: &str) -> &str {
    let b = u.trim_end_matches('/');
    b.strip_suffix("/api/v1/supervisor/stream").unwrap_or(b)
}

/// `range` and `ids` only, sensor-id characters only (percent-encoded quotes and spaces too).
pub fn history_query_ok(q: &str) -> bool {
    q.len() <= 4096
        && q.split('&').filter(|kv| !kv.is_empty()).all(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            matches!(k, "range" | "ids")
                && v.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '.' | '_' | '-' | '*' | ',' | '%' | '+')
                })
        })
}

fn acting(p: &Principal) -> orca_core::router_link::ActingUser {
    orca_core::router_link::ActingUser {
        user_id: p.user_id,
        name: p.username.clone(),
        admin: p.admin,
    }
}

const NO_ROUTER: &str = "card controls are not connected (no router control plane linked)";

fn safe_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

async fn card_action(
    State(state): State<Arc<AppState>>,
    principal: Option<Extension<Principal>>,
    Path((card, action)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Response {
    if glance(&state).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(Extension(p)) = principal.filter(|p| p.admin) else {
        return err(StatusCode::FORBIDDEN, "admins only");
    };
    if !safe_id(&card, 8) || !matches!(action.as_str(), "online" | "offline") {
        return err(StatusCode::BAD_REQUEST, "bad card or action");
    }
    let Some(link) = state.options.router.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, NO_ROUTER);
    };
    let reason = body
        .as_ref()
        .and_then(|b| b.0.get("reason"))
        .and_then(Value::as_str)
        .map(|r| crate::audit::AuditLog::clip(r, 200))
        .unwrap_or_else(|| "Glance".into());
    state.audit.write(
        None,
        "glance-card",
        json!({"user": p.username, "card": card, "action": action, "reason": reason}),
    );
    relay(
        link.call(
            "POST",
            &format!("cards/{card}/{action}"),
            &acting(&p),
            &json!({"reason": reason}),
        )
        .await,
    )
}

async fn job(
    State(state): State<Arc<AppState>>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Response {
    if glance(&state).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(Extension(p)) = principal else {
        return err(StatusCode::UNAUTHORIZED, "sign in");
    };
    if !safe_id(&id, 96) {
        return err(StatusCode::BAD_REQUEST, "bad job id");
    }
    let Some(link) = state.options.router.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, NO_ROUTER);
    };
    relay(
        link.call("GET", &format!("jobs/{id}"), &acting(&p), &Value::Null)
            .await,
    )
}

/// The router's answer, status and JSON body, as it came.
fn relay(r: Result<(u16, Value), String>) -> Response {
    match r {
        Ok((st, body)) => {
            let code = StatusCode::from_u16(st).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, Json(body)).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_queries_and_bases() {
        assert!(history_query_ok(
            "range=10m&ids=gpu.A.core,bmc.%22CPU1%20Temp%22,fan.*"
        ));
        assert!(history_query_ok(""));
        assert!(!history_query_ok("ids=a&url=http://x"));
        assert!(!history_query_ok("ids=a/../b"));
        assert!(!history_query_ok("ids=a#b"));
        assert_eq!(
            supervisor_base("http://127.0.0.1:9890/"),
            "http://127.0.0.1:9890"
        );
        assert_eq!(
            supervisor_base("http://h:1/api/v1/supervisor/stream"),
            "http://h:1"
        );
    }
}
