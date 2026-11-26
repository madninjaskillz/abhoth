use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{HeaderMap, HeaderValue};
use orca_core::router_link::{Introspection, key_digest, sign_acting_user};

use super::keys::{CacheTimes, Introspector};
use super::*;

struct Table(Mutex<HashMap<String, Introspection>>);

#[async_trait::async_trait]
impl Introspector for Table {
    async fn introspect(&self, d: &str) -> Result<Introspection, String> {
        Ok(self.0.lock().unwrap().get(d).cloned().unwrap_or_default())
    }
}

const ADMIN_KEY: &str = "abk_admin_aaaaaaaaaaaaaaaaaaaa";
const USER_KEY: &str = "abk_user_bbbbbbbbbbbbbbbbbbbbbb";
const JARVIS_KEY: &str = "abk_svc_cccccccccccccccccccccc";
const DASH_KEY: &str = "abk_svc_dddddddddddddddddddddd";

fn intro(uid: i64, name: &str, role: Role, kid: i64, kname: &str, scopes: &[&str]) -> Introspection {
    Introspection {
        active: true,
        user_id: Some(uid),
        name: Some(name.into()),
        role: Some(role),
        key_id: Some(kid),
        key_name: Some(kname.into()),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        disabled: false,
    }
}

fn access() -> Access {
    let mut t = HashMap::new();
    t.insert(key_digest(ADMIN_KEY), intro(1, "admin", Role::Admin, 1, "cli", &[]));
    t.insert(key_digest(USER_KEY), intro(2, "bob", Role::User, 2, "laptop", &[]));
    t.insert(
        key_digest(JARVIS_KEY),
        intro(1, "admin", Role::Service, 3, "jarvis", &["chat", "switch", "group:switchers"]),
    );
    t.insert(key_digest(DASH_KEY), intro(1, "admin", Role::Service, 4, "dash", &["read"]));
    let cache = KeyCache::new(
        std::sync::Arc::new(Table(Mutex::new(t))),
        CacheTimes::default(),
        std::sync::Arc::new(|| 0),
    );
    Access::with_parts(
        AccessSettings {
            accounts_url: Some("http://web".into()),
            accounts_token: Some("secret:accounts_token".into()),
            anonymous_lan: vec!["10.1.0.0/16".into()],
            ..Default::default()
        },
        Some(b"tok\n".to_vec()),
        Some(cache),
        std::sync::Arc::new(|| 1_000),
    )
}

fn with_key(k: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_str(&format!("Bearer {k}")).unwrap());
    h
}

async fn who(a: &Access, h: &HeaderMap, ip: &str) -> Result<Principal, AuthError> {
    a.authenticate(h, Some(ip.parse().unwrap()), "GET", "/admin/api/state").await
}

#[tokio::test]
async fn principals_from_keys_peers_and_signatures() {
    let a = access();
    let lan = "10.1.2.3";
    assert!(matches!(who(&a, &with_key(ADMIN_KEY), lan).await, Ok(Principal::Key(i)) if i.role == Role::Admin));
    assert_eq!(
        who(&a, &with_key("abk_nope_eeeeeeeeeeeeeeeeeeeeee"), lan).await,
        Err(AuthError::Unauthorized("not a valid API key"))
    );
    assert_eq!(who(&a, &HeaderMap::new(), lan).await, Ok(Principal::Anonymous { lan: true }));
    assert_eq!(who(&a, &HeaderMap::new(), "192.0.2.9").await, Ok(Principal::Anonymous { lan: false }));
    assert_eq!(who(&a, &HeaderMap::new(), "127.0.0.1").await, Ok(Principal::Loopback));
    // a forwarded-for header does not make a caller local
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
    assert_eq!(who(&a, &h, "192.0.2.9").await, Ok(Principal::Anonymous { lan: false }));

    // the web server's signed acting user (token compared trimmed, as read from a file)
    let u = ActingUser { user_id: 1, name: "admin".into(), admin: true };
    let mut h = HeaderMap::new();
    h.insert(ACTING_USER_HEADER, HeaderValue::from_str(&sign_acting_user(b"tok", &u, 990, "GET", "/admin/api/state")).unwrap());
    assert_eq!(who(&a, &h, "192.0.2.9").await, Ok(Principal::Acting(u.clone())));
    let mut h = HeaderMap::new();
    h.insert(ACTING_USER_HEADER, HeaderValue::from_str(&sign_acting_user(b"tok", &u, 990, "GET", "/admin/api/config")).unwrap());
    assert!(matches!(who(&a, &h, "192.0.2.9").await, Err(AuthError::Unauthorized(_))));
}

#[tokio::test]
async fn the_permission_table() {
    use Perm::*;
    let a = access();
    let p = |k: &str| {
        let a = &a;
        let k = k.to_string();
        async move { who(a, &with_key(&k), "192.0.2.9").await.unwrap() }
    };
    let admin = p(ADMIN_KEY).await;
    let user = p(USER_KEY).await;
    let jarvis = p(JARVIS_KEY).await;
    let dash = p(DASH_KEY).await;
    let lan = Principal::Anonymous { lan: true };
    let wan = Principal::Anonymous { lan: false };
    let all = [Models, Chat, OnDemand, Leases, ModelInfo, ReadControl, EditControl, Switch];
    assert!(all.iter().all(|&x| a.allows(&admin, x)));
    let row = |p: &Principal| all.iter().map(|&x| a.allows(p, x)).collect::<Vec<_>>();
    //                          Models Chat  OnDem  Lease  MInfo  Read   Edit   Switch
    assert_eq!(row(&user), [true, true, false, true, false, false, false, false]);
    assert_eq!(row(&jarvis), [true, true, true, false, false, false, false, true]);
    assert_eq!(row(&dash), [false, false, false, false, true, true, false, false]);
    assert_eq!(row(&lan), [true, true, false, true, true, false, false, false]);
    assert_eq!(row(&wan), [false; 8]);
    // anonymous callers never switch, whatever the network
    assert!(!a.allows(&lan, Switch) && !a.allows(&lan, OnDemand));
}

#[tokio::test]
async fn route_access_rules() {
    let a = access();
    let user = who(&a, &with_key(USER_KEY), "192.0.2.9").await.unwrap();
    let jarvis = who(&a, &with_key(JARVIS_KEY), "192.0.2.9").await.unwrap();
    let dash = who(&a, &with_key(DASH_KEY), "192.0.2.9").await.unwrap();
    let lan = Principal::Anonymous { lan: true };
    assert!(user.may_use_route("users") && !user.may_use_route("admins"));
    assert!(!user.may_use_route("keys:jarvis"));
    assert!(jarvis.may_use_route("keys:jarvis") && jarvis.may_use_route("keys:switchers"));
    assert!(jarvis.may_use_route("keys:ha, switchers"));
    assert!(!dash.may_use_route("users"), "a read-only key has no chat");
    assert!(lan.may_use_route("users") && !lan.may_use_route("keys:switchers"));
    assert!(Principal::Loopback.may_use_route("admins"));
}

#[test]
fn cidrs_and_settings() {
    let c = Cidr::parse("192.168.0.0/16").unwrap();
    assert!(c.contains("192.168.4.5".parse().unwrap()));
    assert!(!c.contains("192.169.0.1".parse().unwrap()));
    assert!(c.contains("::ffff:192.168.1.1".parse().unwrap()), "v4-mapped v6");
    assert!(Cidr::parse("fd00::/8").unwrap().contains("fd12::1".parse().unwrap()));
    assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
    assert!(Cidr::parse("10.0.0.1").unwrap().contains("10.0.0.1".parse().unwrap()));
    assert!(Cidr::parse("10.0.0.0/33").is_none());
    let bad = AccessSettings {
        accounts_token: Some("abk_inline".into()),
        anonymous_lan: vec!["nope".into()],
        ..Default::default()
    };
    let p = bad.problems();
    assert_eq!(p.len(), 3, "{p:?}");
    assert!(AccessSettings::default().problems().is_empty());
}

#[test]
fn labels_never_carry_keys() {
    let i = Identity {
        user_id: 1,
        name: "admin".into(),
        role: Role::Service,
        key_id: 3,
        key_name: "jarvis".into(),
        scopes: vec![],
    };
    assert_eq!(Principal::Key(i).label(), "service:jarvis#3");
}
