//! The router's own admin pages (D151), on the gateway's port under `/admin/`:
//!
//! - `GET /admin/` and `/admin/assets/*`: the page (the React client's `router-admin` bundle,
//!   `client/dist-router`, embedded in the binary). Every URL in it is relative, so it works
//!   behind a reverse proxy under any prefix and through the Abhoth app's proxy.
//! - `GET /admin/session`: who the caller is (and the CSRF token of a browser session);
//!   `POST /admin/login` `{key}`: sign in with an admin key (a session cookie);
//!   `POST /admin/logout`.
//! - `/admin/api/gateway[/check]`: `router.json` (the gateway's model list) read, checked and
//!   saved (If-Match, backup, atomic write, audit, hot reload).
//!
//! Everything else under `/admin/api/` is the control plane's admin API ([`crate::control_api`]).
//! The pages need `--control router.toml`; with `control.enabled = false` they still work for
//! config and keys (the admin-only plane), and they never switch a layout by themselves.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::access::sessions;
use crate::access::{AuthError, Principal};
use crate::control_api::ControlPlane;
use crate::state::AppState;

#[derive(rust_embed::RustEmbed)]
#[folder = "$ABHOTH_ROUTER_UI_DIST"]
struct Ui;

/// The pages' Content-Security-Policy: same-origin everything, framed only by the same origin
/// (the Abhoth app embeds the page through its own proxy).
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; worker-src 'self' blob:; connect-src 'self'; frame-ancestors 'self'; base-uri 'self'; form-action 'self'; object-src 'none'";

/// Paths this module answers (before the legacy `/admin/*` calls and the admin API).
pub fn is_ui_path(path: &str) -> bool {
    matches!(
        path,
        "/admin"
            | "/admin/"
            | "/admin/index.html"
            | "/admin/session"
            | "/admin/login"
            | "/admin/logout"
    ) || path.starts_with("/admin/assets/")
        || path.starts_with("/admin/brand/")
}

/// Is the page built into this binary?
pub fn bundled() -> bool {
    Ui::get("index.html").is_some()
}

fn json_resp(status: u16, v: &Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        axum::Json(v.clone()),
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn err(status: u16, msg: &str) -> Response {
    json_resp(status, &json!({"error": msg}))
}

fn harden(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
}

fn asset(name: &str) -> Response {
    let Some(f) = Ui::get(name) else {
        if name == "index.html" {
            let mut r = (
                StatusCode::SERVICE_UNAVAILABLE,
                "The admin pages are not built into this router (client/dist-router was missing at build time).",
            )
                .into_response();
            harden(&mut r);
            return r;
        }
        return err(404, "Not Found");
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let mut r = Response::new(Body::from(f.data.into_owned()));
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // Hashed asset names never change; the page itself always comes fresh.
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if name.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-store"
        }),
    );
    harden(&mut r);
    r
}

/// Handle one of [`is_ui_path`]'s paths.
pub async fn handle(
    st: &Arc<AppState>,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    raw: &Bytes,
) -> Response {
    match (method.clone(), path) {
        (Method::GET | Method::HEAD, "/admin") => {
            // Relative, so a proxy's prefix is kept.
            let mut r = StatusCode::PERMANENT_REDIRECT.into_response();
            r.headers_mut()
                .insert(header::LOCATION, HeaderValue::from_static("admin/"));
            r
        }
        (Method::GET | Method::HEAD, "/admin/" | "/admin/index.html") => asset("index.html"),
        (Method::GET | Method::HEAD, p)
            if p.starts_with("/admin/assets/") || p.starts_with("/admin/brand/") =>
        {
            let name = &p["/admin/".len()..];
            if name.split('/').any(|s| s == ".." || s.is_empty()) {
                return err(400, "bad path");
            }
            asset(name)
        }
        (Method::GET, "/admin/session") => session(st, headers, peer).await,
        (Method::POST, "/admin/login") => login(st, headers, peer, raw).await,
        (Method::POST, "/admin/logout") => logout(st, headers),
        _ => err(405, "method not allowed"),
    }
}

fn via(p: &Principal, session: bool) -> &'static str {
    match p {
        _ if session => "session",
        Principal::Key(_) => "key",
        Principal::Loopback => "loopback",
        Principal::Acting(_) => "web",
        Principal::Anonymous { .. } => "none",
    }
}

fn plane_info(cp: &ControlPlane) -> Value {
    json!({
        "localKeys": cp.access.local_keys.is_some(),
        "hasKeys": cp.access.local_keys.as_ref().map(|k| k.active_count() > 0).unwrap_or(false),
        "linked": cp.access.settings.accounts_url.is_some(),
        "loopbackAdmin": cp.access.settings.loopback_admin,
    })
}

fn base_info(st: &AppState) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": crate::discovery::display_name(st.discovery.get()),
        "controlPlane": st.control.get().is_some(),
        "gateway": st.config_path.as_ref().map(|p| p.display().to_string()),
    })
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(x), Some(y)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in y {
            x.insert(k.clone(), v.clone());
        }
    }
    a
}

async fn session(st: &Arc<AppState>, headers: &HeaderMap, peer: Option<IpAddr>) -> Response {
    let base = base_info(st);
    let Some(cp) = st.admin_plane() else {
        return json_resp(
            200,
            &merge(
                base,
                json!({
                    "available": false,
                    "signedIn": false,
                    "admin": false,
                    "message": "The admin pages need the router started with --control router.toml (see ROUTER.md, \"The router's own admin pages\").",
                }),
            ),
        );
    };
    let info = merge(base, plane_info(cp));
    match cp
        .access
        .authenticate_session(headers, peer, "GET", "/admin/session")
        .await
    {
        Ok((p, csrf)) => {
            let signed = !matches!(p, Principal::Anonymous { .. });
            json_resp(
                200,
                &merge(
                    info,
                    json!({
                        "available": true,
                        "signedIn": signed,
                        "admin": p.is_admin(),
                        "principal": if signed { Value::String(p.label()) } else { Value::Null },
                        "via": via(&p, csrf.is_some()),
                        "csrfHeader": sessions::CSRF_HEADER,
                        "csrf": csrf,
                    }),
                ),
            )
        }
        Err(e) => json_resp(
            200,
            &merge(
                info,
                json!({
                    "available": true,
                    "signedIn": false,
                    "admin": false,
                    "message": match e {
                        AuthError::Unauthorized(m) => m,
                        AuthError::Unavailable => "the accounts server can't vouch for keys right now",
                    },
                }),
            ),
        ),
    }
}

async fn login(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    raw: &Bytes,
) -> Response {
    let Some(cp) = st.admin_plane() else {
        return err(
            404,
            "the admin pages are off (start the router with --control)",
        );
    };
    // JSON only: a cross-site form can't send it without a CORS preflight this never answers.
    let json_body = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/json"));
    if !json_body {
        return err(415, "send JSON");
    }
    let key = serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|v| v.get("key").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let from = peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    match cp.access.sign_in(&key, peer).await {
        Ok((identity, source)) => {
            let p = Principal::Key(identity.clone());
            let (id, csrf) = cp.access.sessions.create(identity, source);
            cp.backend
                .record(&p.label(), "admin.sign-in", &from, "", "", "", "");
            let mut r = json_resp(
                200,
                &json!({
                    "ok": true,
                    "principal": p.label(),
                    "admin": true,
                    "csrfHeader": sessions::CSRF_HEADER,
                    "csrf": csrf,
                }),
            );
            if let Ok(v) =
                HeaderValue::from_str(&sessions::set_cookie(&id, sessions::https(headers)))
            {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            r
        }
        Err(e) => {
            if e != crate::access::SignInError::TooMany {
                // Who tried is an address; the key is never written anywhere.
                cp.backend.record(
                    &format!("ip:{from}"),
                    "admin.sign-in-refused",
                    &from,
                    "",
                    "",
                    "",
                    e.message(),
                );
            }
            let mut r = err(e.status(), e.message());
            if e == crate::access::SignInError::TooMany {
                r.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("900"));
            }
            r
        }
    }
}

fn logout(st: &Arc<AppState>, headers: &HeaderMap) -> Response {
    if let (Some(cp), Some(id)) = (st.admin_plane(), sessions::cookie(headers)) {
        cp.access.sessions.remove(id);
    }
    let mut r = json_resp(200, &json!({"ok": true}));
    if let Ok(v) = HeaderValue::from_str(&sessions::clear_cookie(sessions::https(headers))) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

// ── router.json ─────────────────────────────────────────────────────────────────────────────

/// The rev a page edits against: the file's text hash (not its `rev` field).
pub fn gateway_rev(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Problems with a `router.json` text, as the router itself would load it (keys never echoed).
pub fn gateway_problems(text: &str, base_dir: Option<&Path>) -> Vec<Value> {
    let mut out = Vec::new();
    let config: crate::config::RouterConfig = match serde_json::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            out.push(json!({"path": "", "line": e.line(), "message": e.to_string(), "severity": "error"}));
            return out;
        }
    };
    // A key belongs in a keyFile, never in the file.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let mut found = Vec::new();
        literal_keys(&v, "", &mut found);
        for p in found {
            out.push(json!({"path": p, "message": "looks like a key: put it in a file and name it with keyFile", "severity": "error"}));
        }
    }
    if let Err(e) = crate::config::Snapshot::build(config, base_dir) {
        out.push(json!({"path": "", "message": e, "severity": "error"}));
    }
    out
}

fn literal_keys(v: &Value, at: &str, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.starts_with("sk-") || s.starts_with("abk_") || s.starts_with("Bearer ") {
                out.push(at.to_string());
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                literal_keys(x, &format!("{at}[{i}]"), out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                literal_keys(x, &p, out);
            }
        }
        _ => {}
    }
}

fn gateway_path(st: &AppState) -> Option<PathBuf> {
    st.config_path.clone()
}

fn backup(path: &Path, old: &str, rev: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .map(|d| d.join("backups"))
        .unwrap_or_else(|| PathBuf::from("backups"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("backups: {e}"))?;
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "router.json".into());
    let name = format!(
        "{stem}.bak-{}-{rev}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    std::fs::write(dir.join(name), old).map_err(|e| format!("backup: {e}"))?;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let prefix = format!("{stem}.bak-");
        let mut v: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .collect();
        v.sort();
        if v.len() > 100 {
            for p in &v[..v.len() - 100] {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    Ok(())
}

/// Serialises `router.json` saves.
static GATEWAY_SAVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `/admin/api/gateway[/check]` (admins only).
pub async fn gateway(
    st: &Arc<AppState>,
    cp: &ControlPlane,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    raw: &Bytes,
) -> Response {
    let principal = match crate::control_api::authorize(cp, method, path, headers, peer).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !principal.is_admin() {
        return err(403, "only an admin can do that");
    }
    let Some(file) = gateway_path(st) else {
        return err(
            404,
            "this router has no router.json (started without --config)",
        );
    };
    let base_dir = file.parent().map(Path::to_path_buf);
    let body: Value = if raw.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(_) => return err(400, "the body must be JSON"),
        }
    };
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    match (method.clone(), path) {
        (Method::GET, "/admin/api/gateway") => {
            let data = serde_json::from_str::<Value>(&current).unwrap_or(Value::Null);
            let writable = std::fs::metadata(&file)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false);
            let mut r = json_resp(
                200,
                &json!({
                    "rev": gateway_rev(&current),
                    "path": file.display().to_string(),
                    "text": current,
                    "data": data,
                    "error": st.config_error.lock().unwrap().clone(),
                    "writable": writable,
                    "overlay": st.overlay.get().is_some(),
                }),
            );
            if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", gateway_rev(&current))) {
                r.headers_mut().insert(header::ETAG, v);
            }
            r
        }
        (Method::POST, "/admin/api/gateway/check") => {
            let text = body["text"].as_str().unwrap_or("");
            let problems = gateway_problems(text, base_dir.as_deref());
            json_resp(
                200,
                &json!({
                    "ok": problems.is_empty(),
                    "problems": problems,
                    "diff": crate::control_api::live::line_diff(&current, text),
                    "consequences": [],
                    "rev": gateway_rev(text),
                }),
            )
        }
        (Method::PUT, "/admin/api/gateway") => {
            let Some(if_match) = headers
                .get(header::IF_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim_matches('"').to_string())
            else {
                return err(428, "send If-Match with the rev you edited");
            };
            let text = body["text"].as_str().unwrap_or("").to_string();
            let reason = body["reason"].as_str().unwrap_or("").to_string();
            let _g = GATEWAY_SAVE.lock().await;
            let disk = std::fs::read_to_string(&file).unwrap_or_default();
            let disk_rev = gateway_rev(&disk);
            if if_match != disk_rev {
                return json_resp(
                    412,
                    &json!({"error": "the file changed since you loaded it; reload and merge", "currentRev": disk_rev}),
                );
            }
            let problems = gateway_problems(&text, base_dir.as_deref());
            if !problems.is_empty() {
                return json_resp(
                    422,
                    &json!({"error": "router.json has errors; nothing was saved", "problems": problems}),
                );
            }
            if let Err(e) = backup(&file, &disk, &disk_rev) {
                return err(500, &format!("{e}; nothing was saved"));
            }
            let tmp = file.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &file)) {
                let _ = std::fs::remove_file(&tmp);
                return err(500, &format!("could not write router.json: {e}"));
            }
            let diff = crate::control_api::live::line_diff(&disk, &text);
            let new_rev = gateway_rev(&text);
            cp.backend.record(
                &principal.label(),
                "gateway.save",
                "router.json",
                &disk_rev,
                &new_rev,
                &diff,
                &reason,
            );
            let reloaded = crate::reload::reload_now(st);
            json_resp(
                200,
                &json!({
                    "ok": true,
                    "rev": new_rev,
                    "diff": diff,
                    "problems": [],
                    "consequences": [],
                    "reloaded": reloaded.is_ok(),
                    "reloadError": reloaded.err(),
                }),
            )
        }
        _ => err(404, "no such admin call"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_paths() {
        for p in [
            "/admin",
            "/admin/",
            "/admin/session",
            "/admin/login",
            "/admin/assets/x.js",
        ] {
            assert!(is_ui_path(p), "{p}");
        }
        for p in ["/admin/api/state", "/admin/reload", "/admin/cards/A/drain"] {
            assert!(!is_ui_path(p), "{p}");
        }
    }

    #[test]
    fn literal_keys_are_refused() {
        let t = r#"{"upstreams": {"A": {"url": "http://e:1", "keyFile": "k"}}, "deployments": [{"name": "m", "cards": ["A"], "servedId": "m", "params": {"api_key": "sk-abc"}}]}"#;
        let p = gateway_problems(t, None);
        assert!(
            p.iter()
                .any(|x| x["path"] == "deployments[0].params.api_key"),
            "{p:?}"
        );
        assert!(
            !p.iter().any(|x| x.to_string().contains("sk-abc")),
            "never echoed"
        );
    }

    #[test]
    fn bad_json_and_bad_configs_are_problems() {
        assert!(!gateway_problems("{", None).is_empty());
        let unknown_card = r#"{"deployments": [{"name": "m", "cards": ["Z"], "servedId": "m"}]}"#;
        let p = gateway_problems(unknown_card, None);
        assert!(p[0]["message"].as_str().unwrap().contains("unknown card"));
        let good = r#"{"upstreams": {"A": {"url": "http://e:1"}}, "deployments": [{"name": "m", "cards": ["A"], "servedId": "m"}]}"#;
        assert!(gateway_problems(good, None).is_empty());
        assert_eq!(gateway_rev(good).len(), 12);
    }
}
