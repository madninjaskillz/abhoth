//! `ProjectEndpoints`: the project list, the add-project wizard (browse, mkdir, clone) and each
//! project's files (tree, changes, markers, read/save).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::browser::{self, BrowseError};
use super::manager::ProjectRuntime;
use crate::audit::Caller;
use crate::files::service::{FileError, FileService};
use crate::git::service::{GitFile, GitService};
use crate::paths;
use crate::server::AppState;

type Q = Query<HashMap<String, String>>;

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "No such project or worktree.")
}

fn forbidden() -> Response {
    err(StatusCode::FORBIDDEN, "That path is outside the project.")
}

fn caller(addr: SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: None,
    }
}

pub fn describe(rt: &ProjectRuntime) -> Value {
    let root = rt.root();
    json!({
        "id": rt.id(),
        "name": rt.info.name,
        "path": root.to_string_lossy(),
        "primary": rt.is_primary,
        "mode": rt.session.mode(),
        "exists": root.is_dir(),
        "readOnly": rt.read_only,
    })
}

/// One word for the tree's marker: conflict, added, deleted, renamed, untracked or modified.
pub fn kind(f: &GitFile) -> &'static str {
    if f.conflicted() {
        "conflict"
    } else if f.untracked() {
        "untracked"
    } else if f.index == 'A' {
        "added"
    } else if f.index == 'D' || f.work_tree == 'D' {
        "deleted"
    } else if matches!(f.index, 'R' | 'C') {
        "renamed"
    } else {
        "modified"
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    let p = "/api/projects/{pid}";
    Router::new()
        .route("/api/projects", get(list).post(add))
        .route(p, delete(remove))
        .route("/api/fs/browse", get(browse))
        .route("/api/fs/mkdir", post(mkdir))
        .route("/api/projects/clone", post(clone))
        .route("/api/clone/{id}", get(clone_status).delete(clone_cancel))
        .route(&format!("{p}/worktrees"), get(worktrees))
        .route(&format!("{p}/tree"), get(tree))
        .route(&format!("{p}/changes"), get(changes))
        .route(&format!("{p}/markers"), get(markers))
        .route(&format!("{p}/file"), get(read_file).put(write_file))
}

async fn list(State(s): State<Arc<AppState>>) -> Response {
    let projects: Vec<Value> = s.projects.list().iter().map(|r| describe(r)).collect();
    axum::Json(json!({
        "primary": s.projects.primary.id,
        "canAdd": s.projects.can_add_projects(),
        "projects": projects,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AddFolderRequest {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

async fn add(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<AddFolderRequest>,
) -> Response {
    match s
        .projects
        .add(req.path.as_deref().unwrap_or(""), req.name.as_deref())
    {
        Ok(rt) => {
            s.audit.write(
                Some(&caller(addr)),
                "project.add",
                json!({"path": rt.root().to_string_lossy()}),
            );
            axum::Json(describe(&rt)).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn remove(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    s.audit.write(
        Some(&caller(addr)),
        "project.remove",
        json!({"project": pid}),
    );
    match s.projects.remove(&pid).await {
        Ok(true) => axum::Json(json!({"ok": true})).into_response(),
        Ok(false) => not_found(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

fn browse_error(e: BrowseError) -> Response {
    match e {
        BrowseError::BadRequest(m) | BrowseError::Io(m) => err(StatusCode::BAD_REQUEST, &m),
        BrowseError::NotFound(m) => err(StatusCode::NOT_FOUND, &m),
        BrowseError::Exists(m) => err(StatusCode::CONFLICT, &m),
        BrowseError::Denied => err(StatusCode::FORBIDDEN, "Access denied."),
    }
}

async fn browse(Query(q): Q) -> Response {
    match browser::list(q.get("path").map(String::as_str)).await {
        Ok(l) => axum::Json(l).into_response(),
        Err(e) => browse_error(e),
    }
}

#[derive(Deserialize)]
struct CreateFolderRequest {
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

async fn mkdir(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<CreateFolderRequest>,
) -> Response {
    match browser::create_folder(req.parent.as_deref(), req.name.as_deref()) {
        Ok(path) => {
            s.audit
                .write(Some(&caller(addr)), "fs.mkdir", json!({"path": path}));
            axum::Json(json!({"path": path})).into_response()
        }
        Err(e) => browse_error(e),
    }
}

#[derive(Deserialize)]
struct CloneRequest {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

async fn clone(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<CloneRequest>,
) -> Response {
    let url = req.url.clone().unwrap_or_default();
    match s.clones.start(
        &url,
        req.parent.as_deref().unwrap_or(""),
        req.name.as_deref(),
        req.token.as_deref(),
        req.branch.as_deref(),
    ) {
        Ok(job) => {
            s.audit.write(
                Some(&caller(addr)),
                "project.clone",
                json!({"url": url, "destination": job.destination}),
            );
            axum::Json(job).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn clone_status(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match s.clones.get(&id) {
        Some(st) => axum::Json(st).into_response(),
        None => err(StatusCode::NOT_FOUND, "No such clone."),
    }
}

async fn clone_cancel(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    axum::Json(json!({"ok": s.clones.cancel(&id)})).into_response()
}

async fn worktrees(State(s): State<Arc<AppState>>, Path(pid): Path<String>) -> Response {
    match s.projects.get(&pid) {
        Some(rt) => axum::Json(rt.worktrees().await).into_response(),
        None => not_found(),
    }
}

/// The project and the folder (`wt`, or the project's root) a request is about.
async fn folder(
    s: &AppState,
    pid: &str,
    wt: Option<&str>,
) -> Option<(Arc<ProjectRuntime>, PathBuf)> {
    let rt = s.projects.get(pid)?;
    let folder = rt.resolve_worktree(wt).await?;
    folder.is_dir().then_some((rt, folder))
}

async fn tree(State(s): State<Arc<AppState>>, Path(pid): Path<String>, Query(q): Q) -> Response {
    let Some((rt, folder)) = folder(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    rt.ensure_watching(&folder);
    let ignored = q
        .get("ignored")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    match FileService::new(&folder)
        .list(q.get("dir").map(String::as_str), ignored)
        .await
    {
        Ok(l) => axum::Json(l).into_response(),
        Err(FileError::Forbidden(m)) if m.starts_with("Can't read") => {
            err(StatusCode::FORBIDDEN, &m)
        }
        Err(FileError::Forbidden(_)) => forbidden(),
        Err(FileError::NotFound(m)) => err(StatusCode::NOT_FOUND, &m),
        Err(FileError::Io(m)) => err(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

/// Changed files for the tree's markers, relative to the worktree folder shown.
async fn changes(State(s): State<Arc<AppState>>, Path(pid): Path<String>, Query(q): Q) -> Response {
    let Some((rt, folder)) = folder(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    rt.ensure_watching(&folder);
    let status = GitService::new(folder.clone()).status().await;
    let mut files = Vec::new();
    if status.is_repo
        && let Some(root) = &status.root
    {
        let folder_s = folder.to_string_lossy().to_string();
        for f in &status.files {
            let full = paths::normalize_path(&PathBuf::from(root).join(&f.path));
            let full_s = full.to_string_lossy().to_string();
            if !paths::is_under(&full, &folder) || paths::same_path(&full_s, &folder_s) {
                continue;
            }
            files.push(json!({
                "path": paths::to_relative(&folder, &full),
                "kind": kind(f),
                "index": f.index.to_string(),
                "workTree": f.work_tree.to_string(),
            }));
        }
    }
    axum::Json(json!({
        "isRepo": status.is_repo,
        "branch": status.branch,
        "folder": folder.to_string_lossy(),
        "files": files,
    }))
    .into_response()
}

async fn markers(State(s): State<Arc<AppState>>, Path(pid): Path<String>) -> Response {
    match s.projects.get(&pid) {
        Some(rt) => axum::Json(rt.edits.snapshot()).into_response(),
        None => not_found(),
    }
}

async fn read_file(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
) -> Response {
    let Some((_, folder)) = folder(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    let Some(path) = q.get("path") else {
        return err(StatusCode::BAD_REQUEST, "path is required.");
    };
    match FileService::new(&folder).read(path).await {
        Ok(c) => axum::Json(c).into_response(),
        Err(FileError::Forbidden(_)) => forbidden(),
        Err(FileError::NotFound(m)) => err(StatusCode::NOT_FOUND, &m),
        Err(FileError::Io(m)) => err(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

#[derive(Deserialize)]
struct WriteFileRequest {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    bom: bool,
}

async fn write_file(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Q,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<WriteFileRequest>,
) -> Response {
    let Some((_, folder)) = folder(&s, &pid, q.get("wt").map(String::as_str)).await else {
        return not_found();
    };
    let content = req.content.unwrap_or_default();
    match FileService::new(&folder)
        .write(&req.path, &content, req.etag.as_deref(), req.bom)
        .await
    {
        Ok(r) => {
            s.audit.write(
                Some(&caller(addr)),
                "file.write",
                json!({"project": pid, "worktree": folder.to_string_lossy(), "path": req.path,
                    "ok": r.ok, "bytes": crate::util::utf16_len(&content)}),
            );
            let status = if r.ok {
                StatusCode::OK
            } else if r.conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, axum::Json(r)).into_response()
        }
        Err(FileError::Forbidden(_)) => forbidden(),
        Err(FileError::NotFound(m)) | Err(FileError::Io(m)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"ok": false, "error": m})),
        )
            .into_response(),
    }
}
