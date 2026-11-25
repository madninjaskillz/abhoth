//! `/api/auth/*` (state, first-run setup, sign-in, the 2FA step, sign-out) and `/api/account/*`
//! (the signed-in user's password, 2FA, recovery codes, API keys and sessions).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};

use super::db::{Session, User};
use super::store::LoginThrottle;
use super::{
    API_KEY_PREFIX, Current, LoginAuth, Principal, SETUP_COOKIE, Via, error, password, recovery,
    totp,
};
use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/enroll", get(enroll))
        .route("/api/auth/totp", post(totp_code))
        .route("/api/auth/logout", post(logout))
        .route("/api/account", get(account))
        .route("/api/account/password", post(change_password))
        .route("/api/account/totp/start", post(totp_start))
        .route("/api/account/totp/confirm", post(totp_confirm))
        .route("/api/account/totp/disable", post(totp_disable))
        .route("/api/account/recovery-codes", post(new_recovery_codes))
        .route("/api/account/keys", get(keys).post(create_key))
        .route("/api/account/keys/{id}", delete(revoke_key))
        .route("/api/account/sessions", get(sessions))
        .route("/api/account/sessions/{id}", delete(revoke_session))
        .route("/api/account/sessions/revoke-all", post(revoke_all))
}

fn current(req: &Request<Body>) -> Current {
    req.extensions()
        .get::<Current>()
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn no_store(v: Value) -> Response {
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

pub(crate) async fn body_json(req: Request<Body>) -> Value {
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return Value::Null;
    };
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// Argon2 off the async threads.
async fn hash_password(pw: String, cost: password::Cost) -> String {
    tokio::task::spawn_blocking(move || password::hash(&pw, cost))
        .await
        .unwrap_or_default()
}

async fn verify_password(pw: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || password::verify(&pw, &stored))
        .await
        .unwrap_or(false)
}

async fn auth_state(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let c = current(&req);
    no_store(
        state
            .login
            .state_json(c.session.as_ref(), c.setup_cookie.is_some()),
    )
}

/// The signed-in principal (the middleware sets it for `/api/account`).
fn principal(req: &Request<Body>) -> Option<Principal> {
    super::principal_of(req)
}

/// A new browser session for `user` at `stage`: the state JSON with the cookie.
fn signed_in(
    auth: &LoginAuth,
    user: &User,
    stage: &str,
    c: &Current,
    headers: &HeaderMap,
    extra: Option<Value>,
) -> Response {
    if let Some((old, _)) = &c.session {
        auth.db.delete_session(old.id);
    }
    let Some((token, s)) = auth.start_session(user, stage, &c.seen, headers, auth.desktop) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not start a session.",
        );
    };
    let fresh = auth.db.user(user.id).unwrap_or_else(|| user.clone());
    let mut v = auth.state_json(Some(&(s, fresh)), false);
    if let (Some(o), Some(Value::Object(e))) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    with_cookie(no_store(v), LoginAuth::set_cookie(c.seen.https, &token))
}

/// `POST /api/auth/setup {username, password, code?}`: the first admin, only while there are
/// no users, for whoever holds the setup code (the launch token).
async fn setup(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = state.login.clone();
    let c = current(&req);
    let headers = req.headers().clone();
    let body = body_json(req).await;
    let ip = c.seen.ip_text();
    if !auth.setup_needed() {
        return error(StatusCode::CONFLICT, "The admin account exists already.");
    }
    if let Some(r) = locked(&auth, ip.as_deref(), "") {
        return r;
    }
    let code = c
        .setup_cookie
        .clone()
        .unwrap_or_else(|| text(&body, "code").trim().to_string());
    if !auth.launch_token_matches(Some(&code)) {
        auth.throttle.fail(
            &LoginThrottle::ip_key(ip.as_deref()),
            auth.throttle.ip_free_failures,
        );
        auth.record("setup.fail", None, None, ip.as_deref(), json!({}));
        return error(StatusCode::FORBIDDEN, "That setup code is not right.");
    }
    let name = match super::check_username(&text(&body, "username")) {
        Ok(n) => n,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let pw = text(&body, "password");
    if let Err(e) = password::check(&pw) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let hash = hash_password(pw, auth.options.cost).await;
    let user = match auth.db.create_user(&name, &hash, true, true, auth.now()) {
        Ok(u) => u,
        Err(super::db::CreateError::NotFirst) => {
            return error(StatusCode::CONFLICT, "The admin account exists already.");
        }
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let p = Principal::of(&user, Via::LaunchToken);
    auth.record(
        "user.created",
        Some(&p),
        Some(&user.username),
        ip.as_deref(),
        json!({"admin": true, "via": "setup"}),
    );
    auth.db.touch_login(user.id, auth.now());
    let r = signed_in(&auth, &user, "ok", &c, &headers, None);
    with_cookie(
        r,
        format!("{SETUP_COOKIE}=; max-age=0; path=/; samesite=strict; httponly"),
    )
}

/// Too many attempts from this IP or for this account: the 429.
fn locked(auth: &LoginAuth, ip: Option<&str>, user: &str) -> Option<Response> {
    let wait = auth
        .throttle
        .locked_for(&LoginThrottle::ip_key(ip))
        .or_else(|| {
            (!user.is_empty())
                .then(|| auth.throttle.locked_for(&LoginThrottle::account_key(user)))
                .flatten()
        })?;
    auth.record(
        "login.locked",
        None,
        Some(user).filter(|u| !u.is_empty()),
        ip,
        json!({"seconds": wait.as_secs()}),
    );
    let mut r = error(
        StatusCode::TOO_MANY_REQUESTS,
        &format!(
            "Too many attempts. Try again in {} min.",
            (wait.as_secs_f64() / 60.0).ceil()
        ),
    );
    if let Ok(v) = (wait.as_secs_f64().ceil() as u64).to_string().parse() {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    Some(r)
}

/// Count a failed attempt (per IP and per account name, known or not).
fn failed(auth: &LoginAuth, ip: Option<&str>, user: &str, step: &str) -> Response {
    let ip_lock = auth
        .throttle
        .fail(&LoginThrottle::ip_key(ip), auth.throttle.ip_free_failures);
    let user_lock = (!user.is_empty())
        .then(|| {
            auth.throttle.fail(
                &LoginThrottle::account_key(user),
                auth.throttle.account_free_failures,
            )
        })
        .flatten();
    auth.record(
        "login.fail",
        None,
        Some(user).filter(|u| !u.is_empty()),
        ip,
        json!({"step": step}),
    );
    if let Some(lock) = ip_lock.or(user_lock) {
        tracing::warn!(
            "Web UI sign-in locked for {}s after repeated failures from {}",
            lock.as_secs(),
            ip.unwrap_or("?")
        );
        auth.record(
            "login.lockout",
            None,
            Some(user).filter(|u| !u.is_empty()),
            ip,
            json!({"seconds": lock.as_secs()}),
        );
    }
    let text = if step == "password" {
        "Wrong username or password."
    } else {
        "That code didn't work."
    };
    error(StatusCode::UNAUTHORIZED, text)
}

async fn login(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = state.login.clone();
    let c = current(&req);
    let headers = req.headers().clone();
    let body = body_json(req).await;
    let name = text(&body, "username").trim().to_string();
    let pw = text(&body, "password");
    let ip = c.seen.ip_text();
    if let Some(r) = locked(&auth, ip.as_deref(), &name) {
        return r;
    }
    let user = auth.db.user_by_name(&name);
    // The same work for an unknown or disabled user as for a wrong password (no probing by
    // timing), and the same answer.
    let stored = user
        .as_ref()
        .map(|u| u.password_hash.clone())
        .unwrap_or_else(|| password::dummy_hash(auth.options.cost).to_string());
    let ok = verify_password(pw.clone(), stored.clone()).await;
    let Some(user) = user.filter(|u| ok && !u.disabled) else {
        return failed(&auth, ip.as_deref(), &name, "password");
    };
    auth.throttle.succeed(&LoginThrottle::ip_key(ip.as_deref()));
    if password::needs_rehash(&stored, auth.options.cost) {
        let hash = hash_password(pw, auth.options.cost).await;
        auth.db.set_password_hash(user.id, &hash);
    }
    let stage = if user.totp_enabled {
        "totp"
    } else if auth.options.require_totp {
        "enroll"
    } else {
        "ok"
    };
    if stage == "ok" {
        auth.throttle
            .succeed(&LoginThrottle::account_key(&user.username));
        auth.db.touch_login(user.id, auth.now());
        auth.record(
            "login.ok",
            Some(&Principal::of(&user, Via::LaunchToken)),
            Some(&user.username),
            ip.as_deref(),
            json!({"totp": false}),
        );
    } else {
        auth.record(
            "login.password_ok",
            None,
            Some(&user.username),
            ip.as_deref(),
            json!({"next": stage}),
        );
    }
    signed_in(&auth, &user, stage, &c, &headers, None)
}

/// The pending session's user at `stage` (the 2FA step of a sign-in).
fn pending(c: &Current, stages: &[&str]) -> Option<(Session, User)> {
    c.session
        .clone()
        .filter(|(s, _)| stages.contains(&s.stage.as_str()))
}

/// `GET /api/auth/enroll`: the secret to set up (stage "enroll", when 2FA is required).
async fn enroll(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = state.login.clone();
    let c = current(&req);
    let Some((_, user)) = pending(&c, &["enroll"]) else {
        return error(StatusCode::FORBIDDEN, "Sign in first.");
    };
    let secret = match user.totp_pending.clone() {
        Some(s) => s,
        None => {
            let s = totp::new_secret();
            auth.db.set_totp_pending(user.id, Some(&s));
            s
        }
    };
    match super::enroll_json(&user.username, &secret) {
        Some(v) => no_store(v),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "No QR code."),
    }
}

/// Check a TOTP code (or, `recovery`, a recovery code) for `user`; on success the step is
/// used up. Enrolling: against the pending secret.
fn check_code(auth: &LoginAuth, user: &User, code: &str, enrolling: bool) -> Option<&'static str> {
    if enrolling {
        let secret = user.totp_pending.as_deref()?;
        let step = totp::verify(secret, Some(code), auth.now(), 0)?;
        return auth.db.enable_totp(user.id, secret, step).then_some("totp");
    }
    if recovery::looks_like(code) {
        return auth
            .db
            .use_recovery_code(user.id, code, auth.now())
            .then_some("recovery");
    }
    let secret = user.totp_secret.as_deref()?;
    let step = totp::verify(secret, Some(code), auth.now(), user.totp_last_step)?;
    auth.db.use_totp_step(user.id, step).then_some("totp")
}

async fn totp_code(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = state.login.clone();
    let c = current(&req);
    let headers = req.headers().clone();
    let ip = c.seen.ip_text();
    let Some((_, user)) = pending(&c, &["totp", "enroll"]) else {
        return error(StatusCode::FORBIDDEN, "Sign in first.");
    };
    let enrolling = c.session.as_ref().is_some_and(|(s, _)| s.stage == "enroll");
    if let Some(r) = locked(&auth, ip.as_deref(), &user.username) {
        return r;
    }
    let body = body_json(req).await;
    let code = text(&body, "code");
    let Some(kind) = check_code(&auth, &user, code.trim(), enrolling) else {
        return failed(&auth, ip.as_deref(), &user.username, "totp");
    };
    auth.throttle
        .succeed(&LoginThrottle::account_key(&user.username));
    auth.throttle.succeed(&LoginThrottle::ip_key(ip.as_deref()));
    let p = Principal::of(&user, Via::LaunchToken);
    let mut extra = None;
    if enrolling {
        let codes = recovery::new_codes();
        auth.db.set_recovery_codes(user.id, &codes);
        auth.record(
            "totp.enabled",
            Some(&p),
            Some(&user.username),
            ip.as_deref(),
            json!({}),
        );
        extra = Some(json!({"recoveryCodes": codes}));
    }
    if kind == "recovery" {
        auth.record(
            "recovery.used",
            Some(&p),
            Some(&user.username),
            ip.as_deref(),
            json!({"left": auth.db.recovery_codes_left(user.id)}),
        );
    }
    auth.db.touch_login(user.id, auth.now());
    auth.record(
        "login.ok",
        Some(&p),
        Some(&user.username),
        ip.as_deref(),
        json!({"totp": true, "with": kind}),
    );
    signed_in(&auth, &user, "ok", &c, &headers, extra)
}

async fn logout(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let auth = state.login.clone();
    let c = current(&req);
    if let Some((s, u)) = &c.session {
        auth.db.delete_session(s.id);
        state.hub.drop_where(|caller| caller.sid == Some(s.id));
        auth.record(
            "logout",
            Some(&Principal::of(u, Via::LaunchToken)),
            Some(&u.username),
            c.seen.ip_text().as_deref(),
            json!({}),
        );
    }
    with_cookie(
        no_store(auth.state_json(None, false)),
        LoginAuth::clear_cookie(c.seen.https),
    )
}

// ---- /api/account ----

/// The signed-in user, fresh from the database.
// An error is a ready response, used once per request.
#[allow(clippy::result_large_err)]
fn me(state: &AppState, req: &Request<Body>) -> Result<(Principal, User), Response> {
    let p = principal(req).ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Sign in first."))?;
    let u = state
        .login
        .db
        .user(p.user_id)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Sign in first."))?;
    Ok((p, u))
}

macro_rules! me_or_return {
    ($state:expr, $req:expr) => {
        match me(&$state, &$req) {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

async fn account(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (_, u) = me_or_return!(state, req);
    no_store(json!({
        "id": u.id,
        "username": u.username,
        "admin": u.admin,
        "totp": u.totp_enabled,
        "created": u.created,
        "lastLogin": u.last_login,
        "recoveryCodesLeft": state.login.db.recovery_codes_left(u.id),
    }))
}

/// The current password, when the request needs one (throttled like sign-in).
// An error is a ready response, used once per request.
#[allow(clippy::result_large_err)]
async fn confirm_password(
    auth: &LoginAuth,
    user: &User,
    given: String,
    ip: Option<&str>,
) -> Result<(), Response> {
    if let Some(r) = locked(auth, ip, &user.username) {
        return Err(r);
    }
    if verify_password(given, user.password_hash.clone()).await {
        Ok(())
    } else {
        auth.throttle.fail(
            &LoginThrottle::account_key(&user.username),
            auth.throttle.account_free_failures,
        );
        auth.record(
            "password.confirm_fail",
            None,
            Some(&user.username),
            ip,
            json!({}),
        );
        Err(error(
            StatusCode::FORBIDDEN,
            "The current password is wrong.",
        ))
    }
}

async fn change_password(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let ip = current(&req).seen.ip_text();
    let body = body_json(req).await;
    let new = text(&body, "password");
    if let Err(e) = password::check(&new) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    if let Err(r) = confirm_password(&auth, &u, text(&body, "current"), ip.as_deref()).await {
        return r;
    }
    let hash = hash_password(new, auth.options.cost).await;
    auth.db.set_password_hash(u.id, &hash);
    // Every other session ends (this one stays).
    let keep = p.session_id();
    auth.db.delete_sessions_of(u.id, keep);
    state
        .hub
        .drop_where(|c| c.uid == Some(u.id) && c.sid != keep);
    auth.record(
        "password.changed",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

async fn totp_start(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (_, u) = me_or_return!(state, req);
    if u.totp_enabled {
        return error(StatusCode::CONFLICT, "Two-factor sign-in is on already.");
    }
    let secret = totp::new_secret();
    state.login.db.set_totp_pending(u.id, Some(&secret));
    match super::enroll_json(&u.username, &secret) {
        Some(v) => no_store(v),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "No QR code."),
    }
}

async fn totp_confirm(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let ip = current(&req).seen.ip_text();
    if u.totp_enabled || u.totp_pending.is_none() {
        return error(StatusCode::CONFLICT, "Start the setup first.");
    }
    if let Some(r) = locked(&auth, ip.as_deref(), &u.username) {
        return r;
    }
    let body = body_json(req).await;
    if check_code(&auth, &u, text(&body, "code").trim(), true).is_none() {
        return failed(&auth, ip.as_deref(), &u.username, "totp");
    }
    let codes = recovery::new_codes();
    auth.db.set_recovery_codes(u.id, &codes);
    auth.record(
        "totp.enabled",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"recoveryCodes": codes}))
}

async fn totp_disable(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let ip = current(&req).seen.ip_text();
    let body = body_json(req).await;
    if let Err(r) = confirm_password(&auth, &u, text(&body, "password"), ip.as_deref()).await {
        return r;
    }
    if auth.options.require_totp {
        return error(
            StatusCode::FORBIDDEN,
            "This server requires two-factor sign-in.",
        );
    }
    auth.db.disable_totp(u.id);
    auth.record(
        "totp.disabled",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"ok": true}))
}

async fn new_recovery_codes(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let ip = current(&req).seen.ip_text();
    let body = body_json(req).await;
    if !u.totp_enabled {
        return error(StatusCode::CONFLICT, "Turn on two-factor sign-in first.");
    }
    if let Err(r) = confirm_password(&auth, &u, text(&body, "password"), ip.as_deref()).await {
        return r;
    }
    let codes = recovery::new_codes();
    auth.db.set_recovery_codes(u.id, &codes);
    auth.record(
        "recovery.regenerated",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({}),
    );
    no_store(json!({"recoveryCodes": codes}))
}

async fn keys(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (_, u) = me_or_return!(state, req);
    no_store(json!(state.login.db.api_keys_of(u.id)))
}

async fn create_key(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let ip = current(&req).seen.ip_text();
    let body = body_json(req).await;
    let name = text(&body, "name").trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return error(
            StatusCode::BAD_REQUEST,
            "Name the key (1 to 64 characters).",
        );
    }
    if auth.db.api_keys_of(u.id).len() >= 50 {
        return error(
            StatusCode::BAD_REQUEST,
            "50 keys at most; revoke some first.",
        );
    }
    let secret = super::store_token();
    let key = format!("{API_KEY_PREFIX}{secret}");
    let prefix: String = key.chars().take(API_KEY_PREFIX.len() + 6).collect();
    let Some(k) = auth
        .db
        .create_api_key(u.id, &name, &key, &prefix, auth.now())
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save the key.");
    };
    auth.record(
        "apikey.created",
        Some(&p),
        Some(&u.username),
        ip.as_deref(),
        json!({"key": k.id, "name": name}),
    );
    let mut v = json!(k);
    v["key"] = json!(key);
    no_store(v)
}

async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    if !auth.db.delete_api_key(u.id, id) {
        return error(StatusCode::NOT_FOUND, "No such key.");
    }
    auth.record(
        "apikey.revoked",
        Some(&p),
        Some(&u.username),
        current(&req).seen.ip_text().as_deref(),
        json!({"key": id}),
    );
    no_store(json!({"ok": true}))
}

async fn sessions(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let list: Vec<Value> = state
        .login
        .db
        .sessions_of(u.id)
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "created": s.created,
                "lastSeen": s.last_seen,
                "ip": s.ip,
                "userAgent": s.user_agent,
                "current": Some(s.id) == p.session_id(),
                "desktop": s.desktop,
            })
        })
        .collect();
    no_store(json!(list))
}

async fn revoke_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    if !auth.db.sessions_of(u.id).iter().any(|s| s.id == id) {
        return error(StatusCode::NOT_FOUND, "No such session.");
    }
    auth.db.delete_session(id);
    state.hub.drop_where(|c| c.sid == Some(id));
    auth.record(
        "session.revoked",
        Some(&p),
        Some(&u.username),
        current(&req).seen.ip_text().as_deref(),
        json!({"session": id}),
    );
    no_store(json!({"ok": true}))
}

/// Sign out everywhere: every browser session of this user ends, this one too.
async fn revoke_all(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (p, u) = me_or_return!(state, req);
    let auth = state.login.clone();
    let c = current(&req);
    let n = auth.db.delete_sessions_of(u.id, None);
    state.hub.drop_where(|caller| caller.uid == Some(u.id));
    auth.record(
        "session.revoked_all",
        Some(&p),
        Some(&u.username),
        c.seen.ip_text().as_deref(),
        json!({"sessions": n}),
    );
    let r = no_store(json!({"ok": true}));
    if matches!(p.via, Via::Session { .. }) {
        with_cookie(r, LoginAuth::clear_cookie(c.seen.https))
    } else {
        r
    }
}
