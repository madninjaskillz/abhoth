//! The contract between the Abhoth web server and `abhoth router` (ROUTER-CONFIG.md 11.1-11.2,
//! D-RC5, D-RC6). Both sides share one secret, the **link token** (the router's
//! `secrets.accounts_token`, the web server's `ABHOTH_ROUTER_TOKEN_FILE`). It is used for two
//! things only:
//!
//! - **Key introspection:** the router asks the web server who an `abk_…` key belongs to
//!   (`POST /api/internal/keys/introspect`, `Authorization: Bearer <link token>`), sending the
//!   key's SHA-256, never the key. The answer is an [`Introspection`].
//! - **The acting user:** the web server proxies the admin pages' calls (`/api/admin/router/*`)
//!   to the router's `/admin/api/*` and says who is acting in [`ACTING_USER_HEADER`], an
//!   HMAC-SHA256 over the user, the time, the method and the path, so a header copied from one
//!   call can't be replayed on another path or after [`ACTING_MAX_SKEW_SECS`].
//!
//! Neither side ever sends a key or the token in a URL, a log line or an answer.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The introspection endpoint on the web server.
pub const INTROSPECT_PATH: &str = "/api/internal/keys/introspect";
/// Who is acting, on proxied admin calls (see [`sign_acting_user`]).
pub const ACTING_USER_HEADER: &str = "x-abhoth-acting-user";
/// How far the signed time may be from the router's clock.
pub const ACTING_MAX_SKEW_SECS: i64 = 60;
/// User API keys start with this (D140).
pub const API_KEY_PREFIX: &str = "abk_";

/// The SHA-256 of a key, lower-case hex: what the router sends and the web server looks up
/// (the accounts database stores the same hash).
pub fn key_digest(key: &str) -> String {
    hex(&Sha256::digest(key.as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The role of a key's principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
    /// A named service key (Jarvis, the video pipelines, HA, the dash buttons): owned by the admin
    /// who issued it, limited to its `scopes`, never valid in the web UI itself.
    Service,
}

/// Request body of the introspection call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrospectRequest {
    /// [`key_digest`] of the presented key.
    pub key_sha256: String,
}

/// The web server's answer. `active: false` (and nothing else) for an unknown or revoked key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Introspection {
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    /// The username (users) or the owning admin's username (service keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<i64>,
    /// The key's own name ("jarvis", "laptop").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Service keys: what they may do (`chat`, `models`, `switch`, `leases`, `read`,
    /// `group:<name>`); empty for user and admin keys, whose role decides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// The account (or the service key's owner) is disabled: treat as inactive.
    #[serde(default)]
    pub disabled: bool,
}

/// The scopes a service key may hold (anything else is refused when the key is made).
pub const SERVICE_SCOPES: &[&str] = &["chat", "models", "switch", "leases", "read"];

/// Whether `s` is a valid service-key scope: one of [`SERVICE_SCOPES`] or `group:<name>`.
pub fn valid_scope(s: &str) -> bool {
    SERVICE_SCOPES.contains(&s)
        || s.strip_prefix("group:").is_some_and(|g| {
            !g.is_empty()
                && g.len() <= 40
                && g.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// One signed acting user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActingUser {
    pub user_id: i64,
    pub name: String,
    pub admin: bool,
}

fn mac_input(u: &ActingUser, ts: i64, method: &str, path: &str) -> String {
    format!(
        "v1|{}|{}|{}|{}|{}|{}",
        u.user_id,
        u8::from(u.admin),
        ts,
        enc(&u.name),
        method.to_ascii_uppercase(),
        path
    )
}

/// Names are percent-encoded so `;` and `|` can't split the header.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn dec(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn mac(token: &[u8], input: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(token).expect("hmac takes any key length");
    m.update(input.as_bytes());
    hex(&m.finalize().into_bytes())
}

/// The header value for `u` acting on `method path` at unix time `ts`:
/// `v1;<uid>;<0|1>;<ts>;<name>;<hmac>`. `path` is the router-side path (`/admin/api/...`,
/// without the query).
pub fn sign_acting_user(token: &[u8], u: &ActingUser, ts: i64, method: &str, path: &str) -> String {
    format!(
        "v1;{};{};{};{};{}",
        u.user_id,
        u8::from(u.admin),
        ts,
        enc(&u.name),
        mac(token, &mac_input(u, ts, method, path))
    )
}

/// Why an acting-user header was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActingError {
    Malformed,
    BadSignature,
    Expired,
}

/// Check a header made by [`sign_acting_user`] for this `method path` at time `now`.
pub fn verify_acting_user(
    token: &[u8],
    header: &str,
    now: i64,
    method: &str,
    path: &str,
) -> Result<ActingUser, ActingError> {
    let parts: Vec<&str> = header.trim().split(';').collect();
    let [v, uid, admin, ts, name, sig] = parts.as_slice() else {
        return Err(ActingError::Malformed);
    };
    if *v != "v1" {
        return Err(ActingError::Malformed);
    }
    let user_id: i64 = uid.parse().map_err(|_| ActingError::Malformed)?;
    let admin = match *admin {
        "0" => false,
        "1" => true,
        _ => return Err(ActingError::Malformed),
    };
    let ts: i64 = ts.parse().map_err(|_| ActingError::Malformed)?;
    let name = dec(name).ok_or(ActingError::Malformed)?;
    let u = ActingUser {
        user_id,
        name,
        admin,
    };
    let want = mac(token, &mac_input(&u, ts, method, path));
    if !fixed_time_eq(want.as_bytes(), sig.as_bytes()) {
        return Err(ActingError::BadSignature);
    }
    if (now - ts).abs() > ACTING_MAX_SKEW_SECS {
        return Err(ActingError::Expired);
    }
    Ok(u)
}

/// Constant-time comparison (tokens, MACs).
pub fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> ActingUser {
        ActingUser {
            user_id: 7,
            name: "al;ice|x".into(),
            admin: true,
        }
    }

    #[test]
    fn a_signed_user_verifies_on_the_same_call_only() {
        let t = b"link-token";
        let h = sign_acting_user(t, &alice(), 1000, "post", "/admin/api/config");
        assert_eq!(
            verify_acting_user(t, &h, 1010, "POST", "/admin/api/config"),
            Ok(alice())
        );
        // another path, method, token, or too late
        assert_eq!(
            verify_acting_user(t, &h, 1010, "POST", "/admin/api/switch"),
            Err(ActingError::BadSignature)
        );
        assert_eq!(
            verify_acting_user(t, &h, 1010, "GET", "/admin/api/config"),
            Err(ActingError::BadSignature)
        );
        assert_eq!(
            verify_acting_user(b"other", &h, 1010, "POST", "/admin/api/config"),
            Err(ActingError::BadSignature)
        );
        assert_eq!(
            verify_acting_user(t, &h, 1000 + ACTING_MAX_SKEW_SECS + 1, "POST", "/admin/api/config"),
            Err(ActingError::Expired)
        );
    }

    #[test]
    fn a_tampered_header_is_refused() {
        let t = b"link-token";
        let h = sign_acting_user(
            t,
            &ActingUser {
                user_id: 3,
                name: "bob".into(),
                admin: false,
            },
            50,
            "GET",
            "/admin/api/state",
        );
        let promoted = h.replacen(";0;", ";1;", 1);
        assert_eq!(
            verify_acting_user(t, &promoted, 50, "GET", "/admin/api/state"),
            Err(ActingError::BadSignature)
        );
        assert_eq!(
            verify_acting_user(t, "v1;x", 50, "GET", "/"),
            Err(ActingError::Malformed)
        );
    }

    #[test]
    fn digests_and_scopes() {
        assert_eq!(
            key_digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(valid_scope("switch"));
        assert!(valid_scope("group:switchers"));
        assert!(!valid_scope("group:"));
        assert!(!valid_scope("admin"));
        assert!(!valid_scope("group:a b"));
    }

    #[test]
    fn introspection_round_trips_and_inactive_is_minimal() {
        let inactive = serde_json::to_value(Introspection::default()).unwrap();
        assert_eq!(inactive, serde_json::json!({"active": false, "disabled": false}));
        let svc = Introspection {
            active: true,
            user_id: Some(1),
            name: Some("admin".into()),
            role: Some(Role::Service),
            key_id: Some(9),
            key_name: Some("jarvis".into()),
            scopes: vec!["chat".into(), "switch".into()],
            disabled: false,
        };
        let back: Introspection =
            serde_json::from_value(serde_json::to_value(&svc).unwrap()).unwrap();
        assert_eq!(back, svc);
    }
}
