//! Who may do what on the router (ROUTER-CONFIG.md 11.1): principals from D140 keys (checked by
//! introspection, [`keys`]), the web server's signed acting user (admin pages), loopback and
//! anonymous LAN callers; the permission table; route access (`users`, `admins`,
//! `keys:<names>`); and the gate in front of `/admin/api/*`.
//!
//! All of it is inert unless `control.enabled` is on: with it off the router keeps today's
//! data-plane behaviour (no keys needed).

pub mod keys;

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::HeaderMap;
use orca_core::router_link::{
    ACTING_USER_HEADER, ActingError, ActingUser, Role, verify_acting_user,
};
use serde::{Deserialize, Serialize};

pub use keys::{Identity, KeyCache, KeyError};

/// `[access]` in `router.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AccessSettings {
    /// The Abhoth web server (introspection, D-RC6), e.g. "http://abhoth-web:8192".
    pub accounts_url: Option<String>,
    /// The link token shared with the web server: a secret reference (`secret:<id>`), never a
    /// value.
    pub accounts_token: Option<String>,
    /// CIDRs whose callers may chat without a key during the transition (open question 6).
    /// Switching always needs a key.
    pub anonymous_lan: Vec<String>,
    /// Requests from loopback act as an admin (the CLI on the router's own host or container).
    pub loopback_admin: bool,
    /// `/v1/model/info` stays open to anonymous LAN callers until the legacy mirror ends
    /// (open question 15).
    pub model_info_lan: bool,
    pub cache_seconds: u64,
    pub negative_cache_seconds: u64,
    pub stale_seconds: u64,
}

impl Default for AccessSettings {
    fn default() -> Self {
        Self {
            accounts_url: None,
            accounts_token: None,
            anonymous_lan: Vec::new(),
            loopback_admin: true,
            model_info_lan: true,
            cache_seconds: 60,
            negative_cache_seconds: 10,
            stale_seconds: 900,
        }
    }
}

impl AccessSettings {
    /// Validation messages (field path, problem).
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, c) in self.anonymous_lan.iter().enumerate() {
            if Cidr::parse(c).is_none() {
                out.push((
                    format!("access.anonymous_lan[{i}]"),
                    format!("not a CIDR: {c}"),
                ));
            }
        }
        if let Some(t) = &self.accounts_token
            && !t.starts_with("secret:")
        {
            out.push((
                "access.accounts_token".into(),
                "must be a secret reference (secret:<id>), never the token".into(),
            ));
        }
        if self.accounts_url.is_some() != self.accounts_token.is_some() {
            out.push((
                "access".into(),
                "accounts_url and accounts_token go together".into(),
            ));
        }
        out
    }
}

/// An IPv4/IPv6 network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.trim().split_once('/').unwrap_or((s.trim(), ""));
        let net: IpAddr = a.parse().ok()?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = if b.is_empty() { max } else { b.parse().ok()? };
        (bits <= max).then_some(Self { net, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let m = if self.bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.bits)
                };
                (u32::from(n) & m) == (u32::from(i) & m)
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let m = if self.bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.bits)
                };
                (u128::from(n) & m) == (u128::from(i) & m)
            }
            _ => false,
        }
    }
}

/// Who a request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// No credentials; `lan`: from an `anonymous_lan` network.
    Anonymous { lan: bool },
    /// From loopback with `loopback_admin`.
    Loopback,
    /// An `abk_…` key (user, admin or service).
    Key(Identity),
    /// A signed-in web UI user, through the web server's proxy.
    Acting(ActingUser),
}

/// What a principal may do (the table in 11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    Models,
    Chat,
    /// Start an on-demand switch (also needs the route to allow it).
    OnDemand,
    Leases,
    ModelInfo,
    /// Read config, state, jobs, plans, audit.
    ReadControl,
    /// Edit config, offline/online, holds, release others' leases.
    EditControl,
    /// Switch layouts (admins; service keys with `switch`).
    Switch,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        match self {
            Principal::Loopback => true,
            Principal::Key(i) => i.role == Role::Admin,
            Principal::Acting(a) => a.admin,
            Principal::Anonymous { .. } => false,
        }
    }

    fn scoped(&self, scope: &str) -> bool {
        matches!(self, Principal::Key(i) if i.role == Role::Service && i.scopes.iter().any(|s| s == scope))
    }

    /// The permission table. `model_info_lan`: the transition rule for `/v1/model/info`.
    pub fn allows(&self, perm: Perm, model_info_lan: bool) -> bool {
        if self.is_admin() {
            return true;
        }
        match self {
            Principal::Anonymous { lan } => match perm {
                Perm::Models | Perm::Chat | Perm::Leases => *lan,
                Perm::ModelInfo => *lan && model_info_lan,
                _ => false,
            },
            Principal::Key(i) if i.role == Role::User => {
                matches!(perm, Perm::Models | Perm::Chat | Perm::Leases)
            }
            Principal::Key(_) => match perm {
                Perm::Models => self.scoped("models") || self.scoped("chat"),
                Perm::Chat => self.scoped("chat"),
                Perm::OnDemand | Perm::Switch => self.scoped("switch"),
                Perm::Leases => self.scoped("leases"),
                Perm::ReadControl | Perm::ModelInfo => self.scoped("read"),
                Perm::EditControl => false,
            },
            // A signed-in user who is not an admin (the web server only proxies admin calls).
            Principal::Acting(_) => matches!(perm, Perm::Models),
            Principal::Loopback => true,
        }
    }

    /// May this principal use a route whose `access` is `users`, `admins` or `keys:<a,b>`
    /// (a list of key names or `group:` scopes)?
    pub fn may_use_route(&self, access: &str) -> bool {
        if self.is_admin() {
            return true;
        }
        let access = access.trim();
        if access == "anonymous" {
            return true;
        }
        if access.is_empty() || access == "users" {
            return match self {
                Principal::Key(i) if i.role == Role::Service => {
                    self.scoped("chat") || self.scoped("models")
                }
                Principal::Key(_) => true,
                Principal::Anonymous { lan } => *lan,
                Principal::Acting(_) => true,
                Principal::Loopback => true,
            };
        }
        if access == "admins" {
            return false;
        }
        if let Some(list) = access.strip_prefix("keys:") {
            let Principal::Key(i) = self else {
                return false;
            };
            return list.split(',').map(str::trim).any(|n| {
                !n.is_empty()
                    && (n == i.key_name || i.scopes.iter().any(|s| s == &format!("group:{n}")))
            });
        }
        false
    }

    /// For logs and the audit: never a key.
    pub fn label(&self) -> String {
        match self {
            Principal::Anonymous { lan: true } => "anonymous-lan".into(),
            Principal::Anonymous { lan: false } => "anonymous".into(),
            Principal::Loopback => "loopback".into(),
            Principal::Key(i) => match i.role {
                Role::Service => format!("service:{}#{}", i.key_name, i.key_id),
                _ => format!("user:{}#{}", i.name, i.key_id),
            },
            Principal::Acting(a) => format!("web:{}#{}", a.name, a.user_id),
        }
    }

    /// The JSON the admin pages show for "who am I".
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "principal": self.label(),
            "admin": self.is_admin(),
        })
    }
}

/// Why a request could not be authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 401: a key or signature that is not valid.
    Unauthorized(&'static str),
    /// 503: the accounts server could not vouch for the key.
    Unavailable,
}

/// The runtime side of `[access]`.
pub struct Access {
    pub settings: AccessSettings,
    lan: Vec<Cidr>,
    token: Option<Vec<u8>>,
    pub keys: Option<KeyCache>,
    now_unix: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Access {
    /// `token`: the resolved link token (from `accounts_token`); `None` disables key checks and
    /// the acting-user header (every key is then refused).
    pub fn new(settings: AccessSettings, token: Option<Vec<u8>>) -> Self {
        let keys = match (&settings.accounts_url, &token) {
            (Some(url), Some(t)) => Some(KeyCache::new(
                Arc::new(keys::HttpIntrospector::new(
                    url,
                    String::from_utf8_lossy(t).trim().to_string(),
                )),
                keys::CacheTimes {
                    positive: std::time::Duration::from_secs(settings.cache_seconds),
                    negative: std::time::Duration::from_secs(settings.negative_cache_seconds),
                    stale: std::time::Duration::from_secs(settings.stale_seconds),
                },
                keys::system_clock(),
            )),
            _ => None,
        };
        Self::with_parts(settings, token, keys, Arc::new(|| chrono::Utc::now().timestamp()))
    }

    /// Tests: a given key cache and clock.
    pub fn with_parts(
        settings: AccessSettings,
        token: Option<Vec<u8>>,
        keys: Option<KeyCache>,
        now_unix: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let lan = settings
            .anonymous_lan
            .iter()
            .filter_map(|c| Cidr::parse(c))
            .collect();
        Self {
            settings,
            lan,
            token: token.map(|t| String::from_utf8_lossy(&t).trim().as_bytes().to_vec()),
            keys,
            now_unix,
        }
    }

    fn in_lan(&self, ip: Option<IpAddr>) -> bool {
        ip.is_some_and(|ip| self.lan.iter().any(|c| c.contains(ip)))
    }

    /// Who is calling. `peer`: the TCP peer (not X-Forwarded-For: a header can't make a caller
    /// local or LAN). `path`: without the query.
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        method: &str,
        path: &str,
    ) -> Result<Principal, AuthError> {
        if let Some(h) = headers
            .get(ACTING_USER_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            let Some(token) = &self.token else {
                return Err(AuthError::Unauthorized("no link token is configured"));
            };
            return match verify_acting_user(token, h, (self.now_unix)(), method, path) {
                Ok(u) => Ok(Principal::Acting(u)),
                Err(ActingError::Expired) => Err(AuthError::Unauthorized("acting user expired")),
                Err(_) => Err(AuthError::Unauthorized("acting user not signed")),
            };
        }
        if let Some(key) = bearer(headers) {
            let Some(cache) = &self.keys else {
                return Err(AuthError::Unauthorized("keys are not checked here"));
            };
            return match cache.check(key).await {
                Ok(i) => Ok(Principal::Key(i)),
                Err(KeyError::Invalid) => Err(AuthError::Unauthorized("not a valid API key")),
                Err(KeyError::Unavailable(_)) => Err(AuthError::Unavailable),
            };
        }
        if self.settings.loopback_admin && peer.is_some_and(|p| p.is_loopback()) {
            return Ok(Principal::Loopback);
        }
        Ok(Principal::Anonymous {
            lan: self.in_lan(peer),
        })
    }

    pub fn allows(&self, p: &Principal, perm: Perm) -> bool {
        p.allows(perm, self.settings.model_info_lan)
    }
}

/// The bearer token of a request (`Authorization: Bearer x` or `x-api-key: x`).
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    if let Some(a) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let (scheme, key) = a.split_once(' ')?;
        return scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| key.trim())
            .filter(|k| !k.is_empty());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests;
