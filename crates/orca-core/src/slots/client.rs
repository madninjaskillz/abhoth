//! The per-copy chat clients (`SlotClientFactory`), the slot defaults and sampling wrappers
//! (`SlotDefaultsChatClient`, `LeadSamplingChatClient`) and the slot holder's client
//! (`SlottedChatClient`): every request goes to the copy its lease is on, a transport failure
//! fails the holder over to another copy and retries once, and the Lead's requests pin their
//! prefix on Strata (released after the final answer).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::allocator::{LEAD_KIND, SlotAllocator, SlotLease};
use super::copy_models::{CopyModelClient, CopyModels};
use crate::client::request_parameters::thinking_switch_first;
use crate::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, ClientSettings, OpenAiClient,
    RequestStats, ServedModels, StreamControl, StreamEvent, is_transport_failure,
    server_identity::ServerKind, usage::SLOTS_CATEGORY,
};
use crate::config::model::Map;
use crate::config::{OrcaConfig, SamplingConfig, SlotCopyConfig, sampling_presets};
use crate::logging::LogSink;

/// Builds a copy's client (tests replace the HTTP client).
pub type CopyClientOverride = Arc<dyn Fn(&SlotCopyConfig) -> Arc<dyn ChatBackend> + Send + Sync>;

/// Copy name -> its server kind (for the Lead's prefix pin).
pub type ServerKindProbe = Arc<dyn Fn(String) -> BoxFuture<'static, ServerKind> + Send + Sync>;

/// One chat client per model copy, cached: an OpenAI-compatible client at the copy's base URL
/// with the id the copy serves, the copy's extraBody defaults and sampling.
pub struct SlotClientFactory {
    config: OrcaConfig,
    clients: Mutex<HashMap<String, Arc<dyn ChatBackend>>>,
    copy_clients: Mutex<HashMap<String, Arc<CopyModelClient>>>,
    override_: Option<CopyClientOverride>,
    served: Arc<ServedModels>,
    /// What each copy serves (asked of the copy).
    pub models: Arc<CopyModels>,
}

const LEAD_KEY: &str = "(lead endpoint)";

fn env_key(env: Option<&str>) -> Option<String> {
    env.filter(|e| !e.is_empty())
        .and_then(|e| std::env::var(e).ok())
}

impl SlotClientFactory {
    pub fn new(
        config: OrcaConfig,
        override_: Option<CopyClientOverride>,
        models: Option<Arc<CopyModels>>,
        served: Option<Arc<ServedModels>>,
    ) -> Self {
        Self {
            config,
            clients: Mutex::new(HashMap::new()),
            copy_clients: Mutex::new(HashMap::new()),
            override_,
            served: served.unwrap_or_else(|| Arc::new(ServedModels::new())),
            models: models.unwrap_or_default(),
        }
    }

    /// The model id requests to a copy use: what the copy serves, when known, else copy.model /
    /// slots.model. `None` for an unknown copy.
    pub fn model_for_copy(&self, copy_name: &str) -> Option<String> {
        let slots = self.config.slots.as_ref()?;
        let copy = slots
            .copies
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(copy_name))?;
        let preferred = copy.model.clone().or_else(|| slots.model.clone());
        self.models
            .known(&copy.name, preferred.as_deref())
            .or(preferred)
    }

    pub fn for_copy(&self, copy: &str) -> Arc<dyn ChatBackend> {
        let key = copy.to_lowercase();
        if let Some(c) = self.clients.lock().unwrap().get(&key) {
            return c.clone();
        }
        let c = self.create(copy);
        self.clients.lock().unwrap().entry(key).or_insert(c).clone()
    }

    /// The Lead's own endpoint (slots.lead). Panics when none is configured (as C# throws).
    pub fn for_lead(&self) -> Arc<dyn ChatBackend> {
        if let Some(c) = self.clients.lock().unwrap().get(LEAD_KEY) {
            return c.clone();
        }
        let slots = self
            .config
            .slots
            .as_ref()
            .expect("slots are not configured");
        let lead = slots.lead.as_ref().expect("slots.lead is not configured");
        let endpoint = SlotCopyConfig {
            name: lead.name.clone(),
            base_url: lead.base_url.clone(),
            slots: 1,
            ..SlotCopyConfig::default()
        };
        let c = if let Some(o) = &self.override_ {
            o(&endpoint)
        } else {
            let key = env_key(lead.api_key_env.as_deref()).or_else(|| lead.api_key.clone());
            let client = self.build(
                &endpoint.base_url,
                lead.model.clone().or_else(|| slots.model.clone()),
                key,
                slots.lead_sends_reasoning_back(),
                slots.lead_effective_extra_body(),
                false,
            );
            match LeadSampling::resolve(lead.sampling.as_ref()) {
                Some(s) => Arc::new(LeadSampling::new(client, s)) as Arc<dyn ChatBackend>,
                None => client,
            }
        };
        self.clients
            .lock()
            .unwrap()
            .entry(LEAD_KEY.into())
            .or_insert(c)
            .clone()
    }

    fn create(&self, copy_name: &str) -> Arc<dyn ChatBackend> {
        let slots = self
            .config
            .slots
            .as_ref()
            .expect("slots are not configured");
        let copy = slots
            .copies
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(copy_name))
            .unwrap_or_else(|| panic!("unknown slot copy '{copy_name}'"))
            .clone();
        if let Some(o) = &self.override_ {
            return o(&copy);
        }
        let preferred = copy
            .model
            .clone()
            .or_else(|| slots.model.clone())
            .or_else(|| self.config.lm_studio.primary_model().map(str::to_string))
            .unwrap_or_else(|| "default".into());
        let echo = slots.copy_sends_reasoning_back(&copy);
        let extra = slots.copy_effective_extra_body(&copy);
        let me = FactoryParts {
            config: self.config.clone(),
            served: self.served.clone(),
            key: self.copy_key(),
        };
        let base_url = copy.base_url.clone();
        let client = Arc::new(CopyModelClient::new(
            &copy.name,
            &copy.base_url,
            self.copy_key(),
            &preferred,
            self.models.clone(),
            Arc::new(move |model: &str| {
                me.build(
                    &base_url,
                    Some(model.to_string()),
                    None,
                    echo,
                    extra.clone(),
                    true,
                )
            }),
        ));
        self.copy_clients
            .lock()
            .unwrap()
            .insert(copy.name.to_lowercase(), client.clone());
        match LeadSampling::resolve(copy.sampling.as_ref()) {
            Some(s) => Arc::new(LeadSampling::new(client, s)),
            None => client,
        }
    }

    /// The copies' key: slots.apiKeyEnv, slots.apiKey, then lmStudio.apiKey.
    fn copy_key(&self) -> Option<String> {
        copy_key(&self.config)
    }

    fn build(
        &self,
        base_url: &str,
        model: Option<String>,
        key: Option<String>,
        echo: bool,
        extra: Option<Map<Value>>,
        direct: bool,
    ) -> Arc<dyn ChatBackend> {
        FactoryParts {
            config: self.config.clone(),
            served: self.served.clone(),
            key: self.copy_key(),
        }
        .build(base_url, model, key, echo, extra, direct)
    }
}

/// The copies' key: slots.apiKeyEnv, slots.apiKey, then lmStudio.apiKey.
pub fn copy_key(config: &OrcaConfig) -> Option<String> {
    let slots = config.slots.as_ref()?;
    env_key(slots.api_key_env.as_deref())
        .or_else(|| slots.api_key.clone())
        .or_else(|| Some(config.lm_studio.api_key.clone()))
}

struct FactoryParts {
    config: OrcaConfig,
    served: Arc<ServedModels>,
    key: Option<String>,
}

impl FactoryParts {
    fn build(
        &self,
        base_url: &str,
        model: Option<String>,
        key: Option<String>,
        echo: bool,
        extra: Option<Map<Value>>,
        direct: bool,
    ) -> Arc<dyn ChatBackend> {
        let model = model
            .or_else(|| self.config.lm_studio.primary_model().map(str::to_string))
            .unwrap_or_else(|| "default".into());
        let key = key
            .or_else(|| self.key.clone())
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| "not-needed".into());
        let lm = &self.config.lm_studio;
        let mut settings = ClientSettings::new(base_url, key.clone(), Some(model));
        settings.request_timeout = Duration::from_secs(lm.timeout_seconds.max(1) as u64);
        settings.idle_timeout = Duration::from_secs(lm.streaming_timeout_seconds.max(1) as u64);
        settings.echo_reasoning = echo;
        settings.direct = direct;
        settings.session_field = self.config.agent.uncensored.field();
        settings.uncensored_gate =
            Some(crate::client::uncensored_gate::UncensoredGate::for_config(
                &self.config,
                base_url,
                Some(key.clone()),
            ));
        settings.vision = lm.vision;
        let inner: Arc<dyn ChatBackend> =
            Arc::new(OpenAiClient::new(settings, self.served.clone()));
        Arc::new(SlotDefaults::new(inner, extra))
    }
}

/// slots.*.sampling: the endpoint's own sampler values replace the request's.
pub struct LeadSampling {
    inner: Arc<dyn ChatBackend>,
    sampling: SamplingConfig,
}

impl LeadSampling {
    pub fn new(inner: Arc<dyn ChatBackend>, sampling: SamplingConfig) -> Self {
        Self { inner, sampling }
    }

    /// The config with its preset filling unset fields; `None` when it would change nothing.
    pub fn resolve(config: Option<&SamplingConfig>) -> Option<SamplingConfig> {
        let config = config?;
        let mut resolved = match sampling_presets::get(config.preset.as_deref()) {
            Some(preset) => preset.merged_with(Some(config)),
            None => config.clone(),
        };
        resolved.preset = None;
        (resolved != SamplingConfig::default()).then_some(resolved)
    }

    pub fn apply(&self, options: &ChatOptions) -> ChatOptions {
        let mut o = options.clone();
        let s = &self.sampling;
        if let Some(t) = s.temperature {
            o.temperature = Some(t);
        }
        if let Some(p) = s.top_p {
            o.top_p = Some(p);
        }
        if let Some(k) = s.top_k {
            o.top_k = Some(k);
        }
        if let Some(p) = s.presence_penalty {
            o.presence_penalty = Some(p);
        }
        if let Some(m) = s.min_p {
            o.set("min_p", crate::client::f32_json(m));
        }
        if let Some(r) = s.repetition_penalty {
            o.set("repeat_penalty", crate::client::f32_json(r));
            o.set("repetition_penalty", crate::client::f32_json(r));
        }
        o
    }
}

#[async_trait]
impl ChatBackend for LeadSampling {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .complete(actor, messages, &self.apply(options))
            .await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .stream_with_control(actor, messages, &self.apply(options), on_event)
            .await
    }
}

/// Adds the slot extraBody fields a request doesn't set itself (`chat_template_kwargs` is
/// merged, the request's keys winning, `enable_thinking` first).
pub struct SlotDefaults {
    inner: Arc<dyn ChatBackend>,
    defaults: Option<Map<Value>>,
}

const TEMPLATE_KWARGS: &str = "chat_template_kwargs";

impl SlotDefaults {
    pub fn new(inner: Arc<dyn ChatBackend>, defaults: Option<Map<Value>>) -> Self {
        Self { inner, defaults }
    }

    pub fn with_defaults(&self, options: &ChatOptions) -> ChatOptions {
        let mut o = options.clone();
        let Some(defaults) = self.defaults.as_ref().filter(|d| !d.is_empty()) else {
            return o;
        };
        for (k, v) in defaults {
            match o.additional.get(k) {
                None => {
                    o.additional.insert(k.clone(), v.clone());
                }
                Some(request) if k == TEMPLATE_KWARGS => {
                    if let Some(merged) = merge_kwargs(v, request) {
                        o.additional.insert(k.clone(), merged);
                    }
                }
                Some(_) => {}
            }
        }
        o
    }
}

/// Both sides' kwargs in one object, the request winning per key; `None` unless both are
/// objects.
pub fn merge_kwargs(defaults: &Value, request: &Value) -> Option<Value> {
    let (Value::Object(d), Value::Object(r)) = (defaults, request) else {
        return None;
    };
    let mut values: IndexMap<String, Value> =
        d.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in r {
        values.insert(k.clone(), v.clone());
    }
    Some(thinking_switch_first(&values))
}

#[async_trait]
impl ChatBackend for SlotDefaults {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .complete(actor, messages, &self.with_defaults(options))
            .await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.inner
            .stream_with_control(actor, messages, &self.with_defaults(options), on_event)
            .await
    }
}

/// Copy name -> its chat client.
pub type CopyClientFn = Arc<dyn Fn(&str) -> Arc<dyn ChatBackend> + Send + Sync>;

pub const PREFIX_KEY: &str = "strata_prefix";
/// The release request's time limit on a server known to be Strata.
pub const RELEASE_TIMEOUT_STRATA: Duration = Duration::from_secs(15);
/// The same when the server could not be identified.
pub const RELEASE_TIMEOUT_UNKNOWN: Duration = Duration::from_secs(2);

/// The chat client of one slot holder (the Lead or a sub-agent). See the module docs.
pub struct SlottedChatClient {
    allocator: Arc<SlotAllocator>,
    lease: Arc<SlotLease>,
    copy_client: CopyClientFn,
    fallback: Arc<dyn ChatBackend>,
    server_kind: Option<ServerKindProbe>,
    pin_skip_logged: Mutex<Option<String>>,
    next_upgrade: Mutex<Option<Instant>>,
    in_flight: AtomicUsize,
    last_activity: Mutex<Option<Instant>>,
    prompt_tokens: AtomicI64,
    cached_tokens: AtomicI64,
    /// Where the "Slot usage" lines of requests on a slot go (the Slots logger).
    usage_log: Option<Arc<dyn LogSink>>,
}

impl SlottedChatClient {
    pub fn new(
        allocator: Arc<SlotAllocator>,
        lease: Arc<SlotLease>,
        copy_client: CopyClientFn,
        fallback: Arc<dyn ChatBackend>,
        server_kind: Option<ServerKindProbe>,
    ) -> Self {
        Self {
            allocator,
            lease,
            copy_client,
            fallback,
            server_kind,
            pin_skip_logged: Mutex::new(None),
            next_upgrade: Mutex::new(None),
            in_flight: AtomicUsize::new(0),
            last_activity: Mutex::new(None),
            prompt_tokens: AtomicI64::new(0),
            cached_tokens: AtomicI64::new(0),
            usage_log: None,
        }
    }

    /// Log each request on a slot ("Slot usage {owner} on {slot}: ...") to `log`.
    pub fn with_usage_log(mut self, log: Option<Arc<dyn LogSink>>) -> Self {
        self.usage_log = log;
        self
    }

    pub fn lease(&self) -> &Arc<SlotLease> {
        &self.lease
    }

    pub fn fallback(&self) -> &Arc<dyn ChatBackend> {
        &self.fallback
    }

    /// Prompt tokens sent through this holder.
    pub fn prompt_tokens(&self) -> i64 {
        self.prompt_tokens.load(Ordering::SeqCst)
    }

    /// How many of them the server had cached.
    pub fn cached_tokens(&self) -> i64 {
        self.cached_tokens.load(Ordering::SeqCst)
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    pub fn last_activity(&self) -> Option<Instant> {
        *self.last_activity.lock().unwrap()
    }

    fn begin(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        *self.last_activity.lock().unwrap() = Some(Instant::now());
        InFlight(self)
    }

    /// Give the slot back when no request has run for `idle` (as of `now`); the next request
    /// leases again. True when it released one.
    pub async fn release_if_idle(&self, idle: Duration, now: Instant) -> bool {
        if self.lease.slot().is_none() || self.lease.released() || self.in_flight() {
            return false;
        }
        let last = {
            let mut last = self.last_activity.lock().unwrap();
            match *last {
                Some(l) => l,
                None => {
                    // Leased without a request through this client: idle from now.
                    *last = Some(now);
                    return false;
                }
            }
        };
        let since = now.saturating_duration_since(last);
        if since < idle {
            return false;
        }
        let slot = self.lease.slot();
        if !self.allocator.park(&self.lease).await {
            return false;
        }
        *self.next_upgrade.lock().unwrap() = None;
        tracing::info!(
            "Slot {} of {} released after {:.1} min idle",
            slot.unwrap_or_default(),
            self.lease.owner(),
            since.as_secs_f64() / 60.0
        );
        true
    }

    /// For tests: set when the last request ran.
    pub fn set_last_activity(&self, at: Instant) {
        *self.last_activity.lock().unwrap() = Some(at);
    }

    fn is_lead(&self) -> bool {
        self.allocator.config().pin_lead_prefix && self.lease.request().kind == LEAD_KIND
    }

    /// The Lead's requests carry `"strata_prefix": {"messages": n - 1}` (see C#).
    pub fn with_lead_pin(&self, messages: &[ChatMessage], options: &ChatOptions) -> ChatOptions {
        if !self.is_lead() {
            return options.clone();
        }
        let wire = wire_message_count(messages);
        if wire < 2 || options.additional.contains_key(PREFIX_KEY) {
            return options.clone();
        }
        let mut o = options.clone();
        o.set(PREFIX_KEY, json!({"messages": wire - 1}));
        o
    }

    async fn pin_server_kind(&self, slot: Option<&str>) -> Option<ServerKind> {
        slot?;
        if !self.is_lead() {
            return None;
        }
        let (Some(probe), Some(copy)) = (&self.server_kind, self.lease.copy()) else {
            return Some(ServerKind::Strata);
        };
        let kind = probe(copy.clone()).await;
        let mut logged = self.pin_skip_logged.lock().unwrap();
        if logged.as_deref() != Some(copy.as_str()) {
            *logged = Some(copy.clone());
            if matches!(kind, ServerKind::Ninfer | ServerKind::Other) {
                tracing::info!(
                    "Copy {copy} is not a Strata server ({kind:?}): the Lead's prefix pin and its release are not sent"
                );
            } else {
                tracing::debug!(
                    "Copy {copy} server kind: {kind:?}; the Lead's prefix pin and its release are sent"
                );
            }
        }
        Some(kind)
    }

    fn pins(kind: Option<ServerKind>) -> bool {
        matches!(kind, Some(ServerKind::Strata | ServerKind::Unknown))
    }

    fn release_timeout(kind: Option<ServerKind>) -> Duration {
        if kind == Some(ServerKind::Unknown) {
            RELEASE_TIMEOUT_UNKNOWN
        } else {
            RELEASE_TIMEOUT_STRATA
        }
    }

    /// After the Lead's final answer: the same prompt with `"strata_prefix": {"tokens": 1}` and
    /// one output token, so the pin is cleared. Failures are ignored.
    pub async fn release_lead_pin(
        &self,
        client: &Arc<dyn ChatBackend>,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        timeout: Duration,
    ) {
        if wire_message_count(messages) < 2 {
            return;
        }
        let mut o = options.clone();
        o.set(PREFIX_KEY, json!({"tokens": 1}));
        o.max_output_tokens = Some(1);
        match tokio::time::timeout(timeout, client.complete(actor, messages, &o)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::debug!("Releasing the Lead's prefix pin failed: {e}"),
            Err(_) => tracing::debug!("Releasing the Lead's prefix pin timed out"),
        }
    }

    async fn current(&self) -> (Arc<dyn ChatBackend>, Option<String>) {
        if self.lease.slot().is_none() && !self.lease.released() {
            let due = {
                let mut next = self.next_upgrade.lock().unwrap();
                let now = Instant::now();
                if next.is_none_or(|n| now >= n) {
                    let wait = (self.allocator.config().retry_seconds * 5).max(5) as u64;
                    *next = Some(now + Duration::from_secs(wait));
                    true
                } else {
                    false
                }
            };
            if due {
                self.allocator.try_upgrade(&self.lease).await;
            }
        }
        let slot = self.lease.slot();
        match self.lease.copy() {
            Some(copy) => ((self.copy_client)(&copy), slot),
            None => (self.fallback.clone(), None),
        }
    }

    fn record(&self, slot: Option<&str>, stats: &RequestStats) {
        if let Some(usage) = stats.usage {
            self.prompt_tokens
                .fetch_add(usage.prompt_tokens.unwrap_or(0), Ordering::SeqCst);
            self.cached_tokens
                .fetch_add(usage.cached_tokens.unwrap_or(0), Ordering::SeqCst);
        }
        // Requests on the fallback are logged by the fallback client itself (when it logs usage).
        if let Some(slot) = slot {
            stats.log(
                self.usage_log.as_deref(),
                SLOTS_CATEGORY,
                &format!("{} on {slot}", self.lease.owner()),
            );
        }
    }

    async fn complete_core(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let (client, slot) = self.current().await;
        let kind = self.pin_server_kind(slot.as_deref()).await;
        let sent = if Self::pins(kind) {
            self.with_lead_pin(messages, options)
        } else {
            options.clone()
        };
        let mut stats = RequestStats::start(Some(options), None, None, None);
        match client.complete(actor, messages, &sent).await {
            Ok(r) => {
                stats.observe_response(&r);
                self.record(slot.as_deref(), &stats);
                if Self::pins(kind) && self.is_lead() && r.tool_calls.is_empty() {
                    self.release_lead_pin(
                        &client,
                        actor,
                        messages,
                        options,
                        Self::release_timeout(kind),
                    )
                    .await;
                }
                Ok(r)
            }
            Err(e) if slot.is_some() && is_transport_failure(&e) => {
                self.allocator.failover(&self.lease).await;
                let (client, slot) = self.current().await;
                let kind = self.pin_server_kind(slot.as_deref()).await;
                let sent = if Self::pins(kind) {
                    self.with_lead_pin(messages, options)
                } else {
                    options.clone()
                };
                let mut stats = RequestStats::start(Some(options), None, None, None);
                let r = client.complete(actor, messages, &sent).await?;
                stats.observe_response(&r);
                self.record(slot.as_deref(), &stats);
                Ok(r)
            }
            Err(e) => Err(e),
        }
    }

    async fn stream_core(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let mut attempt = 0;
        loop {
            let (client, slot) = self.current().await;
            let kind = self.pin_server_kind(slot.as_deref()).await;
            let sent = if Self::pins(kind) {
                self.with_lead_pin(messages, options)
            } else {
                options.clone()
            };
            let mut started = false;
            // Logged when the attempt ends, also when the caller drops the request (cancel).
            let mut log = SlotStreamLog {
                client: self,
                slot: slot.clone(),
                stats: Some(RequestStats::start(Some(options), None, None, None)),
            };
            let result = {
                let stats = log.stats.as_mut().expect("armed");
                let mut tracked = |e: StreamEvent| {
                    started = true;
                    stats.observe_event(&e);
                    on_event(e)
                };
                client
                    .stream_with_control(actor, messages, &sent, &mut tracked)
                    .await
            };
            if let Some(stats) = log.stats.as_mut() {
                match &result {
                    Ok(r) => stats.observe_stream_end(r),
                    Err(ChatError::Interrupted { partial, .. }) => {
                        stats.observe_stream_end(partial)
                    }
                    Err(_) => {}
                }
            }
            let fails_over = matches!(&result, Err(e)
                if !started && attempt == 0 && slot.is_some() && is_transport_failure(e));
            if fails_over {
                log.stats = None;
            }
            drop(log);
            match result {
                Ok(r) => {
                    if !r.stopped && r.tool_calls.is_empty() && Self::pins(kind) && self.is_lead() {
                        self.release_lead_pin(
                            &client,
                            actor,
                            messages,
                            options,
                            Self::release_timeout(kind),
                        )
                        .await;
                    }
                    return Ok(r);
                }
                Err(e)
                    if !started && attempt == 0 && slot.is_some() && is_transport_failure(&e) =>
                {
                    attempt += 1;
                    self.allocator.failover(&self.lease).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Records a streamed attempt's stats on drop (C#: Record in the stream's finally).
struct SlotStreamLog<'a> {
    client: &'a SlottedChatClient,
    slot: Option<String>,
    stats: Option<RequestStats>,
}

impl Drop for SlotStreamLog<'_> {
    fn drop(&mut self) {
        if let Some(stats) = self.stats.take() {
            self.client.record(self.slot.as_deref(), &stats);
        }
    }
}

/// Ends a request on drop (also when the caller drops the future).
struct InFlight<'a>(&'a SlottedChatClient);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        *self.0.last_activity.lock().unwrap() = Some(Instant::now());
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Messages as an OpenAI-compatible request sends them (each tool result is its own message).
pub fn wire_message_count(messages: &[ChatMessage]) -> usize {
    messages.len()
}

#[async_trait]
impl ChatBackend for SlottedChatClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let _busy = self.begin();
        self.complete_core(actor, messages, options).await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let _busy = self.begin();
        self.stream_core(actor, messages, options, on_event).await
    }
}
