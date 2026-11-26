use super::*;
use crate::config::model::SlotsConfig;

fn model(id: &str) -> RouterModel {
    RouterModel {
        id: id.into(),
        kind: "local".into(),
        ..Default::default()
    }
}

fn router_config() -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.connection = Some("router".into());
    c.lm_studio.base_url = "http://router.lan:8010/v1".into();
    c
}

fn get<'a>(ov: &'a [(String, Value)], k: &str) -> Option<&'a Value> {
    ov.iter().find(|(p, _)| p == k).map(|(_, v)| v)
}

#[test]
fn root_of_takes_any_form_of_the_url() {
    for u in [
        "http://h:8010",
        "http://h:8010/",
        "http://h:8010/v1",
        "http://h:8010/v1/",
        "http://h:8010/V1",
        " h:8010/v1 ",
    ] {
        assert_eq!(root_of(u), "http://h:8010", "{u}");
    }
    assert_eq!(
        root_of("https://r.example/abhoth/v1"),
        "https://r.example/abhoth"
    );
    assert_eq!(base_url_of("http://h:8010/", None), "http://h:8010/v1");
}

#[test]
fn only_a_document_naming_the_router_service_identifies_one() {
    let doc = json!({"service": "abhoth-router", "api": 1, "name": "DT", "version": "0.16.9",
        "capabilities": ["chat", "model-meta"], "auth": {"required": false, "keys": true}});
    let info = parse_info(&doc.to_string()).unwrap();
    assert_eq!(info.name, "DT");
    assert_eq!(info.paths.models, MODELS_PATH);
    assert!(parse_info(r#"{"service":"something-else"}"#).is_none());
    assert!(parse_info("<html>not found</html>").is_none());
    // Unknown fields from a newer router are fine.
    let newer = json!({"service": "abhoth-router", "api": 2, "future": {"x": 1}});
    assert_eq!(parse_info(&newer.to_string()).unwrap().api, 2);
}

#[test]
fn server_mode_gets_no_overrides_from_apply() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = crate::config::ConfigManager::new(Some(dir.path()));
    m.load().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let notices = rt.block_on(apply(&mut m, &probe_client()));
    assert!(notices.is_empty());
    assert!(m.overlay_paths().is_empty());
}

#[test]
fn the_router_owned_settings_are_turned_off_even_without_metadata() {
    let mut c = router_config();
    c.lm_studio.provider = Some("anthropic".into());
    c.lm_studio.backup_on_refusal = true;
    c.lm_studio.backup_min_response_length = 20;
    c.lm_studio.pair_model_programming = true;
    c.slots = Some(SlotsConfig {
        enabled: true,
        ..Default::default()
    });
    c.profiles.active = Some("dense2".into());
    let (ov, notices) = overrides(&c, None);
    assert!(notices.is_empty());
    assert_eq!(get(&ov, "lmStudio.provider"), Some(&Value::Null));
    assert_eq!(get(&ov, "lmStudio.backupOnRefusal"), Some(&json!(false)));
    assert_eq!(
        get(&ov, "lmStudio.backupMinResponseLength"),
        Some(&json!(0))
    );
    assert_eq!(
        get(&ov, "lmStudio.pairModelProgramming"),
        Some(&json!(false))
    );
    assert_eq!(get(&ov, "slots.enabled"), Some(&json!(false)));
    assert_eq!(get(&ov, "profiles.active"), Some(&Value::Null));
    assert_eq!(
        get(&ov, "context.autoDetectContextWindow"),
        Some(&json!(false))
    );
    // Nothing about the model without metadata.
    assert!(get(&ov, "context.contextWindowSize").is_none());
    assert!(get(&ov, "lmStudio.models").is_none());
}

#[test]
fn the_chosen_models_metadata_replaces_the_local_model_settings() {
    let mut c = router_config();
    c.lm_studio.models = vec!["Coder".into(), "backup".into()];
    c.lm_studio.native_tool_calling = false;
    let mut coder = model("coder");
    coder.context_window = Some(65536);
    coder.max_output_tokens = Some(65536);
    coder.vision = Some(false);
    coder.tools = Some(true);
    coder.sampling = json!({"temperature": 0.6, "top_k": 20, "reasoning_effort": "low"})
        .as_object()
        .unwrap()
        .clone();
    let doc = RouterModels {
        models: vec![model("other"), coder],
        ..Default::default()
    };
    let (ov, notices) = overrides(&c, Some(&doc));
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(get(&ov, "context.contextWindowSize"), Some(&json!(65536)));
    assert_eq!(get(&ov, "lmStudio.vision"), Some(&json!(false)));
    assert_eq!(get(&ov, "lmStudio.nativeToolCalling"), Some(&json!(true)));
    // No output cap from the router's limit (none is sent; the negotiator is skipped).
    assert!(get(&ov, "lmStudio.maxTokens").is_none());
    // The router's spelling, and no client-side backups.
    assert_eq!(get(&ov, "lmStudio.models"), Some(&json!(["coder"])));
    assert_eq!(
        get(&ov, "lmStudio.modelSampling"),
        Some(&json!({"coder": {"temperature": 0.6, "topK": 20}}))
    );
}

#[test]
fn unknown_capabilities_leave_the_local_values() {
    let mut c = router_config();
    c.lm_studio.models = vec!["m".into()];
    let doc = RouterModels {
        models: vec![model("m")],
        ..Default::default()
    };
    let (ov, _) = overrides(&c, Some(&doc));
    assert!(get(&ov, "lmStudio.vision").is_none());
    assert!(get(&ov, "lmStudio.nativeToolCalling").is_none());
    assert!(get(&ov, "context.contextWindowSize").is_none());
    assert!(get(&ov, "lmStudio.models").is_none(), "unchanged list");
}

#[test]
fn a_local_per_model_sampling_value_wins_over_the_routers() {
    let mut c = router_config();
    c.lm_studio.models = vec!["m".into()];
    let mut ms = crate::config::model::Map::new();
    ms.insert(
        "m".into(),
        crate::config::model::SamplingConfig {
            temperature: Some(1.1),
            ..Default::default()
        },
    );
    c.lm_studio.model_sampling = Some(ms);
    let mut m = model("m");
    m.sampling = json!({"temperature": 0.6, "top_p": 0.9})
        .as_object()
        .unwrap()
        .clone();
    let (ov, _) = overrides(
        &c,
        Some(&RouterModels {
            models: vec![m],
            ..Default::default()
        }),
    );
    let s = &get(&ov, "lmStudio.modelSampling").unwrap()["m"];
    assert_eq!(s["topP"], json!(0.9));
    assert!((s["temperature"].as_f64().unwrap() - 1.1).abs() < 1e-6);
}

#[test]
fn no_model_chosen_picks_a_live_one_and_says_so() {
    let c = router_config();
    let mut live = model("live-one");
    live.status = Some("live".into());
    let mut cold = model("cold-one");
    cold.status = Some("cold".into());
    let doc = RouterModels {
        models: vec![cold, live],
        ..Default::default()
    };
    let (ov, notices) = overrides(&c, Some(&doc));
    assert_eq!(get(&ov, "lmStudio.models"), Some(&json!(["live-one"])));
    assert!(notices[0].contains("live-one"), "{notices:?}");
}

#[test]
fn a_model_the_router_does_not_offer_is_kept_and_reported() {
    let mut c = router_config();
    c.lm_studio.models = vec!["gone".into()];
    let doc = RouterModels {
        models: vec![model("m")],
        ..Default::default()
    };
    let (ov, notices) = overrides(&c, Some(&doc));
    assert!(get(&ov, "lmStudio.models").is_none());
    assert!(notices[0].contains("'gone'"), "{notices:?}");
}

#[test]
fn router_overrides_never_reach_config_json() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = crate::config::ConfigManager::new(Some(dir.path()));
    m.load().unwrap();
    m.config.lm_studio.connection = Some("router".into());
    m.config.lm_studio.models = vec!["a".into(), "b".into()];
    m.config.context.context_window_size = 8192;
    m.save().unwrap();
    m.load().unwrap();
    let mut meta = model("a");
    meta.context_window = Some(131072);
    meta.tools = Some(true);
    let (ov, _) = overrides(
        &m.config,
        Some(&RouterModels {
            models: vec![meta],
            ..Default::default()
        }),
    );
    m.apply_router(&ov).unwrap();
    assert_eq!(m.config.context.context_window_size, 131072);
    assert!(m.config.lm_studio.native_tool_calling);
    assert_eq!(m.config.lm_studio.models, vec!["a".to_string()]);
    assert!(m.config.lm_studio.is_router());
    m.save().unwrap();
    let text = std::fs::read_to_string(m.file_path()).unwrap();
    let saved: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(saved["context"]["contextWindowSize"], json!(8192));
    assert_eq!(saved["lmStudio"]["models"], json!(["a", "b"]));
    assert_eq!(saved["lmStudio"]["nativeToolCalling"], json!(false));
    assert_eq!(saved["lmStudio"]["connection"], json!("router"));
}

/// A tiny HTTP server: answers `path` with `body`, records the request headers.
async fn serve_once(
    path: &'static str,
    body: Value,
) -> (String, std::sync::Arc<Mutex<Vec<(String, String)>>>) {
    use axum::routing::get;
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        path,
        get(move |h: axum::http::HeaderMap| {
            let s2 = s2.clone();
            let body = body.clone();
            async move {
                let mut v = s2.lock().unwrap();
                for (k, val) in &h {
                    v.push((k.to_string(), val.to_str().unwrap_or("").to_string()));
                }
                axum::Json(body)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn identify_asks_the_well_known_path_without_switching() {
    let doc = json!({"service": "abhoth-router", "api": 1, "name": "Test router"});
    let (root, seen) = serve_once(WELL_KNOWN_PATH, doc).await;
    let info = identify(&format!("{root}/v1/"), &probe_client())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.name, "Test router");
    let h = seen.lock().unwrap().clone();
    assert!(h.contains(&(NO_SWITCH_HEADER.to_string(), "1".to_string())));
    assert!(!h.iter().any(|(k, _)| k == "authorization"));
}

#[tokio::test]
async fn a_plain_server_is_not_a_router_and_a_dead_one_is_an_error() {
    // Answers 404 on the well-known path.
    let (root, _) = serve_once("/v1/models", json!({"data": []})).await;
    assert_eq!(identify(&root, &probe_client()).await, Ok(None));
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    assert!(identify(&dead, &probe_client()).await.is_err());
}

#[tokio::test]
async fn model_metadata_is_fetched_with_the_key_and_no_switch() {
    let doc = json!({"principal": "user:j#1", "claude": true,
        "models": [{"id": "m", "contextWindow": 4096, "tools": true, "kind": "local"}]});
    let (root, seen) = serve_once(MODELS_PATH, doc).await;
    let m = fetch_models(&format!("{root}/v1"), "abk_secret", &probe_client())
        .await
        .unwrap();
    assert!(m.claude);
    assert_eq!(m.find("M").unwrap().context_window, Some(4096));
    let h = seen.lock().unwrap().clone();
    assert!(h.contains(&("authorization".to_string(), "Bearer abk_secret".to_string())));
    assert!(h.contains(&(NO_SWITCH_HEADER.to_string(), "1".to_string())));
}

#[test]
fn a_sessions_save_keeps_the_router_values_out_of_config_json() {
    use crate::config::schema::{ProfileOverlay, ROUTER_OVERLAY};
    let dir = tempfile::tempdir().unwrap();
    let mut m = crate::config::ConfigManager::new(Some(dir.path()));
    m.load().unwrap();
    m.config.lm_studio.connection = Some("router".into());
    m.save().unwrap();
    m.load().unwrap();
    let mut meta = model("a");
    meta.context_window = Some(131072);
    let (ov, _) = overrides(
        &m.config,
        Some(&RouterModels {
            models: vec![meta],
            ..Default::default()
        }),
    );
    m.apply_router(&ov).unwrap();
    let overlay = ProfileOverlay::of(&m).unwrap();
    assert_eq!(overlay.profile, ROUTER_OVERLAY);
    assert!(overlay.covers("context.contextWindowSize"));
    assert!(!overlay.covers("permissions.autoApproveAll"));
    // As the web UI's project settings save: the session's config adopted with the overlay.
    let mut session = m.config.clone();
    session.permissions.auto_approve_read_only = !session.permissions.auto_approve_read_only;
    let mut saver = crate::config::ConfigManager::new(Some(dir.path()));
    saver.load().unwrap();
    saver.adopt(session.clone(), overlay.paths.clone());
    saver.save().unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(saver.file_path()).unwrap()).unwrap();
    assert_eq!(saved["context"]["contextWindowSize"], json!(8192));
    assert!(saved["lmStudio"]["models"].as_array().unwrap().is_empty());
    assert_eq!(
        saved["permissions"]["autoApproveReadOnly"],
        json!(session.permissions.auto_approve_read_only)
    );
}

#[test]
fn lm_studios_placeholder_key_is_not_sent_to_a_router() {
    let mut c = router_config();
    crate::config::manager::synthesize_default_provider(&mut c);
    assert_eq!(c.lm_studio.api_key, "lm-studio");
    let (ov, _) = overrides(&c, None);
    assert_eq!(get(&ov, "lmStudio.apiKey"), Some(&json!("")));
    assert_eq!(get(&ov, "providers.lmstudio.apiKey"), Some(&json!("")));
    // A real key stays.
    c.lm_studio.api_key = "abk_user_x".into();
    let (ov, _) = overrides(&c, None);
    assert!(get(&ov, "lmStudio.apiKey").is_none());
}

#[tokio::test]
async fn the_placeholder_key_is_not_sent_for_metadata() {
    let (root, seen) = serve_once(MODELS_PATH, json!({"models": []})).await;
    fetch_models(&root, "lm-studio", &probe_client())
        .await
        .unwrap();
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|(k, _)| k == "authorization")
    );
}
