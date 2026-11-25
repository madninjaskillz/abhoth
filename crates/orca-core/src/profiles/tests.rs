//! Ported from tests/OpenOrca.Core.Tests/Profiles/ProfileTests.cs (ProfileOverlayTests,
//! ProfileServiceTests, LiveCardOptionsTests; ConfigOverridesTests are in config_overrides.rs).

use std::time::Duration;

use serde_json::{Value, json};

use super::config_overrides as co;
use super::*;
use crate::config::{ConfigManager, ProfilesConfig, SlotsConfig};

const USER_CONFIG: &str = r#"
{
  "lmStudio": { "baseUrl": "http://box:8010/v1", "apiKey": "secret-1", "models": ["m"] },
  "agent": { "maxConcurrent": 4 },
  "slots": {
    "registryUrl": "http://box:8099", "model": "flash", "apiKeyEnv": "MAYA_API_KEY",
    "copies": [ { "name": "A", "baseUrl": "http://box:8080/v1", "slots": 3 } ]
  }
}
"#;

fn dense() -> ProfileDefinition {
    let mut p = ProfileDefinition {
        layout: Some("dense2".into()),
        ..ProfileDefinition::default()
    };
    p.client.insert("agent.peerMode".into(), json!(true));
    p.client.insert("agent.maxConcurrent".into(), json!(3));
    p.client.insert("slots.model".into(), json!("qwen3.8-27b"));
    p.client.insert(
        "slots.copies".into(),
        json!([{"name":"A","baseUrl":"http://box:8094/v1","slots":2},{"name":"B","baseUrl":"http://box:8096/v1","slots":2}]),
    );
    // A section the file doesn't have at all.
    p.client.insert("planner.effort".into(), json!("x"));
    p
}

fn loaded(text: &str) -> ConfigManager {
    let mut m = ConfigManager::new(Some(std::path::Path::new("unused")));
    m.load_from_text(text).unwrap();
    m
}

#[test]
fn apply_profile_overrides_the_loaded_config() {
    let mut manager = loaded(USER_CONFIG);
    manager.config.sandbox_mode = true;
    manager.apply_profile("dense2", &dense()).unwrap();
    let c = &manager.config;
    assert_eq!(c.active_profile.as_deref(), Some("dense2"));
    assert!(c.agent.peer_mode);
    assert_eq!(c.agent.max_concurrent, 3);
    let slots = c.slots.as_ref().unwrap();
    assert_eq!(slots.model.as_deref(), Some("qwen3.8-27b"));
    assert_eq!(
        slots
            .copies
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B"]
    );
    assert_eq!(slots.api_key_env.as_deref(), Some("MAYA_API_KEY"));
    assert!(c.sandbox_mode);
    assert_eq!(c.providers.len(), 1);
    assert!(c.providers.contains_key("lmstudio"));
}

#[test]
fn save_after_profile_keeps_the_profile_out_of_config_json() {
    let mut manager = loaded(USER_CONFIG);
    manager.apply_profile("dense2", &dense()).unwrap();
    manager.config.profiles.active = Some("dense2".into());
    manager.config.session.max_sessions = 7;
    let saved: Value = serde_json::from_str(&manager.serialize_for_save().unwrap()).unwrap();
    assert_eq!(saved["agent"]["maxConcurrent"], 4);
    assert!(!co::has(&saved, "agent.peerMode"));
    assert_eq!(saved["slots"]["model"], "flash");
    assert_eq!(saved["slots"]["copies"].as_array().unwrap().len(), 1);
    assert!(!co::has(&saved, "planner"));
    assert_eq!(saved["profiles"]["active"], "dense2");
    assert_eq!(saved["session"]["maxSessions"], 7);
}

#[test]
fn mode_over_the_profile_wins_and_stays_out_of_config_json() {
    let mut manager = loaded(USER_CONFIG);
    manager.apply_profile("dense2", &dense()).unwrap();
    manager.apply_mode("orchestrator").unwrap();
    assert!(!manager.config.agent.peer_mode);
    assert_eq!(manager.config.agent.lead_mode, "orchestrator");
    assert_eq!(manager.config.active_profile.as_deref(), Some("dense2"));
    assert_eq!(manager.config.agent.max_concurrent, 3);

    let saved: Value = serde_json::from_str(&manager.serialize_for_save().unwrap()).unwrap();
    assert!(!co::has(&saved, "agent.peerMode"));
    assert_ne!(saved["agent"]["leadMode"], "orchestrator");

    manager.apply_mode("peer").unwrap();
    assert!(manager.config.agent.peer_mode);
    assert_eq!(
        manager.apply_mode("sideways").unwrap_err(),
        "Unknown mode 'sideways' (one of auto, hands-on, orchestrator, peer)."
    );
}

#[test]
fn agent_modes_default_bridge_and_result_mode() {
    use agent_modes::*;
    assert_eq!(default_of(Some(&dense())), "peer");
    let mut own = ProfileDefinition {
        mode: Some("hands-on".into()),
        ..Default::default()
    };
    own.client.insert("agent.peerMode".into(), json!(true));
    assert_eq!(default_of(Some(&own)), "hands-on");
    assert_eq!(default_of(Some(&ProfileDefinition::default())), "auto");
    let mut bridge = ProfileDefinition::default();
    bridge.client.insert("agent.engine".into(), json!("claude"));
    assert!(is_bridge(Some(&bridge)));
    assert!(!is_bridge(Some(&dense())));
    assert_eq!(of_result(Some("auto"), &["agent.peerMode=true"]), "peer");
    assert_eq!(
        of_result(Some("orchestrator"), &["agent.peerMode=false"]),
        "orchestrator"
    );
    assert_eq!(normalize(Some(" Hands-On ")), Some("hands-on"));
    assert_eq!(normalize(Some("x")), None);
}

#[test]
fn catalog_resolves_old_names_through_aliases_and_reads_the_v3_sections() {
    let c = ProfileCatalog::parse(
        r#"{"profiles":{"dense-abl-x2":{"layout":"dense-abl-x2","mode":"auto","cards":{"A":{"engine":"ninfer","engineProfile":"g","model":"m"},"B":"same"},"client":{}}},
         "aliases":{"dense4-abl":"dense-abl-x2","older":"dense4-abl"},
         "layoutAliases":{"dense4-abl":"dense-abl-x2"},
         "layouts":{"dense-abl-x2":{"cards":{"A":{"engine":"ninfer","slots":4}}}},
         "engines":{"ninfer":{"title":"ninfer"}},
         "engineProfiles":{"g":{"engine":"ninfer","slots":4,"vramGiB":7.5}},
         "models":{"m":{"title":"Dense","vision":true,"artifacts":{"ninfer":{"file":"m.ninfer"}}}},
         "live":{"layout":"dense4-abl"}}"#,
    )
    .unwrap();
    assert_eq!(c.resolve(Some("DENSE4-ABL")).as_deref(), Some("dense-abl-x2"));
    assert_eq!(c.resolve(Some("older")).as_deref(), Some("dense-abl-x2"));
    assert_eq!(c.resolve(Some("nope")), None);
    assert!(c.find("dense4-abl").is_some());
    assert!(c.fits(c.find("dense4-abl").unwrap()));
    assert_eq!(
        c.find("dense-abl-x2").unwrap().mode.as_deref(),
        Some("auto")
    );
    assert_eq!(c.engine_profiles.as_ref().unwrap()["g"].slots, 4);
    assert_eq!(
        c.engine_profiles.as_ref().unwrap()["g"].vram_gib,
        Some(7.5)
    );
    assert!(c.models.as_ref().unwrap()["m"].artifacts.get("ninfer").is_some());
}

#[test]
fn profile_with_slots_on_a_config_without_slots_saves_no_slots_section() {
    let mut manager = loaded(r#"{ "lmStudio": { "baseUrl": "http://x/v1" } }"#);
    let mut p = ProfileDefinition::default();
    p.client.insert("slots.model".into(), json!("m"));
    p.client
        .insert("slots.registryUrl".into(), json!("http://r:8099"));
    manager.apply_profile("p", &p).unwrap();
    assert!(manager.config.slots.is_some());
    let saved: Value = serde_json::from_str(&manager.serialize_for_save().unwrap()).unwrap();
    assert!(!co::has(&saved, "slots"));
}

#[test]
fn snapshot_has_the_resolved_settings_without_secrets() {
    let mut manager = loaded(USER_CONFIG);
    manager.config.slots.as_mut().unwrap().api_key = Some("secret-2".into());
    manager.apply_profile("dense2", &dense()).unwrap();
    let snap = snapshot(&manager.config);
    let text = snap.to_string();
    assert_eq!(snap["profile"], "dense2");
    assert_eq!(snap["agent"]["peerMode"], true);
    assert_eq!(snap["slots"]["copies"].as_array().unwrap().len(), 2);
    assert_eq!(snap["slots"]["apiKeyEnv"], "MAYA_API_KEY");
    assert!(!snap["agent"]["peerMaxTokens"].is_null());
    assert!(!text.contains("secret-1"));
    assert!(!text.contains("secret-2"));
}

#[test]
fn server_url_defaults_to_the_registry_host() {
    let mut config = crate::config::OrcaConfig {
        profiles: ProfilesConfig::default(),
        ..Default::default()
    };
    assert_eq!(resolve_server_url(&config), None);
    config.slots = Some(SlotsConfig {
        registry_url: Some("http://box:8099".into()),
        ..SlotsConfig::default()
    });
    assert_eq!(
        resolve_server_url(&config).as_deref(),
        Some("http://box:8098")
    );
    config.profiles.server_url = Some("http://other:1/".into());
    assert_eq!(
        resolve_server_url(&config).as_deref(),
        Some("http://other:1")
    );
}

// ---- ProfileServiceTests (a local fake API instead of the fake HttpMessageHandler) ----

pub(crate) const CATALOG: &str = r#"
{
  "version": 1, "etag": "e1",
  "layouts": { "flash2": { "title": "Flash x2", "cards": { "A": { "engine": "strata", "slots": 3 }, "B": { "engine": "strata", "slots": 2 } } },
               "dense2": { "cards": { "A": { "engine": "ninfer", "slots": 2, "model": "m.ninfer" }, "B": { "engine": "ninfer", "slots": 2, "model": "m.ninfer" } } } },
  "profiles": {
    "flash2": { "title": "Flash", "layout": "flash2", "client": { "slots.model": "flash" } },
    "dense2": { "title": "Dense", "layout": "dense2", "client": { "agent.peerMode": true } },
    "claude": { "layout": null, "client": { "agent.engine": "claude" } }
  },
  "live": { "layout": "flash2", "healthy": true, "cards": {}, "leases": [] },
  "layoutStatus": { "dense2": { "needsRestore": true, "restore": { "restoreBytes": 23719715844, "restoreSeconds": 338, "text": "needs restore (≈24 GB, ~6 min)" } } }
}
"#;

#[tokio::test]
async fn catalog_is_fetched_then_served_from_the_cache_when_offline() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("profiles-cache.json");
    let server = orca_testkit::FakeOpenAi::start_with(
        vec![],
        orca_testkit::ServerOptions::default().route("GET /profiles", 200, CATALOG),
    )
    .await;
    let root = server.base_url().trim_end_matches("/v1").to_string();
    let online = ProfileService::new(Some(&root), Some(cache.clone()), Duration::from_secs(4));
    let (c, err) = online.get_catalog().await;
    assert_eq!(err, None);
    let c = c.unwrap();
    assert!(!c.from_cache);
    assert!(c.fits(c.find("FLASH2").unwrap()));
    assert!(!c.fits(c.find("dense2").unwrap()));
    assert!(c.fits(c.find("claude").unwrap()));
    assert_eq!(
        c.restore_note(c.find("dense2").unwrap()).as_deref(),
        Some("needs restore (≈24 GB, ~6 min)")
    );
    assert_eq!(c.layouts["flash2"].summary(), "A strata×3, B strata×2");

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let offline = ProfileService::new(
        Some(&format!("http://127.0.0.1:{port}")),
        Some(cache),
        Duration::from_secs(4),
    );
    let (cached, error) = offline.get_catalog().await;
    assert!(error.is_some());
    let cached = cached.unwrap();
    assert!(cached.from_cache);
    assert_eq!(cached.profiles.len(), 3);
}

#[tokio::test]
async fn switch_refusal_and_job_following() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let app = axum::Router::new()
        .route(
            "/switch",
            axum::routing::post(|body: String| async move {
                if body.contains("busy") {
                    (
                        axum::http::StatusCode::CONFLICT,
                        r#"{"error":"pig-slots leases held: B1 (bench)"}"#,
                    )
                } else {
                    (
                        axum::http::StatusCode::ACCEPTED,
                        r#"{"id":"j1","state":"queued","estimateSeconds":120}"#,
                    )
                }
            }),
        )
        .route(
            "/jobs/j1",
            axum::routing::get(move || {
                let n = p.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n < 3 {
                        r#"{"id":"j1","state":"running","steps":[{"t":0.5,"msg":"stopping"}]}"#
                    } else {
                        r#"{"id":"j1","state":"done","seconds":131.2,"steps":[{"t":0.5,"msg":"stopping"},{"t":130,"msg":"done"}]}"#
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let service = ProfileService::new(
        Some(&format!("http://{addr}")),
        None,
        Duration::from_secs(4),
    );

    let refused = service.request_switch("busy", "test", None).await.unwrap();
    assert_eq!(refused.state.as_deref(), Some("refused"));
    assert!(refused.error.unwrap().contains("leases held"));

    let job = service.request_switch("dense2", "test", None).await.unwrap();
    assert_eq!(job.id.as_deref(), Some("j1"));
    let mut steps = Vec::new();
    let done = service
        .wait_for_job(
            job,
            |s| steps.push(s.msg.clone()),
            Duration::from_secs(60),
            Some(Duration::from_millis(5)),
        )
        .await;
    assert!(done.succeeded());
    assert_eq!(steps, vec!["stopping", "done"]);
    assert_eq!(done.seconds, Some(131.2));
}

#[test]
fn the_cards_alias_reaches_the_web_ui() {
    let json = r#"{"layout":"merged-inkling-x2","cards":{"A":{"engine":"ninfer","slots":4,"model":"inkling2-mini-q8.ninfer",
          "options":{"alias":"inkling-2-mini","max-context":65536,"spec":null},"healthy":true}}}"#;
    let live: LiveLayout = serde_json::from_str(json).unwrap();
    assert_eq!(
        live.cards["A"].options.as_ref().unwrap()["alias"],
        "inkling-2-mini"
    );
    let round = serde_json::to_value(&live).unwrap();
    assert_eq!(round["cards"]["A"]["options"]["alias"], "inkling-2-mini");
}

#[test]
fn numbers_may_come_as_strings() {
    let c = ProfileCatalog::parse(
        r#"{"version":"2","layouts":{"x":{"cards":{"A":{"engine":"strata","slots":"3"}}}}}"#,
    )
    .unwrap();
    assert_eq!(c.version, 2);
    assert_eq!(c.layouts["x"].summary(), "A strata×3");
}
