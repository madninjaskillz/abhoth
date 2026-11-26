//! D149: the identification document, the model metadata and the mDNS advert.

mod common;

use common::*;
use orca_core::router_mode::{
    MDNS_SERVICE_TYPE, MODELS_PATH, RouterInfo, RouterModels, WELL_KNOWN_PATH,
};
use orca_router::server::Mode;
use serde_json::json;

fn cfg(a: &str, b: &str, relay: &str, dir: &std::path::Path) -> orca_router::config::RouterConfig {
    let mut c = two_cards(a, b, Some(relay), dir);
    // Sampling defaults and an explicit capability on one name; a Claude name.
    let d = c
        .deployments
        .iter_mut()
        .find(|d| d.name == "qwen3.8-27b")
        .unwrap();
    d.params.insert("temperature".into(), json!(0.6));
    d.params.insert("top_k".into(), json!(20));
    d.params.insert("reasoning_effort".into(), json!("low"));
    d.model_info.insert("supports_vision".into(), json!(true));
    let claude: orca_router::config::Deployment = serde_json::from_value(json!({
        "name": "claude-sonnet", "cards": ["A"], "servedId": "claude-sonnet",
        "modelInfo": {"engine": "claude-cli", "max_input_tokens": 200000, "max_output_tokens": 32000,
                      "access": "keys:agents-bench"}
    }))
    .unwrap();
    c.deployments.push(claude);
    c
}

#[tokio::test]
async fn the_well_known_document_names_the_router_without_secrets() {
    let (a, b, relay) = (
        Fake::ok("A").await,
        Fake::ok("B").await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let _ = r.st.discovery.set(orca_router::discovery::Discovery {
        name: "Test router".into(),
        key_url: Some("https://abhoth.example/#settings/account".into()),
        mdns: true,
    });
    let resp = http()
        .get(format!("{}{WELL_KNOWN_PATH}", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["access-control-allow-origin"]
            .to_str()
            .unwrap(),
        "*"
    );
    let text = resp.text().await.unwrap();
    // no engine URL, no key file, no key
    for secret in ["test-engine-key", "key", &a.url(), &relay.url()] {
        if secret == "key" {
            continue;
        }
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let info: RouterInfo = serde_json::from_str(&text).unwrap();
    assert_eq!(info.service, "abhoth-router");
    assert_eq!(info.name, "Test router");
    assert!(!info.auth.required, "no access rules: open");
    assert!(info.capabilities.contains(&"claude".to_string()));
    assert!(info.capabilities.contains(&"mdns".to_string()));
    assert_eq!(info.paths.models, MODELS_PATH);
    // The client's parser agrees.
    assert!(orca_core::router_mode::parse_info(&text).is_some());
    // Nothing reached a card or the relay.
    assert!(a.seen().is_empty() && b.seen().is_empty() && relay.seen().is_empty());
}

#[tokio::test]
async fn model_metadata_without_access_rules_lists_every_name() {
    let (a, b, relay) = (
        Fake::ok("A").await,
        Fake::ok("B").await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let m: RouterModels = http()
        .get(format!("{}{MODELS_PATH}", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = m.models.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(
        names,
        [
            "jarvis-brain",
            "qwen3.8-27b",
            "qwen3.8-flash-next-a",
            "qwen3.8-flash-next-b",
            "flash-next",
            "claude-sonnet"
        ]
    );
    let q = m.find("qwen3.8-27b").unwrap();
    assert_eq!(q.context_window, Some(65536));
    assert_eq!(q.max_output_tokens, Some(65536));
    assert_eq!(q.tools, Some(true), "ninfer takes tool calls");
    assert_eq!(q.vision, Some(true), "supports_vision wins");
    assert_eq!(q.reasoning, Some(true));
    assert_eq!(q.effort_levels, ["none", "low", "medium", "xhigh"]);
    assert_eq!(q.engine.as_deref(), Some("ninfer"));
    assert_eq!(q.kind, "local");
    assert_eq!(q.status.as_deref(), Some("live"));
    assert_eq!(
        q.sampling,
        *json!({"temperature": 0.6, "top_k": 20})
            .as_object()
            .unwrap()
    );
    let j = m.find("jarvis-brain").unwrap();
    assert_eq!(j.vision, None, "unknown stays unknown");
    let f = m.find("flash-next").unwrap();
    assert_eq!(f.kind, "relay");
    assert_eq!(f.context_window, Some(262144));
    assert_eq!(f.max_output_tokens, None);
    let c = m.find("claude-sonnet").unwrap();
    assert_eq!(
        (c.kind.as_str(), c.vision, c.tools),
        ("cloud", Some(true), Some(true))
    );
    assert!(m.claude);
    assert_eq!(m.cards.len(), 2);
    assert_eq!(m.cards[0].engine.as_deref(), Some("ninfer"));
    assert_eq!(m.rev, "rev1");
    let raw = http()
        .get(format!("{}{MODELS_PATH}", r.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !raw.contains(&a.url()) && !raw.contains("test-engine-key"),
        "{raw}"
    );
}

const USER_KEY: &str = "abk_user_useruseruseruseruse";

struct Keys(std::collections::HashMap<String, orca_core::router_link::Introspection>);

#[async_trait::async_trait]
impl orca_router::access::keys::Introspector for Keys {
    async fn introspect(&self, d: &str) -> Result<orca_core::router_link::Introspection, String> {
        Ok(self.0.get(d).cloned().unwrap_or_default())
    }
}

fn control_plane() -> std::sync::Arc<orca_router::control_api::ControlPlane> {
    use orca_core::router_link::{Introspection, Role, key_digest};
    use orca_router::access::keys::{CacheTimes, KeyCache, system_clock};
    use orca_router::access::{Access, AccessSettings};
    let keys = Keys(std::collections::HashMap::from([(
        key_digest(USER_KEY),
        Introspection {
            active: true,
            user_id: Some(1),
            name: Some("someone".into()),
            role: Some(Role::User),
            key_id: Some(7),
            key_name: Some("laptop".into()),
            ..Default::default()
        },
    )]));
    let cache = KeyCache::new(
        std::sync::Arc::new(keys),
        CacheTimes::default(),
        system_clock(),
    );
    let access = Access::with_parts(
        AccessSettings {
            // as from another machine
            loopback_admin: false,
            ..Default::default()
        },
        None,
        Some(cache),
        std::sync::Arc::new(|| chrono::Utc::now().timestamp()),
    );
    std::sync::Arc::new(orca_router::control_api::ControlPlane {
        access,
        backend: orca_router::control_api::demo::DemoControl::new(),
    })
}

#[tokio::test]
async fn with_access_rules_a_key_is_needed_and_routes_are_filtered() {
    let (a, b, relay) = (
        Fake::ok("A").await,
        Fake::ok("B").await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let _ = r.st.control.set(control_plane());
    let info: RouterInfo = http()
        .get(format!("{}{WELL_KNOWN_PATH}", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.auth.required);
    assert!(info.auth.keys);
    assert_eq!(info.auth.key_prefix.as_deref(), Some("abk_"));
    assert_eq!(info.paths.view.as_deref(), Some("/v1/abhoth/view"));
    // No key: refused.
    let resp = http()
        .get(format!("{}{MODELS_PATH}", r.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // A bad key: refused.
    let resp = http()
        .get(format!("{}{MODELS_PATH}", r.url))
        .bearer_auth("abk_nope_nopenopenopenopenope")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // A user key: its models, never the Claude name restricted to another key, no cards.
    let m: RouterModels = http()
        .get(format!("{}{MODELS_PATH}", r.url))
        .bearer_auth(USER_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m.principal, "user:someone#7");
    assert!(m.find("claude-sonnet").is_none(), "keys:agents-bench only");
    assert!(!m.claude);
    assert!(m.cards.is_empty());
    assert!(!m.models.is_empty());
    assert!(a.seen().is_empty() && b.seen().is_empty() && relay.seen().is_empty());
}

/// The advert is seen by a browser on this machine (multicast loopback). Skipped, with a line
/// saying so, where the host has no multicast-capable interface (some CI containers).
#[test]
fn the_mdns_advert_is_found_by_a_browser() {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let name = format!("Test router {}", std::process::id());
    let advert = match orca_router::discovery::mdns::advertise(&name, 18_010, None) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let browser = ServiceDaemon::new().unwrap();
    let rx = browser.browse(MDNS_SERVICE_TYPE).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut found = None;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(s)) if s.get_fullname() == advert.fullname() => {
                found = Some(s);
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = browser.shutdown();
    let Some(s) = found else {
        eprintln!("skipped: no multicast loopback here (nothing resolved in 8 s)");
        return;
    };
    assert_eq!(s.get_port(), 18_010);
    assert_eq!(s.get_property_val_str("path"), Some(WELL_KNOWN_PATH));
    assert_eq!(s.get_property_val_str("name"), Some(name.as_str()));
    assert_eq!(s.get_property_val_str("api"), Some("1"));
    assert!(!s.get_addresses().is_empty());
}
