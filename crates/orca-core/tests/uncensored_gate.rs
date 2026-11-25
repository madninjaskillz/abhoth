//! `UncensoredGate`: which server gets the `/uncensored` field, and the 400 safety net. Ported
//! from the gate part of UncensoredTests.cs (the config, wire-order and slot parts are in
//! `client.rs` / `slots_client.rs`).

use std::sync::{Arc, Mutex};

use orca_core::client::served_models::ServedModels;
use orca_core::client::uncensored_gate::{self, UncensoredGate};
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ClientSettings, OpenAiClient, ServerKind,
    client_for_model, client_for_provider,
};
use orca_core::config::{
    AgentUncensoredConfig, OrcaConfig, ProviderConfig, SlotCopyConfig, SlotLeadConfig, SlotsConfig,
};
use orca_testkit::{FakeOpenAi, Reply, ServerOptions};
use serde_json::{Value, json};

const FIELD: &str = "experimental_speed_projection";

/// ninfer's answer to "experimental_speed_projection": true.
const REFUSAL: &str = r#"{"error":{"message":"experimental_speed_projection is not supported by this server","type":"invalid_request_error","param":"experimental_speed_projection","code":"speed_projection_not_supported"}}"#;

/// A fake server that says it is `kind` on `/health` and `/v1/models` (Unknown: answers
/// neither, 503).
async fn server(kind: ServerKind, replies: Vec<Reply>) -> FakeOpenAi {
    let options = match kind {
        ServerKind::Unknown => ServerOptions::default()
            .route("GET /health", 503, "")
            .route("GET /v1/models", 503, ""),
        ServerKind::Strata => ServerOptions::default()
            .route(
                "GET /health",
                200,
                r#"{"status": "ok", "max_context": 262144, "images": true, "service": "strata"}"#,
            )
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"m","owned_by":"llamacpp"}]}"#,
            ),
        ServerKind::Ninfer => ServerOptions::default()
            .route("GET /health", 200, r#"{"status":"ok"}"#)
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"m","max_model_len":131072,"object":"model","owned_by":"ninfer"}],"object":"list"}"#,
            ),
        ServerKind::Other => ServerOptions::default()
            .route("GET /health", 200, r#"{"status":"ok"}"#)
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"m","owned_by":"llamacpp"}]}"#,
            ),
    };
    FakeOpenAi::start_with(replies, options).await
}

fn ok() -> Reply {
    Reply::text("ok")
}

fn gateway_config(base_url: &str) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.base_url = base_url.to_string();
    c.lm_studio.models = vec!["m".into()];
    c
}

fn client(config: &OrcaConfig) -> OpenAiClient {
    client_for_model(config, Arc::new(ServedModels::new()), "m")
}

async fn ask(c: &OpenAiClient, streaming: bool) -> Result<String, ChatError> {
    let messages = [ChatMessage::user("hi")];
    let options = ChatOptions::default();
    if streaming {
        let mut text = String::new();
        c.stream("t", &messages, &options, &mut |e| {
            if let orca_core::client::StreamEvent::Text(t) = e {
                text.push_str(&t);
            }
        })
        .await?;
        Ok(text)
    } else {
        Ok(c.complete("t", &messages, &options).await?.text)
    }
}

fn raw_bodies(s: &FakeOpenAi) -> Vec<String> {
    s.requests()
        .iter()
        .map(|r| serde_json::to_string(&r.body).unwrap())
        .collect()
}

/// Every notice in this test binary (the hook is process-wide; tests filter by their server).
fn told() -> Arc<Mutex<Vec<String>>> {
    static TOLD: std::sync::OnceLock<Arc<Mutex<Vec<String>>>> = std::sync::OnceLock::new();
    TOLD.get_or_init(|| {
        let told = Arc::new(Mutex::new(Vec::new()));
        let sink = told.clone();
        uncensored_gate::set_on_field_refused(Some(Arc::new(move |t: &str| {
            sink.lock().unwrap().push(t.to_string());
        })));
        told
    })
    .clone()
}

#[tokio::test]
async fn a_ninfer_server_never_gets_the_field_and_its_requests_are_as_with_the_switch_off() {
    let s = server(ServerKind::Ninfer, vec![ok(), ok(), ok()]).await;
    let mut config = gateway_config(&s.base_url());
    let off = client(&config);
    ask(&off, false).await.unwrap();
    assert!(s.probes().is_empty(), "off: the server is not even asked");

    config.agent.uncensored.session = Some(true);
    let on = client(&config);
    assert_eq!(ask(&on, false).await.unwrap(), "ok");
    ask(&on, true).await.unwrap();

    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 3, "no 400, so no second attempt");
    assert!(bodies.iter().all(|b| !b.contains(FIELD)));
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        s.probes(),
        vec!["GET /v1/models"],
        "identified once (/v1/models names ninfer), not per request"
    );
    assert!(!uncensored_gate::has_refused(&s.base_url()));
}

#[tokio::test]
async fn split_layout_ninfer_copy_gets_no_field_the_strata_copy_still_does() {
    let dense = server(ServerKind::Ninfer, vec![ok(), ok()]).await;
    let flash = server(ServerKind::Strata, vec![ok()]).await;
    let mut a_config = gateway_config(&dense.base_url());
    a_config.agent.uncensored.session = Some(true);
    let a = client(&a_config);
    let mut b_config = gateway_config(&flash.base_url());
    b_config.agent.uncensored.session = Some(true);
    let b = client(&b_config);

    ask(&a, false).await.unwrap();
    ask(&b, false).await.unwrap();
    ask(&a, false).await.unwrap();

    assert!(raw_bodies(&dense).iter().all(|b| !b.contains(FIELD)));
    let last = &flash.requests()[0].body;
    assert_eq!(last[FIELD], true);
    // The field goes last (before the SDK's `model`).
    let keys: Vec<&String> = last.as_object().unwrap().keys().collect();
    assert_eq!(keys[keys.len() - 2], FIELD);
}

#[tokio::test]
async fn another_kind_of_server_still_gets_the_field_and_ninfer_does_once_it_is_listed() {
    let gateway = server(ServerKind::Other, vec![ok()]).await;
    let mut config = gateway_config(&gateway.base_url());
    config.agent.uncensored.session = Some(true);
    ask(&client(&config), false).await.unwrap();
    assert_eq!(gateway.requests()[0].body[FIELD], true);

    let dense = server(ServerKind::Ninfer, vec![ok()]).await;
    let mut config = gateway_config(&dense.base_url());
    config.agent.uncensored.session = Some(true);
    config.agent.uncensored.server_kinds = Some(vec!["strata".into(), "ninfer".into()]);
    ask(&client(&config), false).await.unwrap();
    assert_eq!(dense.requests()[0].body[FIELD], true);
}

#[tokio::test]
async fn an_unidentified_server_still_gets_the_field() {
    let s = server(ServerKind::Unknown, vec![ok(), ok()]).await;
    let mut config = gateway_config(&s.base_url());
    config.agent.uncensored.session = Some(true);
    let c = client(&config);
    ask(&c, false).await.unwrap();
    ask(&c, false).await.unwrap();

    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies
            .iter()
            .all(|b| b.contains(&format!("\"{FIELD}\":true")))
    );
    assert_eq!(
        s.probes().len(),
        2,
        "asked once (both probes); the second request does not wait for it again"
    );
    let d = AgentUncensoredConfig::default();
    assert!(d.is_sent_to(ServerKind::Unknown));
    assert!(d.is_sent_to(ServerKind::Strata));
    assert!(d.is_sent_to(ServerKind::Other));
    assert!(!d.is_sent_to(ServerKind::Ninfer));
    let none = AgentUncensoredConfig {
        server_kinds: None,
        ..Default::default()
    };
    assert!(!none.is_sent_to(ServerKind::Strata));
}

async fn refusal_case(streaming: bool) {
    // Not identified, so the field goes out and the 400 is the only signal.
    let s = server(
        ServerKind::Unknown,
        vec![Reply::error(400, REFUSAL), ok(), ok(), ok()],
    )
    .await;
    let mut config = gateway_config(&s.base_url());
    config.agent.uncensored.session = Some(true);
    let c = client(&config);
    let told = told();

    assert_eq!(ask(&c, streaming).await.unwrap(), "ok");
    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains(&format!("\"{FIELD}\":true")));
    assert!(!bodies[1].contains(FIELD));
    // The retry is the same request without the one field.
    let mut first: Value = serde_json::from_str(&bodies[0]).unwrap();
    first.as_object_mut().unwrap().remove(FIELD);
    assert_eq!(first, s.requests()[1].body);
    assert!(uncensored_gate::has_refused(&s.base_url()));

    assert_eq!(ask(&c, streaming).await.unwrap(), "ok"); // remembered: one request, no field
    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[2].contains(FIELD));

    // A second client for the same server (a sub-agent's) knows too.
    ask(&client(&config), false).await.unwrap();
    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 4);
    assert!(!bodies[3].contains(FIELD));

    let mine: Vec<String> = told
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.contains(&s.base_url()))
        .cloned()
        .collect();
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert!(mine[0].contains(FIELD));
    assert!(mine[0].contains("speed_projection_not_supported"));
    assert!(mine[0].contains("sent again without it"));
    assert!(mine[0].contains("(HTTP 400 (invalid_request_error: speed_projection_not_supported))"));
}

#[tokio::test]
async fn a_server_that_refuses_the_field_is_retried_once_without_it_remembered_and_the_user_is_told_once()
 {
    refusal_case(false).await;
}

#[tokio::test]
async fn a_streaming_server_that_refuses_the_field_is_retried_once_without_it() {
    refusal_case(true).await;
}

#[tokio::test]
async fn a_refusal_of_a_field_the_request_set_itself_is_retried_without_it_and_other_errors_are_not_retried()
 {
    let s = server(ServerKind::Unknown, vec![Reply::error(400, REFUSAL), ok()]).await;
    let uncensored = AgentUncensoredConfig {
        session: Some(true),
        ..Default::default()
    };
    let mut settings = ClientSettings::new(s.base_url(), "k", Some("m".into()));
    settings.session_field = uncensored.field();
    settings.uncensored_gate = Some(Arc::new(UncensoredGate::new(
        uncensored,
        s.base_url(),
        None,
        None,
    )));
    let c = OpenAiClient::new(settings, Arc::new(ServedModels::new()));
    // slots.extraBody may set the field as well: the retry drops that too.
    let mut options = ChatOptions::default();
    options.additional.insert(FIELD.into(), json!(true));
    c.complete("t", &[ChatMessage::user("hi")], &options)
        .await
        .unwrap();
    let bodies = raw_bodies(&s);
    assert_eq!(bodies.len(), 2);
    assert!(!bodies[1].contains(FIELD));

    assert!(uncensored_gate::is_field_refusal_message(
        "HTTP 400 (invalid_request_error: speed_projection_not_supported)"
    ));
    assert!(!uncensored_gate::is_field_refusal_message(
        "HTTP 400 (invalid_request_error: context_length_exceeded)"
    ));
    let http = |status: u16, body: &str| ChatError::Http {
        status,
        url: "u".into(),
        body: body.into(),
    };
    assert!(uncensored_gate::is_field_refusal(&http(400, REFUSAL)));
    assert!(uncensored_gate::is_field_refusal(&http(422, REFUSAL)));
    assert!(!uncensored_gate::is_field_refusal(&http(500, REFUSAL)));
    assert!(!uncensored_gate::is_field_refusal(&http(
        400,
        r#"{"error":{"code":"context_length_exceeded"}}"#
    )));
}

#[tokio::test]
async fn other_errors_are_not_retried() {
    let s = server(
        ServerKind::Unknown,
        vec![Reply::error(400, r#"{"error":{"message":"bad"}}"#), ok()],
    )
    .await;
    let mut config = gateway_config(&s.base_url());
    config.agent.uncensored.session = Some(true);
    assert!(ask(&client(&config), false).await.is_err());
    assert_eq!(s.requests().len(), 1);
    assert!(!uncensored_gate::has_refused(&s.base_url()));
}

fn slot_config(copy_url: &str, lead_url: &str) -> OrcaConfig {
    let mut c = gateway_config("http://gateway.invalid:1/v1");
    c.slots = Some(SlotsConfig {
        registry_url: Some("http://fake".into()),
        model: Some("m".into()),
        copies: vec![SlotCopyConfig {
            name: "A".into(),
            base_url: copy_url.into(),
            slots: 2,
            ..Default::default()
        }],
        lead: Some(SlotLeadConfig {
            name: "L".into(),
            base_url: lead_url.into(),
            model: Some("m".into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    c
}

#[test]
fn field_for_counts_slot_copies_and_the_lead_endpoint_as_local_but_not_an_anthropic_base_url() {
    let mut config = slot_config("http://box-a:8001/v1", "http://cpu-box:8013/v1");
    config.agent.uncensored.session = Some(true);
    let u = config.agent.uncensored.clone();
    assert!(u.field_for(&config, "p", "http://BOX-A:9000/v1").is_some());
    assert!(u.field_for(&config, "p", "http://cpu-box:1/v1").is_some());
    assert!(
        u.field_for(&config, "p", "http://gateway.invalid:1/v1")
            .is_some()
    );
    assert!(
        u.field_for(&config, "p", "https://api.openai.com/v1")
            .is_none()
    );
    assert!(u.field_for(&config, "p", "").is_none());

    config.lm_studio.provider = Some("anthropic".into());
    config.lm_studio.base_url = "https://api.anthropic.com".into();
    assert!(
        u.field_for(&config, "p", "https://api.anthropic.com/v1")
            .is_none()
    );

    let off = AgentUncensoredConfig {
        session: Some(false),
        ..u.clone()
    };
    assert!(
        off.field_for(&config, "p", "http://box-a:8001/v1")
            .is_none()
    );
}

#[tokio::test]
async fn a_named_provider_gets_the_field_only_on_the_local_servers_host_or_when_listed() {
    let s = server(ServerKind::Other, vec![ok(), ok(), ok()]).await;
    let mut config = gateway_config("http://10.255.0.1:1/v1"); // the local server is elsewhere
    config.agent.uncensored.enabled = true;
    let hosted = ProviderConfig {
        base_url: s.base_url(),
        models: vec!["m".into()],
        ..Default::default()
    };
    let served = Arc::new(ServedModels::new());
    let c = client_for_provider(&config, served.clone(), "hosted", &hosted, "m");
    ask(&c, false).await.unwrap();
    assert!(!raw_bodies(&s)[0].contains(FIELD));

    config.agent.uncensored.providers = Some(vec!["Hosted".into()]);
    let c = client_for_provider(&config, served.clone(), "hosted", &hosted, "m");
    ask(&c, false).await.unwrap();
    assert_eq!(s.requests()[1].body[FIELD], true);

    // Same host as the local server (lmStudio.baseUrl): sent without being listed.
    config.agent.uncensored.providers = None;
    config.lm_studio.base_url = "http://127.0.0.1:9/v1".into();
    let c = client_for_provider(&config, served, "hosted", &hosted, "m");
    ask(&c, false).await.unwrap();
    assert_eq!(s.requests()[2].body[FIELD], true);
}
