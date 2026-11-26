//! Who is calling, as the switch side needs it (ROUTER-CONFIG.md 7.2, 11.1).
//!
//! [`CallerResolver`] is the seam between the switch code (on-demand gate, profiles shim) and
//! 🟨C's accounts integration: [`crate::access::Access`] implements it (keys introspected by
//! the web server with the 60 s [`crate::access::KeyCache`], signed acting users, loopback,
//! anonymous LAN). Tests use [`StaticCallers`].
//!
//! The switch side only needs: a principal label (never a key), whether the caller may switch
//! layouts / take cards on- and offline (`Perm::Switch`), whether it may start an on-demand
//! switch (`Perm::OnDemand`), and the calendar event grants its key carries (none until event
//! keys land in the introspection answer).

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

use axum::http::HeaderMap;

use crate::access::{Access, AuthError, Perm, bearer};
use crate::guards::Caller;

/// A caller as the switch side sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Resolved {
    /// `principal` is `None` for anonymous callers.
    pub caller: Caller,
    /// May switch layouts and take cards on/offline (shim POSTs).
    pub may_switch: bool,
    /// May start an on-demand switch (also needs the route's `cold = "on_demand"`).
    pub may_on_demand: bool,
}

/// Why a caller could not be resolved: `(status, message)`, 401 or 503.
pub type ResolveError = (u16, String);

pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Resolved, ResolveError>> + Send + 'a>>;

/// Resolves the caller of one request. Object safe.
pub trait CallerResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        headers: &'a HeaderMap,
        peer: Option<IpAddr>,
        method: &'a str,
        path: &'a str,
    ) -> ResolveFuture<'a>;
}

impl CallerResolver for Access {
    fn resolve<'a>(
        &'a self,
        headers: &'a HeaderMap,
        peer: Option<IpAddr>,
        method: &'a str,
        path: &'a str,
    ) -> ResolveFuture<'a> {
        Box::pin(async move {
            match self.authenticate(headers, peer, method, path).await {
                Ok(p) => {
                    let anonymous = matches!(p, crate::access::Principal::Anonymous { .. });
                    Ok(Resolved {
                        caller: Caller {
                            principal: (!anonymous).then(|| p.label()),
                            grants: Vec::new(),
                        },
                        may_switch: self.allows(&p, Perm::Switch),
                        may_on_demand: self.allows(&p, Perm::OnDemand),
                    })
                }
                Err(AuthError::Unauthorized(m)) => Err((401, m.to_string())),
                Err(AuthError::Unavailable) => {
                    Err((503, "the accounts server could not check the key".into()))
                }
            }
        })
    }
}

/// A fixed key table (tests). Unknown keys are refused (401); no key is anonymous.
#[derive(Default)]
pub struct StaticCallers {
    keys: HashMap<String, Resolved>,
}

impl StaticCallers {
    pub fn with(mut self, key: &str, r: Resolved) -> Self {
        self.keys.insert(key.to_string(), r);
        self
    }
}

impl CallerResolver for StaticCallers {
    fn resolve<'a>(
        &'a self,
        headers: &'a HeaderMap,
        _peer: Option<IpAddr>,
        _method: &'a str,
        _path: &'a str,
    ) -> ResolveFuture<'a> {
        let r = match bearer(headers) {
            None => Ok(Resolved::default()),
            Some(k) => self
                .keys
                .get(k)
                .cloned()
                .ok_or((401, "not a valid API key".to_string())),
        };
        Box::pin(async move { r })
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;
    use crate::access::AccessSettings;

    #[tokio::test]
    async fn access_without_accounts_refuses_keys_and_never_switches() {
        let a = Access::new(AccessSettings::default(), None);
        let mut h = HeaderMap::new();
        let anon = a.resolve(&h, None, "POST", "/switch").await.unwrap();
        assert_eq!(anon.caller.principal, None);
        assert!(!anon.may_switch && !anon.may_on_demand);
        h.insert("authorization", "Bearer abk_x".parse().unwrap());
        assert_eq!(
            a.resolve(&h, None, "POST", "/switch").await.unwrap_err().0,
            401
        );
    }

    #[tokio::test]
    async fn static_table() {
        let s = StaticCallers::default().with(
            "k1",
            Resolved {
                may_switch: true,
                ..Resolved::default()
            },
        );
        let mut h = HeaderMap::new();
        assert!(!s.resolve(&h, None, "GET", "/").await.unwrap().may_switch);
        h.insert("authorization", "Bearer k1".parse().unwrap());
        assert!(s.resolve(&h, None, "GET", "/").await.unwrap().may_switch);
        h.insert("authorization", "Bearer k2".parse().unwrap());
        assert!(s.resolve(&h, None, "GET", "/").await.is_err());
    }
}
