//! `WebUiServer`: the axum host inside the CLI process, serving the embedded React client, the
//! hub over the live sessions, the terminal websocket and the API. Every request needs a
//! signed-in user, an API key or the launch token (see [`crate::login`], D140).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State, WebSocketUpgrade};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
    /// The launch token: the URL this process prints or gives the desktop window carries it.
    /// It stands for the first admin (with `login.exposed`: it only unlocks the first-run setup).
    pub token: String,
    /// The OpenOrca home whose config.json the settings page's server defaults edit.
    pub config_home: Option<PathBuf>,
    /// Shell for the terminal panel (`None`: pwsh, then Windows PowerShell, then cmd; $SHELL elsewhere).
    pub shell: Option<String>,
    /// The project list file (`None`: projects.json in the OpenOrca home).
    pub projects_file: Option<PathBuf>,
    /// Makes the session for another project folder; `None`: one project only.
    pub session_factory: Option<SessionFactory>,
    /// Audit log (JSON lines); `None`: `webui/audit.log` in the state home.
    pub audit_file: Option<PathBuf>,
    /// Accounts: the database, exposure, 2FA policy, session limits (D140).
    pub login: crate::login::LoginOptions,
    /// The inference server's profiles API; `None` = off.
    pub profiles_url: Option<String>,
    /// `abhoth router`'s control plane: key introspection and the admin pages' proxy
    /// (ROUTER-CONFIG.md 11.1-11.2); `None` = not linked.
    pub router: Option<crate::router_admin::RouterLink>,
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
    /// github.com: the GitHub App's device flow and token endpoints, and clone URLs;
    /// OPENORCA_GITHUB_URL overrides it.
    pub github_web: String,
    /// The GitHub App for "Clone from GitHub" (`None`: `GitHubAppConfig::load` from the
    /// environment or github-app.json next to the project list, asked on each use).
    pub github_app: Option<crate::git::github_app::GitHubAppConfigFn>,
    /// The clock the GitHub sign-in reads (`None`: the system clock; tests).
    pub github_clock: Option<Arc<dyn crate::git::github_app::Clock>>,
    /// The OAuth app for "Sign in with GitHub" with each user's own account (`None`:
    /// `github.oauthClientId` in config.json, asked on each use; D147).
    pub github_oauth_client_id: Option<crate::git::github_user::ClientIdFn>,
    /// How this machine's `gh` / `git credential` are run (`None`: the real ones; tests).
    pub github_machine: Option<crate::git::github_user::MachineCredentials>,
    /// Whether this machine's GitHub sign-ins may be used (`None`: only for the desktop app or
    /// a local, non-exposed server; see `github_user::machine_credentials_allowed`).
    pub github_machine_allowed: Option<bool>,
    /// The bench runner (Benchmarks page); `None` = off.
    pub bench: Option<crate::bench::BenchOptions>,
    /// The Glance page's live feed and its optional public view; `None` = off.
    pub glance: Option<crate::glance::GlanceOptions>,
    /// Set when the server backs the desktop window (`abhoth --desktop`, D110).
    pub desktop: Option<DesktopHooks>,
    /// This machine (MACHINE.md) without the desktop window (every `--hostui` unless
    /// `--hostui-no-machine`, D156): its page at
    /// /machine.html for the host's admins. The desktop app passes its handler in [`DesktopHooks`].
    pub machine: Option<MachineFn>,
}

/// Picks a folder with the system dialog, starting in the given one; `None` when cancelled.
pub type PickFolderFn = Arc<
    dyn Fn(Option<PathBuf>) -> futures_util::future::BoxFuture<'static, Option<PathBuf>>
        + Send
        + Sync,
>;

/// What the desktop app adds to the web host (only for its own window: the listener is
/// loopback-only and the token never leaves the process).
#[derive(Clone)]
pub struct DesktopHooks {
    pub pick_folder: PickFolderFn,
    /// This machine (D155): `/api/admin/machine/*` and `/api/machine/*`, answered by the app's
    /// own in-process router and engines. `None`: not available.
    pub machine: Option<MachineFn>,
}

/// Answers a `/api/admin/machine/*` or `/api/machine/*` request (an admin's; checked first).
pub type MachineFn = Arc<
    dyn Fn(
            axum::http::Request<axum::body::Body>,
        ) -> futures_util::future::BoxFuture<'static, axum::response::Response>
        + Send
        + Sync,
>;

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
            login: crate::login::LoginOptions::default(),
            profiles_url: None,
            router: None,
            profiles_cache_file: None,
            image_directory: None,
            settings: None,
            client_dir: None,
            resume_grace: Duration::from_secs(30),
            github_api: orca_core::brand::env_var("GITHUB_API_URL")
                .ok_or(std::env::VarError::NotPresent)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://api.github.com".into()),
            github_web: orca_core::brand::env_var("GITHUB_URL")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://github.com".into()),
            github_app: None,
            github_clock: None,
            github_oauth_client_id: None,
            github_machine: None,
            github_machine_allowed: None,
            bench: None,
            glance: None,
            desktop: None,
            machine: None,
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
    pub bench_queue: Option<Arc<crate::bench::BenchQueue>>,
    /// The Glance feeds (SUPERVISOR.md 12); `None` = off.
    pub glance: Option<Arc<crate::glance::Glance>>,
    pub http: reqwest::Client,
    pub clones: crate::projects::clone::CloneService,
    /// The GitHub sign-in ("Clone from GitHub").
    pub github_app: Arc<crate::git::github_app::GitHubAppService>,
    /// Each user's own GitHub credentials ("Clone from GitHub" without an app, D147).
    pub github_user: Arc<crate::git::github_user::GitHubUserService>,
    /// The accounts.
    pub login: Arc<crate::login::LoginAuth>,
    /// Key requests waiting for an admin (D149, in memory).
    pub key_requests: crate::key_requests::KeyRequests,
    /// Sign-in with Abhoth (D154): codes and link requests waiting.
    pub sso: crate::sso::Sso,
}

#[derive(rust_embed::RustEmbed)]
#[folder = "$ABHOTH_CLIENT_DIST"]
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
        let state_home = projects.store().state_dir();
        let audit = Arc::new(AuditLog::new(
            options
                .audit_file
                .clone()
                .or_else(|| Some(state_home.join("webui").join("audit.log"))),
        ));
        let db_path = options
            .login
            .db_path
            .clone()
            .unwrap_or_else(|| state_home.join("abhoth.db"));
        let login = Arc::new(
            crate::login::LoginAuth::new(
                options.login.clone(),
                db_path,
                options.token.clone(),
                options.desktop.is_some(),
                audit.clone(),
            )
            .map_err(StartError::BadOptions)?,
        );
        {
            let pm = Arc::downgrade(&projects);
            hub.set_access(Arc::new(move |caller, pid| {
                pm.upgrade()
                    .and_then(|pm| pm.info(pid))
                    .is_none_or(|info| caller.can_access(info.owner))
            }));
        }
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
        let bench_queue = bench.as_ref().map(|b| {
            let q = crate::bench::BenchQueue::new(
                b.clone(),
                b.options.dir.clone(),
                profiles.clone(),
                true,
            );
            // Bench protection through the router's calendar, when a router is linked.
            if let Some(link) = options.router.clone() {
                q.set_protect(Arc::new(crate::router_admin::RouterBenchProtect { link }));
            }
            q
        });
        let glance = options.glance.clone().map(crate::glance::Glance::new);
        let clones = crate::projects::clone::CloneService::new(projects.clone());
        let github_app = {
            use crate::git::github_app::{GitHubAccountStore, GitHubAppConfig, GitHubAppService};
            let state_dir = projects.store().state_dir();
            let config = options.github_app.clone().unwrap_or_else(|| {
                let dir = state_dir.clone();
                Arc::new(move || GitHubAppConfig::load(&dir))
            });
            Arc::new(GitHubAppService::new(
                config,
                &options.github_web,
                &options.github_api,
                GitHubAccountStore::new(&state_dir),
                options.github_clock.clone(),
            ))
        };
        let _ = projects.github_app.set(github_app.clone());
        let github_user = {
            use crate::git::github_user::{
                GitHubUserOptions, GitHubUserService, MachineCredentials, load_client_id,
                machine_credentials_allowed,
            };
            let home = options.config_home.clone();
            let svc = Arc::new(GitHubUserService::new(GitHubUserOptions {
                web: options.github_web.clone(),
                api: options.github_api.clone(),
                state_dir: projects.store().state_dir(),
                machine: options
                    .github_machine
                    .clone()
                    .unwrap_or_else(MachineCredentials::system),
                machine_allowed: options
                    .github_machine_allowed
                    .unwrap_or_else(|| machine_credentials_allowed(&options)),
                client_id: options
                    .github_oauth_client_id
                    .clone()
                    .unwrap_or_else(|| Arc::new(move || load_client_id(home.as_deref()))),
            }));
            let l = Arc::downgrade(&login);
            svc.set_is_admin(Arc::new(move |uid| {
                l.upgrade()
                    .and_then(|l| l.db.user(uid))
                    .is_some_and(|u| u.admin && !u.disabled)
            }));
            svc
        };
        let _ = projects.github_user.set(github_user.clone());
        let state = Arc::new(AppState {
            github_app,
            github_user,
            options: options.clone(),
            clones,
            login,
            projects,
            hub,
            audit,
            port,
            ptys: Arc::new(crate::pty::PtyRegistry::default()),
            key_requests: Default::default(),
            sso: Default::default(),
            previews: crate::files::preview::PreviewKeys::new(),
            profiles,
            bench,
            bench_queue,
            glance,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        });
        if let Some(g) = &state.glance {
            g.start(state.http.clone(), state.options.router.clone())
                .map_err(StartError::BadOptions)?;
            if let (Some(p), Some(feed)) = (&g.options.public, g.public_feed()) {
                let dir = state.options.client_dir.clone();
                let files: crate::glance::public::ClientFiles =
                    Arc::new(move |rel: &str| client_file_in(dir.as_deref(), rel));
                let r = crate::glance::public::router(
                    feed,
                    p.clone(),
                    g.options.ping,
                    files,
                    state.options.login.trusted_proxies.clone(),
                );
                let (task, bound) =
                    crate::glance::public::serve(r, &p.listen)
                        .await
                        .map_err(|e| {
                            g.stop();
                            StartError::BadOptions(e)
                        })?;
                tracing::info!("public Glance on http://{bound}/");
                g.set_public_addr(bound);
                g.add_task(task);
            }
        }
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

    /// URL with the launch token, to print or open (it signs in as the first admin, or leads
    /// to the first-run setup).
    pub fn url_with_token(&self) -> String {
        format!(
            "{}?token={}",
            self.base_url,
            crate::util::escape_data(self.state.login.setup_code())
        )
    }

    /// No user exists yet: the first-run setup is open (to the launch token).
    pub fn setup_needed(&self) -> bool {
        self.state.login.setup_needed()
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
        if let Some(g) = &self.state.glance {
            g.stop();
        }
        if let Some(q) = &self.state.bench_queue {
            q.stop().await;
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
        if let Some(g) = &self.state.glance {
            g.stop();
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    let mut api: Router<Arc<AppState>> = Router::new()
        .route("/api/info", get(info))
        .route("/api/parts", get(parts_status))
        .route("/hub/ws", get(hub_socket))
        .merge(crate::projects::endpoints::routes())
        .merge(crate::git::endpoints::routes())
        .merge(crate::git::github_app_endpoints::routes())
        .merge(crate::git::github_user_endpoints::routes())
        .merge(crate::settings::routes())
        .merge(crate::connection::routes())
        .merge(crate::files::attachments::routes())
        .merge(crate::files::preview::routes())
        .merge(crate::pty::routes());
    if state.options.desktop.is_some() {
        api = api.route("/api/desktop/pick-folder", post(pick_folder));
    }
    if machine_fn(&state.options).is_some() {
        api = api
            .route("/api/admin/machine/{*rest}", axum::routing::any(machine))
            .route("/api/machine/{*rest}", axum::routing::any(machine));
    }
    if state.options.profiles_url.is_some() {
        api = api.merge(crate::profiles::routes());
    }
    if state.bench.is_some() {
        api = api.merge(crate::bench::endpoints::routes());
    }
    if state.glance.is_some() {
        api = api.merge(crate::glance::endpoints::routes());
    }
    api.merge(crate::login::routes())
        .merge(crate::router_admin::routes())
        .merge(crate::sso::routes())
        .fallback(static_or_index)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::login::login_auth,
        ))
        .with_state(state)
}

fn machine_fn(o: &WebUiOptions) -> Option<MachineFn> {
    o.desktop
        .as_ref()
        .and_then(|d| d.machine.clone())
        .or_else(|| o.machine.clone())
}

/// This machine (D155): admins only, then the app's handler.
async fn machine(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(f) = machine_fn(&state.options) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    if !crate::login::principal_of(&req).is_some_and(|p| p.admin) {
        return crate::login::error(
            axum::http::StatusCode::FORBIDDEN,
            "Only an admin can do that.",
        );
    }
    f(req).await
}

/// `POST /api/desktop/pick-folder` `{start}`: the system folder dialog (desktop window only).
async fn pick_folder(
    State(state): State<Arc<AppState>>,
    body: Option<axum::Json<serde_json::Value>>,
) -> Response {
    let Some(hooks) = state.options.desktop.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let start = body
        .and_then(|b| b.0["start"].as_str().map(PathBuf::from))
        .filter(|p| p.is_dir());
    let picked = (hooks.pick_folder)(start).await;
    axum::Json(json!({"path": picked.map(|p| p.to_string_lossy().into_owned())})).into_response()
}

async fn info(
    State(state): State<Arc<AppState>>,
    principal: Option<axum::Extension<crate::login::Principal>>,
) -> Response {
    let admin = principal.as_ref().is_some_and(|p| p.admin);
    let primary = state
        .projects
        .get(&state.projects.primary.id)
        .filter(|_| admin);
    let (wd, mode) = primary
        .map(|p| (p.session.working_directory(), p.session.mode()))
        .unwrap_or_default();
    let mut info = json!({
        "workingDirectory": wd,
        "mode": mode,
        "primary": if admin { state.projects.primary.id.clone() } else { String::new() },
        "canAddProjects": state.projects.can_add_projects(),
        "caseInsensitivePaths": crate::paths::case_insensitive(),
        "version": env!("CARGO_PKG_VERSION"),
        "terminal": crate::pty::is_supported(),
        "auth": "login",
        "user": principal.as_ref().map(|p| p.username.clone()),
        "admin": admin,
        "bench": state.bench.is_some(),
        "profiles": state.options.profiles_url.is_some(),
        "router": admin && state.options.router.is_some(),
        // This machine and Local Engines (D155, D156): admins, where the host runs them.
        "machine": admin && machine_fn(&state.options).is_some(),
        "glance": state.glance.is_some(),
        // The React client picks its hub transport from this (absent: the C# SignalR hub).
        "hub": "ws",
    });
    if state.options.desktop.is_some() {
        info["desktop"] = json!(true);
    }
    // The router / supervisor running alongside (single-container mode): state, restarts and
    // the last error. Admins only (errors can name files).
    let parts = crate::parts::all();
    if admin && !parts.is_empty() {
        info["parts"] = json!(parts);
    }
    axum::Json(info).into_response()
}

/// `GET /api/parts`: the extra parts' status (admins; `{}` when the web UI runs alone).
async fn parts_status(principal: Option<axum::Extension<crate::login::Principal>>) -> Response {
    if !principal.as_ref().is_some_and(|p| p.admin) {
        return (axum::http::StatusCode::FORBIDDEN, "admins only").into_response();
    }
    axum::Json(json!(crate::parts::all())).into_response()
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
    // Sign-in mode: the signed-in user and the client IP behind the proxy.
    let caller = crate::audit::request_caller().unwrap_or(Caller {
        ip: Some(addr.ip().to_string()),
        ..Default::default()
    });
    let hub = state.hub.clone();
    ws.max_message_size(crate::hub::MAX_RECEIVE_BYTES)
        .on_upgrade(move |socket| hub.serve_socket(socket, state, q.resume, q.last, caller))
}

/// A file of the client build, from the override folder or the embedded copy.
fn client_file(state: &AppState, rel: &str) -> Option<Vec<u8>> {
    client_file_in(state.options.client_dir.as_deref(), rel)
}

/// [`client_file`] for a given override folder (the public Glance listener reads it too).
pub(crate) fn client_file_in(client_dir: Option<&std::path::Path>, rel: &str) -> Option<Vec<u8>> {
    if rel.split('/').any(|p| p == ".." || p.is_empty()) && !rel.is_empty() {
        return None;
    }
    match client_dir {
        Some(dir) => {
            let p = dir.join(rel);
            p.is_file().then(|| std::fs::read(p).ok()).flatten()
        }
        None => ClientDist::get(rel).map(|f| f.data.into_owned()),
    }
}

/// Client pages the app shows in a frame of its own page (same origin only).
const FRAMED_PAGES: [&str; 1] = ["machine.html"];

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
            // Pages (index.html, glance.html, machine.html) and brand/ (branding.json, which a
            // host may replace) are never cached; the images are hashed assets.
            let cache = if rel.ends_with(".html") || rel.starts_with("brand/") {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            let mut r = Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache);
            // This Machine is shown in a frame of the settings (same origin); without this
            // the default DENY made WebView2 block it.
            if FRAMED_PAGES.contains(&rel.as_str()) {
                r = r
                    .header(header::X_FRAME_OPTIONS, "SAMEORIGIN")
                    .header(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'self'");
            }
            return r.body(Body::from(bytes)).unwrap();
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
