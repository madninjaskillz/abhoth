//! `/api/github/user/*` (D147): "Clone from GitHub" with the caller's own GitHub credentials,
//! for every user (not admin-only like the shared GitHub App). Which credentials, the order
//! and the security rules are in `github_user`. No response carries a token.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::json;

use crate::audit::Caller;
use crate::git::github_app::{GitHubAppError, check_repo};
use crate::git::github_app_endpoints::guard;
use crate::git::github_user::{GitHubUserService, Source};
use crate::git::secret_box;
use crate::login::Principal;
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/github/user", get(status))
        .route("/api/github/user/token", post(set_token).delete(forget))
        .route("/api/github/user/recheck", post(recheck))
        .route("/api/github/user/device", post(device_start))
        .route("/api/github/user/device/{flow_id}", post(device_poll))
        .route("/api/github/user/repos", get(repos))
        .route("/api/github/user/branches", get(branches))
        .route("/api/github/user/clone", post(clone))
}

fn caller(addr: SocketAddr, p: &Principal) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: Some(p.username.clone()),
        uid: Some(p.user_id),
        admin: p.admin,
        sid: None,
    }
}

async fn status(State(s): State<Arc<AppState>>, Extension(p): Extension<Principal>) -> Response {
    let svc = &s.github_user;
    let r = svc.resolve(p.user_id, p.admin).await;
    let app_available = p.admin && s.github_app.config().configured();
    axum::Json(json!({
        "source": r.source,
        "sourceText": r.source.text(),
        "signedIn": r.token.is_some(),
        "login": r.login,
        "avatarUrl": r.avatar_url,
        "notes": r.notes,
        "saved": matches!(r.source, Source::Saved | Source::Oauth),
        // This machine's gh / Git Credential Manager sign-ins: the desktop app or a local,
        // non-exposed server, for admins only.
        "machineAllowed": svc.machine_allowed() && p.admin,
        "machineServer": svc.machine_allowed(),
        "oauthConfigured": svc.client_id().is_some(),
        "protection": secret_box::protection(),
        "appAvailable": app_available,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    save: bool,
}

async fn set_token(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<TokenRequest>,
) -> Response {
    match s
        .github_user
        .set_token(p.user_id, req.token.as_deref().unwrap_or(""), req.save)
        .await
    {
        Ok(r) => {
            s.audit.write(
                Some(&caller(addr, &p)),
                "github.user.token",
                json!({"login": r.login, "saved": req.save}),
            );
            axum::Json(json!({"ok": true, "source": r.source, "login": r.login})).into_response()
        }
        Err(e) => guard(e),
    }
}

async fn forget(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let removed = s.github_user.forget(p.user_id);
    s.audit.write(
        Some(&caller(addr, &p)),
        "github.user.forget",
        json!({"removed": removed}),
    );
    axum::Json(json!({"ok": true, "removed": removed})).into_response()
}

/// Read this machine's sign-ins again (after `gh auth login`).
async fn recheck(State(s): State<Arc<AppState>>) -> Response {
    s.github_user.recheck_machine().await;
    axum::Json(json!({"ok": true})).into_response()
}

async fn device_start(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
) -> Response {
    match s.github_user.start_device(p.user_id).await {
        Ok(d) => axum::Json(d).into_response(),
        Err(e @ GitHubAppError::Http(_)) => guard(e),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn device_poll(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(flow_id): Path<String>,
) -> Response {
    let r = s.github_user.poll_device(p.user_id, &flow_id).await;
    if r.state == "done" {
        s.audit.write(
            Some(&caller(addr, &p)),
            "github.user.signin",
            json!({"login": r.login}),
        );
    }
    axum::Json(r).into_response()
}

fn bad_query(name: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": format!("Bad value for {name}.")})),
    )
        .into_response()
}

async fn repos(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let int = |name: &str, default: i64| match q.get(name).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(v) => v.trim().parse::<i64>().map_err(|_| name.to_string()),
    };
    let (page, per_page) = match (int("page", 1), int("perPage", 30)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(n), _) | (_, Err(n)) => return bad_query(&n),
    };
    let reload = q.get("reload").is_some_and(|v| v == "true");
    match s
        .github_user
        .repos(
            p.user_id,
            p.admin,
            q.get("q").map(String::as_str),
            page,
            per_page,
            reload,
        )
        .await
    {
        Ok(r) => axum::Json(r).into_response(),
        Err(e) => guard(e),
    }
}

async fn branches(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match s
        .github_user
        .branches(
            p.user_id,
            p.admin,
            q.get("repo").map(String::as_str).unwrap_or(""),
        )
        .await
    {
        Ok(b) => axum::Json(b).into_response(),
        Err(e) => guard(e),
    }
}

#[derive(Deserialize)]
struct CloneRequest {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

async fn clone(
    State(s): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<CloneRequest>,
) -> Response {
    let repo = match check_repo(req.repo.as_deref().unwrap_or("")) {
        Ok(r) => r,
        Err(e) => return guard(e),
    };
    if let Err(e) =
        crate::projects::endpoints::path_allowed(&s, &p, req.parent.as_deref().unwrap_or(""))
    {
        return (StatusCode::FORBIDDEN, axum::Json(json!({"error": e}))).into_response();
    }
    let url = match s.github_user.clone_url(&repo) {
        Ok(u) => u,
        Err(e) => return guard(e),
    };
    let r = s.github_user.resolve(p.user_id, p.admin).await;
    let name = req
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| repo.split('/').nth(1).unwrap_or("").to_string());
    let options = crate::projects::clone::CloneOptions {
        token: r.token.clone(),
        // Fetch, pull and push of the new project use its owner's GitHub credentials then.
        link: (r.source != Source::Anonymous).then(|| GitHubUserService::link_for(p.user_id)),
        // A server that mustn't use its own machine's sign-ins: git gets no credential helper
        // of the server's (an anonymous clone stays anonymous).
        isolate: !s.github_user.machine_allowed(),
    };
    match s.clones.start_opts(
        &url,
        req.parent.as_deref().unwrap_or(""),
        Some(&name),
        req.branch.as_deref(),
        options,
    ) {
        Ok(job) => {
            s.audit.write(
                Some(&caller(addr, &p)),
                "project.clone",
                json!({"repo": repo, "branch": req.branch, "destination": job.destination,
                    "via": "github-user", "source": r.source}),
            );
            axum::Json(job).into_response()
        }
        Err(e) => guard(GitHubAppError::Argument(e)),
    }
}
