//! Sticky slots for one OpenOrca process (`SlotRuntime`): the allocator, the per-copy clients,
//! the fallback (normal provider) client and the Lead's lease. [`SlotRuntime::lead_client`] is
//! what the app talks to; inside a sub-agent's flow ([`SlotRuntime::with_ambient`]) it routes
//! to that agent's own slot client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::allocator::{LEAD_KIND, SlotAllocator, SlotLease, SlotRequest};
use super::client::{ServerKindProbe, SlotClientFactory, SlottedChatClient, copy_key};
use crate::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, ServerIdentity, ServerKind,
    StreamControl, StreamEvent,
};
use crate::config::{OrcaConfig, SlotLeadConfig};

pub const LEAD_OWNER: &str = "lead";

tokio::task_local! {
    static AMBIENT: Arc<dyn ChatBackend>;
}

type BusyFn = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct SlotRuntime {
    pub allocator: Arc<SlotAllocator>,
    pub clients: Arc<SlotClientFactory>,
    pub fallback: Arc<dyn ChatBackend>,
    server_kind: Option<ServerKindProbe>,
    /// The Lead's lease (its slot may be `None`: none free yet, or copies down).
    pub lead_lease: Arc<SlotLease>,
    /// The Lead's pinned client.
    pub lead_slot_client: Arc<SlottedChatClient>,
    /// The Lead's own endpoint (slots.lead), or `None` when it leases a slot.
    pub lead_endpoint: Option<SlotLeadConfig>,
    lead_target: Arc<dyn ChatBackend>,
    lead_busy: Mutex<Option<BusyFn>>,
    shutdown: AtomicBool,
    idle_check: AtomicBool,
    idle_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SlotRuntime {
    pub fn new(
        allocator: Arc<SlotAllocator>,
        clients: Arc<SlotClientFactory>,
        fallback: Arc<dyn ChatBackend>,
        server_kind: Option<ServerKindProbe>,
    ) -> Arc<Self> {
        let lead_lease = allocator.new_lease(
            SlotRequest::new(LEAD_OWNER, LEAD_KIND, "OpenOrca lead")
                .with_prefer_copy(Some(&allocator.lead_copy())),
        );
        let lead_slot_client = Arc::new(Self::make_client(
            &allocator,
            &clients,
            &fallback,
            &server_kind,
            lead_lease.clone(),
        ));
        let lead_endpoint = allocator
            .config()
            .lead
            .clone()
            .filter(|l| !l.base_url.is_empty());
        let lead_target: Arc<dyn ChatBackend> = if lead_endpoint.is_some() {
            clients.for_lead()
        } else {
            lead_slot_client.clone()
        };
        let rt = Arc::new(Self {
            allocator,
            clients,
            fallback,
            server_kind,
            lead_lease,
            lead_slot_client,
            lead_endpoint,
            lead_target,
            lead_busy: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            idle_check: AtomicBool::new(false),
            idle_timer: Mutex::new(None),
        });
        // The Lead leases lazily (its first request) and gives the slot back when idle.
        if rt.lead_endpoint.is_none()
            && let Some(idle) = rt.idle_release()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let period = (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(60));
            let me: Weak<Self> = Arc::downgrade(&rt);
            *rt.idle_timer.lock().unwrap() = Some(handle.spawn(async move {
                let mut tick =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    tick.tick().await;
                    let Some(rt) = me.upgrade() else { return };
                    rt.release_lead_if_idle(Instant::now()).await;
                }
            }));
        }
        rt
    }

    fn make_client(
        allocator: &Arc<SlotAllocator>,
        clients: &Arc<SlotClientFactory>,
        fallback: &Arc<dyn ChatBackend>,
        server_kind: &Option<ServerKindProbe>,
        lease: Arc<SlotLease>,
    ) -> SlottedChatClient {
        let c = clients.clone();
        SlottedChatClient::new(
            allocator.clone(),
            lease,
            Arc::new(move |copy: &str| c.for_copy(copy)),
            fallback.clone(),
            server_kind.clone(),
        )
    }

    /// Production wiring, or `None` when `slots` is absent or disabled.
    pub fn create(config: &OrcaConfig, fallback: Arc<dyn ChatBackend>) -> Option<Arc<Self>> {
        let allocator = SlotAllocator::create(config)?;
        Some(Self::new(
            allocator,
            Arc::new(SlotClientFactory::new(config.clone(), None, None, None)),
            fallback,
            Some(Self::copy_kind_probe(config, None)),
        ))
    }

    /// Copy name -> its server kind, asked of the copy itself (ServerIdentity, once per copy).
    pub fn copy_kind_probe(
        config: &OrcaConfig,
        identity: Option<ServerIdentity>,
    ) -> ServerKindProbe {
        let identity = identity.unwrap_or_else(ServerIdentity::shared);
        let config = config.clone();
        Arc::new(move |copy: String| {
            let identity = identity.clone();
            let url = config.slots.as_ref().and_then(|s| {
                s.copies
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&copy))
                    .map(|c| c.base_url.clone())
            });
            let key = copy_key(&config);
            Box::pin(async move {
                match url.filter(|u| !u.trim().is_empty()) {
                    Some(url) => identity.get(&url, key.as_deref()).await.kind,
                    None => ServerKind::Unknown,
                }
            })
        })
    }

    /// slots.idleReleaseMinutes; `None` = never release for idleness.
    pub fn idle_release(&self) -> Option<Duration> {
        let m = self.allocator.config().idle_release_minutes;
        (m > 0).then(|| Duration::from_secs(m as u64 * 60))
    }

    /// True while the Lead's turn runs: its slot is not idle then.
    pub fn set_lead_busy(&self, f: Option<impl Fn() -> bool + Send + Sync + 'static>) {
        *self.lead_busy.lock().unwrap() = f.map(|f| Arc::new(f) as BusyFn);
    }

    pub fn lead_request_in_flight(&self) -> bool {
        self.lead_slot_client.in_flight()
    }

    /// Give the Lead's slot back when idle for [`Self::idle_release`] and no turn is running.
    pub async fn release_lead_if_idle(&self, now: Instant) -> bool {
        if self.lead_endpoint.is_some() || self.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let Some(idle) = self.idle_release() else {
            return false;
        };
        if self.idle_check.swap(true, Ordering::SeqCst) {
            return false;
        }
        let busy = self.lead_busy.lock().unwrap().clone();
        let r = if busy.is_some_and(|b| b()) {
            false
        } else {
            self.lead_slot_client.release_if_idle(idle, now).await
        };
        self.idle_check.store(false, Ordering::SeqCst);
        r
    }

    /// Where the Lead runs: the endpoint name ("CPU") or its slot ("A1"; `None` = none yet).
    pub fn lead_location(&self) -> Option<String> {
        self.lead_endpoint
            .as_ref()
            .map(|e| e.name.clone())
            .or_else(|| self.lead_lease.slot())
    }

    /// The model the Lead's requests go to; `None` while it holds no slot.
    pub fn lead_model(&self) -> Option<String> {
        match &self.lead_endpoint {
            Some(e) => e
                .model
                .clone()
                .or_else(|| self.allocator.config().model.clone()),
            None => self.model_for_copy(self.lead_lease.copy().as_deref()),
        }
    }

    pub fn model_for_copy(&self, copy: Option<&str>) -> Option<String> {
        let copy = copy.filter(|c| !c.is_empty())?;
        if let Some(m) = self.clients.model_for_copy(copy) {
            return Some(m);
        }
        let config = self.allocator.config();
        config
            .copies
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(copy))
            .and_then(|c| c.model.clone())
            .or_else(|| config.model.clone())
    }

    /// The model behind a slot name ("A2", or "A2*" when borrowed).
    pub fn model_for_slot(&self, slot: Option<&str>) -> Option<String> {
        self.model_for_copy(Self::copy_of_slot(slot).as_deref())
    }

    /// "A2" / "A2*" -> "A".
    pub fn copy_of_slot(slot: Option<&str>) -> Option<String> {
        let c = slot?
            .trim_end_matches('*')
            .trim_end_matches(|ch: char| ch.is_ascii_digit());
        (!c.is_empty()).then(|| c.to_string())
    }

    /// The app-wide client: the ambient sub-agent client inside [`Self::with_ambient`], else
    /// the Lead's (pinned client, or its own endpoint).
    pub fn lead_client(self: &Arc<Self>) -> Arc<dyn ChatBackend> {
        Arc::new(AmbientClient {
            lead: self.lead_target.clone(),
        })
    }

    /// A pinned client for a holder's lease (a sub-agent's).
    pub fn client_for(&self, lease: Arc<SlotLease>) -> Arc<SlottedChatClient> {
        Arc::new(Self::make_client(
            &self.allocator,
            &self.clients,
            &self.fallback,
            &self.server_kind,
            lease,
        ))
    }

    /// Run `fut` with the app-wide client routed to `client` (a sub-agent's flow).
    pub async fn with_ambient<F: std::future::Future>(
        client: Arc<dyn ChatBackend>,
        fut: F,
    ) -> F::Output {
        AMBIENT.scope(client, fut).await
    }

    /// Take the Lead's slot now; never waits, never fails.
    pub async fn start_lead(&self) {
        if self.lead_endpoint.is_some() {
            return;
        }
        self.allocator.try_upgrade(&self.lead_lease).await;
    }

    /// Release every lease (exit, Ctrl+C). Idempotent; bounded to a couple of seconds.
    pub async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(h) = self.idle_timer.lock().unwrap().take() {
            h.abort();
        }
        self.allocator.release_all(Duration::from_secs(2)).await;
    }
}

impl Drop for SlotRuntime {
    fn drop(&mut self) {
        if let Some(h) = self.idle_timer.lock().unwrap().take() {
            h.abort();
        }
    }
}

struct AmbientClient {
    lead: Arc<dyn ChatBackend>,
}

impl AmbientClient {
    fn target(&self) -> Arc<dyn ChatBackend> {
        AMBIENT
            .try_with(|c| c.clone())
            .unwrap_or_else(|_| self.lead.clone())
    }
}

#[async_trait]
impl ChatBackend for AmbientClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.target().complete(actor, messages, options).await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.target()
            .stream_with_control(actor, messages, options, on_event)
            .await
    }
}
