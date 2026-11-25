//! `GitHubAppEndpoints`: /api/github/app/*, the "Clone from GitHub" wizard. Sign in with the
//! GitHub App's device flow, list the repositories its installations reach and their branches,
//! clone one (git gets the token through the credential helper), sign out. No response carries
//! a token.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use crate::audit::Caller;
use crate::git::github_app::{GitHubAppConfig, GitHubAppError, check_repo, expires_text};
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/github/app", get(status))
        .route("/api/github/app/device", post(device_start))
        .route("/api/github/app/device/{flow_id}", post(device_poll))
        .route("/api/github/app/signout", post(sign_out))
        .route("/api/github/app/repos", get(repos))
        .route("/api/github/app/branches", get(branches))
        .route("/api/github/app/clone", post(clone))
}

fn caller(addr: SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        ..Default::default()
    }
}

/// 409, not 401, for a GitHub sign-in problem: a 401 means the web UI's own session ended.
fn guard(e: GitHubAppError) -> Response {
    match e {
        GitHubAppError::Auth(m) => (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": m, "signedOut": true})),
        )
            .into_response(),
        GitHubAppError::Argument(m) | GitHubAppError::Invalid(m) => {
            (StatusCode::BAD_REQUEST, axum::Json(json!({"error": m}))).into_response()
        }
        GitHubAppError::Http(m) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": format!("Couldn't reach GitHub: {m}")})),
        )
            .into_response(),
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Response {
    let svc = &s.github_app;
    let config = svc.config();
    let account = if config.configured() {
        svc.account()
    } else {
        None
    };
    axum::Json(json!({
        "configured": config.configured(),
        "slug": config.slug,
        "installUrl": svc.install_url(),
        "signedIn": account.is_some(),
        "login": account.as_ref().and_then(|a| a.login.clone()),
        "avatarUrl": account.as_ref().and_then(|a| a.avatar_url.clone()),
        "expires": account.as_ref().and_then(expires_text),
        "setup": if config.configured() { None } else {
            Some(format!(
                "Set {} and {} for the server (or write {{\"clientId\": \"…\", \"slug\": \"…\"}} to {} in its state folder), then reload this page.",
                GitHubAppConfig::CLIENT_ID_VARIABLE,
                GitHubAppConfig::SLUG_VARIABLE,
                GitHubAppConfig::FILE_NAME
            ))
        },
    }))
    .into_response()
}

async fn device_start(State(s): State<Arc<AppState>>) -> Response {
    match s.github_app.start_device().await {
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
    Path(flow_id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let p = s.github_app.poll(&flow_id).await;
    if p.state == "done" {
        s.audit.write(
            Some(&caller(addr)),
            "github.signin",
            json!({"login": p.login}),
        );
    }
    axum::Json(p).into_response()
}

async fn sign_out(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let login = s.github_app.account().and_then(|a| a.login);
    let removed = s.github_app.sign_out();
    s.audit.write(
        Some(&caller(addr)),
        "github.signout",
        json!({"login": login, "removed": removed}),
    );
    axum::Json(json!({
        "ok": true,
        "removed": removed,
        "revokeUrl": format!("{}/settings/apps/authorizations", s.github_app.web_base()),
    }))
    .into_response()
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
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let int = |name: &str, default: i64| match q.get(name).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(v) => v.trim().parse::<i64>().map_err(|_| name.to_string()),
    };
    let (page, per_page) = match (int("page", 1), int("perPage", 30)) {
        (Ok(p), Ok(pp)) => (p, pp),
        (Err(name), _) | (_, Err(name)) => return bad_query(&name),
    };
    let reload = match q.get("reload").map(|v| v.to_ascii_lowercase()) {
        None => false,
        Some(v) if v == "true" => true,
        Some(v) if v == "false" || v.is_empty() => false,
        Some(_) => return bad_query("reload"),
    };
    match s
        .github_app
        .repos(q.get("q").map(String::as_str), page, per_page, reload)
        .await
    {
        Ok(r) => axum::Json(json!({
            "total": r.total,
            "page": r.page,
            "perPage": r.per_page,
            "items": r.items,
            "installations": r.installations,
            "installUrl": s.github_app.install_url(),
        }))
        .into_response(),
        Err(e) => guard(e),
    }
}

async fn branches(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match s
        .github_app
        .branches(q.get("repo").map(String::as_str).unwrap_or(""))
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<CloneRequest>,
) -> Response {
    let repo = match check_repo(req.repo.as_deref().unwrap_or("")) {
        Ok(r) => r,
        Err(e) => return guard(e),
    };
    let token = match s.github_app.access_token(false).await {
        Ok(t) => t,
        Err(e) => return guard(e),
    };
    let url = match s.github_app.clone_url(&repo) {
        Ok(u) => u,
        Err(e) => return guard(e),
    };
    let name = req
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| repo.split('/').nth(1).unwrap_or("").to_string());
    match s.clones.start_with(
        &url,
        req.parent.as_deref().unwrap_or(""),
        Some(&name),
        Some(&token),
        req.branch.as_deref(),
        true,
    ) {
        Ok(job) => {
            s.audit.write(
                Some(&caller(addr)),
                "project.clone",
                json!({"repo": repo, "branch": req.branch, "destination": job.destination, "via": "github-app"}),
            );
            axum::Json(job).into_response()
        }
        Err(e) => guard(GitHubAppError::Argument(e)),
    }
}
