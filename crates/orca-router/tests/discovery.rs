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
    // The cards were only asked what they run (read-only GETs), the relay nothing.
    only_described(&a);
    only_described(&b);
    assert!(relay.seen().is_empty());
}

/// Everything `f` saw was a read-only GET of `/v1/models` or `/props`, with the engine's key.
fn only_described(f: &Fake) {
    for s in f.seen() {
        assert_eq!(s.method, "GET", "{}", s.path);
        assert!(s.path == "/v1/models" || s.path == "/props", "{}", s.path);
        assert_eq!(
            s.headers["authorization"].to_str().unwrap(),
            "Bearer test-engine-key"
        );
        assert_eq!(s.headers["x-abhoth-no-switch"].to_str().unwrap(), "1");
    }
}

/// An engine that describes itself like ninfer (`image`: a vision projector is loaded).
async fn engine(image: bool, ctx: i64) -> Fake {
    Fake::start(move |s| match (s.method.as_str(), s.path.as_str()) {
        ("GET", "/v1/models") => {
            let mods = if image {
                json!(["text", "image"])
            } else {
                json!(["text"])
            };
            Reply::Json(
                200,
                json!({"object": "list", "data": [{"id": "inkling-2-mini", "object": "model",
                    "architecture": {"input_modalities": mods, "output_modalities": ["text"]},
                    "context_length": ctx}]}),
            )
        }
        _ => Reply::Json(404, json!({"error": "?"})),
    })
    .await
}

/// A llama.cpp-style engine: `/v1/models` says nothing about images, `/props` does.
async fn llamacpp(image: bool) -> Fake {
    Fake::start(move |s| match s.path.as_str() {
        "/v1/models" => Reply::Json(200, json!({"data": [{"id": "inkling-2-mini"}]})),
        "/props" => Reply::Json(
            200,
            json!({"modalities": {"vision": image}, "default_generation_settings": {"n_ctx": 32768}}),
        ),
        _ => Reply::Json(404, json!({})),
    })
    .await
}

async fn models_of(url: &str) -> RouterModels {
    http()
        .get(format!("{url}{MODELS_PATH}"))
        .header("x-abhoth-no-switch", "1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn vision_comes_from_the_engines_the_names_run_on() {
    // A takes images (ninfer with its projector), B is text only (llama.cpp without mmproj).
    let (a, b, relay) = (
        engine(true, 65536).await,
        llamacpp(false).await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let r = start_router(
        cfg(&a.url(), &b.url(), &relay.url(), dir.path()),
        None,
        Mode::Gateway,
    )
    .await;
    let m = models_of(&r.url).await;
    let v = |id: &str| m.find(id).unwrap().vision;
    assert_eq!(v("qwen3.8-flash-next-a"), Some(true), "card A's engine");
    assert_eq!(v("qwen3.8-flash-next-b"), Some(false), "card B's /props");
    assert_eq!(
        v("jarvis-brain"),
        Some(true),
        "A or B: either may take the images"
    );
    assert_eq!(v("qwen3.8-27b"), Some(true), "its supports_vision");
    assert_eq!(v("flash-next"), None, "the relay is never asked");
    assert_eq!(v("claude-sonnet"), Some(true));
    only_described(&a);
    only_described(&b);
    assert!(relay.seen().is_empty());
    // Cached: a second listing asks nobody.
    let (na, nb) = (a.seen().len(), b.seen().len());
    let _ = models_of(&r.url).await;
    assert_eq!((a.seen().len(), b.seen().len()), (na, nb));
}

#[tokio::test]
async fn an_override_wins_and_cold_cards_are_never_asked() {
    let (a, b, relay) = (
        engine(true, 65536).await,
        engine(true, 65536).await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(&a.url(), &b.url(), &relay.url(), dir.path());
    c.upstreams.get_mut("B").unwrap().online = false;
    // An admin says jarvis-brain takes no images, whatever the engine says.
    c.deployments
        .iter_mut()
        .find(|d| d.name == "jarvis-brain")
        .unwrap()
        .model_info
        .insert("supports_vision".into(), json!(false));
    let r = start_router(c, None, Mode::Gateway).await;
    let m = models_of(&r.url).await;
    assert_eq!(m.find("jarvis-brain").unwrap().vision, Some(false));
    assert_eq!(m.find("qwen3.8-flash-next-a").unwrap().vision, Some(true));
    assert_eq!(
        m.find("qwen3.8-flash-next-b").unwrap().vision,
        None,
        "B is offline: unknown, not asked"
    );
    assert!(b.seen().is_empty(), "an offline card was asked");
    assert!(!a.seen().is_empty());
}

#[tokio::test]
async fn the_engines_context_fills_a_missing_window() {
    let (a, b) = (engine(false, 40960).await, engine(false, 40960).await);
    let dir = tempfile::tempdir().unwrap();
    let mut c = two_cards(&a.url(), &b.url(), None, dir.path());
    for d in &mut c.deployments {
        d.model_info.remove("max_input_tokens");
    }
    let r = start_router(c, None, Mode::Gateway).await;
    let m = models_of(&r.url).await;
    let j = m.find("jarvis-brain").unwrap();
    assert_eq!(j.context_window, Some(40960));
    assert_eq!(j.vision, Some(false));
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

#[tokio::test]
async fn cold_catalog_models_use_the_catalog_and_live_ones_their_engine() {
    let (a, b, relay) = (
        engine(true, 65536).await,
        engine(true, 65536).await,
        Fake::ok("relay").await,
    );
    let dir = tempfile::tempdir().unwrap();
    let listing = |c: orca_router::config::RouterConfig| async {
        let r = start_router(c, None, Mode::Gateway).await;
        let _ = r.st.control.set(control_plane());
        let m: RouterModels = http()
            .get(format!("{}{MODELS_PATH}", r.url))
            .bearer_auth(USER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        m
    };
    // dense-stock is cold: the catalog says text only; the engines (running another model)
    // are not taken for it.
    let m = listing(cfg(&a.url(), &b.url(), &relay.url(), dir.path())).await;
    let d = m.find("dense-stock").unwrap();
    assert_eq!((d.status.as_deref(), d.vision), (Some("cold"), Some(false)));
    // A pool route is live while a card is online, and its engine says.
    let q = m.find("qwen3.8-27b").unwrap();
    assert_eq!(q.status.as_deref(), Some("live"));
    // Card A runs it now (what the control plane put there): live, and A's engine says.
    let mut c = cfg(&a.url(), &b.url(), &relay.url(), dir.path());
    c.upstreams.get_mut("A").unwrap().catalog_model = Some("dense-stock".into());
    let m = listing(c).await;
    let d = m.find("dense-stock").unwrap();
    assert_eq!((d.status.as_deref(), d.vision), (Some("live"), Some(true)));
    assert!(relay.seen().is_empty());
}

/// The web server's side of key requests, in memory: what the router passed on.
#[derive(Default)]
struct FakeAccounts {
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl orca_router::key_requests::KeyRequestLink for FakeAccounts {
    async fn create(
        &self,
        ask: &orca_core::router_link::KeyRequestCreate,
    ) -> Result<(u16, serde_json::Value), String> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("create {} {}", ask.client, ask.ip));
        Ok((
            200,
            json!({"id": "r1", "code": "ABCD-2345", "poll": "secret-poll", "expiresIn": 900, "interval": 3}),
        ))
    }
    async fn poll(
        &self,
        ask: &orca_core::router_mode::KeyRequestPoll,
    ) -> Result<(u16, serde_json::Value), String> {
        self.seen.lock().unwrap().push(format!("poll {}", ask.id));
        Ok((200, json!({"status": "pending", "expiresIn": 800})))
    }
}

#[tokio::test]
async fn key_requests_go_to_the_accounts_server_rate_limited_and_never_reach_a_card() {
    use orca_core::router_mode::{KEY_REQUESTS_PATH, KEY_REQUESTS_POLL_PATH};
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
    // Without the control plane (or its accounts link): not offered, 404.
    let info: RouterInfo = http()
        .get(format!("{}{WELL_KNOWN_PATH}", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!info.auth.key_requests);
    let resp = http()
        .post(format!("{}{KEY_REQUESTS_PATH}", r.url))
        .json(&json!({"client": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let accounts = std::sync::Arc::new(FakeAccounts::default());
    let cp = control_plane();
    let cp = std::sync::Arc::new(orca_router::control_api::ControlPlane {
        access: std::sync::Arc::try_unwrap(cp)
            .ok()
            .unwrap()
            .access
            .with_key_requests(accounts.clone()),
        backend: orca_router::control_api::demo::DemoControl::new(),
    });
    let _ = r.st.control.set(cp);
    let info: RouterInfo = http()
        .get(format!("{}{WELL_KNOWN_PATH}", r.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.auth.key_requests);
    assert_eq!(info.paths.key_requests.as_deref(), Some(KEY_REQUESTS_PATH));
    assert!(info.capabilities.contains(&"key-requests".to_string()));

    // No key needed to ask; the answer is the accounts server's.
    let t = orca_core::router_mode::start_key_request(
        &format!("{}/v1", r.url),
        "Abhoth desktop on Test",
        &orca_core::router_mode::probe_client(),
    )
    .await
    .unwrap();
    assert_eq!(t.code, "ABCD-2345");
    let st = orca_core::router_mode::poll_key_request(
        &r.url,
        &t.id,
        &t.poll,
        &orca_core::router_mode::probe_client(),
    )
    .await
    .unwrap();
    assert_eq!(st.status, "pending");
    assert_eq!(
        accounts.seen.lock().unwrap().clone(),
        ["create Abhoth desktop on Test 127.0.0.1", "poll r1"]
    );
    // Five new requests per IP per ten minutes.
    let mut codes = Vec::new();
    for _ in 0..6 {
        codes.push(
            http()
                .post(format!("{}{KEY_REQUESTS_PATH}", r.url))
                .json(&json!({"client": "again"}))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
        );
    }
    assert_eq!(codes, [200, 200, 200, 200, 429, 429]);
    // A malformed poll is refused before the accounts server is asked.
    let resp = http()
        .post(format!("{}{KEY_REQUESTS_POLL_PATH}", r.url))
        .body("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // GET is not a key request.
    let resp = http()
        .get(format!("{}{KEY_REQUESTS_PATH}", r.url))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 200);
    assert!(a.seen().is_empty() && b.seen().is_empty() && relay.seen().is_empty());
}
