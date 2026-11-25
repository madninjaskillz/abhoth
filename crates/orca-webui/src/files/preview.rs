//! The HTML preview (`PreviewEndpoints`): an HTML file of the project in a sandboxed frame,
//! its relative links, styles, scripts and images working.
//! * `POST /api/projects/{pid}/preview?wt=` `{path}` mints `/preview/{token}/{file name}`. The
//!   token is signed (HMAC-SHA256, a key made at start-up), names the project, worktree and the
//!   file's folder (the scope), expires after an hour and is bound to the client's address.
//! * `GET/HEAD /preview/{token}/{path}`: a file under that folder; no "..", no link out, no
//!   hidden segment. Every answer carries a sandboxing CSP, no referrer, no sniffing.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::server::AppState;

pub const PREFIX: &str = "/preview";
pub const LIFETIME_SECS: i64 = 3600;
pub const CSP: &str =
    "sandbox allow-scripts allow-forms allow-popups allow-modals; frame-ancestors 'self'";
const MAX_INJECT_BYTES: u64 = 16 * 1024 * 1024;

/// Told to the app where the page is, so the address bar and back/forward can follow it.
pub const NAV_SCRIPT: &str = "<script>/* openorca preview */(function(){if(window.parent===window)return;function s(){try{parent.postMessage({type:'openorca-preview',href:location.href,title:document.title},'*')}catch(e){}}['pushState','replaceState'].forEach(function(k){var f=history[k];history[k]=function(){var r=f.apply(this,arguments);s();return r}});addEventListener('hashchange',s);addEventListener('popstate',s);addEventListener('load',s);s()})();</script>";

/// The key previews are signed with (random per server).
pub struct PreviewKeys {
    pub key: Vec<u8>,
}

impl PreviewKeys {
    pub fn new() -> Self {
        Self {
            key: crate::util::random_bytes(32),
        }
    }

    fn mac(&self, data: &[u8]) -> [u8; 32] {
        hmac_sha256(&self.key, data)
    }

    pub fn sign(&self, g: &Grant) -> String {
        let payload = g.payload();
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(self.mac(&payload))
        )
    }

    pub fn verify(&self, token: &str, ip: &str, now: i64) -> Option<Grant> {
        let (p, s) = token.split_once('.')?;
        if p.is_empty() {
            return None;
        }
        let payload = URL_SAFE_NO_PAD.decode(p).ok()?;
        let sig = URL_SAFE_NO_PAD.decode(s).ok()?;
        let expected = self.mac(&payload);
        if sig.len() != expected.len()
            || sig
                .iter()
                .zip(expected.iter())
                .fold(0u8, |a, (x, y)| a | (x ^ y))
                != 0
        {
            return None;
        }
        let text = String::from_utf8(payload).ok()?;
        let f: Vec<&str> = text.split('\n').collect();
        if f.len() != 6 {
            return None;
        }
        let g = Grant {
            pid: f[0].into(),
            wt: (f[2] == "1").then(|| f[1].to_string()),
            dir: f[3].into(),
            expires: f[4].parse().ok()?,
            ip: f[5].into(),
        };
        (now <= g.expires && g.ip == ip).then_some(g)
    }
}

impl Default for PreviewKeys {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub pid: String,
    pub wt: Option<String>,
    pub dir: String,
    pub expires: i64,
    pub ip: String,
}

impl Grant {
    fn payload(&self) -> Vec<u8> {
        [
            self.pid.as_str(),
            self.wt.as_deref().unwrap_or(""),
            if self.wt.is_some() { "1" } else { "0" },
            &self.dir,
            &self.expires.to_string(),
            &self.ip,
        ]
        .join("\n")
        .into_bytes()
    }
}

/// 127.0.0.1 and ::1 are one machine; IPv4 in IPv6 as IPv4.
pub fn client_ip(addr: IpAddr) -> String {
    let ip = match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(addr),
        a => a,
    };
    if ip.is_loopback() {
        "loopback".into()
    } else {
        ip.to_string()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn is_html(path: &str) -> bool {
    let p = path.to_lowercase();
    p.ends_with(".html") || p.ends_with(".htm")
}

/// The Content-Type a browser needs for a page's parts (text in UTF-8); unknown: bytes.
pub fn content_type_for(path: &str) -> String {
    let lower = path.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let t = match ext {
        "mjs" | "js" => "text/javascript".to_string(),
        "wasm" => "application/wasm".to_string(),
        "map" => "application/json".to_string(),
        _ => mime_guess::from_ext(ext)
            .first()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_else(|| "application/octet-stream".into()),
    };
    if t.starts_with("text/") || t == "application/json" || t == "image/svg+xml" {
        format!("{t}; charset=utf-8")
    } else {
        t
    }
}

fn after_tag(text: &[u8], tag: &str) -> Option<usize> {
    let lower: Vec<u8> = text.iter().map(u8::to_ascii_lowercase).collect();
    let tag = tag.as_bytes();
    let mut i = 0;
    while i + tag.len() <= lower.len() {
        let pos = lower[i..].windows(tag.len()).position(|w| w == tag)?;
        let at = i + pos;
        let next = lower.get(at + tag.len()).copied().unwrap_or(0);
        if matches!(next, b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/') {
            return lower[at..]
                .iter()
                .position(|&c| c == b'>')
                .map(|e| at + e + 1);
        }
        i = at + tag.len();
    }
    None
}

/// The navigation script after `<head>` (or `<html>`, or the doctype; never before it).
pub fn inject_nav_script(html: &[u8]) -> Vec<u8> {
    let at = after_tag(html, "<head")
        .or_else(|| after_tag(html, "<html"))
        .or_else(|| after_tag(html, "<!doctype"))
        .unwrap_or(if html.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        });
    let mut out = Vec::with_capacity(html.len() + NAV_SCRIPT.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(NAV_SCRIPT.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects/{pid}/preview", post(mint))
        .route(&format!("{PREFIX}/{{token}}"), get(bare))
        .route(&format!("{PREFIX}/{{token}}/"), get(serve_root))
        .route(&format!("{PREFIX}/{{token}}/{{*rest}}"), get(serve))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}

async fn mint(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<Value>,
) -> Response {
    let wt = q.get("wt").cloned();
    let Some(rt) = s.projects.get(&pid) else {
        return err(StatusCode::NOT_FOUND, "No such project or worktree.");
    };
    let Some(folder) = rt.resolve_worktree(wt.as_deref()).await else {
        return err(StatusCode::NOT_FOUND, "No such project or worktree.");
    };
    let rel = req["path"]
        .as_str()
        .unwrap_or("")
        .replace('\\', "/")
        .trim()
        .trim_start_matches('/')
        .to_string();
    if !is_html(&rel) {
        return err(
            StatusCode::BAD_REQUEST,
            "Only .html and .htm files can be previewed.",
        );
    }
    let Some(full) = paths::resolve(&folder, Some(&rel)) else {
        return err(StatusCode::FORBIDDEN, "That path is outside the project.");
    };
    if !full.is_file() {
        return err(StatusCode::NOT_FOUND, "No such file.");
    }
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
        None => (String::new(), rel.clone()),
    };
    let expires = now() + LIFETIME_SECS;
    let token = s.previews.sign(&Grant {
        pid,
        wt,
        dir: dir.clone(),
        expires,
        ip: client_ip(addr.ip()),
    });
    let base = format!("{PREFIX}/{token}/");
    axum::Json(json!({
        "url": format!("{base}{}", crate::util::escape_data(&name)),
        "base": base,
        "dir": dir,
        "expires": crate::util::iso_from_unix(expires, 0),
    }))
    .into_response()
}

async fn bare(Path(token): Path<String>) -> Response {
    redirect(&format!("{PREFIX}/{token}/"))
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

fn secure(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    r
}

fn message(status: StatusCode, text: &str) -> Response {
    let esc = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        format!("<!doctype html><meta charset=utf-8><body style=\"font:14px system-ui;color:#555;padding:24px\">{esc}</body>"),
    )
        .into_response()
}

async fn serve_root(
    State(s): State<Arc<AppState>>,
    Path(token): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    secure(serve_inner(&s, &token, "", addr, &headers).await)
}

async fn serve(
    State(s): State<Arc<AppState>>,
    Path((token, rest)): Path<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    secure(serve_inner(&s, &token, &rest, addr, &headers).await)
}

async fn serve_inner(
    s: &AppState,
    token: &str,
    rest: &str,
    addr: SocketAddr,
    headers: &HeaderMap,
) -> Response {
    let Some(grant) = s.previews.verify(token, &client_ip(addr.ip()), now()) else {
        return message(
            StatusCode::FORBIDDEN,
            "This preview link has expired or is not valid here. Reload the preview in OpenOrca.",
        );
    };
    let Some(rt) = s.projects.get(&grant.pid) else {
        return message(StatusCode::NOT_FOUND, "The project is gone.");
    };
    let Some(folder) = rt.resolve_worktree(grant.wt.as_deref()).await else {
        return message(StatusCode::NOT_FOUND, "The project is gone.");
    };
    let Some(scope) = paths::resolve(&folder, Some(&grant.dir)) else {
        return message(StatusCode::NOT_FOUND, "The project is gone.");
    };
    let rel = rest.replace('\\', "/");
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| p.starts_with('.')) {
        return message(StatusCode::NOT_FOUND, "Not found.");
    }
    let Some(mut full) = paths::resolve(&scope, Some(&rel)) else {
        return message(StatusCode::FORBIDDEN, "Outside the previewed folder.");
    };
    if full.is_dir() {
        if !rel.is_empty() && !rel.ends_with('/') {
            let escaped: Vec<String> = parts.iter().map(|p| crate::util::escape_data(p)).collect();
            return redirect(&format!("{PREFIX}/{token}/{}/", escaped.join("/")));
        }
        match ["index.html", "index.htm"]
            .iter()
            .map(|n| full.join(n))
            .find(|p| p.is_file())
        {
            Some(index) => full = index,
            None => return message(StatusCode::NOT_FOUND, "No index.html in this folder."),
        }
    }
    let Ok(meta) = std::fs::metadata(&full)
        .map_err(|_| ())
        .and_then(|m| if m.is_file() { Ok(m) } else { Err(()) })
    else {
        return message(StatusCode::NOT_FOUND, "Not found.");
    };
    let name = full.to_string_lossy().to_string();
    let ctype = content_type_for(&name);
    let Ok(bytes) = tokio::fs::read(&full).await else {
        return message(StatusCode::NOT_FOUND, "Not found.");
    };
    let body = if is_html(&name) && meta.len() <= MAX_INJECT_BYTES {
        inject_nav_script(&bytes)
    } else {
        bytes
    };
    let mut r = ([(header::CONTENT_TYPE, ctype)], body).into_response();
    // A sandboxed page's own fetches come from origin "null": answer them (the token still holds).
    if headers.get(header::ORIGIN).is_some_and(|o| o == "null") {
        r.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("null"),
        );
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn tokens_verify_only_for_the_same_client_and_until_expiry() {
        let k = PreviewKeys::new();
        let g = Grant {
            pid: "p".into(),
            wt: None,
            dir: "site".into(),
            expires: 100,
            ip: "loopback".into(),
        };
        let t = k.sign(&g);
        assert_eq!(k.verify(&t, "loopback", 50), Some(g.clone()));
        assert_eq!(k.verify(&t, "10.0.0.2", 50), None);
        assert_eq!(k.verify(&t, "loopback", 101), None);
        assert_eq!(PreviewKeys::new().verify(&t, "loopback", 50), None);
        assert_eq!(k.verify("garbage", "loopback", 50), None);
    }

    #[test]
    fn the_nav_script_goes_after_head_never_before_the_doctype() {
        let out = String::from_utf8(inject_nav_script(
            b"<!DOCTYPE html><HTML><Head lang=x><title>t</title>",
        ))
        .unwrap();
        assert!(out.starts_with("<!DOCTYPE html><HTML><Head lang=x><script>"));
        let out = String::from_utf8(inject_nav_script(b"<!doctype html><p>x")).unwrap();
        assert!(out.starts_with("<!doctype html><script>"));
        let out = String::from_utf8(inject_nav_script(b"<header>x")).unwrap();
        assert!(out.starts_with("<script>"));
        assert_eq!(content_type_for("a.JS"), "text/javascript; charset=utf-8");
        assert_eq!(content_type_for("a.png"), "image/png");
    }
}
