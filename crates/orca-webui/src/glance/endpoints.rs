//! The signed-in Glance API: the snapshot and the SSE stream for every user, and the card
//! online/offline actions for admins (`admin_only` covers every non-GET under `/api/glance`).
//! The actions go to the control plane from the server, with the user's name in the audit log:
//! the old dash POSTed from the browser to an unauthenticated LAN port.

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

fn control_url(state: &AppState) -> Option<String> {
    let g = glance(state)?;
    g.options
        .control_url
        .clone()
        .or_else(|| state.options.profiles_url.clone())
        .map(|u| u.trim_end_matches('/').to_string())
}

fn safe_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    let Some(base) = control_url(&state) else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "card controls are not connected (no control plane configured)",
        );
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
    let url = format!("{base}/cards/{card}/{action}");
    let sent = state
        .http
        .post(&url)
        .json(&json!({"requestedBy": format!("glance:{}", p.username), "reason": reason}))
        .send()
        .await;
    relay(sent).await
}

async fn job(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if glance(&state).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !safe_id(&id, 96) {
        return err(StatusCode::BAD_REQUEST, "bad job id");
    }
    let Some(base) = control_url(&state) else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "card controls are not connected",
        );
    };
    relay(state.http.get(format!("{base}/jobs/{id}")).send().await).await
}

/// The control plane's answer, status and JSON body, as it came (errors without the URL).
async fn relay(sent: reqwest::Result<reqwest::Response>) -> Response {
    match sent {
        Ok(r) => {
            let code = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = r.json::<Value>().await.unwrap_or_else(|_| json!({}));
            (code, Json(body)).into_response()
        }
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            &format!("control plane: {}", e.without_url()),
        ),
    }
}
