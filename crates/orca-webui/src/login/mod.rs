//! Web UI accounts (D140): users in SQLite (`<home>/abhoth.db`), server-side browser sessions,
//! per-user API keys, optional TOTP 2FA with recovery codes, and the admin audit log.
//!
//! Every request but the app shell, its assets and `/api/auth/*` needs a principal:
//! - a browser session cookie (HttpOnly, SameSite=Lax, Secure over HTTPS); state-changing
//!   requests then also need the session's CSRF header, and they and websockets an Origin equal
//!   to this site;
//! - `Authorization: Bearer abk_...`, a user's API key (scripts, the CLI);
//! - the launch token this process made (`?token=` in the URL it printed or gave the desktop
//!   window, or the `X-OpenOrca-Token` header): it stands for the first admin, except with
//!   `--hostui-login` (an exposed server), where it only unlocks the first-run setup.
//!
//! With no users yet every page redirects to `/setup`, which creates the first admin (once,
//! atomically) for whoever holds the launch token (the setup code).

pub mod accounts;
pub mod admin;
pub mod db;
pub mod endpoints;
pub mod password;
pub mod recovery;
pub mod store;
pub mod totp;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::audit::{AuditLog, Caller};
use crate::auth::starts_with_segment as seg;
use crate::server::AppState;
use db::{Session, User, UserDb};
use password::Cost;
use store::{Clock, LoginThrottle};

pub const CSRF_HEADER: &str = "X-OpenOrca-CSRF";
pub const ISSUER: &str = orca_core::brand::PRODUCT_NAME;
/// API keys start with this (and are told apart from the launch token by it).
pub const API_KEY_PREFIX: &str = "abk_";
/// Holds the setup code from the launch URL while no user exists.
pub const SETUP_COOKIE: &str = "abhoth_setup";
/// A half-signed-in session (password done, 2FA code pending) lasts this long.
pub const PENDING_SECONDS: i64 = 5 * 60;

/// How the accounts work on this server.
#[derive(Clone)]
pub struct LoginOptions {
    /// The database; `None`: `abhoth.db` next to the project list (the state home).
    pub db_path: Option<PathBuf>,
    /// Where the old single account (`account.json`, `--hostui-login` before D140) may be:
    /// imported as the first admin while the database has no users.
    pub legacy_dir: Option<PathBuf>,
    /// `--hostui-login`: a server reachable from outside. The launch token then only unlocks
    /// the first-run setup, and plain HTTP is refused except from this machine.
    pub exposed: bool,
    /// Reverse proxies whose X-Forwarded-For / -Proto / -Host are believed.
    pub trusted_proxies: Vec<IpAddr>,
    /// Make TOTP 2FA mandatory (set up at the next sign-in); else opt-in per user.
    pub require_totp: bool,
    pub idle_timeout: Duration,
    pub absolute_timeout: Duration,
    /// Create the first admin from these while there are no users (`ABHOTH_ADMIN_USER`,
    /// `ABHOTH_ADMIN_PASSWORD_FILE`).
    pub bootstrap_user: Option<String>,
    pub bootstrap_password_file: Option<PathBuf>,
    /// The same with the password itself (tests).
    pub bootstrap_password: Option<String>,
    /// The time (tests move it); `None`: the system clock.
    pub clock: Option<Clock>,
    /// Argon2 cost (tests use [`Cost::FAST`]).
    pub cost: Cost,
    /// Where users who aren't admins may open projects; `None`: the projects locations
    /// (`ABHOTH_PROJECT_ROOTS` / `ABHOTH_PROJECTS_ROOT`, else /projects on Linux).
    pub user_roots: Option<Vec<PathBuf>>,
    /// The desktop app (D142): with no account yet, make a local admin named after the OS user,
    /// without a password (it signs in only through this process's launch token).
    pub local_admin: bool,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            db_path: None,
            legacy_dir: None,
            exposed: false,
            trusted_proxies: Vec::new(),
            require_totp: false,
            idle_timeout: Duration::from_secs(12 * 3600),
            absolute_timeout: Duration::from_secs(7 * 24 * 3600),
            bootstrap_user: None,
            bootstrap_password_file: None,
            bootstrap_password: None,
            clock: None,
            cost: Cost::default(),
            user_roots: None,
            local_admin: false,
        }
    }
}

/// Who a request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: i64,
    pub username: String,
    pub admin: bool,
    pub via: Via,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// A browser session (cookie): the session id and its CSRF token.
    Session {
        id: i64,
        csrf: String,
    },
    ApiKey(i64),
    /// The launch token (the first admin).
    LaunchToken,
}

impl Principal {
    fn of(user: &User, via: Via) -> Self {
        Self {
            user_id: user.id,
            username: user.username.clone(),
            admin: user.admin,
            via,
        }
    }

    pub fn session_id(&self) -> Option<i64> {
        match &self.via {
            Via::Session { id, .. } => Some(*id),
            _ => None,
        }
    }

    pub fn caller(&self, ip: Option<String>) -> Caller {
        Caller {
            ip,
            user: Some(self.username.clone()),
            uid: Some(self.user_id),
            admin: self.admin,
            sid: self.session_id(),
        }
    }
}

/// May `uid` (an admin or not) see a project owned by `owner`? Projects without an owner are
/// the server's own (the primary project, bench runs, projects from before accounts): admins'.
pub fn can_access(uid: Option<i64>, admin: bool, owner: Option<i64>) -> bool {
    admin || (owner.is_some() && owner == uid)
}

pub struct LoginAuth {
    pub options: LoginOptions,
    pub db: UserDb,
    pub throttle: LoginThrottle,
    clock: Clock,
    launch_token: String,
    /// This server backs the desktop window (its sessions live as long as this process).
    pub desktop: bool,
    started: i64,
    has_users: AtomicBool,
    last_purge: AtomicI64,
    /// The file audit log (also gets the account events).
    audit_file: Arc<AuditLog>,
}

/// Constant-time byte comparison.
pub fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The request as this server sees it once the trusted proxy's headers are applied.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub ip: Option<IpAddr>,
    pub https: bool,
    /// Scheme-less authority ("host:port" or "host").
    pub host: Option<String>,
}

impl Seen {
    pub fn ip_text(&self) -> Option<String> {
        self.ip.map(|i| i.to_string())
    }
}

pub(crate) fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The desktop's local admin's name (D142): the OS user, cut down to what a username allows,
/// else "admin".
pub fn local_admin_name(os_user: Option<&str>) -> String {
    let cleaned: String = os_user
        .unwrap_or("")
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '.'])
        .chars()
        .take(32)
        .collect();
    check_username(&cleaned).unwrap_or_else(|_| "admin".to_string())
}

/// A username: 1-64 letters, digits and `. _ - @`.
pub fn check_username(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return Err("A username is 1 to 64 characters.".into());
    }
    if !n
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
    {
        return Err("A username has letters, digits and . _ - @ only.".into());
    }
    Ok(n.to_string())
}

impl LoginAuth {
    /// Open the database, import the old single account when there are no users yet, then
    /// create the first admin from the bootstrap settings if there still are none.
    pub fn new(
        options: LoginOptions,
        db_path: PathBuf,
        launch_token: String,
        desktop: bool,
        audit_file: Arc<AuditLog>,
    ) -> Result<Self, String> {
        let db = UserDb::open(&db_path)?;
        Self::with_db(options, db, launch_token, desktop, audit_file)
    }

    pub fn with_db(
        options: LoginOptions,
        db: UserDb,
        launch_token: String,
        desktop: bool,
        audit_file: Arc<AuditLog>,
    ) -> Result<Self, String> {
        let clock = options.clock.clone().unwrap_or_else(store::system_clock);
        let throttle = LoginThrottle::new(clock.clone());
        let started = clock() as i64;
        let launch_token = if launch_token.is_empty() {
            crate::util::new_token()
        } else {
            launch_token
        };
        let auth = Self {
            options,
            db,
            throttle,
            clock,
            launch_token,
            desktop,
            started,
            has_users: AtomicBool::new(false),
            last_purge: AtomicI64::new(0),
            audit_file,
        };
        auth.import_legacy();
        auth.bootstrap()?;
        auth.create_local_admin();
        Ok(auth)
    }

    /// The desktop's first start (D142): a local admin named after the OS user, no password.
    /// An imported old account or a bootstrap admin comes first (then there are users already).
    fn create_local_admin(&self) {
        if !self.options.local_admin || self.db.user_count() > 0 {
            return;
        }
        let os_user = ["USERNAME", "USER", "LOGNAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()));
        let name = local_admin_name(os_user.as_deref());
        match self.db.create_user(&name, "", true, true, self.now()) {
            Ok(u) => {
                self.record(
                    "user.created",
                    None,
                    Some(&u.username),
                    None,
                    json!({"admin": true, "via": "desktop", "localOnly": true}),
                );
                tracing::info!("web UI: made the local admin {} (desktop)", u.username);
            }
            // Another process got there first: fine.
            Err(e) => tracing::debug!("web UI: no local admin: {e}"),
        }
    }

    fn import_legacy(&self) {
        let Some(dir) = &self.options.legacy_dir else {
            return;
        };
        if self.db.user_count() > 0 {
            return;
        }
        let Some(account) = accounts::AccountStore::new(dir)
            .load()
            .filter(|a| !a.username.is_empty() && !a.password_hash.is_empty())
        else {
            return;
        };
        if let Ok(u) = self.db.import_legacy(&account, self.now()) {
            // account.json stays as it was (the rollback reads it).
            self.record(
                "user.imported",
                None,
                Some(&u.username),
                None,
                json!({"from": "account.json", "totp": u.totp_enabled}),
            );
            tracing::info!(
                "web UI: imported the account {} as the first admin",
                u.username
            );
        }
    }

    fn bootstrap(&self) -> Result<(), String> {
        if self.db.user_count() > 0 {
            return Ok(());
        }
        let Some(user) = self
            .options
            .bootstrap_user
            .as_deref()
            .filter(|u| !u.trim().is_empty())
        else {
            return Ok(());
        };
        let password = match (
            &self.options.bootstrap_password,
            &self.options.bootstrap_password_file,
        ) {
            (Some(p), _) => p.clone(),
            (None, Some(file)) => std::fs::read_to_string(file)
                .map_err(|_| {
                    format!(
                        "The admin password file {} could not be read.",
                        file.display()
                    )
                })?
                .trim_end_matches(['\r', '\n'])
                .to_string(),
            (None, None) => {
                return Err(
                    "ABHOTH_ADMIN_USER needs ABHOTH_ADMIN_PASSWORD_FILE (a file holding the password)."
                        .into(),
                );
            }
        };
        let name = check_username(user)?;
        password::check(&password)?;
        let hash = password::hash(&password, self.options.cost);
        match self.db.create_user(&name, &hash, true, true, self.now()) {
            Ok(u) => {
                self.record(
                    "user.created",
                    None,
                    Some(&u.username),
                    None,
                    json!({"admin": true, "via": "bootstrap"}),
                );
                Ok(())
            }
            Err(db::CreateError::NotFirst) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn now(&self) -> i64 {
        (self.clock)() as i64
    }

    /// The code `/setup` asks for (the launch token).
    pub fn setup_code(&self) -> &str {
        &self.launch_token
    }

    pub fn launch_token_matches(&self, candidate: Option<&str>) -> bool {
        candidate.is_some_and(|c| {
            !c.is_empty() && fixed_time_eq(c.as_bytes(), self.launch_token.as_bytes())
        })
    }

    /// First-run setup: no user exists yet, or (exposed) only the desktop's local admin, which
    /// must get a password before the server is reachable with one (D142).
    pub fn setup_needed(&self) -> bool {
        if self.has_users.load(Ordering::Relaxed) {
            return false;
        }
        let ready = if self.options.exposed {
            self.db.password_user_count() > 0
        } else {
            self.db.user_count() > 0
        };
        if ready {
            self.has_users.store(true, Ordering::Relaxed);
        }
        !ready
    }

    /// The local-only admin the setup gives a password to (exposed, D142).
    pub fn setup_user(&self) -> Option<User> {
        if self.db.password_user_count() > 0 {
            return None;
        }
        self.db.first_admin().filter(User::local_only)
    }

    /// An account event: in the database's audit table and the audit log file.
    pub fn record(
        &self,
        event: &str,
        actor: Option<&Principal>,
        target: Option<&str>,
        ip: Option<&str>,
        detail: Value,
    ) {
        self.db.audit(
            self.now(),
            event,
            actor.map(|a| a.user_id),
            actor.map(|a| a.username.as_str()),
            target,
            ip,
            &detail,
        );
        let mut fields = json!({"target": target});
        if let (Some(f), Some(d)) = (fields.as_object_mut(), detail.as_object()) {
            for (k, v) in d {
                f.insert(k.clone(), v.clone());
            }
        }
        self.audit_file.write(
            Some(&Caller {
                ip: ip.map(str::to_string),
                user: actor.map(|a| a.username.clone()),
                ..Default::default()
            }),
            event,
            fields,
        );
    }

    /// `UseForwardedHeaders` with the known proxies and a forward limit of 1.
    pub fn seen(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Seen {
        let mut s = Seen {
            ip: peer,
            https: false,
            host: header_str(headers, "host").map(str::to_string),
        };
        if peer.is_some_and(|p| self.options.trusted_proxies.contains(&p)) {
            if let Some(ip) = header_str(headers, "x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
            {
                s.ip = Some(ip);
            }
            if let Some(proto) =
                header_str(headers, "x-forwarded-proto").and_then(|v| v.rsplit(',').next())
            {
                s.https = proto.trim().eq_ignore_ascii_case("https");
            }
            if let Some(host) =
                header_str(headers, "x-forwarded-host").and_then(|v| v.rsplit(',').next())
            {
                s.host = Some(host.trim().to_string());
            }
        }
        s
    }

    /// "__Host-" pins it to this host, path "/" and Secure.
    pub fn cookie_name(https: bool) -> &'static str {
        if https {
            "__Host-abhoth_session"
        } else {
            "abhoth_session"
        }
    }

    pub fn set_cookie(https: bool, token: &str) -> String {
        format!(
            "{}={token}; path=/; samesite=lax; httponly{}",
            Self::cookie_name(https),
            if https { "; secure" } else { "" }
        )
    }

    pub fn clear_cookie(https: bool) -> String {
        format!(
            "{}=; max-age=0; path=/; samesite=lax; httponly{}",
            Self::cookie_name(https),
            if https { "; secure" } else { "" }
        )
    }

    fn purge(&self, now: i64) {
        let last = self.last_purge.load(Ordering::Relaxed);
        if now - last < 60 {
            return;
        }
        self.last_purge.store(now, Ordering::Relaxed);
        self.db.purge_sessions(
            now,
            self.options.idle_timeout.as_secs() as i64,
            self.options.absolute_timeout.as_secs() as i64,
            PENDING_SECONDS,
            self.desktop.then_some(self.started),
        );
    }

    /// The session behind a cookie, if it is still valid (any stage), with its user.
    pub fn session(&self, token: Option<&str>) -> Option<(Session, User)> {
        let token = token.filter(|t| !t.is_empty())?;
        let now = self.now();
        self.purge(now);
        let s = self.db.session_by_token(token)?;
        let expired = if s.desktop {
            // The desktop window's session lasts while the process that made it runs.
            !self.desktop || s.created < self.started
        } else if s.stage == "ok" {
            now - s.created > self.options.absolute_timeout.as_secs() as i64
                || now - s.last_seen > self.options.idle_timeout.as_secs() as i64
        } else {
            now - s.created > PENDING_SECONDS
        };
        let user = self.db.user(s.user_id).filter(|u| !u.disabled);
        let Some(user) = user.filter(|_| !expired) else {
            self.db.delete_session(s.id);
            return None;
        };
        if now - s.last_seen >= 30 {
            self.db.touch_session(s.id, now);
        }
        Some((s, user))
    }

    /// Start a browser session; returns the cookie value and the row.
    pub fn start_session(
        &self,
        user: &User,
        stage: &str,
        seen: &Seen,
        headers: &HeaderMap,
        desktop: bool,
    ) -> Option<(String, Session)> {
        let token = store_token();
        let s = self.db.create_session(
            &token,
            user.id,
            stage,
            &store_token(),
            self.now(),
            seen.ip_text().as_deref(),
            header_str(headers, "user-agent"),
            desktop && self.desktop,
        )?;
        Some((token, s))
    }

    /// The principal of an `Authorization: Bearer` API key (`Err`: a key that is not valid).
    fn api_key(&self, headers: &HeaderMap) -> Option<Result<Principal, ()>> {
        let auth = header_str(headers, "authorization")?;
        let (scheme, key) = auth.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let key = key.trim();
        let found = key
            .starts_with(API_KEY_PREFIX)
            .then(|| self.db.api_key_by_secret(key))
            .flatten()
            .and_then(|k| {
                self.db
                    .user(k.user_id)
                    .filter(|u| !u.disabled)
                    .map(|u| (k, u))
            });
        Some(match found {
            // Service keys (ROUTER-CONFIG.md 11.1) work only at the router, never here: their
            // owner is an admin, and the key must not carry that admin's rights.
            Some((k, _)) if k.kind != db::KEY_USER => Err(()),
            Some((k, u)) => {
                self.db.touch_api_key(k.id, self.now());
                Ok(Principal::of(&u, Via::ApiKey(k.id)))
            }
            None => Err(()),
        })
    }

    /// The owner the launch token stands for (not on an exposed server).
    fn launch_principal(&self) -> Option<Principal> {
        if self.options.exposed {
            return None;
        }
        self.db
            .first_admin()
            .map(|u| Principal::of(&u, Via::LaunchToken))
    }

    /// The JSON the client reads at `/api/auth/state`.
    pub fn state_json(&self, session: Option<&(Session, User)>, setup_cookie: bool) -> Value {
        let setup = self.setup_needed();
        let ok = session.filter(|(s, _)| s.stage == "ok");
        json!({
            "mode": "login",
            "setup": setup,
            "setupCodeNeeded": setup && !setup_cookie,
            "setupUser": if setup { self.setup_user().map(|u| u.username) } else { None },
            "stage": session.map_or("signedOut", |(s, _)| s.stage.as_str()),
            "user": ok.map(|(_, u)| u.username.clone()),
            "userId": ok.map(|(_, u)| u.id),
            "admin": ok.is_some_and(|(_, u)| u.admin),
            "totp": session.is_some_and(|(_, u)| u.totp_enabled),
            "totpRequired": self.options.require_totp,
            "csrf": session.map(|(s, _)| s.csrf.clone()),
            "idleMinutes": self.options.idle_timeout.as_secs() / 60,
            "desktop": session.is_some_and(|(s, _)| s.desktop),
        })
    }
}

/// 256-bit random, URL-safe base64.
pub fn store_token() -> String {
    use base64::Engine;
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random bytes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// What the middleware found, for the `/api/auth` and `/api/account` handlers.
#[derive(Clone, Default)]
pub struct Current {
    pub session: Option<(Session, User)>,
    pub seen: Seen,
    pub setup_cookie: Option<String>,
}

/// The Origin header names this host (scheme, host and port as the proxy forwarded them).
pub fn origin_matches(headers: &HeaderMap, seen: &Seen) -> bool {
    let Some(origin) = header_str(headers, "origin").and_then(|o| reqwest::Url::parse(o).ok())
    else {
        return false;
    };
    let scheme = if seen.https { "https" } else { "http" };
    let Some(host) = seen
        .host
        .as_deref()
        .and_then(|h| reqwest::Url::parse(&format!("{scheme}://{h}")).ok())
    else {
        return false;
    };
    origin.scheme().eq_ignore_ascii_case(scheme)
        && origin
            .host_str()
            .zip(host.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && origin.port_or_known_default() == host.port_or_known_default()
}

fn csrf_ok(headers: &HeaderMap, session: Option<&Session>) -> bool {
    session.is_some_and(|s| {
        fixed_time_eq(
            header_str(headers, CSRF_HEADER).unwrap_or("").as_bytes(),
            s.csrf.as_bytes(),
        )
    })
}

fn harden(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    r
}

pub(crate) fn error(code: StatusCode, text: &str) -> Response {
    let mut r = (code, axum::Json(json!({"error": text}))).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Paths only admins may use (server and global settings, the bench, the shared GitHub
/// sign-in, the desktop's folder dialog, user management).
pub fn admin_only(path: &str, method: &Method) -> bool {
    seg(path, "/api/admin")
        || seg(path, "/api/settings")
        || seg(path, "/api/bench")
        || seg(path, "/api/github/app")
        || seg(path, "/api/desktop")
        // A project's settings are saved to the shared config.json.
        || project_of(path).is_some_and(|pid| path == format!("/api/projects/{pid}/settings"))
        || (method != Method::GET
            && ["save", "delete", "switch"]
                .iter()
                .any(|p| seg(path, &format!("/api/profiles/{p}"))))
}

/// The project id of `/api/projects/{pid}[/...]` (not `/api/projects/clone`).
pub fn project_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/projects/")?;
    let pid = rest.split('/').next().filter(|p| !p.is_empty())?;
    (pid != "clone" || rest.contains('/')).then_some(pid)
}

fn redirect(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap()
}

/// The URL without its `token` query parameter.
fn without_token(path: &str, pairs: &[(String, String)]) -> String {
    let rest: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k != crate::auth::QUERY_NAME)
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::util::escape_data(k),
                crate::util::escape_data(v)
            )
        })
        .collect();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

pub async fn login_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let auth = state.login.clone();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let seen = auth.seen(peer, req.headers());
    // The router's key introspection (ROUTER-CONFIG.md 11.1) carries its own bearer token,
    // which the handler checks; no session, CSRF or HTTPS rule applies to it.
    if crate::router_admin::is_internal(req.uri().path()) && req.method() == Method::POST {
        return harden(next.run(req).await);
    }
    let loopback = seen.ip.is_some_and(|ip| ip.is_loopback());
    // Exposed: plain HTTP only from this machine; everyone else via the HTTPS proxy.
    if auth.options.exposed && !seen.https && !loopback {
        return harden((StatusCode::FORBIDDEN, "HTTPS required.").into_response());
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let websocket = crate::auth::is_websocket(&headers);
    let unsafe_method =
        !(method == Method::GET || method == Method::HEAD || method == Method::OPTIONS);
    let is_api = seg(&path, "/api") || seg(&path, "/hub");
    let page_get = method == Method::GET && !websocket && !is_api;
    let ip = seen.ip_text();

    // A browser request from another site: refused outright.
    if (unsafe_method || websocket)
        && header_str(&headers, "origin").is_some()
        && !origin_matches(&headers, &seen)
    {
        state.audit.write(
            Some(&Caller {
                ip: ip.clone(),
                ..Default::default()
            }),
            "origin.rejected",
            json!({"path": path, "origin": header_str(&headers, "origin").unwrap_or("")}),
        );
        return harden(error(StatusCode::FORBIDDEN, "Cross-site request refused."));
    }
    // The HTML preview's files carry their own signed, expiring token; the brand images and
    // hashed assets are public (the sign-in and setup pages show them).
    if (method == Method::GET || method == Method::HEAD)
        && !websocket
        && (seg(&path, crate::files::preview::PREFIX)
            || seg(&path, "/brand")
            || seg(&path, "/assets")
            || path == "/favicon.ico")
    {
        return harden(next.run(req).await);
    }

    let pairs = crate::auth::query_pairs(req.uri().query());
    let query_token = pairs
        .iter()
        .find(|(k, _)| k == crate::auth::QUERY_NAME)
        .map(|(_, v)| v.clone());
    let launch = auth.launch_token_matches(query_token.as_deref())
        || auth.launch_token_matches(header_str(&headers, crate::auth::HEADER_NAME));
    let setup_cookie =
        crate::auth::cookie(&headers, SETUP_COOKIE).filter(|c| auth.launch_token_matches(Some(c)));
    let session =
        auth.session(crate::auth::cookie(&headers, LoginAuth::cookie_name(seen.https)).as_deref());

    // ---- first run: nothing but the setup ----
    if auth.setup_needed() {
        if page_get && launch && query_token.is_some() {
            // The launch URL: remember the setup code, then the setup page.
            let mut r = redirect("/setup");
            if let Ok(v) = format!(
                "{SETUP_COOKIE}={}; path=/; samesite=strict; httponly{}",
                auth.setup_code(),
                if seen.https { "; secure" } else { "" }
            )
            .parse()
            {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
            return harden(r);
        }
        if seg(&path, "/api/auth") {
            req.extensions_mut().insert(Current {
                session: None,
                seen,
                setup_cookie,
            });
            return harden(next.run(req).await);
        }
        if page_get {
            if path == "/setup" {
                return harden(next.run(req).await);
            }
            return harden(redirect("/setup"));
        }
        let mut r = (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "Create the admin account first (/setup).", "setup": true})),
        )
            .into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return harden(r);
    }

    // ---- who is it ----
    let mut principal: Option<Principal> = None;
    let mut cookie_auth = false;
    match auth.api_key(&headers) {
        Some(Ok(p)) => principal = Some(p),
        Some(Err(())) => {
            auth.record(
                "apikey.rejected",
                None,
                None,
                ip.as_deref(),
                json!({"path": path}),
            );
            return harden(error(StatusCode::UNAUTHORIZED, "Not a valid API key."));
        }
        None => {}
    }
    if principal.is_none() && launch {
        principal = auth.launch_principal();
        // The printed URL / desktop window: a browser session from here on, token out of the
        // address bar.
        if let (Some(p), true, true) = (&principal, page_get, query_token.is_some())
            && let Some(user) = auth.db.user(p.user_id)
            && let Some((token, s)) = auth.start_session(&user, "ok", &seen, &headers, auth.desktop)
        {
            if let Some(old) = &session {
                auth.db.delete_session(old.0.id);
            }
            auth.record(
                "login.ok",
                Some(p),
                Some(&user.username),
                ip.as_deref(),
                json!({"via": if s.desktop { "desktop" } else { "launch-token" }}),
            );
            auth.db.touch_login(user.id, auth.now());
            let mut r = redirect(&without_token(&path, &pairs));
            if let Ok(v) = LoginAuth::set_cookie(seen.https, &token).parse() {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
            return harden(r);
        }
    }
    if principal.is_none()
        && let Some((s, u)) = session.as_ref().filter(|(s, _)| s.stage == "ok")
    {
        principal = Some(Principal::of(
            u,
            Via::Session {
                id: s.id,
                csrf: s.csrf.clone(),
            },
        ));
        cookie_auth = true;
    }

    // The sign-in API and the app shell (it shows the sign-in page).
    if seg(&path, "/api/auth") || (page_get && principal.is_none()) {
        if unsafe_method
            && seg(&path, "/api/auth")
            && !seg(&path, "/api/auth/login")
            && !seg(&path, "/api/auth/setup")
            && !csrf_ok(&headers, session.as_ref().map(|(s, _)| s))
        {
            return harden(error(StatusCode::FORBIDDEN, "Missing or wrong CSRF token."));
        }
        if page_get && path == "/setup" {
            return harden(redirect("/"));
        }
        req.extensions_mut().insert(Current {
            session,
            seen,
            setup_cookie,
        });
        return harden(next.run(req).await);
    }
    let Some(principal) = principal else {
        let mut r = error(StatusCode::UNAUTHORIZED, "Sign in first.");
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return harden(r);
    };
    if cookie_auth {
        // A cookie rides along on any request: state changes need the CSRF token, and they
        // and websockets an Origin from this site.
        if (unsafe_method || websocket) && !origin_matches(&headers, &seen) {
            return harden(error(StatusCode::FORBIDDEN, "Cross-site request refused."));
        }
        if unsafe_method && !csrf_ok(&headers, session.as_ref().map(|(s, _)| s)) {
            state.audit.write(
                Some(&principal.caller(ip.clone())),
                "csrf.rejected",
                json!({"path": path}),
            );
            return harden(error(StatusCode::FORBIDDEN, "Missing or wrong CSRF token."));
        }
    }
    if admin_only(&path, &method) && !principal.admin {
        return harden(error(StatusCode::FORBIDDEN, "Only an admin can do that."));
    }
    if let Some(pid) = project_of(&path)
        && let Some(info) = state.projects.info(pid)
        && !can_access(Some(principal.user_id), principal.admin, info.owner)
    {
        return harden(error(StatusCode::NOT_FOUND, "No such project."));
    }
    let caller = principal.caller(ip);
    req.extensions_mut().insert(principal);
    req.extensions_mut().insert(Current {
        session,
        seen,
        setup_cookie,
    });
    harden(crate::audit::with_caller(caller, next.run(req)).await)
}

/// The principal the middleware attached (handlers behind it always have one).
pub fn principal_of(req: &Request<Body>) -> Option<Principal> {
    req.extensions().get::<Principal>().cloned()
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    endpoints::routes().merge(admin::routes())
}

/// `PngByteQRCode(data).GetGraphic(6, black, white, drawQuietZones: true)` at ECC level M.
pub fn qr_png(text: &str) -> Option<Vec<u8>> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let size = (width + 2 * QUIET) * SCALE;
    let mut img = image::GrayImage::from_pixel(size as u32, size as u32, image::Luma([255]));
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let (x, y) = ((i % width + QUIET) * SCALE, (i / width + QUIET) * SCALE);
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    img.put_pixel((x + dx) as u32, (y + dy) as u32, image::Luma([0]));
                }
            }
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The QR code of a TOTP secret as a data URL, the secret and its otpauth URI.
pub fn enroll_json(user: &str, secret: &str) -> Option<Value> {
    use base64::Engine;
    let uri = totp::uri(ISSUER, user, secret);
    let png = qr_png(&uri)?;
    Some(json!({
        "qr": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)),
        "secret": secret,
        "uri": uri,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_paths_and_project_ids_are_recognised() {
        assert!(admin_only("/api/admin/users", &Method::GET));
        assert!(admin_only("/api/settings", &Method::GET));
        assert!(admin_only("/api/bench/start", &Method::POST));
        assert!(admin_only("/api/github/app/clone", &Method::POST));
        assert!(admin_only("/api/profiles/switch", &Method::POST));
        assert!(!admin_only("/api/profiles", &Method::GET));
        assert!(admin_only("/api/projects/abc/settings", &Method::PUT));
        assert!(!admin_only("/api/projects/abc/file", &Method::PUT));
        assert!(!admin_only("/api/settingsx", &Method::GET));
        assert_eq!(project_of("/api/projects/abc/file"), Some("abc"));
        assert_eq!(project_of("/api/projects/abc"), Some("abc"));
        assert_eq!(project_of("/api/projects/clone"), None);
        assert_eq!(project_of("/api/projects"), None);
    }

    #[test]
    fn the_local_admins_name_comes_from_the_os_user() {
        assert_eq!(local_admin_name(Some("james")), "james");
        assert_eq!(local_admin_name(Some("Jane Doe")), "Jane-Doe");
        assert_eq!(local_admin_name(Some(r" DOMAIN\bob ")), "DOMAIN-bob");
        assert_eq!(local_admin_name(Some("!!!")), "admin");
        assert_eq!(local_admin_name(None), "admin");
        assert_eq!(local_admin_name(Some(&"x".repeat(100))).len(), 32);
    }

    fn local_auth(dir: &std::path::Path, legacy: Option<PathBuf>) -> LoginAuth {
        let options = LoginOptions {
            local_admin: true,
            legacy_dir: legacy,
            cost: Cost::FAST,
            ..Default::default()
        };
        let db = db::UserDb::open(&dir.join("abhoth.db")).unwrap();
        LoginAuth::with_db(
            options,
            db,
            String::new(),
            true,
            Arc::new(AuditLog::new(None)),
        )
        .unwrap()
    }

    #[test]
    fn the_desktop_makes_one_local_admin_without_a_password_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = local_auth(dir.path(), None);
        let users = a.db.users();
        assert_eq!(users.len(), 1);
        assert!(users[0].admin && users[0].local_only());
        assert!(!a.setup_needed());
        assert_eq!(a.launch_principal().unwrap().user_id, users[0].id);
        drop(a);
        // The next start: the same account, no second one.
        let b = local_auth(dir.path(), None);
        assert_eq!(b.db.users(), users);
        // An exposed server on this home asks for its password first.
        let exposed = LoginAuth::with_db(
            LoginOptions {
                exposed: true,
                cost: Cost::FAST,
                ..Default::default()
            },
            db::UserDb::open(&dir.path().join("abhoth.db")).unwrap(),
            String::new(),
            false,
            Arc::new(AuditLog::new(None)),
        )
        .unwrap();
        assert!(exposed.setup_needed());
        assert_eq!(exposed.setup_user().unwrap().id, users[0].id);
        assert!(exposed.launch_principal().is_none());
        exposed
            .db
            .set_password_hash(users[0].id, &password::hash("now it has one", Cost::FAST));
        assert!(exposed.setup_user().is_none());
        assert!(!exposed.setup_needed());
    }

    #[test]
    fn the_old_account_comes_before_the_local_admin() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("webui");
        accounts::AccountStore::new(&legacy)
            .create("old", "the old long password")
            .unwrap();
        let a = local_auth(dir.path(), Some(legacy));
        let users = a.db.users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].username, "old");
        assert!(!users[0].local_only());
    }

    #[test]
    fn owners_and_admins_reach_a_project() {
        assert!(can_access(Some(2), false, Some(2)));
        assert!(!can_access(Some(3), false, Some(2)));
        assert!(!can_access(Some(3), false, None));
        assert!(can_access(Some(1), true, Some(2)));
        assert!(can_access(Some(1), true, None));
        assert!(!can_access(None, false, None));
    }

    #[test]
    fn usernames_are_checked() {
        assert_eq!(check_username(" ann ").unwrap(), "ann");
        assert!(check_username("").is_err());
        assert!(check_username("a b").is_err());
        assert!(check_username("ann@example.org").is_ok());
        assert!(check_username(&"a".repeat(65)).is_err());
    }
}
