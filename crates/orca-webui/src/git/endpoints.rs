//! `/api/projects/{pid}/git/*` (status, diff, stage/unstage, commit, fetch/pull/push, the branch
//! menu, stashes, HEAD content), worktrees, the project's GitHub token and "Validate token".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::service::{GitError, GitResult, GitService};
use super::tokens::{self, APP_LINK};
use crate::audit::{AuditLog, Caller};
use crate::server::AppState;

type Q = Query<HashMap<String, String>>;

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": "No such project or worktree."})),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"error": msg}))).into_response()
}

fn caller(addr: SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        ..Default::default()
    }
}

fn flag(q: &HashMap<String, String>, k: &str) -> Option<bool> {
    q.get(k).map(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

/// The git service for a request's project and worktree, or `None` (404).
pub async fn service(state: &AppState, pid: &str, wt: Option<&str>) -> Option<GitService> {
    let rt = state.projects.get(pid)?;
    let folder = rt.resolve_worktree(wt).await?;
    Some(GitService::new(folder))
}

/// `GitEndpoints.Guard`: argument and state errors become exit code 2.
pub fn guard(r: Result<GitResult, GitError>) -> GitResult {
    r.unwrap_or_else(|e| GitResult::failed(2, &e.to_string()))
}

pub fn to_result(r: &GitResult) -> Response {
    let status = if r.ok() {
        StatusCode::OK
    } else if r.exit_code == 2 {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::CONFLICT
    };
    (
        status,
        axum::Json(json!({"ok": r.ok(), "output": r.output, "error": r.error, "code": r.code})),
    )
        .into_response()
}

pub fn routes() -> Router<Arc<AppState>> {
    let g = "/api/projects/{pid}/git";
    Router::new()
        .route(&format!("{g}/status"), get(status))
        .route(&format!("{g}/diff"), get(diff))
        .route(&format!("{g}/head"), get(head))
        .route(&format!("{g}/stage"), post(stage))
        .route(&format!("{g}/unstage"), post(unstage))
        .route(&format!("{g}/commit"), post(commit))
        .route(&format!("{g}/fetch"), post(fetch))
        .route(&format!("{g}/pull"), post(pull))
        .route(&format!("{g}/push"), post(push))
        .route(&format!("{g}/branches"), get(list_branches))
        .route(&format!("{g}/stashes"), get(list_stashes))
        .route(&format!("{g}/{{*op}}"), post(branch_op))
        .route(
            "/api/projects/{pid}/worktrees",
            post(add_worktree).delete(remove_worktree),
        )
        .route(
            "/api/projects/{pid}/github",
            get(get_token).put(put_token).delete(delete_token),
        )
        .route("/api/github/validate", post(validate))
}

async fn status(State(s): State<Arc<AppState>>, Path(pid): Path<String>, Query(q): Q) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    let st = svc.status().await;
    let merge = if st.is_repo {
        svc.merge_state().await.ok()
    } else {
        None
    };
    let files: Vec<Value> = st
        .files
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "origPath": f.orig_path,
                "index": f.index.to_string(),
                "workTree": f.work_tree.to_string(),
                "staged": f.staged(),
                "unstaged": f.unstaged(),
                "untracked": f.untracked(),
                "conflicted": f.conflicted(),
            })
        })
        .collect();
    axum::Json(json!({
        "merging": merge.as_ref().is_some_and(|m| m.merging),
        "mergeBranch": merge.and_then(|m| m.branch),
        "isRepo": st.is_repo,
        "root": st.root,
        "branch": st.branch,
        "upstream": st.upstream,
        "ahead": st.ahead,
        "behind": st.behind,
        "error": st.error,
        "files": files,
    }))
    .into_response()
}

async fn diff(State(s): State<Arc<AppState>>, Path(pid): Path<String>, Query(q): Q) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    let Some(path) = q.get("path") else {
        return bad_request("path is required.");
    };
    match svc
        .diff(
            path,
            flag(&q, "staged").unwrap_or(false),
            q.get("origPath").map(String::as_str),
        )
        .await
    {
        Ok(Some(d)) => axum::Json(d).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn head(State(s): State<Arc<AppState>>, Path(pid): Path<String>, Query(q): Q) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    let path = q.get("path").cloned().unwrap_or_default();
    match svc.head_content(&path).await {
        Ok((exists, text, binary)) => {
            axum::Json(json!({"path": path, "exists": exists, "content": text, "binary": binary}))
                .into_response()
        }
        Err(e) => bad_request(&e.to_string()),
    }
}

fn paths_of(body: &Value) -> Vec<String> {
    body["paths"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

async fn stage(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    to_result(&guard(svc.stage(&paths_of(&body)).await))
}

async fn unstage(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    to_result(&guard(svc.unstage(&paths_of(&body)).await))
}

async fn commit(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let wt = q.get("wt").map(String::as_str);
    let Some(svc) = service(&s, &pid, wt).await else {
        return not_found();
    };
    let message = body["message"].as_str().unwrap_or_default();
    let r = guard(svc.commit(message).await);
    s.audit.write(
        Some(&caller(addr)),
        "git.commit",
        json!({"project": pid, "worktree": wt, "message": AuditLog::clip(message, 120)}),
    );
    to_result(&r)
}

async fn remote_op(
    s: &AppState,
    pid: &str,
    wt: Option<&str>,
    op: &str,
    addr: SocketAddr,
) -> Response {
    let Some(svc) = service(s, pid, wt).await else {
        return not_found();
    };
    let r = match s.projects.git_token(pid).await {
        Ok(token) => guard(svc.remote(op, token.as_deref()).await),
        Err(e) => GitResult::failed(2, &e),
    };
    s.audit.write(
        Some(&caller(addr)),
        &format!("git.{op}"),
        json!({"project": pid, "worktree": wt, "ok": r.ok()}),
    );
    to_result(&r)
}

async fn fetch(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    remote_op(&s, &pid, q.get("wt").map(String::as_str), "fetch", addr).await
}

async fn pull(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    remote_op(&s, &pid, q.get("wt").map(String::as_str), "pull", addr).await
}

async fn push(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    remote_op(&s, &pid, q.get("wt").map(String::as_str), "push", addr).await
}

async fn list_branches(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    match svc.branches().await {
        Ok(b) => axum::Json(b).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn list_stashes(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
) -> Response {
    let Some(svc) = service(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    match svc.stashes().await {
        Ok(b) => axum::Json(b).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

fn s_of(body: &Value, k: &str) -> Option<String> {
    body.get(k).and_then(Value::as_str).map(str::to_string)
}

fn b_of(body: &Value, k: &str, default: bool) -> bool {
    body.get(k).and_then(Value::as_bool).unwrap_or(default)
}

/// The branch menu's POSTs: one git change in the request's worktree, audit-logged as git.<name>.
async fn branch_op(
    State(s): State<Arc<AppState>>,
    Path((pid, op)): Path<(String, String)>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    let wt = q.get("wt").map(String::as_str);
    let known = [
        "checkout",
        "branch",
        "branch/rename",
        "branch/delete",
        "publish",
        "merge",
        "merge/abort",
        "merge/continue",
        "merge/resolve",
        "stash",
        "stash/apply",
        "stash/pop",
        "stash/drop",
    ];
    if !known.contains(&op.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(svc) = service(&s, &pid, wt).await else {
        return not_found();
    };
    let no_body = matches!(op.as_str(), "publish" | "merge/abort" | "merge/continue");
    let req: Value = if no_body {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) if v.is_object() => v,
            _ => return bad_request("Bad request body."),
        }
    };
    let token = match s.projects.git_token(&pid).await {
        Ok(t) => t,
        Err(e) => {
            let r = GitResult::failed(2, &e);
            return to_result(&r);
        }
    };
    let mut fields = serde_json::Map::new();
    let name = match op.as_str() {
        "branch" => "branch.create".to_string(),
        "stash" => "stash.push".to_string(),
        o => o.replace('/', "."),
    };
    let r = match op.as_str() {
        "checkout" => {
            let branch = s_of(&req, "branch").unwrap_or_default();
            let stash = b_of(&req, "stash", false);
            fields.insert("branch".into(), json!(branch));
            fields.insert("stash".into(), json!(stash));
            guard(svc.checkout(&branch, stash).await)
        }
        "branch" => {
            let name = s_of(&req, "name").unwrap_or_default();
            let from = s_of(&req, "from");
            let checkout = b_of(&req, "checkout", true);
            fields.insert("branch".into(), json!(name));
            fields.insert("from".into(), json!(from));
            fields.insert("checkout".into(), json!(checkout));
            guard(svc.create_branch(&name, from.as_deref(), checkout).await)
        }
        "branch/rename" => {
            let name = s_of(&req, "name").unwrap_or_default();
            let new_name = s_of(&req, "newName").unwrap_or_default();
            fields.insert("branch".into(), json!(name));
            fields.insert("newName".into(), json!(new_name));
            guard(svc.rename_branch(&name, &new_name).await)
        }
        "branch/delete" => {
            let name = s_of(&req, "name").unwrap_or_default();
            let force = b_of(&req, "force", false);
            fields.insert("branch".into(), json!(name));
            fields.insert("force".into(), json!(force));
            guard(svc.delete_branch(&name, force).await)
        }
        "publish" => guard(svc.publish(token.as_deref()).await),
        "merge" => {
            let branch = s_of(&req, "branch").unwrap_or_default();
            fields.insert("branch".into(), json!(branch));
            guard(svc.merge(&branch).await)
        }
        "merge/abort" => guard(svc.abort_merge().await),
        "merge/continue" => guard(svc.continue_merge().await),
        "merge/resolve" => {
            let paths = paths_of(&req);
            fields.insert("paths".into(), json!(paths));
            guard(svc.stage(&paths).await)
        }
        "stash" => {
            let message = s_of(&req, "message");
            fields.insert(
                "message".into(),
                json!(AuditLog::clip(message.as_deref().unwrap_or(""), 120)),
            );
            guard(svc.stash(message.as_deref()).await)
        }
        stash_op => {
            let index = req.get("index").and_then(Value::as_i64).unwrap_or(-1) as i32;
            fields.insert("index".into(), json!(index));
            guard(svc.stash_op(&stash_op["stash/".len()..], index).await)
        }
    };
    let mut entry = serde_json::Map::new();
    entry.insert("project".into(), json!(pid));
    entry.insert("worktree".into(), json!(wt));
    entry.insert("ok".into(), json!(r.ok()));
    entry.insert("code".into(), json!(r.code));
    entry.extend(fields);
    s.audit.write(
        Some(&caller(addr)),
        &format!("git.{name}"),
        Value::Object(entry),
    );
    to_result(&r)
}

async fn add_worktree(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<Value>,
) -> Response {
    let Some(svc) = service(&s, &pid, None).await else {
        return not_found();
    };
    let branch = s_of(&req, "branch").unwrap_or_default();
    let new_branch = b_of(&req, "newBranch", false);
    let from = s_of(&req, "from");
    let r = guard(svc.add_worktree(&branch, new_branch, from.as_deref()).await);
    if let Some(rt) = s.projects.get(&pid) {
        rt.forget_worktrees().await;
    }
    s.audit.write(
        Some(&caller(addr)),
        "git.worktree.add",
        json!({"project": pid, "branch": branch, "newBranch": new_branch, "from": from, "ok": r.ok(), "path": if r.ok() { Some(&r.output) } else { None }}),
    );
    to_result(&r)
}

async fn remove_worktree(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(svc) = service(&s, &pid, None).await else {
        return not_found();
    };
    let path = q.get("path").cloned().unwrap_or_default();
    let force = flag(&q, "force") == Some(true);
    let r = guard(svc.remove_worktree(&path, force).await);
    if let Some(rt) = s.projects.get(&pid) {
        rt.forget_worktrees().await;
    }
    s.audit.write(
        Some(&caller(addr)),
        "git.worktree.remove",
        json!({"project": pid, "path": path, "force": force, "ok": r.ok(), "code": r.code}),
    );
    to_result(&r)
}

async fn get_token(State(s): State<Arc<AppState>>, Path(pid): Path<String>) -> Response {
    if s.projects.get(&pid).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "No such project."})),
        )
            .into_response();
    }
    match s.projects.tokens.get(&pid) {
        Some(t) if t == APP_LINK => axum::Json(json!({
            "token": match s.github_app.account().and_then(|a| a.login) {
                Some(who) => format!("GitHub sign-in (@{who})"),
                None => "GitHub sign-in (signed out)".to_string(),
            },
            "githubApp": true,
        }))
        .into_response(),
        Some(t) if t.starts_with(crate::git::github_user::USER_LINK_PREFIX) => axum::Json(json!({
            "token": "Your GitHub sign-in (Clone from GitHub)",
            "githubApp": false,
            "githubUser": true,
        }))
        .into_response(),
        t => axum::Json(json!({"token": t.map(|t| tokens::mask(&t)), "githubApp": false}))
            .into_response(),
    }
}

async fn put_token(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<Value>,
) -> Response {
    if s.projects.get(&pid).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "No such project."})),
        )
            .into_response();
    }
    match s
        .projects
        .tokens
        .set(&pid, req["token"].as_str().unwrap_or(""))
    {
        Ok(()) => {
            s.audit.write(
                Some(&caller(addr)),
                "github.token_set",
                json!({"project": pid}),
            );
            let t = s.projects.tokens.get(&pid).unwrap_or_default();
            axum::Json(json!({"token": tokens::mask(&t)})).into_response()
        }
        Err(e) => bad_request(&e),
    }
}

async fn delete_token(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let removed = s.projects.tokens.remove(&pid);
    s.audit.write(
        Some(&caller(addr)),
        "github.token_removed",
        json!({"project": pid, "removed": removed}),
    );
    axum::Json(json!({"ok": removed})).into_response()
}

/// "Validate token": a token typed in the wizard, or the one saved for a project; against a
/// URL or the project's origin.
async fn validate(State(s): State<Arc<AppState>>, axum::Json(req): axum::Json<Value>) -> Response {
    let pid = s_of(&req, "pid");
    let token = match s_of(&req, "token").filter(|t| !t.trim().is_empty()) {
        Some(t) => Some(t.trim().to_string()),
        None => match &pid {
            Some(p) => match s.projects.git_token(p).await {
                Ok(t) => t,
                Err(e) => return axum::Json(json!({"ok": false, "error": e})).into_response(),
            },
            None => None,
        },
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return bad_request("No token to check.");
    };
    let mut url = s_of(&req, "url");
    if url.is_none()
        && let Some(rt) = pid.as_deref().and_then(|p| s.projects.get(p))
    {
        url = GitService::new(rt.root()).origin().await;
    }
    let r = github_check(&s.http, &s.options.github_api, &token, url.as_deref()).await;
    axum::Json(r).into_response()
}

/// `GitHubCheck.ValidateAsync`: who the token is (GET /user) and, given a repository, whether
/// it can read and push.
pub async fn github_check(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    repo_url: Option<&str>,
) -> Value {
    let get = |path: String| {
        let req = http
            .get(format!("{}{path}", api.trim_end_matches('/')))
            .bearer_auth(token)
            .header("User-Agent", "OpenOrca-WebUI")
            .header("Accept", "application/vnd.github+json")
            .timeout(std::time::Duration::from_secs(20));
        async move {
            let res = req.send().await?;
            let status = res.status().as_u16();
            let body = res.json::<Value>().await.ok();
            Ok::<_, reqwest::Error>((status, body))
        }
    };
    let fail = |e: reqwest::Error| json!({"ok": false, "login": null, "repo": null, "canPush": null, "error": format!("Couldn't reach GitHub: {e}")});
    let (st, user) = match get("/user".into()).await {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    if st != 200 {
        let error = if st == 401 {
            "GitHub rejected the token (expired or revoked?).".to_string()
        } else {
            format!("GitHub answered {st}.")
        };
        return json!({"ok": false, "login": null, "repo": null, "canPush": null, "error": error});
    }
    let login = user
        .as_ref()
        .and_then(|u| u["login"].as_str())
        .map(str::to_string);
    let Some(repo) = owner_repo(repo_url) else {
        return json!({"ok": true, "login": login, "repo": null, "canPush": null, "error": null});
    };
    let (rs, body) = match get(format!("/repos/{repo}")).await {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    if rs != 200 {
        return json!({"ok": false, "login": login, "repo": repo, "canPush": null,
            "error": format!("The token can't see {repo} (GitHub answered {rs}). Give it access to this repository.")});
    }
    let push = body
        .as_ref()
        .and_then(|b| b["permissions"]["push"].as_bool())
        .unwrap_or(false);
    json!({"ok": true, "login": login, "repo": repo, "canPush": push,
        "error": if push { None } else { Some("Read-only: the token can't push to this repository.") }})
}

/// "owner/repo" from a github.com URL (https or ssh form), else `None`.
pub fn owner_repo(url: Option<&str>) -> Option<String> {
    use std::sync::OnceLock;
    static URL: OnceLock<regex::Regex> = OnceLock::new();
    static BARE: OnceLock<regex::Regex> = OnceLock::new();
    let url = url?.trim();
    if url.is_empty() {
        return None;
    }
    let re = URL.get_or_init(|| {
        regex::Regex::new(
            r"github\.com[/:]([A-Za-z0-9][A-Za-z0-9-]*)/([A-Za-z0-9._-]+?)(?:\.git)?/?$",
        )
        .unwrap()
    });
    if let Some(m) = re.captures(url) {
        return Some(format!("{}/{}", &m[1], &m[2]));
    }
    BARE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$").unwrap())
        .is_match(url)
        .then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_repo_reads_https_ssh_and_bare_forms() {
        assert_eq!(
            owner_repo(Some("https://github.com/acme/tool.git")).as_deref(),
            Some("acme/tool")
        );
        assert_eq!(
            owner_repo(Some("git@github.com:acme/tool.git")).as_deref(),
            Some("acme/tool")
        );
        assert_eq!(owner_repo(Some("acme/tool")).as_deref(), Some("acme/tool"));
        assert_eq!(owner_repo(Some("https://gitlab.com/a/b")), None);
    }
}
