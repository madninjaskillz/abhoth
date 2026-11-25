//! Ported from tests/OpenOrca.Core.Tests/Slots: LeadPinTests, LazyLeadLeaseTests,
//! LeadEndpointTests, CopyModelTests, CopyServedModelTests.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::testing::{FakeSlotHealth, FakeSlotRegistry};
use super::*;
use crate::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, ClientSettings, OpenAiClient,
    Role, ServedModels, ServerKind, StreamControl, StreamEvent, ToolCall,
};
use crate::config::{OrcaConfig, SamplingConfig, SlotCopyConfig, SlotLeadConfig, SlotsConfig};

// ---- a scripted in-memory backend (CapturingHandler / ScriptedChatClient) ----

#[derive(Default)]
struct Capture {
    text: String,
    reply_with_tool_call: bool,
    /// Requests from this index on fail.
    fail_from: Option<usize>,
    /// Requests after the first never answer.
    hang_after_first: bool,
    gate: Option<Arc<tokio::sync::Notify>>,
    bodies: Mutex<Vec<Value>>,
    release_waited: Mutex<Option<Duration>>,
}

impl Capture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            text: "hi".into(),
            ..Self::default()
        })
    }

    fn with_text(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: text.into(),
            ..Self::default()
        })
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }

    fn body(&self) -> Option<Value> {
        self.bodies().last().cloned()
    }

    fn json(&self) -> Value {
        self.body().unwrap_or(Value::Null)
    }

    async fn answer(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let client = OpenAiClient::new(
            ClientSettings::new("http://capture/v1", "k", Some("m".into())),
            Arc::new(ServedModels::new()),
        );
        let n = {
            let mut b = self.bodies.lock().unwrap();
            b.push(client.build_body(messages, options, false)?);
            b.len()
        };
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        if self.hang_after_first && n > 1 {
            let started = Instant::now();
            struct Waited<'a>(&'a Capture, Instant);
            impl Drop for Waited<'_> {
                fn drop(&mut self) {
                    *self.0.release_waited.lock().unwrap() = Some(self.1.elapsed());
                }
            }
            let _w = Waited(self, started);
            std::future::pending::<()>().await;
        }
        if self.fail_from.is_some_and(|f| n > f) {
            return Err(ChatError::Server("release failed".into()));
        }
        let mut r = ChatResponse {
            text: self.text.clone(),
            ..ChatResponse::default()
        };
        if self.reply_with_tool_call {
            r.tool_calls.push(ToolCall {
                id: "c9".into(),
                name: "read".into(),
                arguments: "{}".into(),
            });
        }
        Ok(r)
    }
}

struct CaptureClient(Arc<Capture>);

#[async_trait]
impl ChatBackend for CaptureClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.0.answer(messages, options).await
    }

    async fn stream_with_control(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.0.answer(messages, options).await?;
        on_event(StreamEvent::Text(r.text.clone()));
        Ok(r)
    }
}

fn backend(c: &Arc<Capture>) -> Arc<dyn ChatBackend> {
    Arc::new(CaptureClient(c.clone()))
}

async fn send(
    client: &dyn ChatBackend,
    messages: &[ChatMessage],
    options: &ChatOptions,
    streaming: bool,
) -> ChatResponse {
    if streaming {
        let mut text = String::new();
        let r = client
            .stream_with_control("lead", messages, options, &mut |e| {
                if let StreamEvent::Text(t) = e {
                    text.push_str(&t);
                }
                StreamControl::Continue
            })
            .await
            .unwrap();
        assert_eq!(text, r.text);
        r
    } else {
        client.complete("lead", messages, options).await.unwrap()
    }
}

fn hi() -> Vec<ChatMessage> {
    vec![ChatMessage::user("hi")]
}

// ---- LeadPinTests ----

fn pin_config(pin: bool) -> SlotsConfig {
    SlotsConfig {
        registry_url: Some("http://fake".into()),
        agents_per_copy: 0,
        pin_lead_prefix: pin,
        copies: vec![
            SlotCopyConfig {
                name: "A".into(),
                base_url: "http://a/v1".into(),
                slots: 2,
                ..SlotCopyConfig::default()
            },
            SlotCopyConfig {
                name: "B".into(),
                base_url: "http://b/v1".into(),
                slots: 2,
                ..SlotCopyConfig::default()
            },
        ],
        lead_copy: Some("A".into()),
        ..SlotsConfig::default()
    }
}

fn allocator_with(
    config: SlotsConfig,
    registry: FakeSlotRegistry,
    health: FakeSlotHealth,
    pid: u32,
) -> Arc<SlotAllocator> {
    SlotAllocator::new(
        config,
        Some(Arc::new(registry)),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(health),
        AllocatorOptions {
            owner: Some("tester@box".into()),
            host: Some("box".into()),
            pid: Some(pid),
            ..AllocatorOptions::default()
        },
    )
}

fn pin_allocator(config: SlotsConfig) -> Arc<SlotAllocator> {
    allocator_with(
        config,
        FakeSlotRegistry::default(),
        FakeSlotHealth::default(),
        1,
    )
}

fn kind_probe(kind: ServerKind, asked: Option<Arc<Mutex<Vec<String>>>>) -> client::ServerKindProbe {
    Arc::new(move |copy: String| {
        if let Some(a) = &asked {
            a.lock().unwrap().push(copy);
        }
        Box::pin(async move { kind })
    })
}

async fn pin_client(
    allocator: Arc<SlotAllocator>,
    kind: &str,
    wire: &Arc<Capture>,
    fallback: Option<&Arc<Capture>>,
    server_kind: Option<client::ServerKindProbe>,
) -> SlottedChatClient {
    let owner = if kind == LEAD_KIND { "lead" } else { "x" };
    let lease = allocator
        .try_acquire(&SlotRequest::new(owner, kind, "t").with_prefer_copy(Some("A")))
        .await
        .unwrap();
    let w = backend(wire);
    SlottedChatClient::new(
        allocator,
        lease,
        Arc::new(move |_: &str| w.clone()),
        backend(fallback.unwrap_or(wire)),
        server_kind,
    )
}

/// A Lead history after two tool rounds; the second round ran two tools at once.
fn history() -> Vec<ChatMessage> {
    let call = |calls: Vec<(&str, &str)>| ChatMessage {
        role: Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: calls
            .into_iter()
            .map(|(id, name)| ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            })
            .collect(),
        tool_call_id: None,
        leading_empty_part: false,
        parts: Vec::new(),
    };
    vec![
        ChatMessage::system("sys"),
        ChatMessage::user("build it"),
        call(vec![("c1", "read")]),
        ChatMessage::tool_result("c1", "file a"),
        call(vec![("c2", "subagent"), ("c3", "subagent")]),
        ChatMessage::tool_result("c2", "done x"),
        ChatMessage::tool_result("c3", "done y"),
    ]
}

#[tokio::test]
async fn lead_requests_pin_everything_but_the_last_wire_message() {
    for streaming in [false, true] {
        let wire = Capture::new();
        let client = pin_client(
            pin_allocator(pin_config(true)),
            LEAD_KIND,
            &wire,
            None,
            None,
        )
        .await;
        send(&client, &history(), &ChatOptions::default(), streaming).await;
        let first = &wire.bodies()[0];
        let sent = first["messages"].as_array().unwrap().len();
        assert_eq!(sent, 7);
        assert_eq!(first["strata_prefix"]["messages"], json!(sent - 1));
    }
}

#[tokio::test]
async fn sub_agents_do_not_pin() {
    let wire = Capture::new();
    let client = pin_client(
        pin_allocator(pin_config(true)),
        AGENT_KIND,
        &wire,
        None,
        None,
    )
    .await;
    client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert!(wire.json().get("strata_prefix").is_none());
}

#[tokio::test]
async fn switched_off_no_pin() {
    let wire = Capture::new();
    let client = pin_client(
        pin_allocator(pin_config(false)),
        LEAD_KIND,
        &wire,
        None,
        None,
    )
    .await;
    client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert!(wire.json().get("strata_prefix").is_none());
}

#[tokio::test]
async fn on_the_fallback_no_pin() {
    let health = FakeSlotHealth::default();
    health.set_down("A");
    health.set_down("B");
    let wire = Capture::new();
    let fallback = Capture::new();
    let allocator = allocator_with(pin_config(true), FakeSlotRegistry::default(), health, 1);
    let client = pin_client(allocator, LEAD_KIND, &wire, Some(&fallback), None).await;
    client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert!(wire.body().is_none());
    assert!(fallback.json().get("strata_prefix").is_none());
}

#[tokio::test]
async fn a_first_request_with_one_message_is_not_pinned_and_caller_options_are_untouched() {
    let wire = Capture::new();
    let client = pin_client(
        pin_allocator(pin_config(true)),
        LEAD_KIND,
        &wire,
        None,
        None,
    )
    .await;
    let mut options = ChatOptions::default();
    options.set("min_p", json!(0.05));

    client.complete("x", &hi(), &options).await.unwrap();
    assert!(wire.json().get("strata_prefix").is_none());
    assert_eq!(wire.bodies().len(), 1);

    client.complete("x", &history(), &options).await.unwrap();
    let pinned = &wire.bodies()[1];
    assert_eq!(pinned["strata_prefix"]["messages"], 6);
    assert_eq!(pinned["min_p"], json!(0.05));
    assert!(!options.additional.contains_key(client::PREFIX_KEY));
}

#[test]
fn pin_lead_prefix_defaults_on_and_reads_from_config_json() {
    assert!(SlotsConfig::default().pin_lead_prefix);
    let config: OrcaConfig = serde_json::from_str(r#"{"slots":{"pinLeadPrefix":false}}"#).unwrap();
    assert!(!config.slots.unwrap().pin_lead_prefix);
}

#[tokio::test]
async fn the_leads_final_answer_releases_its_pin_with_the_same_prompt_and_one_token() {
    for streaming in [false, true] {
        let wire = Capture::new();
        let client = pin_client(
            pin_allocator(pin_config(true)),
            LEAD_KIND,
            &wire,
            None,
            None,
        )
        .await;
        let mut options = ChatOptions {
            max_output_tokens: Some(4000),
            ..ChatOptions::default()
        };
        options.set("min_p", json!(0.05));
        send(&client, &history(), &options, streaming).await;

        let bodies = wire.bodies();
        assert_eq!(bodies.len(), 2);
        let (answer, release) = (&bodies[0], &bodies[1]);
        assert_eq!(answer["messages"], release["messages"]);
        assert_eq!(release["strata_prefix"]["tokens"], 1);
        assert!(release["strata_prefix"].get("messages").is_none());
        assert_eq!(release["max_completion_tokens"], 1);
        assert_eq!(release["min_p"], json!(0.05));
        assert_eq!(options.max_output_tokens, Some(4000));
    }
}

#[tokio::test]
async fn a_lead_tool_call_keeps_the_pin_and_agents_and_switched_off_send_no_release() {
    let wire = Arc::new(Capture {
        text: "hi".into(),
        reply_with_tool_call: true,
        ..Capture::default()
    });
    pin_client(
        pin_allocator(pin_config(true)),
        LEAD_KIND,
        &wire,
        None,
        None,
    )
    .await
    .complete("x", &history(), &ChatOptions::default())
    .await
    .unwrap();
    assert_eq!(wire.bodies().len(), 1);

    let agent = Capture::new();
    pin_client(
        pin_allocator(pin_config(true)),
        AGENT_KIND,
        &agent,
        None,
        None,
    )
    .await
    .complete("x", &history(), &ChatOptions::default())
    .await
    .unwrap();
    assert_eq!(agent.bodies().len(), 1);

    let off = Capture::new();
    pin_client(
        pin_allocator(pin_config(false)),
        LEAD_KIND,
        &off,
        None,
        None,
    )
    .await
    .complete("x", &history(), &ChatOptions::default())
    .await
    .unwrap();
    assert_eq!(off.bodies().len(), 1);
}

#[tokio::test]
async fn a_failed_release_is_ignored() {
    let wire = Arc::new(Capture {
        text: "hi".into(),
        fail_from: Some(1),
        ..Capture::default()
    });
    let client = pin_client(
        pin_allocator(pin_config(true)),
        LEAD_KIND,
        &wire,
        None,
        None,
    )
    .await;
    let r = client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(r.text, "hi");
    assert_eq!(wire.bodies().len(), 2);
}

#[tokio::test]
async fn a_copy_that_is_not_strata_gets_no_pin_and_no_release_request() {
    for kind in [ServerKind::Ninfer, ServerKind::Other] {
        for streaming in [false, true] {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let wire = Capture::new();
            let client = pin_client(
                pin_allocator(pin_config(true)),
                LEAD_KIND,
                &wire,
                None,
                Some(kind_probe(kind, Some(asked.clone()))),
            )
            .await;
            send(&client, &history(), &ChatOptions::default(), streaming).await;
            let bodies = wire.bodies();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0]["messages"].as_array().unwrap().len(), 7);
            assert!(bodies[0].get("strata_prefix").is_none());
            assert_eq!(*asked.lock().unwrap(), vec!["A"]);
        }
    }
}

#[tokio::test]
async fn on_strata_and_on_a_copy_that_could_not_be_identified_the_pin_and_its_release_are_sent_as_before()
 {
    for kind in [ServerKind::Strata, ServerKind::Unknown] {
        for streaming in [false, true] {
            let wire = Capture::new();
            let client = pin_client(
                pin_allocator(pin_config(true)),
                LEAD_KIND,
                &wire,
                None,
                Some(kind_probe(kind, None)),
            )
            .await;
            send(&client, &history(), &ChatOptions::default(), streaming).await;
            let bodies = wire.bodies();
            assert_eq!(bodies.len(), 2);
            assert_eq!(bodies[0]["strata_prefix"]["messages"], 6);
            assert_eq!(bodies[1]["strata_prefix"]["tokens"], 1);
        }
    }
}

#[tokio::test]
async fn a_probe_that_throws_counts_as_not_identified_and_the_kind_is_not_asked_when_nothing_would_pin()
 {
    // The Rust probe cannot throw: a failed ServerIdentity probe answers Unknown, the case
    // checked here.
    let wire = Capture::new();
    let client = pin_client(
        pin_allocator(pin_config(true)),
        LEAD_KIND,
        &wire,
        None,
        Some(kind_probe(ServerKind::Unknown, None)),
    )
    .await;
    client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(wire.bodies().len(), 2);
    assert!(wire.bodies()[0].get("strata_prefix").is_some());

    let asked = Arc::new(Mutex::new(Vec::new()));
    pin_client(
        pin_allocator(pin_config(true)),
        AGENT_KIND,
        &Capture::new(),
        None,
        Some(kind_probe(ServerKind::Strata, Some(asked.clone()))),
    )
    .await
    .complete("x", &history(), &ChatOptions::default())
    .await
    .unwrap();
    pin_client(
        pin_allocator(pin_config(false)),
        LEAD_KIND,
        &Capture::new(),
        None,
        Some(kind_probe(ServerKind::Strata, Some(asked.clone()))),
    )
    .await
    .complete("x", &history(), &ChatOptions::default())
    .await
    .unwrap();
    assert!(asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn on_a_copy_that_could_not_be_identified_a_hanging_release_is_given_up_after_two_seconds() {
    assert_eq!(client::RELEASE_TIMEOUT_STRATA, Duration::from_secs(15));
    assert_eq!(client::RELEASE_TIMEOUT_UNKNOWN, Duration::from_secs(2));
    let copy = Arc::new(Capture {
        text: "done".into(),
        hang_after_first: true,
        ..Capture::default()
    });
    let client = pin_client(
        pin_allocator(pin_config(true)),
        LEAD_KIND,
        &copy,
        None,
        Some(kind_probe(ServerKind::Unknown, None)),
    )
    .await;
    let r = client
        .complete("x", &history(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(r.text, "done");
    assert_eq!(copy.bodies().len(), 2);
    let waited = copy.release_waited.lock().unwrap().unwrap();
    assert!(waited >= Duration::from_secs(1) && waited <= Duration::from_secs(10));
}

// TheRuntimesProbe_AsksTheCopyItself_OncePerCopy: the probe is ServerIdentity::get per copy
// URL (its once-per-server caching and detection are covered by client::server_identity's
// tests); here only the unknown-copy answer.
#[tokio::test]
async fn the_runtimes_probe_answers_unknown_for_no_such_copy() {
    let config = OrcaConfig {
        slots: Some(pin_config(true)),
        ..OrcaConfig::default()
    };
    let probe = SlotRuntime::copy_kind_probe(&config, None);
    assert_eq!(probe("Z".into()).await, ServerKind::Unknown);
}

// ---- LazyLeadLeaseTests ----

fn lazy_config(idle_minutes: i32) -> OrcaConfig {
    OrcaConfig {
        slots: Some(SlotsConfig {
            registry_url: Some("http://fake".into()),
            retry_seconds: 1,
            idle_release_minutes: idle_minutes,
            ..pin_config(true)
        }),
        ..OrcaConfig::default()
    }
}

fn runtime_with(
    config: &OrcaConfig,
    registry: Arc<FakeSlotRegistry>,
    copy: Arc<dyn ChatBackend>,
    fallback: Arc<dyn ChatBackend>,
) -> Arc<SlotRuntime> {
    let allocator = SlotAllocator::new(
        config.slots.clone().unwrap(),
        Some(registry),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(FakeSlotHealth::default()),
        AllocatorOptions {
            owner: Some("tester@box".into()),
            host: Some("box".into()),
            pid: Some(4242),
            ..AllocatorOptions::default()
        },
    );
    let factory = SlotClientFactory::new(
        config.clone(),
        Some(Arc::new(move |_: &SlotCopyConfig| copy.clone())),
        None,
        None,
    );
    SlotRuntime::new(allocator, Arc::new(factory), fallback, None)
}

fn lazy(idle_minutes: i32) -> (Arc<SlotRuntime>, Arc<FakeSlotRegistry>) {
    let registry = Arc::new(FakeSlotRegistry::new(&["A1", "A2", "B1", "B2"]));
    let rt = runtime_with(
        &lazy_config(idle_minutes),
        registry.clone(),
        backend(&Capture::with_text("ok")),
        backend(&Capture::with_text("fallback")),
    );
    (rt, registry)
}

fn keys(r: &FakeSlotRegistry) -> Vec<String> {
    r.snapshot().keys().cloned().collect()
}

#[tokio::test]
async fn no_lease_until_the_first_request_then_the_leads_copy() {
    let (rt, registry) = lazy(15);
    assert!(registry.snapshot().is_empty());
    assert!(rt.lead_location().is_none());
    assert_eq!(registry.claims(), 0);

    let lead = rt.lead_client();
    let r = lead
        .complete("lead", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(rt.lead_location().as_deref(), Some("A1"));
    assert_eq!(keys(&registry), vec!["A1"]);

    send(lead.as_ref(), &hi(), &ChatOptions::default(), true).await;
    assert_eq!(rt.lead_location().as_deref(), Some("A1"));
    assert_eq!(registry.claims(), 1);
    rt.shutdown().await;
}

#[tokio::test]
async fn idle_release_gives_the_slot_back_and_the_next_request_leases_again() {
    let (rt, registry) = lazy(15);
    rt.lead_client()
        .complete("lead", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    let now = Instant::now();
    let min = |m: u64| now + Duration::from_secs(m * 60);

    assert!(!rt.release_lead_if_idle(min(14)).await);
    assert_eq!(rt.lead_location().as_deref(), Some("A1"));

    let busy = Arc::new(AtomicBool::new(true));
    let b = busy.clone();
    rt.set_lead_busy(Some(move || b.load(Ordering::SeqCst)));
    assert!(!rt.release_lead_if_idle(min(60)).await);
    assert_eq!(rt.lead_location().as_deref(), Some("A1"));

    busy.store(false, Ordering::SeqCst);
    assert!(rt.release_lead_if_idle(min(16)).await);
    assert!(rt.lead_location().is_none());
    assert!(registry.snapshot().is_empty());
    assert!(!rt.lead_lease.released());
    assert!(!rt.release_lead_if_idle(min(40)).await);

    rt.lead_client()
        .complete("lead", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(rt.lead_location().as_deref(), Some("A1"));
    assert_eq!(keys(&registry), vec!["A1"]);
    rt.shutdown().await;
}

#[tokio::test]
async fn idle_release_zero_keeps_the_slot_until_exit() {
    let (rt, registry) = lazy(0);
    rt.lead_client()
        .complete("lead", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert!(rt.idle_release().is_none());
    assert!(
        !rt.release_lead_if_idle(Instant::now() + Duration::from_secs(2 * 86400))
            .await
    );
    assert_eq!(keys(&registry), vec!["A1"]);
    rt.shutdown().await;
    assert!(registry.snapshot().is_empty());
}

#[tokio::test]
async fn a_request_in_flight_is_never_idle() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let copy = Arc::new(Capture {
        text: "ok".into(),
        gate: Some(gate.clone()),
        ..Capture::default()
    });
    let registry = Arc::new(FakeSlotRegistry::new(&["A1", "A2", "B1", "B2"]));
    let rt = runtime_with(
        &lazy_config(1),
        registry,
        backend(&copy),
        backend(&Capture::new()),
    );
    let lead = rt.lead_client();
    let request =
        tokio::spawn(async move { lead.complete("lead", &hi(), &ChatOptions::default()).await });
    while rt.lead_location().is_none() || copy.bodies().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(rt.lead_request_in_flight());
    assert!(
        !rt.release_lead_if_idle(Instant::now() + Duration::from_secs(3600))
            .await
    );
    gate.notify_one();
    request.await.unwrap().unwrap();
    assert!(!rt.lead_request_in_flight());
    assert!(
        rt.release_lead_if_idle(Instant::now() + Duration::from_secs(120))
            .await
    );
    rt.shutdown().await;
}

// ---- LeadEndpointTests ----

fn endpoint_config(lead_url: &str, agents_per_copy: i32) -> OrcaConfig {
    let mut extra = crate::config::model::Map::new();
    extra.insert("reasoning_effort".into(), json!("low"));
    OrcaConfig {
        slots: Some(SlotsConfig {
            model: Some("qwen3.8-flash-next".into()),
            agents_per_copy,
            retry_seconds: 1,
            extra_body: Some(extra),
            lead: Some(SlotLeadConfig {
                name: "CPU".into(),
                base_url: lead_url.into(),
                model: Some("qwen-cpu".into()),
                ..SlotLeadConfig::default()
            }),
            ..pin_config(true)
        }),
        ..OrcaConfig::default()
    }
}

#[tokio::test]
async fn lead_takes_no_lease_shows_its_endpoint_and_talks_to_it() {
    let config = endpoint_config("http://cpu-box:8013/v1", 2);
    let registry = Arc::new(FakeSlotRegistry::default());
    let built: Arc<Mutex<Vec<String>>> = Arc::default();
    let b = built.clone();
    let allocator = SlotAllocator::new(
        config.slots.clone().unwrap(),
        Some(registry.clone()),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(FakeSlotHealth::default()),
        AllocatorOptions::default(),
    );
    let factory = SlotClientFactory::new(
        config.clone(),
        Some(Arc::new(move |c: &SlotCopyConfig| {
            b.lock().unwrap().push(c.name.clone());
            backend(&Capture::with_text(&format!("from {}", c.name)))
        })),
        None,
        None,
    );
    let rt = SlotRuntime::new(allocator, Arc::new(factory), backend(&Capture::new()), None);
    rt.start_lead().await;
    assert!(registry.snapshot().is_empty());
    assert!(rt.lead_lease.slot().is_none());
    assert_eq!(rt.lead_location().as_deref(), Some("CPU"));
    let reply = rt
        .lead_client()
        .complete("lead", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(reply.text, "from CPU");
    assert_eq!(*built.lock().unwrap(), vec!["CPU"]);
}

#[tokio::test]
async fn agents_use_all_four_slots_one_per_copy_before_doubling_up() {
    let mut config = endpoint_config("http://cpu-box:8013/v1", 2);
    config.slots.as_mut().unwrap().whole_copy_per_agent = false;
    let registry = Arc::new(FakeSlotRegistry::default());
    let rt = runtime_with(
        &config,
        registry.clone(),
        backend(&Capture::new()),
        backend(&Capture::new()),
    );
    rt.start_lead().await;
    let mut copies = Vec::new();
    for id in ["w", "x", "y", "z"] {
        copies.push(
            rt.allocator
                .try_acquire(&SlotRequest::agent(id, id))
                .await
                .and_then(|l| l.copy())
                .unwrap_or_default(),
        );
    }
    assert_eq!(copies, vec!["B", "A", "B", "A"]);
    assert_eq!(keys(&registry), vec!["A1", "A2", "B1", "B2"]);
    assert!(
        rt.allocator
            .try_acquire(&SlotRequest::agent("fifth", "fifth"))
            .await
            .is_none()
    );
}

/// Answers chat requests with `ok` and records each request's path and body; `GET` (the
/// copy's model list) answers 404, so the configured id is sent.
async fn one_shot_server() -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let s = s.clone();
        async move {
            use axum::response::IntoResponse;
            if req.method() == axum::http::Method::GET {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
            s.lock().unwrap().push((path, serde_json::from_slice(&bytes).unwrap()));
            (
                [("content-type", "application/json")],
                r#"{"id":"x","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/v1"), seen)
}

#[tokio::test]
async fn lead_endpoint_gets_its_model_and_the_same_extra_body_and_sampling() {
    let (url, seen) = one_shot_server().await;
    let config = endpoint_config(&url, 2);
    let factory = SlotClientFactory::new(config, None, None, None);
    let reply = factory
        .for_lead()
        .complete(
            "lead",
            &hi(),
            &ChatOptions {
                temperature: Some(0.6),
                max_output_tokens: Some(1234),
                ..ChatOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(reply.text, "ok");
    let (path, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(body["model"], "qwen-cpu");
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["temperature"], json!(0.6));
    assert_eq!(body["chat_template_kwargs"]["preserve_thinking"], true);
}

#[test]
fn preserve_thinking_is_on_by_default_merges_with_existing_kwargs_and_yields_to_an_explicit_setting()
 {
    let mut slots = endpoint_config("http://x/v1", 2).slots.unwrap();
    assert!(slots.preserve_thinking);
    assert_eq!(
        slots.effective_extra_body().unwrap()["chat_template_kwargs"]["preserve_thinking"],
        true
    );
    slots.extra_body.as_mut().unwrap().insert(
        "chat_template_kwargs".into(),
        json!({"enable_thinking": true}),
    );
    let merged = slots.effective_extra_body().unwrap();
    assert_eq!(merged["reasoning_effort"], "low");
    assert_eq!(merged["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(merged["chat_template_kwargs"]["preserve_thinking"], true);
    assert!(
        slots.extra_body.as_ref().unwrap()["chat_template_kwargs"]
            .get("preserve_thinking")
            .is_none()
    );
    slots.extra_body.as_mut().unwrap().insert(
        "chat_template_kwargs".into(),
        json!({"preserve_thinking": false}),
    );
    assert_eq!(
        slots.effective_extra_body().unwrap()["chat_template_kwargs"]["preserve_thinking"],
        false
    );
    slots.preserve_thinking = false;
    assert_eq!(slots.effective_extra_body(), slots.extra_body);
}

#[test]
fn lead_context_window_only_with_a_lead_endpoint_and_slots_on() {
    let mut config = endpoint_config("http://x/v1", 2);
    let slots = config.slots.as_mut().unwrap();
    assert_eq!(slots.compute_lead_context_window(), None);
    slots.lead.as_mut().unwrap().context_window = Some(65536);
    assert_eq!(slots.compute_lead_context_window(), Some(65536));
    slots.enabled = false;
    assert_eq!(slots.compute_lead_context_window(), None);
}

#[tokio::test]
async fn lead_endpoint_own_extra_body_preserve_thinking_and_sampling_replace_the_slot_wide_ones() {
    let (url, seen) = one_shot_server().await;
    let mut config = endpoint_config(&url, 2);
    {
        let lead = config.slots.as_mut().unwrap().lead.as_mut().unwrap();
        let mut extra = crate::config::model::Map::new();
        extra.insert(
            "chat_template_kwargs".into(),
            json!({"reasoning_effort": "medium"}),
        );
        lead.extra_body = Some(extra);
        lead.preserve_thinking = Some(false);
        lead.sampling = Some(SamplingConfig {
            preset: Some("qwen-thinking-general".into()),
            top_k: Some(40),
            ..SamplingConfig::default()
        });
    }
    let factory = SlotClientFactory::new(config, None, None, None);
    factory
        .for_lead()
        .complete(
            "lead",
            &hi(),
            &ChatOptions {
                temperature: Some(0.6),
                top_p: Some(0.5),
                max_output_tokens: Some(1234),
                ..ChatOptions::default()
            },
        )
        .await
        .unwrap();
    let body = seen.lock().unwrap()[0].1.clone();
    assert!(body.get("reasoning_effort").is_none());
    let kwargs = &body["chat_template_kwargs"];
    assert_eq!(kwargs["reasoning_effort"], "medium");
    assert!(kwargs.get("preserve_thinking").is_none());
    assert_eq!(body["temperature"], json!(1));
    assert_eq!(body["top_p"], json!(0.95));
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["presence_penalty"], json!(1.5));
    assert_eq!(body["min_p"], json!(0));
    assert_eq!(body["max_completion_tokens"], 1234);
}

#[test]
fn lead_settings_fall_back_to_the_slot_wide_ones() {
    let mut slots = endpoint_config("http://x/v1", 2).slots.unwrap();
    assert!(slots.lead_sends_reasoning_back());
    assert_eq!(
        slots.lead_effective_extra_body().unwrap()["reasoning_effort"],
        "low"
    );
    assert_eq!(
        slots.lead_effective_extra_body().unwrap()["chat_template_kwargs"]["preserve_thinking"],
        true
    );
    slots.lead.as_mut().unwrap().preserve_thinking = Some(false);
    assert!(!slots.lead_sends_reasoning_back());
    assert!(slots.sends_reasoning_back());
    assert!(
        !slots
            .lead_effective_extra_body()
            .unwrap()
            .contains_key("chat_template_kwargs")
    );
    let mut extra = crate::config::model::Map::new();
    extra.insert("chat_template_kwargs".into(), json!({"x": 1}));
    slots.lead.as_mut().unwrap().extra_body = Some(extra);
    assert_eq!(
        slots
            .lead_effective_extra_body()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["chat_template_kwargs"]
    );
    assert_eq!(
        slots.effective_extra_body().unwrap()["reasoning_effort"],
        "low"
    );
}

#[test]
fn lead_sampling_resolves_the_preset_and_skips_an_empty_config() {
    assert!(LeadSampling::resolve(None).is_none());
    assert!(LeadSampling::resolve(Some(&SamplingConfig::default())).is_none());
    assert!(
        LeadSampling::resolve(Some(&SamplingConfig {
            preset: Some("no-such-preset".into()),
            ..SamplingConfig::default()
        }))
        .is_none()
    );
    let resolved = LeadSampling::resolve(Some(&SamplingConfig {
        temperature: Some(0.3),
        top_p: Some(0.8),
        ..SamplingConfig::default()
    }))
    .unwrap();
    assert_eq!(resolved.temperature, Some(0.3));
    assert_eq!(resolved.top_p, Some(0.8));
    assert!(resolved.preset.is_none());

    let sampling = LeadSampling::new(
        backend(&Capture::new()),
        SamplingConfig {
            temperature: Some(0.6),
            top_k: Some(7),
            repetition_penalty: Some(1.1),
            ..SamplingConfig::default()
        },
    );
    let mut original = ChatOptions::default();
    original.set("x", json!(1));
    let applied = sampling.apply(&original);
    assert_eq!(applied.temperature, Some(0.6));
    assert_eq!(applied.top_k, Some(7));
    assert_eq!(applied.additional["repeat_penalty"], json!(1.1));
    assert_eq!(applied.additional["x"], json!(1));
    assert!(!original.additional.contains_key("repeat_penalty"));
    assert!(original.top_k.is_none());
}

#[test]
#[should_panic(expected = "slots.lead is not configured")]
fn for_lead_without_a_lead_endpoint_throws() {
    let mut config = endpoint_config("http://x/v1", 2);
    config.slots.as_mut().unwrap().lead = None;
    SlotClientFactory::new(config, None, None, None).for_lead();
}

// ---- CopyModelTests ----

fn copy_model_config(a: &str, b: &str) -> OrcaConfig {
    let mut extra = crate::config::model::Map::new();
    extra.insert("reasoning_effort".into(), json!("low"));
    OrcaConfig {
        slots: Some(SlotsConfig {
            registry_url: Some("http://fake".into()),
            model: Some("qwen3.8-flash-next".into()),
            copies: vec![
                SlotCopyConfig {
                    name: "A".into(),
                    base_url: a.into(),
                    slots: 3,
                    ..SlotCopyConfig::default()
                },
                SlotCopyConfig {
                    name: "B".into(),
                    base_url: b.into(),
                    slots: 2,
                    ..SlotCopyConfig::default()
                },
            ],
            lead_copy: Some("A".into()),
            agents_per_copy: 2,
            retry_seconds: 1,
            extra_body: Some(extra),
            ..SlotsConfig::default()
        }),
        ..OrcaConfig::default()
    }
}

#[tokio::test]
async fn copy_own_model_extra_body_and_sampling_replace_the_slot_wide_ones_other_copy_keeps_them() {
    let (a, seen_a) = one_shot_server().await;
    let (b, seen_b) = one_shot_server().await;
    let mut config = copy_model_config(&a, &b);
    {
        let dense = &mut config.slots.as_mut().unwrap().copies[0];
        dense.model = Some("qwen3.8-27b-pi".into());
        let mut extra = crate::config::model::Map::new();
        extra.insert(
            "chat_template_kwargs".into(),
            json!({"reasoning_effort": "medium"}),
        );
        dense.extra_body = Some(extra);
        dense.sampling = Some(SamplingConfig {
            top_k: Some(20),
            ..SamplingConfig::default()
        });
    }
    let factory = SlotClientFactory::new(config, None, None, None);
    let options = ChatOptions {
        temperature: Some(0.6),
        ..ChatOptions::default()
    };
    factory
        .for_copy("A")
        .complete("x", &hi(), &options)
        .await
        .unwrap();
    factory
        .for_copy("B")
        .complete("x", &hi(), &options)
        .await
        .unwrap();
    let body_a = seen_a.lock().unwrap()[0].1.clone();
    let body_b = seen_b.lock().unwrap()[0].1.clone();

    assert_eq!(body_a["model"], "qwen3.8-27b-pi");
    assert!(body_a.get("reasoning_effort").is_none());
    assert_eq!(body_a["chat_template_kwargs"]["reasoning_effort"], "medium");
    assert_eq!(body_a["chat_template_kwargs"]["preserve_thinking"], true);
    assert_eq!(body_a["top_k"], 20);

    assert_eq!(body_b["model"], "qwen3.8-flash-next");
    assert_eq!(body_b["reasoning_effort"], "low");
    assert_eq!(body_b["chat_template_kwargs"]["preserve_thinking"], true);
    assert!(body_b.get("top_k").is_none());
}

#[test]
fn copy_settings_fall_back_to_the_slot_wide_ones_and_preserve_thinking_can_be_turned_off_per_copy()
{
    let mut slots = copy_model_config("http://a/v1", "http://b/v1")
        .slots
        .unwrap();
    let copy = slots.copies[1].clone();
    assert!(slots.copy_sends_reasoning_back(&copy));
    assert_eq!(
        slots.copy_effective_extra_body(&copy).unwrap()["reasoning_effort"],
        "low"
    );
    slots.copies[1].preserve_thinking = Some(false);
    let copy = slots.copies[1].clone();
    assert!(!slots.copy_sends_reasoning_back(&copy));
    assert_eq!(slots.copy_effective_extra_body(&copy), slots.extra_body);
    assert!(slots.sends_reasoning_back());
}

#[tokio::test]
async fn agents_spread_by_load_starting_off_the_leads_copy() {
    let mut config = copy_model_config("http://a/v1", "http://b/v1");
    config.slots.as_mut().unwrap().whole_copy_per_agent = false;
    let registry = Arc::new(FakeSlotRegistry::new(&["A1", "A2", "A3", "B1", "B2"]));
    let rt = runtime_with(
        &config,
        registry,
        backend(&Capture::new()),
        backend(&Capture::new()),
    );
    rt.start_lead().await;
    assert_eq!(rt.lead_lease.copy().as_deref(), Some("A"));
    let mut copies = Vec::new();
    for id in ["w", "x", "y", "z"] {
        copies.push(
            rt.allocator
                .try_acquire(&SlotRequest::agent(id, id))
                .await
                .and_then(|l| l.copy())
                .unwrap_or_default(),
        );
    }
    assert_eq!(copies, vec!["B", "B", "A", "A"]);
}

#[tokio::test]
async fn lead_model_is_the_model_of_the_leads_copy_and_slots_name_their_copys_model() {
    let mut config = copy_model_config("http://a/v1", "http://b/v1");
    config.slots.as_mut().unwrap().copies[1].model = Some("qwen3.8-27b-coder".into());
    let registry = Arc::new(FakeSlotRegistry::new(&["A1", "A2", "A3", "B1", "B2"]));
    let rt = runtime_with(
        &config,
        registry,
        backend(&Capture::new()),
        backend(&Capture::new()),
    );
    assert!(rt.lead_model().is_none());
    rt.start_lead().await;
    assert_eq!(rt.lead_lease.copy().as_deref(), Some("A"));
    assert_eq!(rt.lead_model().as_deref(), Some("qwen3.8-flash-next"));
    assert_eq!(
        rt.model_for_slot(Some("B2")).as_deref(),
        Some("qwen3.8-27b-coder")
    );
    assert_eq!(
        rt.model_for_slot(Some("B1*")).as_deref(),
        Some("qwen3.8-27b-coder")
    );
    assert_eq!(
        rt.model_for_slot(Some("A3")).as_deref(),
        Some("qwen3.8-flash-next")
    );
    assert!(rt.model_for_slot(None).is_none());
    assert_eq!(
        SlotRuntime::copy_of_slot(Some("AB12")).as_deref(),
        Some("AB")
    );
}

#[tokio::test]
async fn lead_model_on_its_own_endpoint_is_that_endpoints_model() {
    let mut config = copy_model_config("http://a/v1", "http://b/v1");
    config.slots.as_mut().unwrap().lead = Some(SlotLeadConfig {
        name: "CPU".into(),
        base_url: "http://cpu/v1".into(),
        model: Some("qwen3.8-cpu-lead".into()),
        ..SlotLeadConfig::default()
    });
    let registry = Arc::new(FakeSlotRegistry::new(&["A1", "A2", "A3", "B1", "B2"]));
    let rt = runtime_with(
        &config,
        registry,
        backend(&Capture::new()),
        backend(&Capture::new()),
    );
    assert_eq!(rt.lead_location().as_deref(), Some("CPU"));
    assert_eq!(rt.lead_model().as_deref(), Some("qwen3.8-cpu-lead"));
}

// ---- CopyServedModelTests ----

/// A fake ninfer: lists `served`, answers chat for that id only (404 model_not_found otherwise).
#[derive(Default)]
struct Ninfer {
    served: Mutex<String>,
    chat_models: Mutex<Vec<String>>,
    model_lists: AtomicUsize,
    list_down: AtomicBool,
}

async fn fake_ninfer() -> (String, Arc<Ninfer>) {
    let n = Arc::new(Ninfer {
        served: Mutex::new("inkling-2-mini".into()),
        ..Ninfer::default()
    });
    let st = n.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let n = st.clone();
        async move {
            use axum::http::StatusCode;
            use axum::response::IntoResponse;
            let path = req.uri().path().to_string();
            let served = n.served.lock().unwrap().clone();
            if req.method() == axum::http::Method::GET && path == "/v1/models" {
                n.model_lists.fetch_add(1, Ordering::SeqCst);
                if n.list_down.load(Ordering::SeqCst) {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                return (
                    [("content-type", "application/json")],
                    format!(
                        r#"{{"data":[{{"created":1795219262,"id":"{served}","max_model_len":131072,"object":"model","owned_by":"ninfer"}}],"object":"list"}}"#
                    ),
                )
                    .into_response();
            }
            if path == "/v1/chat/completions" {
                let bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let model = body["model"].as_str().unwrap_or_default().to_string();
                n.chat_models.lock().unwrap().push(model.clone());
                if model != served {
                    return (
                        StatusCode::NOT_FOUND,
                        [("content-type", "application/json")],
                        format!(
                            r#"{{"error":{{"code":"model_not_found","message":"model '{model}' not found","param":"model","type":"invalid_request_error"}}}}"#
                        ),
                    )
                        .into_response();
                }
                if body["stream"] == true {
                    let sse = format!(
                        "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{served}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"ok\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{served}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                    );
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }
                return (
                    [("content-type", "application/json")],
                    format!(
                        r#"{{"id":"x","object":"chat.completion","created":0,"model":"{served}","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}}}"#
                    ),
                )
                    .into_response();
            }
            StatusCode::NOT_FOUND.into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/v1"), n)
}

async fn served_wire(model: Option<&str>) -> (SlotClientFactory, Arc<Ninfer>) {
    let (url, n) = fake_ninfer().await;
    let config = OrcaConfig {
        slots: Some(SlotsConfig {
            registry_url: Some("http://fake".into()),
            model: model.map(str::to_string),
            api_key: Some("test-key".into()),
            copies: vec![SlotCopyConfig {
                name: "A".into(),
                base_url: url,
                slots: 2,
                ..SlotCopyConfig::default()
            }],
            ..SlotsConfig::default()
        }),
        ..OrcaConfig::default()
    };
    (SlotClientFactory::new(config, None, None, None), n)
}

fn chat_models(n: &Ninfer) -> Vec<String> {
    n.chat_models.lock().unwrap().clone()
}

#[tokio::test]
async fn a_copy_serving_another_model_gets_the_id_it_serves_not_slots_model() {
    let (factory, copy) = served_wire(Some("qwen3.8-27b")).await;
    let r = factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(chat_models(&copy), vec!["inkling-2-mini"]);
    assert_eq!(copy.model_lists.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.model_for_copy("A").as_deref(),
        Some("inkling-2-mini")
    );
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(copy.model_lists.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_requests_own_model_id_the_gateway_alias_is_replaced_by_the_copys_model() {
    let (factory, copy) = served_wire(Some("qwen3.8-27b")).await;
    let options = ChatOptions {
        model: Some("qwen3.8-27b".into()),
        temperature: Some(0.6),
        ..ChatOptions::default()
    };
    factory
        .for_copy("A")
        .complete("x", &hi(), &options)
        .await
        .unwrap();
    let r = send(factory.for_copy("A").as_ref(), &hi(), &options, true).await;
    assert_eq!(r.text, "ok");
    assert_eq!(chat_models(&copy), vec!["inkling-2-mini", "inkling-2-mini"]);
    assert_eq!(options.model.as_deref(), Some("qwen3.8-27b"));

    *copy.served.lock().unwrap() = "dense-pi".into();
    factory
        .for_copy("A")
        .complete("x", &hi(), &options)
        .await
        .unwrap();
    assert_eq!(
        chat_models(&copy),
        vec![
            "inkling-2-mini",
            "inkling-2-mini",
            "inkling-2-mini",
            "dense-pi"
        ]
    );
}

#[test]
fn choose_takes_the_first_listed_preference_the_configs_before_the_requests() {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    use copy_models::choose;
    assert_eq!(
        choose(Some(&v(&["a", "b", "c"])), &[Some("b"), Some("c")]).as_deref(),
        Some("b")
    );
    assert_eq!(
        choose(Some(&v(&["a", "b", "c"])), &[Some("x"), Some("c")]).as_deref(),
        Some("c")
    );
    assert!(choose(Some(&v(&["a", "b"])), &[Some("x"), Some("y")]).is_none());
    assert_eq!(
        choose(Some(&v(&["only"])), &[Some("x"), None]).as_deref(),
        Some("only")
    );
}

#[tokio::test]
async fn a_copy_listing_the_preferred_model_gets_the_preferred_model() {
    let (factory, copy) = served_wire(Some("INKLING-2-MINI")).await;
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(chat_models(&copy), vec!["inkling-2-mini"]);
}

#[tokio::test]
async fn card_switched_to_another_model_404_model_not_found_is_rediscovered_and_retried_once() {
    let (factory, copy) = served_wire(Some("qwen3.8-27b")).await;
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    *copy.served.lock().unwrap() = "qwen3.8-27b-pi".into();
    let r = factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(
        chat_models(&copy),
        vec!["inkling-2-mini", "inkling-2-mini", "qwen3.8-27b-pi"]
    );
    assert_eq!(copy.model_lists.load(Ordering::SeqCst), 2);
    assert_eq!(
        factory.model_for_copy("A").as_deref(),
        Some("qwen3.8-27b-pi")
    );
}

#[tokio::test]
async fn card_switched_streaming_is_retried_before_anything_streamed() {
    let (factory, copy) = served_wire(Some("qwen3.8-27b")).await;
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    *copy.served.lock().unwrap() = "dense-pi".into();
    let r = send(
        factory.for_copy("A").as_ref(),
        &hi(),
        &ChatOptions::default(),
        true,
    )
    .await;
    assert_eq!(r.text, "ok");
    assert_eq!(
        chat_models(&copy),
        vec!["inkling-2-mini", "inkling-2-mini", "dense-pi"]
    );
}

#[tokio::test]
async fn a_copy_that_does_not_list_its_models_gets_the_configured_id_and_is_asked_again_later() {
    let (factory, copy) = served_wire(Some("inkling-2-mini")).await;
    copy.list_down.store(true, Ordering::SeqCst);
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(chat_models(&copy), vec!["inkling-2-mini"]);
    assert_eq!(copy.model_lists.load(Ordering::SeqCst), 1);
    // Within 30 s the failed lookup is not repeated.
    factory
        .for_copy("A")
        .complete("x", &hi(), &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(copy.model_lists.load(Ordering::SeqCst), 1);
}
