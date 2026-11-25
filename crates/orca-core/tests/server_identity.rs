//! `ServerIdentityTests.cs` against the fake server's routes (bodies are the servers' real
//! ones). A server that is down answers 503 here (C#'s handler throws): both say nothing
//! about what the server is. The `UncensoredSwitch` halves of the two-keys case wait for
//! that command's port.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orca_core::client::server_identity::{self, smallest_context_window};
use orca_core::client::{ServerIdentity, ServerInfo, ServerKind};
use orca_core::config::{OrcaConfig, SlotCopyConfig, SlotLeadConfig, SlotsConfig};
use orca_testkit::{FakeOpenAi, ServerOptions};

const STRATA_HEALTH: &str = r#"{"status": "ok", "max_context": 262144, "model": "qwen3.8-flash-next", "images": true, "api_key": true, "image_generation": false, "loaded": true, "service": "strata"}"#;
const NINFER_HEALTH: &str = r#"{"status":"ok"}"#;
const NINFER_MODELS: &str = r#"{"data":[{"created":1795219262,"id":"qwen3.8-27b","max_model_len":131072,"object":"model","owned_by":"ninfer"}],"object":"list"}"#;
const LITELLM_MODELS: &str = r#"{"data":[{"id":"qwen3.8-27b","object":"model","created":1677610602,"owned_by":"openai","max_input_tokens":65536},{"id":"inkling-2-mini","object":"model","created":1677610602,"owned_by":"openai"},{"id":"dense-pi","object":"model","created":1677610602,"owned_by":"openai"}],"object":"list"}"#;

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

/// Strata answers /health only (its /v1/models is 404 here, as in the C# handler).
async fn strata() -> FakeOpenAi {
    FakeOpenAi::start_with(
        vec![],
        ServerOptions::default()
            .route("GET /health", 200, STRATA_HEALTH)
            .route("GET /v1/models", 404, ""),
    )
    .await
}

async fn ninfer() -> FakeOpenAi {
    FakeOpenAi::start_with(
        vec![],
        ServerOptions::default()
            .route("GET /health", 200, NINFER_HEALTH)
            .route("GET /v1/models", 200, NINFER_MODELS),
    )
    .await
}

async fn keyed_ninfer(key: &str) -> FakeOpenAi {
    FakeOpenAi::start_with(
        vec![],
        ServerOptions::default()
            .route("GET /health", 200, NINFER_HEALTH)
            .route("GET /v1/models", 200, NINFER_MODELS)
            .requiring_key("GET /v1/models", key),
    )
    .await
}

/// A port nobody listens on.
async fn closed_url() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn strata_is_named_by_its_health_with_its_window_and_images() {
    let server = strata().await;
    let info =
        server_identity::detect(&format!("{}/", server.base_url()), Some("secret"), &http()).await;
    assert_eq!(
        info,
        ServerInfo::new(ServerKind::Strata, Some(262144), Some(true))
    );
    // The list first: a gateway's /health is never asked.
    assert_eq!(server.probes(), ["GET /v1/models", "GET /health"]);
    for a in server.probe_auth() {
        assert_eq!(a.as_deref(), Some("Bearer secret"));
    }
}

#[tokio::test]
async fn a_gateway_is_other_and_its_health_is_never_asked() {
    for models in [
        LITELLM_MODELS,
        // A key limited to one model still lists LiteLLM's owner.
        r#"{"data":[{"id":"qwen3.8-27b","object":"model","created":1677610602,"owned_by":"openai"}],"object":"list"}"#,
        // Several models from any server (LM Studio): not a single-model Strata.
        r#"{"data":[{"id":"a"},{"id":"b"}]}"#,
    ] {
        let server = FakeOpenAi::start_with(
            vec![],
            ServerOptions::default()
                .route("GET /v1/models", 200, models)
                .route(
                    "GET /health",
                    200,
                    r#"{"healthy_endpoints":[],"unhealthy_endpoints":[]}"#,
                ),
        )
        .await;
        let identity = ServerIdentity::new(http());
        let info = identity.get(&server.base_url(), Some("gateway-key")).await;
        assert_eq!(info.kind, ServerKind::Other, "{models}");
        assert_eq!(server.probes(), ["GET /v1/models"]);
    }
}

#[tokio::test]
async fn ninfer_is_named_by_its_model_list_with_its_window() {
    let server = ninfer().await;
    let info = server_identity::detect(&server.base_url(), None, &http()).await;
    assert_eq!(info.kind, ServerKind::Ninfer);
    assert_eq!(info.max_context, Some(131072));
    assert_eq!(info.images, None);
    // Its bare health would add nothing.
    assert_eq!(server.probes(), ["GET /v1/models"]);
    assert_eq!(info.display_name(), "ninfer");
}

#[tokio::test]
async fn other_servers_are_other_only_when_an_answer_rules_strata_out() {
    for (health_status, health, models_status, models, expected) in [
        // llama.cpp: health ok, a model list without an owner we know.
        (
            200,
            r#"{"status":"ok"}"#,
            200,
            r#"{"object":"list","data":[{"id":"m","owned_by":"llamacpp"}]}"#,
            ServerKind::Other,
        ),
        // LM Studio: no /health at all. Strata always serves it, so 404 rules Strata out.
        (404, "", 200, r#"{"data":[{"id":"m"}]}"#, ServerKind::Other),
        (405, "", 404, "", ServerKind::Other),
        // A health page that is not JSON.
        (200, "OK", 404, "", ServerKind::Other),
        // Loading or overloaded: says nothing about what it is.
        (503, "", 503, "", ServerKind::Unknown),
        (401, "", 401, "", ServerKind::Unknown),
    ] {
        let server = FakeOpenAi::start_with(
            vec![],
            ServerOptions::default()
                .route("GET /health", health_status, health)
                .route("GET /v1/models", models_status, models),
        )
        .await;
        let info = server_identity::detect(&server.base_url(), None, &http()).await;
        assert_eq!(
            info.kind, expected,
            "{health_status} {health} {models_status} {models}"
        );
    }
}

#[tokio::test]
async fn a_server_asked_with_a_key_it_refuses_is_not_identified_and_is_asked_again_with_another_key()
 {
    let server = keyed_ninfer("slot-key").await;
    let identity = ServerIdentity::new(http());
    let url = server.base_url();

    let first = identity.get(&url, Some("gateway-key")).await;
    assert_eq!(first.kind, ServerKind::Unknown);
    assert!(first.key_rejected);
    assert_eq!(server.probes().len(), 2);

    // The same key again (or none): no new requests, still not identified.
    assert!(identity.get(&url, Some("gateway-key")).await.key_rejected);
    assert!(identity.get(&url, None).await.key_rejected);
    assert_eq!(server.probes().len(), 2);

    let second = identity.get(&format!("{url}/"), Some("slot-key")).await;
    assert_eq!(
        second,
        ServerInfo::new(ServerKind::Ninfer, Some(131072), None)
    );
    assert_eq!(server.probes().len(), 3);

    // Known from here on, whatever key the caller has.
    assert_eq!(
        identity.get(&url, Some("gateway-key")).await.kind,
        ServerKind::Ninfer
    );
    assert_eq!(identity.known(&url).unwrap().kind, ServerKind::Ninfer);
    assert_eq!(server.probes().len(), 3);
}

#[tokio::test]
async fn one_server_under_two_keys_the_window_uses_the_key_it_accepts() {
    let server = keyed_ninfer("slot-key").await;
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "gateway-key".into();
    config.context.context_window_size = 262144;
    config.slots = Some(SlotsConfig {
        enabled: true,
        api_key: Some("slot-key".into()),
        lead_copy: Some("A".into()),
        copies: vec![SlotCopyConfig {
            name: "A".into(),
            base_url: server.base_url(),
            slots: 4,
            ..Default::default()
        }],
        ..Default::default()
    });
    // Startup: the main server (gateway key, refused) and the Lead's copy (slots' key) are one
    // server.
    assert_eq!(
        smallest_context_window(&config, &ServerIdentity::new(http())).await,
        Some(131072)
    );
}

#[tokio::test]
async fn a_server_that_does_not_answer_is_unknown() {
    let info = server_identity::detect(&closed_url().await, None, &http()).await;
    assert_eq!(info, ServerInfo::UNKNOWN);
}

#[tokio::test]
async fn the_model_list_gives_the_smallest_window_and_a_strata_owner_counts() {
    assert_eq!(
        server_identity::parse_models(
            r#"{"data":[{"id":"a","max_model_len":65536},{"id":"b","max_model_len":32768},{"id":"c","max_model_len":0}]}"#
        ),
        Some(ServerInfo::new(ServerKind::Other, Some(32768), None))
    );
    assert_eq!(
        server_identity::parse_models(r#"{"data":[{"id":"a","owned_by":"Strata"}]}"#)
            .unwrap()
            .kind,
        ServerKind::Strata
    );
    assert_eq!(server_identity::parse_models(r#"{"error":"no"}"#), None);
    assert_eq!(server_identity::parse_models("not json"), None);

    // ninfer's health behind a proxy that also answers the model list.
    let server = FakeOpenAi::start_with(
        vec![],
        ServerOptions::default()
            .route("GET /health", 200, r#"{"status":"ok","max_context":4096}"#)
            .route("GET /v1/models", 200, r#"{"data":[]}"#),
    )
    .await;
    let info = server_identity::detect(&server.base_url(), None, &http()).await;
    assert_eq!(info, ServerInfo::new(ServerKind::Other, Some(4096), None));
}

#[tokio::test]
async fn an_answer_is_asked_once_per_server_whatever_the_url_spelling() {
    let server = ninfer().await;
    let identity = ServerIdentity::new(http());
    let url = server.base_url();
    let root = url.trim_end_matches("/v1").to_string();

    assert_eq!(identity.known(&url), None);
    assert_eq!(identity.get(&url, None).await.kind, ServerKind::Ninfer);
    assert_eq!(
        identity
            .get(&format!("{}/", url.replace("http", "HTTP")), None)
            .await
            .kind,
        ServerKind::Ninfer
    );
    assert_eq!(identity.get(&root, None).await.kind, ServerKind::Ninfer);
    assert_eq!(identity.known(&url).unwrap().kind, ServerKind::Ninfer);
    // One /v1/models in all (it names ninfer: no /health).
    assert_eq!(server.probes().len(), 1);
}

#[tokio::test]
async fn no_answer_is_kept_for_a_minute_then_asked_again() {
    let server = strata().await;
    server.set_down(true);
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = {
        let now = now.clone();
        Arc::new(move || *now.lock().unwrap())
    };
    let identity = ServerIdentity::with_clock(http(), clock);
    let url = server.base_url();

    assert_eq!(identity.get(&url, None).await.kind, ServerKind::Unknown);
    let asked = server.probes().len();

    // The server is back, but within the minute the answer stands (no request per turn).
    server.set_down(false);
    *now.lock().unwrap() += Duration::from_secs(30);
    assert_eq!(identity.get(&url, None).await.kind, ServerKind::Unknown);
    assert_eq!(server.probes().len(), asked);

    *now.lock().unwrap() += Duration::from_secs(31);
    assert_eq!(identity.get(&url, None).await.kind, ServerKind::Strata);

    // A real answer is kept for the process.
    server.set_down(true);
    *now.lock().unwrap() += Duration::from_secs(5 * 3600);
    assert_eq!(identity.get(&url, None).await.kind, ServerKind::Strata);
}

#[tokio::test]
async fn warm_asks_in_the_background() {
    // (C#'s cancelled wait has no Rust counterpart: dropping the future is the cancel, and the
    // background probe is a separate task.)
    let server = strata().await;
    let identity = ServerIdentity::new(http());
    identity.warm(&server.base_url(), None);
    assert_eq!(
        identity.get(&server.base_url(), None).await.kind,
        ServerKind::Strata
    );
    assert_eq!(server.probes().len(), 2);
}

// ---- the context window follows the server, downwards only -----------------------------

fn config(main: &str, copies: &[(&str, &str)]) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = main.to_string();
    if !copies.is_empty() {
        config.slots = Some(SlotsConfig {
            enabled: true,
            copies: copies
                .iter()
                .map(|(name, url)| SlotCopyConfig {
                    name: name.to_string(),
                    base_url: url.to_string(),
                    slots: 1,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
    }
    config
}

#[tokio::test]
async fn the_served_window_is_ninfers_131072_for_a_config_written_for_flash() {
    let server = ninfer().await;
    let identity = ServerIdentity::new(http());
    assert_eq!(
        smallest_context_window(&config(&server.base_url(), &[]), &identity).await,
        Some(131072)
    );
}

#[tokio::test]
async fn the_served_window_is_the_smaller_of_the_main_server_and_the_leads_copy() {
    let (flash, flash2, dense) = (strata().await, strata().await, ninfer().await);
    let other = FakeOpenAi::start_with(
        vec![],
        ServerOptions::default().route("GET /v1/models", 404, ""),
    )
    .await;
    let identity = ServerIdentity::new(http());

    // The Lead runs on its copy (the first one unless slots.leadCopy names another).
    let mut c = config(
        &flash.base_url(),
        &[("A", &dense.base_url()), ("B", &flash2.base_url())],
    );
    assert_eq!(smallest_context_window(&c, &identity).await, Some(131072));
    c.slots.as_mut().unwrap().lead_copy = Some("B".into());
    assert_eq!(smallest_context_window(&c, &identity).await, Some(262144));

    // A server that says nothing leaves the configured window alone.
    assert_eq!(
        smallest_context_window(&config(&other.base_url(), &[]), &identity).await,
        None
    );

    // slots.lead with its own contextWindow wins (the config applies it): nothing is asked.
    c.slots.as_mut().unwrap().lead = Some(SlotLeadConfig {
        name: "L".into(),
        base_url: "http://dense:9000/v1".into(),
        context_window: Some(65536),
        ..Default::default()
    });
    assert_eq!(smallest_context_window(&c, &identity).await, None);
}

#[tokio::test]
async fn anthropic_is_not_asked() {
    let server = strata().await;
    let mut c = config(&server.base_url(), &[]);
    c.lm_studio.provider = Some("anthropic".into());
    assert_eq!(
        smallest_context_window(&c, &ServerIdentity::new(http())).await,
        None
    );
    assert!(server.probes().is_empty());
}
