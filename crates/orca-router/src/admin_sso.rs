//! "Sign in with Abhoth" on the router's admin pages (D154): `GET /admin/sso` (start) and
//! `GET /admin/sso-callback` (finish). The flow itself is in [`crate::access::sso`]; this is the
//! HTTP side and the small pages shown when something is wrong.

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use orca_core::router_link::Role;
use orca_core::sso::{AUTHORIZE_PAGE, callback_for, with_query};

use crate::access::sessions::{self, KeySource};
use crate::access::sso::{FLOW_COOKIE, FLOW_TTL, SsoClient};
use crate::access::{Identity, Principal, SSO_SCOPE, forwarded};
use crate::control_api::ControlPlane;
use crate::state::AppState;

/// The web UI that people sign in at, as their browsers reach it: `sso_url`, else the origin of
/// `--key-url` on a linked router. `None`: Sign-in with Abhoth is off.
pub fn provider_url(st: &AppState, cp: &ControlPlane) -> Option<String> {
    let s = &cp.access.settings;
    if !s.sso || cp.access.sso.is_none() {
        return None;
    }
    if let Some(u) = s
        .sso_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        return Some(u.trim_end_matches('/').to_string());
    }
    s.accounts_url.as_ref()?;
    let key_url = st.discovery.get().and_then(|d| d.key_url.clone())?;
    origin(&key_url)
}

/// "https://h:1/x#y" → "https://h:1".
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}"))
}

/// Linked (it may register itself over the link).
fn linked(cp: &ControlPlane) -> Option<(&str, &[u8])> {
    Some((
        cp.access.settings.accounts_url.as_deref()?,
        cp.access.link_token()?,
    ))
}

/// Does the sign-in page offer the button? Set up, and registered or able to register.
pub fn offered(st: &AppState, cp: &ControlPlane) -> bool {
    provider_url(st, cp).is_some()
        && (linked(cp).is_some()
            || cp
                .access
                .sso
                .as_ref()
                .is_some_and(|s| s.credentials().is_some()))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A small self-contained page (no script). `next`: where its link (or refresh) goes.
fn page(
    status: StatusCode,
    title: &str,
    text: &str,
    next: Option<&str>,
    refresh: bool,
) -> Response {
    let link = next
        .map(|n| {
            format!(
                "<p><a href=\"{}\">Back to the router's admin pages</a></p>",
                escape(n)
            )
        })
        .unwrap_or_default();
    let meta = match (refresh, next) {
        (true, Some(n)) => format!(
            "<meta http-equiv=\"refresh\" content=\"0;url={}\">",
            escape(n)
        ),
        _ => String::new(),
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{meta}<title>{t}</title>\
<style>body{{font:14px system-ui,sans-serif;margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#f6f7f9;color:#1d232b}}\
main{{max-width:28rem;margin:1rem;padding:1.5rem;border:1px solid #d9dde3;border-radius:.6rem;background:#fff}}h1{{font-size:1rem;margin:0 0 .5rem}}a{{color:#2563eb}}\
@media(prefers-color-scheme:dark){{body{{background:#111418;color:#e6e9ee}}main{{background:#1a1f25;border-color:#2c333c}}a{{color:#7aa7ff}}}}</style></head>\
<body><main><h1>{t}</h1><p>{x}</p>{link}</main></body></html>",
        t = escape(title),
        x = escape(text),
    );
    let mut r = (status, html).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'",
        ),
    );
    r
}

fn problem(status: StatusCode, text: &str) -> Response {
    page(status, "Sign in with Abhoth", text, Some("./"), false)
}

fn flow_cookie(value: &str, secure: bool, max_age: u64) -> String {
    format!(
        "{FLOW_COOKIE}={value}; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn add_cookie(r: &mut Response, c: &str) {
    if let Ok(v) = HeaderValue::from_str(c) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
}

fn query_param(q: Option<&str>, name: &str) -> Option<String> {
    q?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(v)) => {
                out.push(v);
                i += 2;
            }
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The callback for the address this request came to: a listed public URL with that host (a
/// proxy's prefix included), else scheme and Host.
fn callback_here(cp: &ControlPlane, headers: &HeaderMap) -> Option<(String, String)> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty() && !h.contains(['/', '@', ' ']))?
        .to_string();
    let scheme = if sessions::https(headers) {
        "https"
    } else {
        "http"
    };
    for u in &cp.access.settings.public_urls {
        if origin(u).is_some_and(|o| o.eq_ignore_ascii_case(&format!("{scheme}://{host}"))) {
            return Some((callback_for(u), host));
        }
    }
    Some((callback_for(&format!("{scheme}://{host}")), host))
}

fn public_hosts(cp: &ControlPlane) -> Vec<String> {
    cp.access
        .settings
        .public_urls
        .iter()
        .filter_map(|u| origin(u))
        .filter_map(|o| o.split_once("://").map(|(_, h)| h.to_string()))
        .collect()
}

/// `GET /admin/sso`: on to the web UI's consent page.
pub async fn start(st: &Arc<AppState>, headers: &HeaderMap, peer: Option<IpAddr>) -> Response {
    let Some(cp) = st.admin_plane() else {
        return problem(
            StatusCode::NOT_FOUND,
            "The admin pages are off (start the router with --control).",
        );
    };
    let (Some(idp), Some(sso)) = (provider_url(st, cp), cp.access.sso.as_ref()) else {
        return problem(
            StatusCode::NOT_FOUND,
            "Sign in with Abhoth is not set up on this router (see ROUTER.md, \"Sign in with Abhoth\"). Sign in with an admin key instead.",
        );
    };
    let from = peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    if !cp.access.sessions.may_try(peer) {
        return problem(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed sign-ins from this address; wait a few minutes.",
        );
    }
    let Some((callback, host)) = callback_here(cp, headers) else {
        return problem(StatusCode::BAD_REQUEST, "The request named no host.");
    };
    let creds = sso.credentials();
    if !creds
        .as_ref()
        .is_some_and(|c| c.redirect_uris.contains(&callback))
    {
        let mayreg = SsoClient::may_register(&host, forwarded(headers), &public_hosts(cp));
        match linked(cp) {
            Some((url, token)) if mayreg => {
                let mut uris: Vec<String> = creds.map(|c| c.redirect_uris).unwrap_or_default();
                for u in cp
                    .access
                    .settings
                    .public_urls
                    .iter()
                    .map(|u| callback_for(u))
                {
                    if !uris.contains(&u) {
                        uris.push(u);
                    }
                }
                uris.push(callback.clone());
                // At most 16 (the provider's limit): the newest stay.
                let skip = uris.len().saturating_sub(16);
                let uris: Vec<String> = uris.into_iter().skip(skip).collect();
                let name = crate::discovery::display_name(st.discovery.get());
                if let Err(e) = sso.register(url, token, &name, uris).await {
                    tracing::warn!("sign-in with Abhoth: registering this router: {e}");
                    return problem(
                        StatusCode::BAD_GATEWAY,
                        &format!("Could not register this router with the Abhoth server: {e}."),
                    );
                }
                cp.backend.record(
                    "router",
                    "admin.sso-registered",
                    &callback,
                    "",
                    "",
                    "",
                    "over the link",
                );
            }
            _ if creds.is_none() => {
                return problem(
                    StatusCode::CONFLICT,
                    "This router is not registered with an Abhoth server yet. On its host run `abhoth router sso link --webui <Abhoth URL> --control router.toml`, then have an Abhoth admin approve it.",
                );
            }
            _ => {
                let at = creds
                    .and_then(|c| c.redirect_uris.first().cloned())
                    .map(|u| {
                        u.trim_end_matches(orca_core::sso::CALLBACK_PATH)
                            .to_string()
                            + "/admin/"
                    })
                    .unwrap_or_default();
                return problem(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "This address is not registered for Sign in with Abhoth. Open the admin pages at {at} (or add this address to access.public_urls and link again)."
                    ),
                );
            }
        }
    }
    let Some(creds) = sso.credentials() else {
        return problem(
            StatusCode::CONFLICT,
            "This router has no Sign in with Abhoth credentials.",
        );
    };
    let flow = match sso.begin(&callback, peer) {
        Ok(f) => f,
        Err(e) => return problem(StatusCode::TOO_MANY_REQUESTS, e),
    };
    let to = with_query(
        &format!("{idp}{AUTHORIZE_PAGE}"),
        &[
            ("response_type", "code"),
            ("client_id", &creds.client_id),
            ("redirect_uri", &callback),
            ("state", &flow.state),
            ("code_challenge", &flow.challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    tracing::debug!("sign-in with Abhoth started from {from}");
    let mut r = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(&to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    add_cookie(
        &mut r,
        &flow_cookie(&flow.binding, sessions::https(headers), FLOW_TTL.as_secs()),
    );
    r
}

/// `GET /admin/sso-callback?code&state` (or `?error&state`): the admin session.
pub async fn callback(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    query: Option<&str>,
) -> Response {
    let Some(cp) = st.admin_plane() else {
        return problem(StatusCode::NOT_FOUND, "The admin pages are off.");
    };
    let (Some(idp), Some(sso)) = (provider_url(st, cp), cp.access.sso.as_ref()) else {
        return problem(
            StatusCode::NOT_FOUND,
            "Sign in with Abhoth is not set up on this router.",
        );
    };
    let from = peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    let secure = sessions::https(headers);
    let refuse = |status: StatusCode, why: &str, count: bool| {
        if count {
            cp.access.sessions.failed(peer);
        }
        cp.backend.record(
            &format!("ip:{from}"),
            "admin.sign-in-refused",
            &from,
            "",
            "",
            "",
            &format!("sign in with Abhoth: {why}"),
        );
        let mut r = problem(status, &format!("Sign-in refused: {why}."));
        add_cookie(&mut r, &flow_cookie("", secure, 0));
        r
    };
    if !cp.access.sessions.may_try(peer) {
        return problem(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed sign-ins from this address; wait a few minutes.",
        );
    }
    let state = query_param(query, "state").unwrap_or_default();
    let flow = match sso.finish(&state, sessions::named_cookie(headers, FLOW_COOKIE)) {
        Ok(f) => f,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e, true),
    };
    if let Some(e) = query_param(query, "error") {
        let why = if e == "access_denied" {
            "you declined at the Abhoth server".to_string()
        } else {
            format!(
                "the Abhoth server said {}",
                e.chars().take(60).collect::<String>()
            )
        };
        return refuse(StatusCode::FORBIDDEN, &why, false);
    }
    let Some(code) = query_param(query, "code").filter(|c| !c.is_empty()) else {
        return refuse(StatusCode::BAD_REQUEST, "no code came back", true);
    };
    let base = cp.access.settings.accounts_url.clone().unwrap_or(idp);
    let who = match sso.exchange(&base, &code, &flow).await {
        Ok(t) => t,
        Err(e) => return refuse(StatusCode::BAD_GATEWAY, &e, true),
    };
    if !who.admin {
        return refuse(
            StatusCode::FORBIDDEN,
            &format!("{} is not an Abhoth admin", who.username),
            false,
        );
    }
    let identity = Identity {
        user_id: who.sub,
        name: who.username.clone(),
        role: Role::Admin,
        key_id: 0,
        key_name: String::new(),
        scopes: vec![SSO_SCOPE.to_string()],
    };
    let p = Principal::Key(identity.clone());
    let (id, _csrf) = cp.access.sessions.create(identity, KeySource::Sso(who.sub));
    cp.backend.record(
        &p.label(),
        "admin.sign-in",
        &from,
        "",
        "",
        "",
        "sign in with Abhoth",
    );
    // A page, not a redirect: the session cookie is SameSite=Strict, and a redirect that ends a
    // cross-site chain would not carry it; this page's own navigation does.
    let mut r = page(
        StatusCode::OK,
        "Signed in",
        &format!("Signed in as {}. Opening the admin pages…", who.username),
        Some("./"),
        true,
    );
    add_cookie(&mut r, &sessions::set_cookie(&id, secure));
    add_cookie(&mut r, &flow_cookie("", secure, 0));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_and_origins() {
        assert_eq!(
            query_param(Some("code=a%2Bb&state=x+y"), "code").as_deref(),
            Some("a+b")
        );
        assert_eq!(
            query_param(Some("code=a&state=x+y"), "state").as_deref(),
            Some("x y")
        );
        assert_eq!(query_param(Some("code=a"), "state"), None);
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(
            origin("https://chat.example.org/#settings/service-keys").as_deref(),
            Some("https://chat.example.org")
        );
        assert_eq!(origin("ftp://x/"), None);
        assert_eq!(
            escape("<a href=\"x\">&"),
            "&lt;a href=&quot;x&quot;&gt;&amp;"
        );
    }
}
