//! `LoginAuth`: sign-in for a web UI reachable from outside (`--hostui-login`). A username and
//! password, then a TOTP code (set up with a QR code), then a session cookie, in place of the URL
//! token. Everything but the page, its assets and `/api/auth/*` needs a signed-in session;
//! state-changing requests also need the session's CSRF header and, like WebSockets, an Origin
//! equal to this site. Requests must arrive over HTTPS (from the trusted reverse proxy) unless
//! they come from this machine.

pub mod accounts;
pub mod store;
pub mod totp;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::audit::{AuditLog, Caller};
use crate::server::AppState;
use accounts::AccountStore;
use store::{Clock, LoginThrottle, SessionStore, WebSession};

pub const CSRF_HEADER: &str = "X-OpenOrca-CSRF";
pub const ISSUER: &str = "OpenOrca";

/// Sign-in mode settings (`--hostui-login`).
#[derive(Clone)]
pub struct LoginOptions {
    /// Folder for account.json and audit.log.
    pub state_dir: PathBuf,
    /// Reverse proxies whose X-Forwarded-For / -Proto / -Host are believed.
    pub trusted_proxies: Vec<IpAddr>,
    /// Refuse plain HTTP except from this machine.
    pub require_https: bool,
    /// Make TOTP 2FA mandatory (set up at the next sign-in); else opt-in from the signed-in UI,
    /// and once set up always asked for.
    pub require_totp: bool,
    pub idle_timeout: Duration,
    pub absolute_timeout: Duration,
    /// Create the account from these when account.json doesn't exist yet.
    pub bootstrap_user: Option<String>,
    pub bootstrap_password_file: Option<PathBuf>,
    /// The time (tests move it); `None`: the system clock.
    pub clock: Option<Clock>,
}

impl LoginOptions {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            trusted_proxies: Vec::new(),
            require_https: true,
            require_totp: false,
            idle_timeout: Duration::from_secs(3600),
            absolute_timeout: Duration::from_secs(12 * 3600),
            bootstrap_user: None,
            bootstrap_password_file: None,
            clock: None,
        }
    }
}

pub struct LoginAuth {
    pub options: LoginOptions,
    pub accounts: AccountStore,
    pub sessions: SessionStore,
    pub throttle: LoginThrottle,
    clock: Clock,
}

/// Constant-time byte comparison.
pub fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The request as this server sees it once the trusted proxy's headers are applied.
#[derive(Debug, Clone)]
struct Seen {
    ip: Option<IpAddr>,
    https: bool,
    /// Scheme-less authority ("host:port" or "host").
    host: Option<String>,
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

impl LoginAuth {
    /// Opens the account store and creates the account from the bootstrap secret when there is
    /// none yet (the password file is read, hashed and never logged).
    pub fn new(options: LoginOptions, audit: &AuditLog) -> Result<Self, String> {
        let clock = options.clock.clone().unwrap_or_else(store::system_clock);
        let accounts = AccountStore::new(&options.state_dir);
        if accounts.load().is_none()
            && let (Some(user), Some(file)) = (
                options.bootstrap_user.as_deref().filter(|u| !u.is_empty()),
                options.bootstrap_password_file.as_ref(),
            )
        {
            let text = std::fs::read_to_string(file).map_err(|_| {
                format!(
                    "The sign-in password file {} does not exist.",
                    file.display()
                )
            })?;
            accounts.create(user, text.trim_end_matches(['\r', '\n']))?;
            audit.write(None, "account.created", json!({"user": user}));
        }
        let sessions = SessionStore::new(
            clock.clone(),
            options.idle_timeout,
            options.absolute_timeout,
        );
        let throttle = LoginThrottle::new(clock.clone());
        Ok(Self {
            options,
            accounts,
            sessions,
            throttle,
            clock,
        })
    }

    fn now(&self) -> i64 {
        (self.clock)() as i64
    }

    /// `UseForwardedHeaders` with the known proxies and a forward limit of 1.
    fn seen(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Seen {
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
    fn cookie_name(https: bool) -> &'static str {
        if https {
            "__Host-openorca_session"
        } else {
            "openorca_session"
        }
    }

    fn set_cookie(https: bool, id: &str) -> String {
        format!(
            "{}={id}; path=/; samesite=strict; httponly{}",
            Self::cookie_name(https),
            if https { "; secure" } else { "" }
        )
    }

    fn clear_cookie(https: bool) -> String {
        format!(
            "{}=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/; samesite=strict; httponly{}",
            Self::cookie_name(https),
            if https { "; secure" } else { "" }
        )
    }

    fn state(&self, s: Option<&WebSession>) -> Value {
        json!({
            "mode": "login",
            "stage": s.map_or("signedOut", |s| s.stage.as_str()),
            "totp": self.accounts.load().is_some_and(|a| a.totp_confirmed),
            "totpRequired": self.options.require_totp,
            "user": s.filter(|s| s.stage == "ok").map(|s| s.user.clone()),
            "csrf": s.map(|s| s.csrf.clone()),
            "idleMinutes": self.sessions.idle.as_secs() / 60,
        })
    }
}

/// The session the middleware found for this request (cookie), for the /api/auth handlers.
#[derive(Clone)]
struct Current {
    session: Option<WebSession>,
    seen: Seen,
}

fn is_public_path(path: &str) -> bool {
    use crate::auth::starts_with_segment as seg;
    seg(path, "/api/auth")
        || seg(path, "/brand")
        || seg(path, "/assets")
        || matches!(path, "/" | "/index.html" | "/favicon.ico")
}

/// The Origin header is present and names this host (scheme, host and port as the proxy
/// forwarded them).
fn origin_matches(headers: &HeaderMap, seen: &Seen) -> bool {
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

fn csrf_ok(headers: &HeaderMap, session: Option<&WebSession>) -> bool {
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

fn status(code: StatusCode) -> Response {
    code.into_response()
}

pub async fn login_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let Some(auth) = state.login.clone() else {
        return next.run(req).await;
    };
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let seen = auth.seen(peer, req.headers());
    let loopback = seen.ip.is_some_and(|ip| ip.is_loopback());
    // Plain HTTP only from this machine; everyone else via the HTTPS proxy.
    if auth.options.require_https && !seen.https && !loopback {
        return harden((StatusCode::FORBIDDEN, "HTTPS required.").into_response());
    }
    let cookie = crate::auth::cookie(req.headers(), LoginAuth::cookie_name(seen.https));
    let session = auth.sessions.get(cookie.as_deref());
    let signed_in = session.as_ref().is_some_and(|s| s.stage == "ok");
    let caller = Caller {
        ip: seen.ip.map(|i| i.to_string()),
        user: session
            .as_ref()
            .filter(|_| signed_in)
            .map(|s| s.user.clone()),
    };
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let websocket = crate::auth::is_websocket(&headers);
    let unsafe_method =
        !(method == Method::GET || method == Method::HEAD || method == Method::OPTIONS);
    let run = |req: Request<Body>| {
        let caller = caller.clone();
        async move { harden(crate::audit::with_caller(caller, next.run(req)).await) }
    };
    // Origin: required and equal to this site on WebSockets and every state-changing request.
    if (websocket || unsafe_method) && !origin_matches(&headers, &seen) {
        state.audit.write(
            Some(&caller),
            "origin.rejected",
            json!({"path": path, "origin": header_str(&headers, "origin").unwrap_or("")}),
        );
        return harden(status(StatusCode::FORBIDDEN));
    }
    // The HTML preview's files: their signed, expiring path token is checked there.
    if crate::auth::starts_with_segment(&path, crate::files::preview::PREFIX)
        && (method == Method::GET || method == Method::HEAD)
        && !websocket
    {
        return run(req).await;
    }
    req.extensions_mut().insert(Current {
        session: session.clone(),
        seen: seen.clone(),
    });
    use crate::auth::starts_with_segment as seg;
    if is_public_path(&path)
        || (!seg(&path, "/api") && !seg(&path, "/hub") && method == Method::GET && !websocket)
    {
        // The app shell (it shows the sign-in form) and the sign-in API.
        if unsafe_method
            && seg(&path, "/api/auth")
            && !seg(&path, "/api/auth/login")
            && !csrf_ok(&headers, session.as_ref())
        {
            return harden(status(StatusCode::FORBIDDEN));
        }
        return run(req).await;
    }
    if !signed_in {
        let mut r = status(StatusCode::UNAUTHORIZED);
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return harden(r);
    }
    // CSRF: a state-changing request carries the session's token.
    if unsafe_method && !csrf_ok(&headers, session.as_ref()) {
        state
            .audit
            .write(Some(&caller), "csrf.rejected", json!({"path": path}));
        return harden(status(StatusCode::FORBIDDEN));
    }
    run(req).await
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/login", post(login))
        .route("/api/auth/enroll/start", post(enroll_start))
        .route("/api/auth/enroll", get(enroll))
        .route("/api/auth/totp", post(totp_code))
        .route("/api/auth/logout", post(logout))
}

fn auth_of(state: &AppState) -> Arc<LoginAuth> {
    state
        .login
        .clone()
        .expect("sign-in routes need sign-in mode")
}

fn current(req: &Request<Body>) -> Current {
    req.extensions()
        .get::<Current>()
        .cloned()
        .unwrap_or(Current {
            session: None,
            seen: Seen {
                ip: None,
                https: false,
                host: None,
            },
        })
}

fn caller_of(c: &Current, user: Option<&str>) -> Caller {
    Caller {
        ip: c.seen.ip.map(|i| i.to_string()),
        user: user.map(str::to_string),
    }
}

fn no_store(v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn with_cookie(mut r: Response, cookie: String) -> Response {
    if let Ok(v) = cookie.parse() {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

async fn body_json(req: Request<Body>) -> Option<Value> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn auth_state(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    no_store(auth.state(current(&req).session.as_ref()))
}

/// Too many attempts from this IP or for this account.
fn locked(auth: &LoginAuth, audit: &AuditLog, c: &Current, user: &str) -> Option<Response> {
    let ip = c.seen.ip.map(|i| i.to_string());
    let wait = auth
        .throttle
        .locked_for(&LoginThrottle::ip_key(ip.as_deref()))
        .or_else(|| {
            (!user.is_empty())
                .then(|| auth.throttle.locked_for(&LoginThrottle::account_key(user)))
                .flatten()
        })?;
    audit.write(
        Some(&caller_of(c, None)),
        "login.locked",
        json!({"user": user, "seconds": wait.as_secs()}),
    );
    let mut r = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(json!({"error": format!(
            "Too many attempts. Try again in {} min.",
            (wait.as_secs_f64() / 60.0).ceil()
        )})),
    )
        .into_response();
    if let Ok(v) = (wait.as_secs_f64().ceil() as u64).to_string().parse() {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    Some(r)
}

fn failed(auth: &LoginAuth, audit: &AuditLog, c: &Current, user: &str, step: &str) -> Response {
    let ip = c.seen.ip.map(|i| i.to_string());
    let ip_lock = auth.throttle.fail(
        &LoginThrottle::ip_key(ip.as_deref()),
        auth.throttle.ip_free_failures,
    );
    let user_lock = (!user.is_empty())
        .then(|| {
            auth.throttle.fail(
                &LoginThrottle::account_key(user),
                auth.throttle.account_free_failures,
            )
        })
        .flatten();
    let caller = caller_of(c, None);
    audit.write(
        Some(&caller),
        "login.fail",
        json!({"user": user, "step": step}),
    );
    if let Some(lock) = ip_lock.or(user_lock) {
        tracing::warn!(
            "Web UI sign-in locked for {}s after repeated failures from {}",
            lock.as_secs(),
            ip.as_deref().unwrap_or("?")
        );
        audit.write(
            Some(&caller),
            "login.lockout",
            json!({"user": user, "seconds": lock.as_secs()}),
        );
    }
    let error = if step == "totp" {
        "That code didn't work."
    } else {
        "Wrong username or password."
    };
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"error": error})),
    )
        .into_response()
}

async fn login(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    let c = current(&req);
    let body = body_json(req).await;
    let user = body
        .as_ref()
        .and_then(|b| b["username"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let password = body
        .as_ref()
        .and_then(|b| b["password"].as_str())
        .unwrap_or("")
        .to_string();
    if let Some(r) = locked(&auth, &state.audit, &c, &user) {
        return r;
    }
    let account = auth.accounts.load();
    // Same work for an unknown user as for a wrong password (no username probing by timing).
    let ok = match &account {
        Some(a) if a.username == user => {
            let (p, h) = (password.clone(), a.password_hash.clone());
            tokio::task::spawn_blocking(move || accounts::verify_password(&p, &h))
                .await
                .unwrap_or(false)
        }
        _ => {
            let p = password.clone();
            let _ = tokio::task::spawn_blocking(move || {
                accounts::verify_password(&p, &accounts::DUMMY_HASH)
            })
            .await;
            false
        }
    };
    let Some(account) = account.filter(|_| ok) else {
        return failed(&auth, &state.audit, &c, &user, "password");
    };
    let ip = c.seen.ip.map(|i| i.to_string());
    auth.throttle.succeed(&LoginThrottle::ip_key(ip.as_deref()));
    if let Some(old) = &c.session {
        auth.sessions.remove(Some(&old.id));
    }
    // 2FA: always once set up; before that only when sign-in mode requires it (set up now).
    let stage = if account.totp_confirmed {
        "totp"
    } else if auth.options.require_totp {
        "enroll"
    } else {
        "ok"
    };
    let https = c.seen.https;
    if stage == "ok" {
        auth.throttle
            .succeed(&LoginThrottle::account_key(&account.username));
        let full = auth
            .sessions
            .promote(&auth.sessions.create(&account.username, "password"));
        state.audit.write(
            Some(&caller_of(&c, Some(&full.user))),
            "login.ok",
            json!({"user": full.user, "totp": false}),
        );
        return with_cookie(
            no_store(auth.state(Some(&full))),
            LoginAuth::set_cookie(https, &full.id),
        );
    }
    let s = auth.sessions.create(&account.username, stage);
    state.audit.write(
        Some(&caller_of(&c, None)),
        "login.password_ok",
        json!({"user": account.username, "next": stage}),
    );
    with_cookie(
        no_store(auth.state(Some(&s))),
        LoginAuth::set_cookie(https, &s.id),
    )
}

/// 2FA setup from a signed-in session (opt in).
async fn enroll_start(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    let c = current(&req);
    let Some(s) = c.session.as_ref().filter(|s| s.stage == "ok") else {
        return status(StatusCode::FORBIDDEN);
    };
    if !auth.accounts.load().is_some_and(|a| !a.totp_confirmed) {
        return status(StatusCode::FORBIDDEN);
    }
    let e = auth.sessions.create(&s.user, "enroll");
    auth.sessions.remove(Some(&s.id));
    with_cookie(
        axum::Json(auth.state(Some(&e))).into_response(),
        LoginAuth::set_cookie(c.seen.https, &e.id),
    )
}

/// The secret as a QR code and as text.
async fn enroll(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    let c = current(&req);
    let Some(s) = c.session.as_ref().filter(|s| s.stage == "enroll") else {
        return status(StatusCode::FORBIDDEN);
    };
    let Some(s) = auth.sessions.update(&s.id, |s| {
        if s.pending_secret.is_none() {
            s.pending_secret = Some(totp::new_secret());
        }
    }) else {
        return status(StatusCode::FORBIDDEN);
    };
    let secret = s.pending_secret.clone().unwrap_or_default();
    let uri = totp::uri(ISSUER, &s.user, &secret);
    let qr = match qr_png(&uri) {
        Some(png) => {
            use base64::Engine;
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(png)
            )
        }
        None => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };
    no_store(json!({"qr": qr, "secret": secret, "uri": uri}))
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

async fn totp_code(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    let c = current(&req);
    let Some(s) = c
        .session
        .clone()
        .filter(|s| s.stage == "totp" || s.stage == "enroll")
    else {
        return status(StatusCode::FORBIDDEN);
    };
    if let Some(r) = locked(&auth, &state.audit, &c, &s.user) {
        return r;
    }
    let body = body_json(req).await;
    let code = body
        .as_ref()
        .and_then(|b| b["code"].as_str())
        .map(str::to_string);
    let Some(mut account) = auth.accounts.load() else {
        return status(StatusCode::FORBIDDEN);
    };
    let enrolling = s.stage == "enroll";
    // The secret shown at setup lives on the session (fetched again: GET /enroll set it).
    let live = auth.sessions.get(Some(&s.id));
    let secret = if enrolling {
        live.and_then(|l| l.pending_secret)
    } else {
        account.totp_secret.clone()
    };
    let last = if enrolling { 0 } else { account.totp_last_step };
    let Some(step) = secret
        .as_deref()
        .and_then(|sec| totp::verify(sec, code.as_deref(), auth.now(), last))
    else {
        return failed(&auth, &state.audit, &c, &s.user, "totp");
    };
    account.totp_last_step = step;
    if enrolling {
        account.totp_secret = secret;
        account.totp_confirmed = true;
        state.audit.write(
            Some(&caller_of(&c, None)),
            "totp.enrolled",
            json!({"user": s.user}),
        );
    }
    let _ = auth.accounts.save(&account);
    auth.throttle.succeed(&LoginThrottle::account_key(&s.user));
    let ip = c.seen.ip.map(|i| i.to_string());
    auth.throttle.succeed(&LoginThrottle::ip_key(ip.as_deref()));
    let full = auth.sessions.promote(&s);
    state.audit.write(
        Some(&caller_of(&c, Some(&full.user))),
        "login.ok",
        json!({"user": full.user}),
    );
    with_cookie(
        no_store(auth.state(Some(&full))),
        LoginAuth::set_cookie(c.seen.https, &full.id),
    )
}

async fn logout(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = auth_of(&state);
    let c = current(&req);
    if let Some(s) = &c.session {
        auth.sessions.remove(Some(&s.id));
        state.audit.write(
            Some(&caller_of(&c, Some(&s.user))),
            "logout",
            json!({"user": s.user}),
        );
    }
    with_cookie(
        axum::Json(auth.state(None)).into_response(),
        LoginAuth::clear_cookie(c.seen.https),
    )
}
