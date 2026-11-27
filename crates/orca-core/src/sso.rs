//! Sign-in with Abhoth (D154): the router's admin pages sign people in with their Abhoth web UI
//! account, through a small OAuth2 authorization-code flow with PKCE (S256). The web UI is the
//! provider; each router is a registered client (`client_id`, exact redirect URIs, a secret
//! stored hashed there and in a 0600 file on the router). Both sides share these paths and
//! shapes. Design: `docs/design/ROUTER-SSO.md`.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The consent page (the web UI's app, which signs the user in first when needed).
pub const AUTHORIZE_PAGE: &str = "/oauth/authorize";
/// What the consent page asks and answers (session-authenticated JSON).
pub const AUTHORIZE_API: &str = "/api/oauth/authorize";
/// The code exchange (server to server; the client's id and secret).
pub const TOKEN_PATH: &str = "/api/oauth/token";
/// A router without the accounts link asks to be registered (an admin approves it).
pub const LINK_REQUESTS_PATH: &str = "/api/oauth/link-requests";
pub const LINK_POLL_PATH: &str = "/api/oauth/link-requests/poll";
/// A linked router registers itself (its link token).
pub const REGISTER_INTERNAL_PATH: &str = "/api/internal/sso/register";
/// The router's callback, beside its admin pages (so its cookies have the same path, `/admin`,
/// as the key sign-in's, also behind a proxy's prefix).
pub const CALLBACK_PATH: &str = "/admin/sso-callback";
/// The router's "Sign in with Abhoth" start.
pub const START_PATH: &str = "/admin/sso";
/// A code lives this long and works once.
pub const CODE_TTL_SECS: i64 = 60;

/// `POST TOKEN_PATH` (JSON).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: String,
    pub redirect_uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub code_verifier: String,
}

/// Who signed in (no token: the router makes its own session from this).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenResponse {
    /// The web UI's user id.
    pub sub: i64,
    pub username: String,
    pub admin: bool,
}

/// What a router registers: a name and its exact redirect URIs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientRegistration {
    pub name: String,
    pub redirect_uris: Vec<String>,
}

/// A registered client's credentials (shown once, kept 0600 by the router).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
    /// The redirect URIs registered with them.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
}

/// A pending link request (`POST LINK_REQUESTS_PATH`'s answer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkTicket {
    pub id: String,
    /// The short code an admin compares on the approval screen ("ABCD-2345").
    pub code: String,
    /// Only the requester has it; it collects the credentials once.
    pub poll: String,
    pub expires_in: i64,
}

/// `POST LINK_POLL_PATH` `{id, poll}` → this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkStatus {
    /// "pending", "approved" (credentials included, once), "denied" or "expired".
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<ClientCredentials>,
}

/// URL-safe base64 without padding.
pub fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The PKCE S256 challenge of a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// A PKCE verifier or challenge: 43 to 128 unreserved characters (RFC 7636 4.1).
pub fn pkce_ok(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Does `verifier` answer `challenge` (S256)?
pub fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    pkce_ok(verifier)
        && crate::router_link::fixed_time_eq(
            pkce_challenge(verifier).as_bytes(),
            challenge.as_bytes(),
        )
}

/// A redirect URI a client may register: absolute http(s), no fragment, no userinfo, and the
/// router's callback path.
pub fn redirect_uri_ok(u: &str) -> Result<(), String> {
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .ok_or("must start with http:// or https://")?;
    if u.len() > 512 || u.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("not a plain URL".into());
    }
    if u.contains('#') || u.contains('?') {
        return Err("no query or fragment".into());
    }
    let (authority, path) = rest.split_once('/').ok_or("needs a path")?;
    if authority.is_empty() || authority.contains('@') {
        return Err("needs a host (and no user name)".into());
    }
    if !format!("/{path}").ends_with(CALLBACK_PATH) {
        return Err(format!("must end with {CALLBACK_PATH}"));
    }
    if path.split('/').any(|s| s == ".." || s == ".") {
        return Err("no . or .. in the path".into());
    }
    Ok(())
}

/// The callback URI for a router base URL ("http://host:8010" or with a proxy prefix).
pub fn callback_for(base: &str) -> String {
    format!("{}{CALLBACK_PATH}", base.trim_end_matches('/'))
}

/// `uri` with `pairs` appended as its query (the URI has none: [`redirect_uri_ok`]).
pub fn with_query(uri: &str, pairs: &[(&str, &str)]) -> String {
    let q: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect();
    format!("{uri}?{}", q.join("&"))
}

/// Percent-encoding for a query value.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A random URL-safe token of `bytes` random bytes.
pub fn random(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("the OS's random source");
    b64url(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc_7636_appendix_b() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(v),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(pkce_matches(
            v,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_matches("short", &pkce_challenge("short")));
        assert!(!pkce_matches(
            &"a".repeat(43),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_ok(&format!("{}!", "a".repeat(43))));
    }

    #[test]
    fn redirect_uris_are_exact_callbacks() {
        assert!(redirect_uri_ok("http://router.lan:8010/admin/sso-callback").is_ok());
        assert!(redirect_uri_ok("https://example.org/router/admin/sso-callback").is_ok());
        for bad in [
            "router.lan:8010/admin/sso-callback",
            "javascript:alert(1)//admin/sso-callback",
            "http://router.lan:8010/admin/sso-callback?next=x",
            "http://router.lan:8010/admin/sso-callback#x",
            "http://user@router.lan/admin/sso-callback",
            "http:///admin/sso-callback",
            "http://router.lan/elsewhere",
            "http://router.lan/../admin/sso-callback",
            "http://router lan/admin/sso-callback",
        ] {
            assert!(redirect_uri_ok(bad).is_err(), "{bad}");
        }
        assert_eq!(
            callback_for("http://h:8010/"),
            "http://h:8010/admin/sso-callback"
        );
    }

    #[test]
    fn queries_are_encoded() {
        assert_eq!(
            with_query(
                "http://h/admin/sso-callback",
                &[("code", "a b"), ("state", "x&y=z")]
            ),
            "http://h/admin/sso-callback?code=a%20b&state=x%26y%3Dz"
        );
        assert_eq!(random(32).len(), 43);
        assert_ne!(random(16), random(16));
    }
}
