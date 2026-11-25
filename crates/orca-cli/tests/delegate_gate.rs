//! Port of `DelegateGateTests`: the free-copy gate never asks for more copies than can ever be
//! free for helpers (agent.minFreeCopiesToDelegate clamped to all copies less the Lead's), for
//! auto mode's delegate choice and for peer mode's hand-off.

use std::sync::Arc;

use orca_cli::pi_agents::{self, FreeCopiesFn};
use orca_core::client::{ChatError, ChatMessage, ChatOptions, ChatResponse};
use orca_core::config::{OrcaConfig, SlotCopyConfig, SlotLeadConfig, SlotsConfig, delegate_gate};
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{AgentChatClient, AgentOrchestrator, DefaultClientProvider};
use orca_core::slots::testing::{FakeSlotHealth, FakeSlotRegistry};
use orca_core::slots::{
    AGENT_KIND, AllocatorOptions, LEAD_KIND, SlotAllocator, SlotClientFactory, SlotRequest,
    SlotRuntime,
};
use orca_tools_pi::lead_mode;

fn config(copies: usize, peer: bool, min_free: Option<i32>) -> OrcaConfig {
    let mut config = OrcaConfig {
        tool_profile: "pi".into(),
        ..OrcaConfig::default()
    };
    config.agent.lead_mode = "auto".into();
    config.agent.peer_mode = peer;
    if let Some(m) = min_free {
        config.agent.min_free_copies_to_delegate = m;
    }
    config.slots = Some(SlotsConfig {
        enabled: true,
        registry_url: Some("http://fake".into()),
        lead_copy: Some("A".into()),
        whole_copy_per_agent: true,
        copies: ["A", "B", "C"][..copies]
            .iter()
            .map(|c| SlotCopyConfig {
                name: c.to_string(),
                base_url: format!("http://{c}/v1"),
                slots: 2,
                ..SlotCopyConfig::default()
            })
            .collect(),
        ..SlotsConfig::default()
    });
    config
}

fn slot_names(config: &OrcaConfig) -> Vec<String> {
    config
        .slots
        .as_ref()
        .unwrap()
        .copies
        .iter()
        .flat_map(|c| [format!("{}1", c.name), format!("{}2", c.name)])
        .collect()
}

fn new_allocator(config: &OrcaConfig, share_lead_copy: bool) -> Arc<SlotAllocator> {
    let names = slot_names(config);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    SlotAllocator::new(
        config.slots.clone().unwrap(),
        Some(Arc::new(FakeSlotRegistry::new(&names))),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(FakeSlotHealth::default()),
        AllocatorOptions {
            owner: Some("t@box".into()),
            host: Some("box".into()),
            pid: Some(1),
            agents_may_share_lead_copy: share_lead_copy,
            ..AllocatorOptions::default()
        },
    )
}

/// An allocator over `config` with the Lead holding a slot on A and, optionally, a helper on B.
async fn allocator(config: &OrcaConfig, helper_on_b: bool) -> Arc<SlotAllocator> {
    let allocator = new_allocator(config, false);
    let lead = allocator
        .try_acquire(&SlotRequest::new("lead", LEAD_KIND, "Lead").with_prefer_copy(Some("A")))
        .await
        .unwrap();
    assert_eq!(lead.copy().as_deref(), Some("A"));
    if helper_on_b {
        let w = allocator
            .try_acquire(&SlotRequest::new("w1", AGENT_KIND, "worker").with_prefer_copy(Some("B")))
            .await
            .unwrap();
        assert_eq!(w.copy().as_deref(), Some("B"));
    }
    allocator
}

struct NoModel;

#[async_trait::async_trait]
impl AgentChatClient for NoModel {
    async fn complete(
        &self,
        _: &str,
        _: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(ChatResponse::default())
    }

    async fn stream(
        &self,
        _: &str,
        _: &[ChatMessage],
        _: &ChatOptions,
        _: &mut (
                 dyn FnMut(orca_core::client::StreamEvent) -> orca_core::client::StreamControl
                     + Send
             ),
    ) -> Result<ChatResponse, ChatError> {
        Ok(ChatResponse::default())
    }
}

fn orchestrator(config: &OrcaConfig) -> Arc<AgentOrchestrator> {
    AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(Arc::new(NoModel))),
        config.clone(),
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    )
}

fn fixed_free(free: &[&str]) -> FreeCopiesFn {
    let free: Vec<String> = free.iter().map(|s| s.to_string()).collect();
    Arc::new(move || {
        let free = free.clone();
        Box::pin(async move { Some(free) })
    })
}

fn allocator_free(allocator: Arc<SlotAllocator>) -> FreeCopiesFn {
    Arc::new(move || {
        let a = allocator.clone();
        Box::pin(async move { Some(a.free_whole_copies().await) })
    })
}

/// Choose `delegate`; returns the result text and the recorded mode.
async fn choose_delegate(config: &OrcaConfig, free: FreeCopiesFn) -> (String, Option<String>) {
    let o = orchestrator(config);
    let choose = pi_agents::lead_mode_chooser(config.clone(), Some(o.clone()), Some(free));
    let text = choose(lead_mode::DELEGATE.into(), Some("five modules".into())).await;
    (text, o.lead_request_mode())
}

#[test]
fn copies_needed_is_clamped_to_the_copies_helpers_can_ever_have() {
    for (copies, min_free, needed) in [
        (2, 2, 1), // two copies, Lead on A: only B can ever be free
        (3, 2, 2), // three copies: 2 stands
        (3, 5, 2),
        (2, 1, 1),
        (2, 0, 0), // 0 = no free-copy requirement
        (3, -1, 0),
        (1, 2, 1),
    ] {
        assert_eq!(
            delegate_gate::copies_needed(&config(copies, false, Some(min_free))),
            needed,
            "{copies} copies, min {min_free}"
        );
    }
}

#[test]
fn copies_needed_with_the_lead_on_its_own_endpoint_counts_every_copy() {
    let mut config = config(2, false, Some(2));
    config.slots.as_mut().unwrap().lead = Some(SlotLeadConfig {
        base_url: "http://cpu/v1".into(),
        ..SlotLeadConfig::default()
    });
    assert_eq!(delegate_gate::copies_needed(&config), 2);
}

#[test]
fn default_is_zero() {
    assert_eq!(OrcaConfig::default().agent.min_free_copies_to_delegate, 0);
}

#[test]
fn default_config_has_no_free_copy_requirement_and_shares_the_leads_copy() {
    let config = config(2, false, None);
    assert_eq!(delegate_gate::copies_needed(&config), 0);
    assert!(delegate_gate::shares_lead_copy(&config));
    assert!(config.slots.unwrap().whole_copy_per_agent); // still one helper per copy
}

#[tokio::test]
async fn auto_two_copies_lead_on_a_gate_two_delegates_when_b_is_free() {
    for (helper_on_b, delegates) in [(false, true), (true, false)] {
        let config = config(2, false, Some(2));
        let allocator = allocator(&config, helper_on_b).await;
        let (text, mode) = choose_delegate(&config, allocator_free(allocator)).await;
        let expected = if delegates {
            lead_mode::DELEGATE
        } else {
            lead_mode::HANDS_ON
        };
        assert_eq!(mode.as_deref(), Some(expected));
        if delegates {
            assert!(
                text.contains("at most 1 piece(s) per subagent call"),
                "{text}"
            );
        } else {
            assert!(text.starts_with(pi_agents::NO_FREE_COPY_NOTE), "{text}");
        }
    }
}

#[tokio::test]
async fn auto_three_copies_gate_two_needs_two() {
    for (free, delegates) in [(&["B"][..], false), (&["B", "C"][..], true)] {
        let config = config(3, false, Some(2));
        let (text, mode) = choose_delegate(&config, fixed_free(free)).await;
        let expected = if delegates {
            lead_mode::DELEGATE
        } else {
            lead_mode::HANDS_ON
        };
        assert_eq!(mode.as_deref(), Some(expected));
        if !delegates {
            assert!(
                text.starts_with(&pi_agents::too_few_free_copies_note(1, 2)),
                "{text}"
            );
        }
    }
}

fn runtime(config: &OrcaConfig, allocator: Arc<SlotAllocator>) -> Arc<SlotRuntime> {
    let fallback: Arc<dyn orca_core::client::ChatBackend> = Arc::new(
        orca_core::orchestration::slot_provider::AgentClientBackend(Arc::new(NoModel)),
    );
    let f = fallback.clone();
    let factory = SlotClientFactory::new(
        config.clone(),
        Some(Arc::new(move |_| f.clone())),
        None,
        None,
    );
    SlotRuntime::new(allocator, Arc::new(factory), fallback, None)
}

#[tokio::test]
async fn peer_two_copies_lead_on_a_gate_two_hands_off_when_b_is_free() {
    for (helper_on_b, blocked) in [(false, false), (true, true)] {
        let config = config(2, true, Some(2));
        let rt = runtime(&config, allocator(&config, helper_on_b).await);
        let refusal = pi_agents::peer_hand_off_block_for(&config, Some(&rt)).await;
        assert_eq!(refusal.is_some(), blocked);
        if blocked {
            assert!(refusal.unwrap().starts_with(
                "No helper can start now: only 0 model copy/copies are free for helpers and a hand-off needs 1"
            ));
        }
    }
}

#[tokio::test]
async fn auto_gate_zero_delegates_with_no_free_copy() {
    let config = config(2, false, Some(0));
    let (text, mode) = choose_delegate(&config, fixed_free(&[])).await;
    assert_eq!(mode.as_deref(), Some(lead_mode::DELEGATE));
    assert!(
        text.contains(
            "1 worker(s) can run at once: 0 on free model copies and 1 sharing your copy"
        ),
        "{text}"
    );
}

#[tokio::test]
async fn auto_gate_zero_counts_the_leads_copy_in_the_pieces_per_call() {
    let config = config(2, false, Some(0));
    let (text, _) = choose_delegate(&config, fixed_free(&["B"])).await;
    assert!(
        text.contains("at most 2 piece(s) per subagent call"),
        "{text}"
    );
}

#[tokio::test]
async fn gate_zero_lets_a_helper_onto_the_leads_copy_after_the_other_copies() {
    for (min_free, second_copy) in [(0, Some("A")), (1, None)] {
        let config = config(2, false, Some(min_free));
        let allocator = new_allocator(&config, delegate_gate::shares_lead_copy(&config));
        allocator
            .try_acquire(&SlotRequest::new("lead", LEAD_KIND, "Lead").with_prefer_copy(Some("A")))
            .await;
        let first = allocator
            .try_acquire(&SlotRequest::new("w1", AGENT_KIND, "worker 1"))
            .await;
        let second = allocator
            .try_acquire(&SlotRequest::new("w2", AGENT_KIND, "worker 2"))
            .await;
        let third = allocator
            .try_acquire(&SlotRequest::new("w3", AGENT_KIND, "worker 3"))
            .await;
        assert_eq!(first.unwrap().copy().as_deref(), Some("B"));
        assert_eq!(second.and_then(|l| l.copy()).as_deref(), second_copy);
        assert!(third.is_none()); // still one of our agents per copy (slots.wholeCopyPerAgent)
    }
}

#[test]
fn slot_allocator_create_sets_lead_copy_sharing_from_the_gate() {
    let mut config = config(2, false, Some(0));
    {
        let slots = config.slots.as_mut().unwrap();
        slots.registry_url = None;
        slots.local_state_file = Some(
            std::env::temp_dir()
                .join(format!("gate-{}.json", std::process::id()))
                .display()
                .to_string(),
        );
    }
    assert!(
        SlotAllocator::create(&config)
            .unwrap()
            .agents_may_share_lead_copy()
    );
    config.agent.min_free_copies_to_delegate = 2;
    assert!(
        !SlotAllocator::create(&config)
            .unwrap()
            .agents_may_share_lead_copy()
    );
}

#[tokio::test]
async fn peer_three_copies_gate_two_needs_two() {
    let config = config(3, true, Some(2));
    let rt = runtime(&config, allocator(&config, true).await);
    let refusal = pi_agents::peer_hand_off_block_for(&config, Some(&rt))
        .await
        .unwrap();
    assert!(refusal.starts_with(
        "No helper can start now: only 1 model copy/copies are free for helpers and a hand-off needs 2"
    ));
}
