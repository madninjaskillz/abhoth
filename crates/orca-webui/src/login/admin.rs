//! `/api/admin/*` (admins only; the middleware checks): users (create, disable, admin rights,
//! password and 2FA resets, delete) and the audit log.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::db::{CreateError, User};
use super::endpoints::{body_json, no_store};
use super::{Current, Principal, error, password};
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/users", get(users).post(create))
        .route(
            "/api/admin/users/{id}",
            axum::routing::patch(update).delete(remove),
        )
        .route("/api/admin/users/{id}/password", post(reset_password))
        .route("/api/admin/users/{id}/totp/reset", post(reset_totp))
        .route("/api/admin/audit", get(audit))
}

// An error is a ready response, used once per request.
#[allow(clippy::result_large_err)]
fn ctx(req: &Request<Body>) -> Result<(Principal, Option<String>), Response> {
    let p = super::principal_of(req)
        .filter(|p| p.admin)
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "Only an admin can do that."))?;
    let ip = req
        .extensions()
        .get::<Current>()
        .and_then(|c| c.seen.ip_text());
    Ok((p, ip))
}

macro_rules! ctx_or_return {
    ($req:expr) => {
        match ctx(&$req) {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

fn describe(state: &AppState, u: &User) -> Value {
    let mut v = json!(u);
    v["localOnly"] = json!(u.local_only());
    v["projects"] = json!(state.projects.count_owned_by(u.id));
    v
}

async fn users(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let _ = ctx_or_return!(req);
    let list: Vec<Value> = state
        .login
        .db
        .users()
        .iter()
        .map(|u| describe(&state, u))
        .collect();
    no_store(json!(list))
}

async fn create(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, ip) = ctx_or_return!(req);
    let auth = state.login.clone();
    let body = body_json(req).await;
    let name = match super::check_username(body["username"].as_str().unwrap_or("")) {
        Ok(n) => n,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let pw = body["password"].as_str().unwrap_or("").to_string();
    if let Err(e) = password::check(&pw) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let admin = body["admin"].as_bool().unwrap_or(false);
    let cost = auth.options.cost;
    let hash = tokio::task::spawn_blocking(move || password::hash(&pw, cost))
        .await
        .unwrap_or_default();
    match auth.db.create_user(&name, &hash, admin, false, auth.now()) {
        Ok(u) => {
            auth.record(
                "user.created",
                Some(&p),
                Some(&u.username),
                ip.as_deref(),
                json!({"admin": admin}),
            );
            no_store(describe(&state, &u))
        }
        Err(CreateError::Taken) => error(StatusCode::CONFLICT, "That username is taken."),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// A user other than the last active admin may lose admin rights or be disabled.
fn would_strand(state: &AppState, target: &User, admin: bool, disabled: bool) -> bool {
    let losing = target.admin && !target.disabled && (!admin || disabled);
    losing && state.login.db.active_admin_count() <= 1
}

async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = ctx_or_return!(req);
    let auth = state.login.clone();
    let body = body_json(req).await;
    let Some(u) = auth.db.user(id) else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    let admin = body["admin"].as_bool().unwrap_or(u.admin);
    let disabled = body["disabled"].as_bool().unwrap_or(u.disabled);
    if id == p.user_id && (disabled || !admin) {
        return error(
            StatusCode::BAD_REQUEST,
            "You can't disable yourself or remove your own admin rights.",
        );
    }
    if would_strand(&state, &u, admin, disabled) {
        return error(StatusCode::BAD_REQUEST, "The last admin stays an admin.");
    }
    if admin != u.admin {
        auth.db.set_admin(id, admin);
        auth.record(
            if admin {
                "user.admin_granted"
            } else {
                "user.admin_revoked"
            },
            Some(&p),
            Some(&u.username),
            ip.as_deref(),
            json!({}),
        );
    }
    if disabled != u.disabled {
        auth.db.set_disabled(id, disabled);
        if disabled {
            auth.db.delete_sessions_of(id, None);
        }
        auth.record(
            if disabled {
                "user.disabled"
            } else {
                "user.enabled"
            },
            Some(&p),
            Some(&u.username),
            ip.as_deref(),
            json!({}),
        );
    }
    // Open sockets carry the old rights: they reconnect with the new ones.
    if admin != u.admin || disabled != u.disabled {
        state.hub.drop_where(|c| c.uid == Some(id));
    }
    let fresh = auth.db.user(id).unwrap_or(u);
    no_store(describe(&state, &fresh))
}

async fn reset_password(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = ctx_or_return!(req);
    let auth = state.login.clone();
    let body = body_json(req).await;
    let Some(u) = auth.db.user(id) else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    let pw = body["password"].as_str().unwrap_or("").to_string();
    if let Err(e) = password::check(&pw) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let cost = auth.options.cost;
    let hash = tokio::task::spawn_blocking(move || password::hash(&pw, cost))
        .await
        .unwrap_or_default();
    auth.db.set_password_hash(id, &hash);
    // The user's sessions end (the admin's own one stays when it is theirs).
    let keep = (id == p.user_id).then(|| p.session_id()).flatten();
    auth.db.delete_sessions_of(id, keep);
    state.hub.drop_where(|c| c.uid == Some(id) && c.sid != keep);
    auth.record(
        "user.password_reset",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

async fn reset_totp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = ctx_or_return!(req);
    let auth = state.login.clone();
    let Some(u) = auth.db.user(id) else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    auth.db.disable_totp(id);
    auth.record(
        "user.totp_reset",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, ip) = ctx_or_return!(req);
    let auth = state.login.clone();
    let Some(u) = auth.db.user(id) else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    if id == p.user_id {
        return error(StatusCode::BAD_REQUEST, "You can't delete yourself.");
    }
    if would_strand(&state, &u, false, true) {
        return error(StatusCode::BAD_REQUEST, "The last admin can't be deleted.");
    }
    state.hub.drop_where(|c| c.uid == Some(id));
    // Their projects leave the list (the folders stay on disk).
    let forgotten = state.projects.forget_owned_by(id).await;
    auth.db.delete_user(id);
    auth.record(
        "user.deleted",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({"projects": forgotten}),
    );
    no_store(json!({"ok": true}))
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
    before: Option<i64>,
}

async fn audit(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AuditQuery>,
    req: Request<Body>,
) -> Response {
    let _ = ctx_or_return!(req);
    no_store(json!(
        state
            .login
            .db
            .audit_entries(q.limit.unwrap_or(200), q.before)
    ))
}
