//! Ported from tests/OpenOrca.Core.Tests/Slots: SlotAllocatorTests (allocator part),
//! SlotBalanceTests, WholeCopyPerAgentTests, HttpSlotRegistryTests.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::testing::{FakeSlotHealth, FakeSlotRegistry};
use super::*;
use crate::config::{SlotCopyConfig, SlotsConfig};

fn copies(names: &[&str], slots: i32) -> Vec<SlotCopyConfig> {
    names
        .iter()
        .map(|n| SlotCopyConfig {
            name: (*n).into(),
            base_url: format!("http://{}/v1", n.to_lowercase()),
            slots,
            ..SlotCopyConfig::default()
        })
        .collect()
}

fn config(agents_per_copy: i32) -> SlotsConfig {
    SlotsConfig {
        agents_per_copy,
        whole_copy_per_agent: false,
        registry_url: Some("http://fake".into()),
        copies: copies(&["A", "B"], 2),
        lead_copy: Some("A".into()),
        retry_seconds: 1,
        ..SlotsConfig::default()
    }
}

struct Rig {
    registry: Arc<FakeSlotRegistry>,
    local: Arc<FakeSlotRegistry>,
    health: Arc<FakeSlotHealth>,
    offset: Arc<AtomicU64>,
    allocator: Arc<SlotAllocator>,
}

impl Rig {
    fn new(with_registry: bool, agents_per_copy: i32) -> Self {
        Self::with(
            config(agents_per_copy),
            FakeSlotRegistry::new(&[]),
            with_registry,
        )
    }

    fn with(config: SlotsConfig, registry: FakeSlotRegistry, with_registry: bool) -> Self {
        let registry = Arc::new(registry);
        let local = Arc::new(FakeSlotRegistry::new(&[]));
        let health = Arc::new(FakeSlotHealth::default());
        let offset = Arc::new(AtomicU64::new(0));
        let base = Instant::now();
        let o = offset.clone();
        let allocator = SlotAllocator::new(
            config,
            with_registry.then(|| registry.clone() as Arc<dyn SlotRegistry>),
            local.clone(),
            health.clone(),
            AllocatorOptions {
                clock: Some(Arc::new(move || {
                    base + Duration::from_secs(o.load(Ordering::SeqCst))
                })),
                owner: Some("tester@box".into()),
                host: Some("box".into()),
                pid: Some(4242),
                agents_may_share_lead_copy: false,
            },
        );
        Self {
            registry,
            local,
            health,
            offset,
            allocator,
        }
    }

    async fn get(&self, r: SlotRequest) -> Option<Arc<SlotLease>> {
        self.allocator.try_acquire(&r).await
    }
}

fn lead() -> SlotRequest {
    SlotRequest::new("lead", "main", "lead").with_prefer_copy(Some("A"))
}

fn agent(id: &str) -> SlotRequest {
    SlotRequest::agent(id, id)
}

fn agent_pref(id: &str, slot: Option<&str>) -> SlotRequest {
    agent(id).with_prefer_slot(slot)
}

fn heavy(id: &str) -> SlotRequest {
    agent(id).with_heavy(true)
}

fn slot(l: &Option<Arc<SlotLease>>) -> String {
    l.as_ref().and_then(|l| l.slot()).unwrap_or_default()
}

fn copy(l: &Option<Arc<SlotLease>>) -> String {
    l.as_ref().and_then(|l| l.copy()).unwrap_or_default()
}

async fn concurrently(
    a: &Arc<SlotAllocator>,
    ids: Vec<SlotRequest>,
) -> Vec<Option<Arc<SlotLease>>> {
    let tasks: Vec<_> = ids
        .into_iter()
        .map(|r| {
            let a = a.clone();
            tokio::spawn(async move { a.try_acquire(&r).await })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

#[tokio::test]
async fn lead_prefers_copy_a_and_agents_spread_least_loaded() {
    let rig = Rig::new(true, 0);
    let l = rig.get(lead()).await;
    let a1 = rig.get(agent("x")).await;
    let a2 = rig.get(agent("y")).await;
    let a3 = rig.get(agent("z")).await;
    assert_eq!(slot(&l), "A1");
    assert_eq!(
        format!("{} {} {}", slot(&a1), slot(&a2), slot(&a3)),
        "B1 B2 A2"
    );
    for x in [&l, &a1, &a2, &a3] {
        assert!(x.as_ref().unwrap().via_registry());
    }
    assert_eq!(rig.registry.snapshot()["B1"], "tester@box:4242:x");
}

#[test]
fn agents_per_copy_defaults_to_one() {
    assert_eq!(SlotsConfig::default().agents_per_copy, 1);
}

#[tokio::test]
async fn agents_per_copy1_one_running_agent_per_copy_the_lead_does_not_count_the_rest_queue() {
    let rig = Rig::new(true, 1);
    let l = rig.get(lead()).await;
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    assert_eq!(slot(&l), "A1");
    assert_eq!(format!("{} {}", slot(&x), slot(&y)), "B1 A2");
    assert!(rig.get(agent("z")).await.is_none());

    let a = rig.allocator.clone();
    let pending = tokio::spawn(async move { a.acquire(&agent("z"), None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!pending.is_finished());
    assert!(!rig.registry.snapshot().contains_key("B2"));

    rig.allocator.release(x.as_ref().unwrap()).await;
    let z = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(z.copy().as_deref(), Some("B"));
}

#[tokio::test]
async fn one_agent_goes_to_the_copy_the_lead_is_not_on_two_agents_b_then_a() {
    let rig = Rig::new(true, 1);
    let l = rig.get(lead()).await;
    let first = rig.get(agent("x")).await;
    assert_eq!(slot(&l), "A1");
    assert_eq!(copy(&first), "B");
    assert_eq!(slot(&rig.get(agent("y")).await), "A2");
}

#[tokio::test]
async fn one_agent_avoids_the_leads_copy_even_when_the_registry_would_tie_to_it() {
    let rig = Rig::new(true, 1);
    rig.registry.occupy("B1");
    rig.get(lead()).await;
    assert_eq!(slot(&rig.get(agent("x")).await), "B2");
}

#[tokio::test]
async fn one_agent_uses_the_leads_copy_when_it_is_the_only_one_free() {
    let rig = Rig::new(true, 1);
    rig.registry.occupy("B1");
    rig.registry.occupy("B2");
    rig.get(lead()).await;
    assert_eq!(slot(&rig.get(agent("x")).await), "A2");
}

#[tokio::test]
async fn agents_per_copy1_concurrent_spawns_never_share_a_copy() {
    let rig = Rig::new(true, 1);
    rig.get(lead()).await;
    let leases = concurrently(
        &rig.allocator,
        (0..6).map(|i| agent(&format!("a{i}"))).collect(),
    )
    .await;
    let mut running: Vec<String> = leases.iter().flatten().filter_map(|l| l.copy()).collect();
    running.sort();
    assert_eq!(running, vec!["A", "B"]);
}

#[tokio::test]
async fn agents_per_copy1_follow_up_waits_rather_than_doubling_up_on_its_old_copy() {
    let rig = Rig::new(true, 1);
    let first = rig.get(agent("x")).await.unwrap();
    let old = first.slot();
    rig.allocator.release(&first).await;
    let busy = rig.get(agent("busy")).await;
    let busy2 = rig.get(agent("busy2")).await;
    let mut c = vec![copy(&busy), copy(&busy2)];
    c.sort();
    assert_eq!(c, vec!["A", "B"]);
    assert!(rig.get(agent_pref("x", old.as_deref())).await.is_none());
}

#[tokio::test]
async fn agents_per_copy2_allows_two_agents_on_a_copy() {
    let rig = Rig::new(true, 2);
    rig.get(lead()).await;
    let mut slots = Vec::new();
    for id in ["x", "y", "z"] {
        slots.push(slot(&rig.get(agent(id)).await));
    }
    assert_eq!(slots.join(" "), "B1 B2 A2");
}

#[tokio::test]
async fn heavy_agent_gets_the_idle_copy_and_counts_double_so_the_next_goes_beside_the_lead() {
    let rig = Rig::new(true, 2);
    rig.get(lead()).await;
    assert_eq!(slot(&rig.get(heavy("big")).await), "B1");
    assert_eq!(slot(&rig.get(agent("x")).await), "A2");
    assert_eq!(slot(&rig.get(agent("y")).await), "B2");
}

#[tokio::test]
async fn live_case_lead_plus_four_agents_two_copies_of_four_never_three_on_the_leads_copy() {
    let mut cfg = config(3);
    cfg.copies = copies(&["A", "B"], 4);
    let rig = Rig::with(
        cfg,
        FakeSlotRegistry::new(&["A1", "A2", "A3", "A4", "B1", "B2", "B3", "B4"]),
        true,
    );
    rig.get(lead()).await;
    let big = rig.get(heavy("big")).await;
    assert_eq!(copy(&big), "B");
    let mut rest = concurrently(&rig.allocator, vec![agent("x"), agent("y"), agent("z")]).await;
    rest.push(big);
    let on_a = rest.iter().filter(|l| copy(l) == "A").count();
    let on_b = rest.iter().filter(|l| copy(l) == "B").count();
    assert_eq!((on_a, on_b), (2, 2));
}

#[tokio::test]
async fn heavy_agent_copy_is_shared_only_when_no_other_copy_is_up() {
    let rig = Rig::new(true, 2);
    rig.get(lead()).await;
    assert_eq!(slot(&rig.get(heavy("big")).await), "B1");
    rig.health.set_down("A");
    assert_eq!(slot(&rig.get(agent("x")).await), "B2");
}

#[tokio::test]
async fn two_heavy_agents_spread_over_both_copies() {
    let rig = Rig::new(true, 2);
    rig.get(lead()).await;
    let a = rig.get(heavy("a")).await;
    let b = rig.get(heavy("b")).await;
    assert_eq!(format!("{} {}", slot(&a), slot(&b)), "B1 A2");
    assert_eq!(slot(&rig.get(agent("x")).await), "B2");
}

#[tokio::test]
async fn lead_takes_copy_b_when_a_is_full() {
    let rig = Rig::new(true, 0);
    rig.registry.occupy("A1");
    rig.registry.occupy("A2");
    assert_eq!(slot(&rig.get(lead()).await), "B1");
}

#[tokio::test]
async fn same_owner_gets_its_existing_lease() {
    let rig = Rig::new(true, 0);
    let first = rig.get(agent("x")).await.unwrap();
    let again = rig.get(agent("x")).await.unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(rig.registry.snapshot().len(), 1);
}

#[tokio::test]
async fn all_busy_try_returns_null_and_acquire_queues_until_a_slot_frees() {
    let rig = Rig::new(true, 0);
    for s in ["A1", "A2", "B1"] {
        rig.registry.occupy(s);
    }
    let mine = rig.get(agent("first")).await.unwrap();
    assert_eq!(mine.slot().as_deref(), Some("B2"));
    assert!(rig.get(agent("second")).await.is_none());

    let waited = Arc::new(Mutex::new(false));
    let w = waited.clone();
    let a = rig.allocator.clone();
    let pending = tokio::spawn(async move {
        a.acquire(
            &agent("second"),
            Some(Box::new(move || *w.lock().unwrap() = true)),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!pending.is_finished());
    assert!(*waited.lock().unwrap());

    rig.allocator.release(&mine).await;
    let got = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.slot().as_deref(), Some("B2"));
}

#[tokio::test]
async fn acquire_picks_up_slots_freed_by_others_on_retry() {
    let rig = Rig::new(true, 0);
    for s in ["A1", "A2", "B1", "B2"] {
        rig.registry.occupy(s);
    }
    let a = rig.allocator.clone();
    let pending = tokio::spawn(async move { a.acquire(&agent("w"), None).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.registry.free("A2");
    let got = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.slot().as_deref(), Some("A2"));
}

#[tokio::test]
async fn acquire_honors_cancellation_while_waiting() {
    let rig = Rig::new(true, 0);
    for s in ["A1", "A2", "B1", "B2"] {
        rig.registry.occupy(s);
    }
    let r = tokio::time::timeout(
        Duration::from_millis(200),
        rig.allocator.acquire(&agent("w"), None),
    )
    .await;
    assert!(r.is_err());
    assert!(!rig.registry.snapshot().values().any(|k| k.ends_with(":w")));
}

#[tokio::test]
async fn heartbeat_renews_registry_leases() {
    let rig = Rig::new(true, 0);
    rig.get(agent("x")).await;
    rig.allocator.heartbeat().await;
    assert_eq!(rig.registry.renewals(), 1);
    assert_eq!(rig.allocator.held()[0].slot().as_deref(), Some("B1"));
}

#[tokio::test]
async fn heartbeat_reclaims_same_slot_after_expiry() {
    let rig = Rig::new(true, 0);
    let lease = rig.get(agent("x")).await.unwrap();
    let was = lease.slot().unwrap();
    rig.registry.expire(lease.key());
    rig.allocator.heartbeat().await;
    assert_eq!(lease.slot().as_deref(), Some(was.as_str()));
    assert_eq!(rig.registry.snapshot()[&was], lease.key());
}

#[tokio::test]
async fn heartbeat_takes_another_slot_when_the_old_one_was_taken() {
    let rig = Rig::new(true, 0);
    let lease = rig.get(agent("x")).await.unwrap();
    let was = lease.slot().unwrap();
    rig.registry.expire(lease.key());
    rig.registry.occupy(&was);
    rig.allocator.heartbeat().await;
    assert!(lease.slot().is_some());
    assert_ne!(lease.slot().unwrap(), was);
}

#[tokio::test]
async fn registry_down_falls_back_to_local_then_returns_to_registry() {
    let rig = Rig::new(true, 0);
    rig.registry.set_down(true);
    let lease = rig.get(agent("x")).await.unwrap();
    assert_eq!(lease.slot().as_deref(), Some("B1"));
    assert!(!lease.via_registry());
    assert_eq!(rig.local.snapshot().len(), 1);
    assert!(!rig.allocator.registry_up());

    rig.registry.set_down(false);
    let still_local = rig.get(agent("y")).await.unwrap();
    assert!(!still_local.via_registry());

    rig.offset.fetch_add(21, Ordering::SeqCst);
    let back = rig.get(agent("z")).await.unwrap();
    assert!(back.via_registry());
}

#[tokio::test]
async fn no_registry_configured_uses_local_only() {
    let rig = Rig::new(false, 0);
    let lease = rig.get(lead()).await.unwrap();
    assert_eq!(lease.slot().as_deref(), Some("A1"));
    assert!(!lease.via_registry());
    assert!(rig.registry.snapshot().is_empty());
}

#[tokio::test]
async fn unhealthy_copy_is_skipped() {
    let rig = Rig::new(true, 0);
    rig.health.set_down("A");
    assert_eq!(slot(&rig.get(lead()).await), "B1");
    assert!(!rig.registry.snapshot().contains_key("A1"));
}

#[tokio::test]
async fn both_copies_down_gives_a_fallback_lease_with_no_slot() {
    let rig = Rig::new(true, 0);
    rig.health.set_down("A");
    rig.health.set_down("B");
    let lease = rig.get(agent("x")).await.unwrap();
    assert!(lease.slot().is_none());
    assert!(rig.registry.snapshot().is_empty());
}

#[tokio::test]
async fn failover_moves_the_lease_to_the_other_copy() {
    let rig = Rig::new(true, 0);
    let lease = rig.get(lead()).await.unwrap();
    assert_eq!(lease.slot().as_deref(), Some("A1"));
    rig.health.set_down("A");
    assert!(rig.allocator.failover(&lease).await);
    assert_eq!(lease.slot().as_deref(), Some("B1"));
    assert_eq!(
        rig.registry.snapshot().keys().cloned().collect::<Vec<_>>(),
        vec!["B1"]
    );
}

#[tokio::test]
async fn failover_with_other_copy_full_runs_on_fallback_and_upgrades_later() {
    let rig = Rig::new(true, 0);
    rig.registry.occupy("B1");
    rig.registry.occupy("B2");
    let lease = rig.get(agent("x")).await.unwrap();
    assert_eq!(lease.slot().as_deref(), Some("A1"));
    rig.allocator.failover(&lease).await;
    assert!(lease.slot().is_none());
    rig.registry.free("B2");
    assert!(rig.allocator.try_upgrade(&lease).await);
    assert_eq!(lease.slot().as_deref(), Some("B2"));
}

#[tokio::test]
async fn follow_up_reclaims_its_previous_slot_when_free() {
    let rig = Rig::new(true, 0);
    rig.get(agent("a")).await;
    let b = rig.get(agent("b")).await.unwrap();
    let old = b.slot();
    rig.allocator.release(&b).await;
    rig.registry.occupy("A2");
    let again = rig.get(agent_pref("b", old.as_deref())).await;
    assert_eq!(slot(&again), old.unwrap());
}

#[tokio::test]
async fn release_and_release_all_free_the_registry() {
    let rig = Rig::new(true, 0);
    let l = rig.get(lead()).await.unwrap();
    let x = rig.get(agent("x")).await.unwrap();
    rig.allocator.release(&x).await;
    assert_eq!(
        rig.registry.snapshot().keys().cloned().collect::<Vec<_>>(),
        vec!["A1"]
    );
    assert!(x.released());
    assert!(x.slot().is_none());

    rig.get(agent("y")).await;
    rig.allocator.release_all(Duration::from_secs(1)).await;
    assert!(rig.registry.snapshot().is_empty());
    assert!(rig.allocator.held().is_empty());
    assert!(l.released());
}

#[test]
fn slot_names_follow_the_copies() {
    assert_eq!(
        SlotAllocator::slot_names(&config(0)),
        vec!["A1", "A2", "B1", "B2"]
    );
}

#[test]
fn health_url_strips_the_v1_suffix() {
    assert_eq!(
        HttpSlotHealthProbe::health_url("http://h:8080/v1/"),
        "http://h:8080/health"
    );
    assert_eq!(
        HttpSlotHealthProbe::health_url("http://h:8080"),
        "http://h:8080/health"
    );
}

// ---- SlotBalanceTests ----

fn balance_rig(agents_per_copy: i32) -> Rig {
    let mut cfg = config(agents_per_copy);
    cfg.copies = copies(&["A", "B"], 4);
    Rig::with(
        cfg,
        FakeSlotRegistry::new(&["A1", "A2", "A3", "A4", "B1", "B2", "B3", "B4"]),
        true,
    )
}

#[tokio::test]
async fn four_concurrent_agents_spread_over_both_copies_never_three_beside_the_lead() {
    for run in 0..20 {
        let rig = balance_rig(3);
        rig.get(lead()).await;
        let leases = concurrently(
            &rig.allocator,
            (0..4).map(|i| agent(&format!("a{i}"))).collect(),
        )
        .await;
        let on_a = leases.iter().filter(|l| copy(l) == "A").count();
        let on_b = leases.iter().filter(|l| copy(l) == "B").count();
        assert_eq!(on_a + on_b, 4);
        assert!(on_a <= 1, "run {run}: A={on_a} B={on_b}");
        assert_eq!(on_b, 3);
    }
}

#[tokio::test]
async fn other_processes_leases_count_as_load() {
    let rig = balance_rig(3);
    rig.registry.occupy_as("B1", "other:1");
    rig.registry.occupy_as("B2", "other:2");
    rig.get(lead()).await;
    assert_eq!(copy(&rig.get(agent("x")).await), "A");
    assert_eq!(copy(&rig.get(agent("y")).await), "B");
}

#[tokio::test]
async fn copy_load_counts_the_lead_our_agents_others_and_heavy_twice() {
    let rig = balance_rig(3);
    rig.registry.occupy_as("B4", "other:1");
    rig.get(lead()).await;
    rig.get(heavy("big")).await;
    let load = rig.allocator.copy_load(None).await;
    assert_eq!(load["a"], 1);
    assert_eq!(load["b"], 3);
}

#[tokio::test]
async fn parked_lead_its_copy_still_loses_ties() {
    let rig = balance_rig(3);
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    assert_eq!(format!("{} {}", copy(&x), copy(&y)), "B A");
}

#[tokio::test]
async fn agents_per_copy_is_still_a_cap() {
    let rig = balance_rig(1);
    rig.get(lead()).await;
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    assert_eq!(format!("{} {}", copy(&x), copy(&y)), "B A");
    assert!(rig.get(agent("z")).await.is_none());
}

#[tokio::test]
async fn agents_per_copy2_six_agents_four_run_two_per_copy() {
    let rig = balance_rig(2);
    rig.get(lead()).await;
    let leases = concurrently(
        &rig.allocator,
        (0..6).map(|i| agent(&format!("a{i}"))).collect(),
    )
    .await;
    assert_eq!(leases.iter().filter(|l| copy(l) == "A").count(), 2);
    assert_eq!(leases.iter().filter(|l| copy(l) == "B").count(), 2);
    assert_eq!(leases.iter().filter(|l| l.is_none()).count(), 2);
}

#[tokio::test]
async fn registry_list_fails_counts_our_own_leases() {
    let mut cfg = config(3);
    cfg.copies = copies(&["A", "B"], 4);
    let registry = FakeSlotRegistry::new(&["A1", "A2", "A3", "A4", "B1", "B2", "B3", "B4"]);
    registry.list_broken.store(true, Ordering::SeqCst);
    let rig = Rig::with(cfg, registry, true);
    rig.get(lead()).await;
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    let z = rig.get(agent("z")).await;
    assert_eq!(format!("{} {} {}", copy(&x), copy(&y), copy(&z)), "B B A");
}

// Health probe (a local server instead of the fake HttpMessageHandler).

async fn health_server(delay: Duration, status: u16) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let app = axum::Router::new().route(
        "/health",
        axum::routing::get(move || {
            c.fetch_add(1, Ordering::SeqCst);
            async move {
                tokio::time::sleep(delay).await;
                axum::http::StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/v1"), calls)
}

fn probe(url: &str, timeout_ms: u64, slow_ms: u64) -> HttpSlotHealthProbe {
    HttpSlotHealthProbe::new(
        &[("B".into(), url.into())],
        Duration::from_millis(timeout_ms),
        Some(Duration::from_millis(slow_ms)),
    )
}

#[tokio::test]
async fn slow_health_answer_under_load_is_alive() {
    let (url, calls) = health_server(Duration::from_millis(400), 200).await;
    assert!(probe(&url, 100, 3000).is_healthy("B").await);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refused_connection_is_down_at_once() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    assert!(
        !probe(&format!("http://127.0.0.1:{port}/v1"), 100, 3000)
            .is_healthy("B")
            .await
    );
}

#[tokio::test]
async fn error_status_is_down() {
    let (url, _) = health_server(Duration::ZERO, 503).await;
    assert!(!probe(&url, 1000, 3000).is_healthy("B").await);
}

#[tokio::test]
async fn no_answer_even_after_the_long_try_is_down() {
    let (url, calls) = health_server(Duration::from_secs(30), 200).await;
    assert!(!probe(&url, 50, 150).is_healthy("B").await);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// ---- WholeCopyPerAgentTests ----

fn whole_rig(whole: bool, n: usize, slots: &[&str]) -> Rig {
    let cfg = SlotsConfig {
        whole_copy_per_agent: whole,
        agents_per_copy: 2,
        registry_url: Some("http://fake".into()),
        copies: copies(&["A", "B", "C"][..n], 2),
        lead_copy: Some("A".into()),
        retry_seconds: 1,
        ..SlotsConfig::default()
    };
    let slots = if slots.is_empty() {
        &["A1", "A2", "B1", "B2", "C1", "C2"][..]
    } else {
        slots
    };
    Rig::with(cfg, FakeSlotRegistry::new(slots), true)
}

#[test]
fn is_on_by_default() {
    assert!(SlotsConfig::default().whole_copy_per_agent);
}

#[tokio::test]
async fn one_agent_per_copy_never_on_the_leads_copy_the_rest_wait() {
    let rig = whole_rig(true, 2, &["A1", "A2", "B1", "B2"]);
    rig.get(lead()).await;
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    assert_eq!(copy(&x), "B");
    assert!(y.is_none());
    rig.allocator.release(x.as_ref().unwrap()).await;
    assert_eq!(copy(&rig.get(agent("y")).await), "B");
}

#[tokio::test]
async fn three_copies_two_agents_run_at_once() {
    let rig = whole_rig(true, 3, &[]);
    rig.get(lead()).await;
    let x = rig.get(agent("x")).await;
    let y = rig.get(agent("y")).await;
    let mut c = vec![copy(&x), copy(&y)];
    c.sort();
    assert_eq!(c, vec!["B", "C"]);
    assert!(rig.get(agent("z")).await.is_none());
}

#[tokio::test]
async fn single_copy_agents_may_still_use_the_leads_copy() {
    let rig = whole_rig(true, 1, &["A1", "A2"]);
    rig.get(lead()).await;
    assert_eq!(slot(&rig.get(agent("x")).await), "A2");
}

#[tokio::test]
async fn off_keeps_agents_per_copy() {
    let rig = whole_rig(false, 2, &["A1", "A2", "B1", "B2"]);
    rig.get(lead()).await;
    assert!(rig.get(agent("x")).await.is_some());
    assert!(rig.get(agent("y")).await.is_some());
}

#[tokio::test]
async fn free_whole_copies_excludes_the_leads_copy_our_agents_and_other_peoples_leases() {
    let rig = whole_rig(true, 3, &[]);
    rig.get(lead()).await;
    let mut free = rig.allocator.free_whole_copies().await;
    free.sort();
    assert_eq!(free, vec!["B", "C"]);
    rig.get(agent("x")).await;
    assert_eq!(rig.allocator.free_whole_copies().await, vec!["C"]);
    rig.registry.occupy("C2");
    assert!(rig.allocator.free_whole_copies().await.is_empty());
}

#[tokio::test]
async fn free_whole_copies_registry_down_counts_only_our_own_leases() {
    let rig = whole_rig(true, 2, &[]);
    rig.get(lead()).await;
    rig.registry.occupy("B1");
    rig.registry.set_down(true);
    assert_eq!(rig.allocator.free_whole_copies().await, vec!["B"]);
}

// ---- HttpSlotRegistryTests ----

/// (path, JSON body, Content-Length header) of each request.
type Seen = Arc<Mutex<Vec<(String, Value, Option<String>)>>>;

#[tokio::test]
async fn speaks_the_pig_slots_protocol_with_buffered_bodies() {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let s = s.clone();
        async move {
            let path = req.uri().path().to_string();
            let len = req
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let answer = match path.as_str() {
                "/claim" if body.get("copy").is_some() => r#"{"ok": false, "reason": "full"}"#,
                "/claim" => r#"{"ok": true, "slot": "B2", "copy": "B", "ttl": 90.0}"#,
                "/renew" => r#"{"renewed": ["k1"], "lost": ["k2"]}"#,
                _ => r#"{"ok": true, "released": ["B2"]}"#,
            };
            s.lock().unwrap().push((path, body, len));
            answer
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let registry = HttpSlotRegistry::new(&format!("http://{addr}/"), Duration::from_secs(5));
    let base = SlotClaim {
        key: "k1".into(),
        owner: "me@box".into(),
        host: "box".into(),
        pid: 42,
        label: "lead".into(),
        kind: "agent".into(),
        ..SlotClaim::default()
    };
    let claim = registry
        .claim(&SlotClaim {
            avoid: Some(vec!["A".into()]),
            ..base.clone()
        })
        .await
        .unwrap();
    let full = registry
        .claim(&SlotClaim {
            kind: "main".into(),
            copy: Some("A".into()),
            ..base.clone()
        })
        .await
        .unwrap();
    let lost = registry.renew(&["k1".into(), "k2".into()]).await.unwrap();
    registry.release_all("box", 42).await.unwrap();

    assert!(claim.ok);
    assert_eq!(claim.slot.as_deref(), Some("B2"));
    assert_eq!(claim.copy.as_deref(), Some("B"));
    assert_eq!(claim.ttl, Some(90.0));
    assert!(!full.ok);
    assert_eq!(full.reason.as_deref(), Some("full"));
    assert_eq!(lost, vec!["k2"]);

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
        vec!["/claim", "/claim", "/renew", "/release"]
    );
    assert!(
        seen.iter()
            .all(|s| s.2.as_deref().is_some_and(|l| l != "0"))
    );
    let first = &seen[0].1;
    assert_eq!(first["key"], "k1");
    assert_eq!(first["owner"], "me@box");
    assert_eq!(first["host"], "box");
    assert_eq!(first["pid"], 42);
    assert_eq!(first["kind"], "agent");
    assert_eq!(first["avoid"][0], "A");
    assert_eq!(seen[1].1["copy"], "A");
    assert_eq!(seen[3].1["pid"], 42);
}
