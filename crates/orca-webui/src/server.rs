//! `WebUiServer`: the axum host inside the CLI process, serving the embedded React client, the
//! hub over the live sessions, the terminal websocket and the API. Every request needs the
//! token (see [`crate::auth`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State, WebSocketUpgrade};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;

use crate::audit::{AuditLog, Caller};
use crate::hub::Hub;
use crate::live::{LiveSession, SessionFactory};
use crate::projects::manager::ProjectManager;
use crate::projects::store::ProjectStore;
use crate::settings::SettingsSchema;

/// The folder generated images are saved in, asked on each request.
pub type ImageDirFn = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// How to host the web UI (`WebUiOptions`).
#[derive(Clone)]
pub struct WebUiOptions {
    /// Address to bind: "127.0.0.1" (default, this machine only) or "0.0.0.0" (the LAN).
    pub host: String,
    /// TCP port; 0 picks a free one (tests).
    pub port: u16,
    /// The access token every request must carry (query, cookie or header).
    pub token: String,
    /// The OpenOrca home whose config.json the settings page's server defaults edit.
    pub config_home: Option<PathBuf>,
    /// Shell for the terminal panel (`None`: pwsh, then Windows PowerShell, then cmd; $SHELL elsewhere).
    pub shell: Option<String>,
    /// The project list file (`None`: projects.json in the OpenOrca home).
    pub projects_file: Option<PathBuf>,
    /// Makes the session for another project folder; `None`: one project only.
    pub session_factory: Option<SessionFactory>,
    /// Audit log (JSON lines); `None`: none (token mode).
    pub audit_file: Option<PathBuf>,
    /// The inference server's profiles API; `None` = off.
    pub profiles_url: Option<String>,
    pub profiles_cache_file: Option<PathBuf>,
    pub image_directory: Option<ImageDirFn>,
    /// `ConfigSchema` for the settings page (`None`: the page reports it unavailable).
    pub settings: Option<Arc<dyn SettingsSchema>>,
    /// Serve the client from this folder instead of the embedded build.
    pub client_dir: Option<PathBuf>,
    /// How long a dropped hub connection can resume.
    pub resume_grace: Duration,
    /// GitHub's API (token validation); OPENORCA_GITHUB_API_URL overrides it.
    pub github_api: String,
    /// The bench runner (Benchmarks page); `None` = off.
    pub bench: Option<crate::bench::BenchOptions>,
}

impl WebUiOptions {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8192,
            token: token.into(),
            config_home: None,
            shell: None,
            projects_file: None,
            session_factory: None,
            audit_file: None,
            profiles_url: None,
            profiles_cache_file: None,
            image_directory: None,
            settings: None,
            client_dir: None,
            resume_grace: Duration::from_secs(30),
            github_api: std::env::var("OPENORCA_GITHUB_API_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://api.github.com".into()),
            bench: None,
        }
    }
}

/// Shared by every handler.
pub struct AppState {
    pub options: WebUiOptions,
    pub projects: Arc<ProjectManager>,
    pub hub: Arc<Hub>,
    pub audit: Arc<AuditLog>,
    /// The bound port (the cookie name carries it).
    pub port: u16,
    pub ptys: Arc<crate::pty::PtyRegistry>,
    pub previews: crate::files::preview::PreviewKeys,
    pub profiles: Option<Arc<orca_core::profiles::ProfileService>>,
    pub bench: Option<Arc<crate::bench::BenchRunner>>,
    pub http: reqwest::Client,
    pub clones: crate::projects::clone::CloneService,
}

#[derive(rust_embed::RustEmbed)]
#[folder = "../../../src/OpenOrca.WebUi/client/dist"]
struct ClientDist;

/// The server failed to start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("{0}")]
    BadOptions(String),
    #[error("Could not listen on {host}:{port}: {message}")]
    Listen {
        host: String,
        port: u16,
        message: String,
    },
}

pub struct WebUiServer {
    state: Arc<AppState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    base_url: String,
}

/// `WebUiServer.ParseAddress`.
pub fn parse_address(host: &str) -> Result<IpAddr, String> {
    match host {
        "localhost" | "" => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        "*" | "0.0.0.0" => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        "::" => Ok(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
        _ => host.trim_matches(['[', ']']).parse().map_err(|_| {
            format!("Not an IP address: {host} (use 127.0.0.1, 0.0.0.0 or a local address).")
        }),
    }
}

impl WebUiServer {
    /// Start listening.
    pub async fn start(
        options: WebUiOptions,
        session: Arc<dyn LiveSession>,
    ) -> Result<Self, StartError> {
        if options.token.is_empty() {
            return Err(StartError::BadOptions("A token is required.".into()));
        }
        let address = parse_address(&options.host).map_err(StartError::BadOptions)?;
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(address, options.port))
            .await
            .map_err(|e| StartError::Listen {
                host: options.host.clone(),
                port: options.port,
                message: e.to_string(),
            })?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(options.port);

        let store = ProjectStore::new(
            options
                .projects_file
                .clone()
                .unwrap_or_else(ProjectStore::default_path),
        );
        let projects = ProjectManager::new(store, session, options.session_factory.clone());
        let hub = Hub::new(options.resume_grace);
        hub.attach_projects(&projects);
        let audit = Arc::new(AuditLog::new(options.audit_file.clone()));
        let profiles = options.profiles_url.as_deref().map(|url| {
            Arc::new(orca_core::profiles::ProfileService::new(
                Some(url),
                options.profiles_cache_file.clone(),
                Duration::from_secs(30),
            ))
        });
        let bench = options.bench.clone().map(|b| {
            crate::bench::BenchRunner::new(b, projects.clone(), audit.clone(), profiles.clone())
        });
        let clones = crate::projects::clone::CloneService::new(projects.clone());
        let state = Arc::new(AppState {
            options: options.clone(),
            clones,
            projects,
            hub,
            audit,
            port,
            ptys: Arc::new(crate::pty::PtyRegistry::default()),
            previews: crate::files::preview::PreviewKeys::new(),
            profiles,
            bench,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        });
        let app = router(state.clone());

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });
        let mut host = if matches!(options.host.as_str(), "0.0.0.0" | "::" | "*") {
            "localhost".to_string()
        } else {
            options.host.clone()
        };
        if host.contains(':') && !host.starts_with('[') {
            host = format!("[{host}]");
        }
        Ok(Self {
            state,
            shutdown: Some(tx),
            task: Some(task),
            base_url: format!("http://{host}:{port}/"),
        })
    }

    pub fn port(&self) -> u16 {
        self.state.port
    }

    /// Base URL as bound ("http://127.0.0.1:8192/").
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// URL with the token, to print or open.
    pub fn url_with_token(&self) -> String {
        format!(
            "{}?token={}",
            self.base_url,
            crate::util::escape_data(&self.state.options.token)
        )
    }

    pub fn projects(&self) -> &Arc<ProjectManager> {
        &self.state.projects
    }

    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Stop: the terminals close, open sockets drop, other projects' sessions stop.
    pub async fn stop(&mut self) {
        let Some(tx) = self.shutdown.take() else {
            return;
        };
        self.state.ptys.close_all();
        self.state.hub.stop();
        let _ = tx.send(());
        if let Some(task) = self.task.take() {
            // Websockets keep a graceful shutdown waiting: give it a moment, then force it.
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
        if let Some(b) = &self.state.bench {
            b.stop_all().await;
        }
        self.state.projects.shutdown().await;
    }
}

impl Drop for WebUiServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            self.state.ptys.close_all();
            self.state.hub.stop();
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    let mut api: Router<Arc<AppState>> = Router::new()
        .route("/api/info", get(info))
        .route("/hub/ws", get(hub_socket))
        .merge(crate::projects::endpoints::routes())
        .merge(crate::git::endpoints::routes())
        .merge(crate::settings::routes())
        .merge(crate::files::attachments::routes())
        .merge(crate::files::preview::routes())
        .merge(crate::pty::routes());
    if state.options.profiles_url.is_some() {
        api = api.merge(crate::profiles::routes());
    }
    if state.bench.is_some() {
        api = api.merge(crate::bench::endpoints::routes());
    }
    api.fallback(static_or_index)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::token_auth,
        ))
        .with_state(state)
}

async fn info(State(state): State<Arc<AppState>>) -> Response {
    let primary = state.projects.get(&state.projects.primary.id);
    let (wd, mode) = primary
        .map(|p| (p.session.working_directory(), p.session.mode()))
        .unwrap_or_default();
    axum::Json(json!({
        "workingDirectory": wd,
        "mode": mode,
        "primary": state.projects.primary.id,
        "canAddProjects": state.projects.can_add_projects(),
        "caseInsensitivePaths": crate::paths::case_insensitive(),
        "version": env!("CARGO_PKG_VERSION"),
        "terminal": crate::pty::is_supported(),
        "auth": "token",
        "bench": state.bench.is_some(),
        "profiles": state.options.profiles_url.is_some(),
        // The React client picks its hub transport from this (absent: the C# SignalR hub).
        "hub": "ws",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct HubQuery {
    resume: Option<String>,
    last: Option<u64>,
}

async fn hub_socket(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HubQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    let caller = Caller {
        ip: Some(addr.ip().to_string()),
        user: None,
    };
    let hub = state.hub.clone();
    ws.max_message_size(crate::hub::MAX_RECEIVE_BYTES)
        .on_upgrade(move |socket| hub.serve_socket(socket, state, q.resume, q.last, caller))
}

/// A file of the client build, from the override folder or the embedded copy.
fn client_file(state: &AppState, rel: &str) -> Option<Vec<u8>> {
    if rel.split('/').any(|p| p == ".." || p.is_empty()) && !rel.is_empty() {
        return None;
    }
    match &state.options.client_dir {
        Some(dir) => {
            let p = dir.join(rel);
            p.is_file().then(|| std::fs::read(p).ok()).flatten()
        }
        None => ClientDist::get(rel).map(|f| f.data.into_owned()),
    }
}

/// Static files (hashed assets cached for a year, index.html never), else index.html for
/// client-side routes; unknown /api and /hub paths are 404.
async fn static_or_index(State(state): State<Arc<AppState>>, method: Method, uri: Uri) -> Response {
    let path = uri.path();
    let is_api =
        path == "/api" || path.starts_with("/api/") || path == "/hub" || path.starts_with("/hub/");
    if is_api {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method == Method::GET || method == Method::HEAD {
        let mut rel = percent_encoding::percent_decode_str(path.trim_start_matches('/'))
            .decode_utf8_lossy()
            .into_owned();
        if rel.is_empty() || rel.ends_with('/') {
            rel.push_str("index.html");
        }
        if let Some(bytes) = client_file(&state, &rel) {
            let mime = mime_guess::from_path(&rel).first_or_octet_stream();
            let cache = if rel.ends_with("index.html") {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            return Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(bytes))
                .unwrap();
        }
    }
    match client_file(&state, "index.html") {
        Some(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(bytes))
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
