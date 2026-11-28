//! The orchestrator against a fake host and plane, with fault injection. Time is paused, so
//! the 1.5 s lease settle, 3 s polls and 180 s chat timeouts run instantly.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::fake::*;
use super::*;
use crate::guards::{Action, Calendar, CalendarEntry, Caller, EventGrant, EventScope, Scope, When};
use crate::leases::SlotView;

fn ink() -> CardSpec {
    CardSpec::new("ninfer", "merged-inkling-4lane", "inkling2-mini", 4)
}
fn dense() -> CardSpec {
    CardSpec::new("ninfer", "merged-4lane", "dense-stock", 2)
}
fn strata(lanes: u32) -> CardSpec {
    CardSpec::new("strata", &format!("{lanes}slot"), "flash-next", lanes)
}
fn spec(a: CardSpec, b: CardSpec) -> Spec {
    let mut s = Spec::new();
    s.insert("A".into(), a);
    s.insert("B".into(), b);
    s
}

struct Rig {
    orch: Orchestrator<FakeHost, Arc<FakePlane>>,
    host: FakeHost,
    plane: Arc<FakePlane>,
    jobs: Arc<MemJobSink>,
}

impl Plane for Arc<FakePlane> {
    fn set_slots(&self, view: SlotView) {
        self.as_ref().set_slots(view)
    }
    fn held_on(&self, cards: &[String]) -> Vec<(String, String)> {
        self.as_ref().held_on(cards)
    }
    fn blocks(&self, a: Action, cards: &[String], caller: &Caller, layout: &str) -> Vec<Block> {
        self.as_ref().blocks(a, cards, caller, layout)
    }
    fn withdraw_routes(&self, cards: &[String]) {
        self.as_ref().withdraw_routes(cards)
    }
    fn commit_routes(&self, layout: Option<&str>, spec: &Spec, ready: &[String]) {
        self.as_ref().commit_routes(layout, spec, ready)
    }
}

fn rig(running: Spec) -> Rig {
    let mut cfg = SwitchConfig::deepthought_like(&["A", "B"]);
    cfg.max_sm_mhz.insert("A".into(), 1597);
    let host = FakeHost::with(running.clone());
    let view = SlotView {
        slots: slot_names(&cfg, &running),
        layout: Some("merged-inkling-x2".into()),
        cards: cfg.cards.clone(),
        ..SlotView::default()
    };
    let plane = Arc::new(FakePlane::new(view));
    let jobs = Arc::new(MemJobSink::default());
    Rig {
        orch: Orchestrator::new(cfg, host.clone(), plane.clone(), jobs.clone()),
        host,
        plane,
        jobs,
    }
}

fn req(layout: &str, target: Spec) -> SwitchRequest {
    SwitchRequest {
        layout: layout.into(),
        target,
        active: Some("merged-inkling-x2".into()),
        requested_by: "test".into(),
        reason: "test".into(),
        caller: Caller::default(),
        action: Action::Switch,
        allow_empty: false,
    }
}

fn steps(j: &JobRecord) -> String {
    j.steps
        .iter()
        .map(|s| s.msg.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn pos(calls: &[String], prefix: &str) -> Option<usize> {
    calls.iter().position(|c| c.starts_with(prefix))
}

// ── restart sets (layoutctl parity) ─────────────────────────────────────────────────────────

#[test]
fn restart_set_matches_layoutctl() {
    let cfg = SwitchConfig::deepthought_like(&["A", "B"]);
    let rs = |cur: Spec, t: Spec| restart_set(&cfg, &cur, &t);
    let off = CardSpec::off;
    // a B-only change restarts B only
    assert_eq!(rs(spec(ink(), ink()), spec(ink(), dense())), ["B"]);
    // nothing changes
    assert!(rs(spec(ink(), ink()), spec(ink(), ink())).is_empty());
    // Strata A restarts while B stays on Strata: B restarts too (arena)
    assert_eq!(
        rs(spec(strata(2), strata(2)), spec(strata(3), strata(2))),
        ["A", "B"]
    );
    // Strata B alone: A stays
    assert_eq!(
        rs(spec(strata(3), strata(2)), spec(strata(3), strata(1))),
        ["B"]
    );
    // Strata starts on A while ninfer stays on B: the ninfer restarts after it (NUMA node 0)
    assert_eq!(rs(spec(ink(), ink()), spec(strata(3), ink())), ["A", "B"]);
    // ... and on B while ninfer stays on A
    assert_eq!(rs(spec(ink(), ink()), spec(ink(), strata(2))), ["A", "B"]);
    // llama.cpp staying next to a Strata start is left alone
    let llama = CardSpec::new("llamacpp", "x", "qwenpi", 2);
    assert_eq!(
        rs(spec(ink(), llama.clone()), spec(strata(3), llama)),
        ["A"]
    );
    // off cards
    assert_eq!(rs(spec(ink(), ink()), spec(ink(), off())), ["B"]);
    assert_eq!(rs(spec(ink(), off()), spec(ink(), ink())), ["B"]);
}

// ── switches ────────────────────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn partial_switch_leaves_a_serving_and_chats_only_b() {
    let r = rig(spec(ink(), ink()));
    let p = r.plane.clone();
    // during the switch, claims see B's slots gone and A's still there
    *p.hook.lock().unwrap() = Some(Box::new(|t, v| {
        let mut b = serde_json::Map::new();
        b.insert("key".into(), json!("during"));
        let res = t.claim(v, &b, NOW, 90.0, None).1;
        Some(format!(
            "{}:{}",
            v.reason.clone().unwrap_or_default(),
            res["slot"]
        ))
    }));
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    let notes = p.notes.lock().unwrap().clone();
    assert!(
        notes[0].starts_with("switching card(s) B to combo-1:\"A"),
        "{notes:?}"
    );
    let calls = r.host.calls();
    assert!(pos(&calls, "stop A").is_none() && pos(&calls, "start A").is_none());
    assert_eq!(r.host.calls_like("chat"), ["chat B"]);
    assert_eq!(r.host.calls_like("health A").len(), 1); // A: /health only
    assert_eq!(*p.withdrawn.lock().unwrap(), vec![vec!["B".to_string()]]);
    // the final view: A1-A4 + B1-B2, unfrozen, the new layout
    let v = p.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4", "B1", "B2"]);
    assert!(!v.frozen);
    assert_eq!(v.layout.as_deref(), Some("combo-1"));
    assert!(steps(&j).contains("(their slots withdrawn; A keeps serving)"));
    assert!(steps(&j).contains("card B: healthy, chat ok in"));
    assert_eq!(j.live.as_deref(), Some("combo-1"));
    assert_eq!(j.from.as_deref(), Some("merged-inkling-x2"));
    assert_eq!(r.jobs.done.lock().unwrap().len(), 1);
    // the job leaves a history line in layoutctl's key set
    let h = j.history_line();
    let mut k: Vec<&String> = h.as_object().unwrap().keys().collect();
    k.sort();
    assert_eq!(
        k,
        [
            "ended",
            "error",
            "estimateSeconds",
            "from",
            "id",
            "layout",
            "profile",
            "requestedBy",
            "seconds",
            "started",
            "state"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn models_are_restored_before_anything_stops_and_power_is_held() {
    let r = rig(spec(ink(), ink()));
    let j = r
        .orch
        .switch(req("flash", spec(strata(3), strata(2))))
        .await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    let c = r.host.calls();
    let first_stop = pos(&c, "stop").unwrap();
    assert!(pos(&c, "restore A").unwrap() < first_stop);
    assert!(pos(&c, "restore B").unwrap() < first_stop);
    assert!(pos(&c, "hold").unwrap() < first_stop);
    assert!(pos(&c, "lock").unwrap() < first_stop);
    // Strata: sequential, A healthy before B starts
    let start_b = pos(&c, "start B").unwrap();
    let a_healthy = c[..start_b].iter().rposition(|x| x == "health A");
    assert!(a_healthy.is_some() && pos(&c, "start A").unwrap() < a_healthy.unwrap());
    // both moved: claims were frozen first
    assert!(r.plane.views.lock().unwrap()[0].frozen);
    let st = r.host.st.lock().unwrap();
    assert_eq!(st.holds, vec![j.estimate_seconds.unwrap() + 120]);
    assert_eq!(st.released, 1);
    assert!(!st.locked);
    assert!(steps(&j).contains("card A: clocks ok (max 1597 MHz)"));
}

#[tokio::test(start_paused = true)]
async fn a_slow_container_death_is_waited_out() {
    let r = rig(spec(ink(), ink()));
    {
        let mut s = r.host.st.lock().unwrap();
        s.zombie_tries.insert("B".into(), 5);
        // GPU memory takes a few polls to drop under 1 GiB
        s.gpu_used.insert("B".into(), vec![30000, 9000, 1500, 200]);
    }
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    assert_eq!(r.host.calls_like("stop B").len(), 6);
    assert_eq!(r.host.calls_like("gpu B").len(), 4);
    assert!(steps(&j).contains("card B: gone after 6 tries"));
}

#[tokio::test(start_paused = true)]
async fn a_container_that_never_dies_rolls_back() {
    let r = rig(spec(ink(), ink()));
    r.host
        .st
        .lock()
        .unwrap()
        .zombie_tries
        .insert("B".into(), u32::MAX);
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "rolled-back", "{}", steps(&j));
    assert!(j.error.as_deref().unwrap().contains("still there"));
    assert_eq!(j.live.as_deref(), Some("merged-inkling-x2"));
    // B still runs Inkling and is routable again
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots.len(), 8);
}

#[tokio::test(start_paused = true)]
async fn gpu_memory_that_never_frees_rolls_back() {
    let r = rig(spec(ink(), ink()));
    r.host
        .st
        .lock()
        .unwrap()
        .gpu_used
        .insert("B".into(), vec![20000; 32]); // past the 90 s wait, then free for the rollback
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "rolled-back", "{}", steps(&j));
    assert!(
        j.error
            .as_deref()
            .unwrap()
            .contains("MiB in use after stopping")
    );
}

#[tokio::test(start_paused = true)]
async fn an_oom_while_starting_is_recorded_and_rolled_back() {
    let r = rig(spec(ink(), ink()));
    r.host.st.lock().unwrap().exits.insert(
        ("B".into(), "dense-stock".into()),
        (
            "CUDA error: out of memory".into(),
            Some("Out of memory: Killed process 4242 (ninfer-serve)".into()),
        ),
    );
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "rolled-back", "{}", steps(&j));
    assert!(
        j.error
            .as_deref()
            .unwrap()
            .contains("exited while starting: CUDA error")
    );
    assert_eq!(j.new_oom_kills.as_ref().unwrap().len(), 1);
    assert!(steps(&j).contains("NEW OOM KILLS: 1"));
    // rollback restarted B on Inkling and chatted with it
    assert_eq!(r.host.st.lock().unwrap().running["B"], ink());
    assert_eq!(r.host.calls_like("chat"), ["chat B"]);
}

#[tokio::test(start_paused = true)]
async fn a_test_chat_timeout_rolls_back_without_touching_a() {
    let r = rig(spec(ink(), ink()));
    r.host
        .st
        .lock()
        .unwrap()
        .chat_hangs
        .insert("dense-stock".into());
    let started = tokio::time::Instant::now();
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "rolled-back", "{}", steps(&j));
    assert!(
        j.error
            .as_deref()
            .unwrap()
            .contains("chat timed out after 180s")
    );
    assert!(started.elapsed() >= Duration::from_secs(180));
    // A was never chatted with (a busy card would queue the chat behind long generations)
    assert!(r.host.calls_like("chat A").is_empty());
    assert!(r.host.calls_like("stop A").is_empty());
    assert_eq!(r.host.st.lock().unwrap().running["B"], ink());
    let commits = r.plane.commits.lock().unwrap().clone();
    assert_eq!(commits.last().unwrap().1, ["A", "B"]);
}

#[tokio::test(start_paused = true)]
async fn a_failed_rollback_keeps_only_healthy_cards_routable() {
    let r = rig(spec(ink(), ink()));
    // both the new model and the old one die on card B
    {
        let mut s = r.host.st.lock().unwrap();
        s.exits
            .insert(("B".into(), "dense-stock".into()), ("boom".into(), None));
        s.exits
            .insert(("B".into(), "inkling2-mini".into()), ("boom".into(), None));
    }
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "failed", "{}", steps(&j));
    let commits = r.plane.commits.lock().unwrap().clone();
    assert_eq!(commits.last().unwrap().1, ["A"]);
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4"]);
    assert!(!v.frozen);
}

#[tokio::test(start_paused = true)]
async fn an_ssh_drop_mid_switch_fails_safe() {
    let r = rig(spec(ink(), ink()));
    // the connection drops right after B is stopped
    let drop_at = {
        let probe = rig(spec(ink(), ink()));
        probe
            .orch
            .switch(req("combo-1", spec(ink(), dense())))
            .await;
        pos(&probe.host.calls(), "stop B").unwrap() + 1
    };
    r.host.st.lock().unwrap().drop_after = Some(drop_at);
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "failed", "{}", steps(&j));
    assert!(j.rollback_error.is_some());
    // B is out of routing and slots, A keeps serving; nothing is left frozen
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4"]);
    assert!(!v.frozen);
    let commits = r.plane.commits.lock().unwrap().clone();
    assert_eq!(commits.last().unwrap().1, ["A"]);
}

#[tokio::test(start_paused = true)]
async fn a_lease_taken_just_before_the_withdrawal_refuses() {
    let r = rig(spec(ink(), ink()));
    // an agent claims a slot on B a moment before the switch withdraws B's slots
    assert_eq!(
        r.plane.claim(
            "agent-1",
            json!({"copy": "B", "owner": "pig", "label": "agent"})
        )["slot"],
        "B1"
    );
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "refused", "{}", steps(&j));
    assert!(
        j.error
            .as_deref()
            .unwrap()
            .contains("pig-slots leases held: B1 (orphan)")
    );
    // nothing stopped, slots back as they were
    assert!(r.host.calls_like("stop").is_empty());
    assert!(r.host.calls_like("hold").is_empty());
    assert_eq!(r.plane.view.lock().unwrap().slots.len(), 8);
}

#[tokio::test(start_paused = true)]
async fn a_lease_on_the_staying_card_does_not_block() {
    let r = rig(spec(ink(), ink()));
    assert_eq!(
        r.plane.claim("lead", json!({"copy": "A", "kind": "main"}))["slot"],
        "A1"
    );
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "done", "{}", steps(&j));
}

#[tokio::test(start_paused = true)]
async fn claims_during_a_full_switch_answer_switching() {
    let r = rig(spec(ink(), ink()));
    *r.plane.hook.lock().unwrap() = Some(Box::new(|t, v| {
        let mut b = serde_json::Map::new();
        b.insert("key".into(), json!("late"));
        let res = t.claim(v, &b, NOW, 90.0, None).1;
        v.frozen
            .then(|| res["reason"].as_str().unwrap_or("").to_string())
    }));
    let j = r
        .orch
        .switch(req("flash", spec(strata(3), strata(2))))
        .await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    assert_eq!(r.plane.notes.lock().unwrap()[0], "switching");
}

#[tokio::test(start_paused = true)]
async fn foreign_holders_and_preconditions_refuse_before_anything_moves() {
    let r = rig(spec(ink(), ink()));
    r.host
        .st
        .lock()
        .unwrap()
        .foreign
        .insert("B".into(), vec!["comfyui".into()]);
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "refused");
    assert!(j.error.unwrap().contains("held by comfyui"));
    // a foreign holder on A does not block a B-only switch
    let r = rig(spec(ink(), ink()));
    r.host
        .st
        .lock()
        .unwrap()
        .foreign
        .insert("A".into(), vec!["comfyui".into()]);
    assert_eq!(
        r.orch.switch(req("c", spec(ink(), dense()))).await.state,
        "done"
    );
    let r = rig(spec(ink(), ink()));
    r.host.st.lock().unwrap().precondition = Some("ninfer build 'x' has no ninfer-serve".into());
    let j = r.orch.switch(req("c", spec(ink(), dense()))).await;
    assert_eq!(j.state, "refused");
    assert!(r.host.calls_like("restore").is_empty());
    let r = rig(spec(ink(), ink()));
    r.host.st.lock().unwrap().restore_fails = true;
    let j = r.orch.switch(req("c", spec(ink(), dense()))).await;
    assert_eq!(j.state, "refused");
    assert!(r.plane.views.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn guards_refuse_and_event_keys_lift_their_own_reservation() {
    let r = rig(spec(ink(), ink()));
    r.plane
        .board
        .declare("video", vec![Action::OnDemand], Scope::All, 300);
    r.plane.board.report(
        "video",
        NOW,
        Some("a video pipeline is running (ai-news)".into()),
    );
    // an admin switch is not stopped by the video guard; an on-demand one is
    let mut od = req("combo-1", spec(ink(), dense()));
    od.action = Action::OnDemand;
    let j = r.orch.switch(od.clone()).await;
    assert_eq!(j.state, "refused");
    assert!(j.error.unwrap().contains("video pipeline"));
    assert!(r.host.calls_like("restore").is_empty());
    // stale guard data: wait
    let r = rig(spec(ink(), ink()));
    r.plane
        .board
        .declare("bench", vec![Action::OnDemand], Scope::All, 600);
    let j = r.orch.switch(od.clone()).await;
    assert_eq!(j.state, "refused");
    assert!(j.error.unwrap().contains("has not reported"));
    // a calendar reservation of card B, lifted for its event's key holders
    let r = rig(spec(ink(), ink()));
    let cal = Arc::new(Calendar::default());
    cal.entries.lock().unwrap().push(CalendarEntry {
        id: "demo".into(),
        reason: "card B reserved".into(),
        blocks: vec![Action::Switch],
        scope: Scope::Cards(vec!["B".into()]),
        when: When::Once {
            from: NOW - 10.0,
            until: NOW + 3600.0,
        },
        event: Some("demo".into()),
    });
    r.plane.board.add_guard(cal);
    assert_eq!(
        r.orch
            .switch(req("combo-1", spec(ink(), dense())))
            .await
            .state,
        "refused"
    );
    let mut with_key = req("combo-1", spec(ink(), dense()));
    with_key.caller = Caller {
        principal: Some("k1".into()),
        grants: vec![EventGrant {
            event: "demo".into(),
            scopes: vec![EventScope::SwitchLayout(None)],
        }],
    };
    assert_eq!(r.orch.switch(with_key).await.state, "done");
}

#[tokio::test(start_paused = true)]
async fn capped_clocks_warn_or_fail() {
    let r = rig(spec(ink(), ink()));
    r.host.st.lock().unwrap().clocks.insert("A".into(), 1380);
    let j = r
        .orch
        .switch(req("flash", spec(strata(3), strata(2))))
        .await;
    assert_eq!(j.state, "done");
    assert!(steps(&j).contains("card A: clocks capped at 1380 MHz (expected 1597); warning only"));
    let mut r = rig(spec(ink(), ink()));
    r.orch.cfg.clock_mismatch = OnMismatch::Fail;
    r.host.st.lock().unwrap().clocks.insert("A".into(), 1380);
    let j = r
        .orch
        .switch(req("flash", spec(strata(3), strata(2))))
        .await;
    // the rollback restarts A too, whose clocks are still capped: failed, nothing hidden
    assert_eq!(j.state, "failed", "{}", steps(&j));
}

#[tokio::test(start_paused = true)]
async fn already_live_restarts_nothing() {
    let r = rig(spec(ink(), ink()));
    let j = r
        .orch
        .switch(req("merged-inkling-x2", spec(ink(), ink())))
        .await;
    assert_eq!(j.state, "done");
    assert_eq!(j.already, Some(true));
    assert!(r.host.calls_like("stop").is_empty() && r.host.calls_like("hold").is_empty());
}

#[tokio::test(start_paused = true)]
async fn one_job_per_server_at_a_time() {
    let r = Arc::new(rig(spec(ink(), ink())));
    r.host.st.lock().unwrap().health_polls = 5;
    let r2 = r.clone();
    let first = tokio::spawn(async move { r2.orch.switch(req("c1", spec(ink(), dense()))).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(r.orch.busy());
    let second = r
        .orch
        .switch(req("merged-inkling-x2", spec(ink(), ink())))
        .await;
    let first = first.await.unwrap();
    assert_eq!(first.state, "done");
    // the second ran after the first ended and switched B back
    assert_eq!(second.state, "done");
    assert_eq!(second.from.as_deref(), Some("merged-inkling-x2"));
    assert_eq!(r.host.st.lock().unwrap().running["B"], ink());
}

#[test]
fn file_sink_writes_layoutctl_job_files() {
    let d = tempfile::tempdir().unwrap();
    let sink = FileJobSink {
        dir: d.path().to_path_buf(),
    };
    let mut j = JobRecord::new("combo-1", "ondemand:dense-stock", "on demand");
    j.steps.push(Step {
        t: 0.1,
        msg: "x".into(),
    });
    j.state = "done".into();
    j.from = Some("merged-inkling-x2".into());
    j.estimate_seconds = Some(90);
    j.ended = Some(j.started.clone());
    j.seconds = Some(56.1);
    j.new_oom_kills = Some(vec![]);
    j.live = Some("combo-1".into());
    sink.finish(&j);
    let f: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(d.path().join("jobs").join(format!("{}.json", j.id))).unwrap(),
    )
    .unwrap();
    let mut k: Vec<&String> = f.as_object().unwrap().keys().collect();
    k.sort();
    // the key set of a real layoutctl job file (2026-11-25)
    assert_eq!(
        k,
        [
            "created",
            "ended",
            "estimateSeconds",
            "from",
            "id",
            "layout",
            "live",
            "newOomKills",
            "profile",
            "reason",
            "requestedBy",
            "seconds",
            "started",
            "state",
            "steps"
        ]
    );
    let h = std::fs::read_to_string(d.path().join("history.jsonl")).unwrap();
    assert_eq!(h.lines().count(), 1);
}

// ── wiring to the control plane's config and router.db ─────────────────────────────────────

fn sample() -> crate::control::schema::ControlConfig {
    let p =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/control/sample.toml");
    crate::control::load::load(&p).unwrap().config
}

#[test]
fn switch_settings_and_targets_come_from_the_config() {
    let cfg = sample();
    let sc = wiring::switch_config(&cfg, "gpu");
    assert_eq!(sc.cards, ["A", "B"]); // C is not managed
    assert!(sc.engines["strata"].sequential);
    assert_eq!(sc.engines["strata"].share.as_deref(), Some("expert-arena"));
    assert_eq!(sc.engines["ninfer"].restart_if_peer_starts, ["strata"]);
    assert_eq!(sc.lease_settle, Duration::from_millis(1500));
    let flash = wiring::target_spec(&cfg, "gpu", "flash-x2", &[]).unwrap();
    assert_eq!(flash["B"], CardSpec::new("strata", "2slot", "flash", 2));
    let dense = wiring::target_spec(&cfg, "gpu", "dense-x2", &[]).unwrap();
    // flash -> dense restarts both; offline B stays off
    assert_eq!(restart_set(&sc, &flash, &dense), ["A", "B"]);
    let off = wiring::target_spec(&cfg, "gpu", "dense-a-small-b", &["B".into()]).unwrap();
    assert!(off["B"].is_off());
    assert_eq!(restart_set(&sc, &dense, &off), ["B"]);
    assert!(wiring::target_spec(&cfg, "gpu", "nope", &[]).is_none());
    assert_eq!(wiring::parse_window("06:45-07:30"), Some((405, 450)));
    assert_eq!(wiring::parse_window("25:00-07:30"), None);
}

#[test]
fn guards_from_the_config() {
    let cfg = sample();
    let (board, _cal) = wiring::guard_board(&cfg, "gpu");
    // music is a file probe: stale until the host probe reports, then blocks switches and ensure
    assert_eq!(board.blocks(Action::Switch, &[], NOW).len(), 1);
    board.report("music", NOW, Some("music mode is on".into()));
    assert_eq!(board.blocks(Action::Ensure, &[], NOW + 1.0).len(), 1);
    board.report("music", NOW + 2.0, None);
    assert!(board.blocks(Action::Switch, &[], NOW + 3.0).is_empty());
    // the quiet window is a calendar entry: 06:50 London on a winter day blocks switches
    let t = chrono::DateTime::parse_from_rfc3339("2026-12-01T06:50:00Z")
        .unwrap()
        .timestamp() as f64;
    board.report("music", t, None);
    let b = board.blocks(Action::Switch, &[], t);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].source, "quiet");
}

#[tokio::test(start_paused = true)]
async fn jobs_land_in_router_db_and_legacy_files() {
    let store = Arc::new(crate::control::store::Store::memory().unwrap());
    let d = tempfile::tempdir().unwrap();
    let sink = Arc::new(wiring::StoreJobSink::new(
        store.clone(),
        "gpu",
        Some(FileJobSink {
            dir: d.path().to_path_buf(),
        }),
    ));
    let mut r = rig(spec(ink(), ink()));
    r.orch.jobs = sink;
    let j = r.orch.switch(req("combo-1", spec(ink(), dense()))).await;
    assert_eq!(j.state, "done");
    let row = store.job(&j.id).unwrap().unwrap();
    assert_eq!(row.state, crate::control::store::JobState::Done);
    assert_eq!(row.target, "combo-1");
    let steps = store.steps(&j.id).unwrap();
    assert_eq!(steps.len(), j.steps.len());
    assert!(
        d.path()
            .join("jobs")
            .join(format!("{}.json", j.id))
            .exists()
    );
    assert!(d.path().join("history.jsonl").exists());
}

#[test]
fn leases_persist_in_router_db() {
    let store = crate::control::store::Store::memory().unwrap();
    crate::leases::store_migrate(&store).unwrap();
    crate::leases::store_migrate(&store).unwrap(); // idempotent
    let mut t = crate::leases::LeaseTable::default();
    let view = SlotView {
        slots: vec!["A1".into(), "B1".into()],
        cards: vec!["A".into(), "B".into()],
        ..SlotView::default()
    };
    let mut b = serde_json::Map::new();
    b.insert("key".into(), json!("k1"));
    t.claim(&view, &b, NOW, 90.0, Some("user:7"));
    t.claim_guard("bench-1", "bench", "bench", None, NOW, 90.0);
    crate::leases::store_save(&store, &t).unwrap();
    let back = crate::leases::store_load(&store).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.leases["A1"].principal.as_deref(), Some("user:7"));
}

#[test]
fn on_demand_settings_follow_the_flag() {
    let mut cfg = sample();
    let mut od = crate::config::OnDemandConfig::default();
    let before = od.clone();
    cfg.control.enabled = false;
    wiring::apply_on_demand(&cfg, "gpu", &mut od);
    assert_eq!(od, before); // flag off: untouched
    cfg.control.enabled = true;
    wiring::apply_on_demand(&cfg, "gpu", &mut od);
    assert!(od.control_gate && od.require_key);
    assert_eq!(od.max_switches_per_principal_per_hour, 2);
    assert_eq!(od.exclude_cards, ["C"]); // unmanaged
}

// ── on-demand through the in-process orchestrator ──────────────────────────────────────────

fn pick(m: &crate::config::OnDemandModel) -> Option<CardSpec> {
    match m.engine.as_str() {
        "ninfer" => Some(CardSpec::new("ninfer", &m.engine_profile, &m.model, 2)),
        "strata" => Some(strata(2)),
        _ => None,
    }
}

fn od_model(engine: &str, profile: &str, model: &str) -> crate::config::OnDemandModel {
    crate::config::OnDemandModel {
        model: model.into(),
        engine: engine.into(),
        engine_profile: profile.into(),
        served_id: String::new(),
        per_card: Default::default(),
    }
}

#[tokio::test(start_paused = true)]
async fn on_demand_runs_through_the_orchestrator() {
    use super::ondemand_switch::{Ask, InProcess, OnDemandSwitcher};
    let running = spec(ink(), ink());
    let r = rig(running.clone());
    let host = r.host;
    let sw = InProcess::new(
        Arc::new(r.orch),
        Arc::new(pick),
        Some("merged-inkling-x2".into()),
        running,
    );
    let want = od_model("ninfer", "merged-4lane", "dense-stock");
    let caller = Caller {
        principal: Some("service:jarvis#3".into()),
        grants: vec![],
    };
    let a = sw
        .ask("dense", "B", &want, "jarvis", caller.clone(), false)
        .await;
    let Ask::Started { job, .. } = a else {
        panic!("{a:?}")
    };
    assert_eq!(sw.job(&job).unwrap().0, "running");
    // while it runs, a second ask waits
    tokio::task::yield_now().await;
    let mut state = String::new();
    for _ in 0..2000 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        state = sw.job(&job).unwrap().0;
        if state != "running" {
            break;
        }
    }
    assert_eq!(state, "done");
    // only card B restarted; A untouched
    assert!(host.calls_like("stop A").is_empty(), "{:?}", host.calls());
    assert!(!host.calls_like("start B").is_empty(), "{:?}", host.calls());
    // the same ask again: already live
    assert!(matches!(
        sw.ask("dense", "B", &want, "jarvis", caller.clone(), false)
            .await,
        Ask::Already(_)
    ));
    // strata on B restarts both cards (share group): refused unless allowed
    let flash = od_model("strata", "2slot", "flash-next");
    let r = sw
        .ask("flash", "A", &flash, "jarvis", caller.clone(), false)
        .await;
    assert!(
        matches!(&r, Ask::Refused(m) if m.contains("restarts")) || matches!(r, Ask::Started { .. }),
        "{r:?}"
    );
    // an unknown engine profile is refused
    assert!(matches!(
        sw.ask("x", "B", &od_model("nope", "p", "m"), "j", caller, false)
            .await,
        Ask::Refused(_)
    ));
}

// ── one instance over two cards (tensor parallel) ───────────────────────────────────────────

/// The dense model as one server over A+B: A's spec spans B, B runs nothing of its own.
fn tp() -> CardSpec {
    let mut s = CardSpec::new("ninfer", "merged-tp-4lane", "dense-stock", 4);
    s.devices = vec!["A".into(), "B".into()];
    s
}

#[test]
fn a_spanning_instance_holds_its_cards() {
    let cfg = SwitchConfig::deepthought_like(&["A", "B"]);
    let off = CardSpec::off;
    // into and out of it, both cards move (B's own server goes or comes back)
    assert_eq!(
        restart_set(&cfg, &spec(ink(), ink()), &spec(tp(), off())),
        ["A", "B"]
    );
    assert_eq!(
        restart_set(&cfg, &spec(tp(), off()), &spec(ink(), ink())),
        ["A", "B"]
    );
    // to an A-only layout: just A
    assert_eq!(
        restart_set(&cfg, &spec(tp(), off()), &spec(ink(), off())),
        ["A"]
    );
    assert!(restart_set(&cfg, &spec(tp(), off()), &spec(tp(), off())).is_empty());
    // who holds what, and a spec that gives B twice
    assert_eq!(holder_of(&spec(tp(), off()), "B").as_deref(), Some("A"));
    assert_eq!(holder_of(&spec(tp(), off()), "A"), None);
    assert_eq!(holder_of(&spec(ink(), ink()), "B"), None);
    assert!(overlaps(&spec(tp(), off())).is_empty());
    assert_eq!(
        overlaps(&spec(tp(), ink())),
        ["card B is used by both A and B"]
    );
    // slots: one upstream with A's lanes, no B slots
    assert_eq!(
        slot_names(&cfg, &spec(tp(), off())),
        ["A1", "A2", "A3", "A4"]
    );
    assert_eq!(tp().cards_on("A"), ["A", "B"]);
    assert!(off().cards_on("B").is_empty());
}

#[tokio::test(start_paused = true)]
async fn adopted_at_start_the_slots_are_what_runs() {
    let r = rig(spec(ink(), ink()));
    r.orch
        .publish_slots(&spec(tp(), CardSpec::off()), Some("merged-dense-tp"));
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4"]);
    assert_eq!(v.layout.as_deref(), Some("merged-dense-tp"));
    assert!(!v.frozen);
}

#[tokio::test(start_paused = true)]
async fn switching_to_one_server_over_both_cards() {
    let r = rig(spec(ink(), ink()));
    // B's memory takes a while to drop after its server stops
    r.host
        .st
        .lock()
        .unwrap()
        .gpu_used
        .insert("B".into(), vec![30000, 9000, 200]);
    let j = r
        .orch
        .switch(req("merged-dense-tp", spec(tp(), CardSpec::off())))
        .await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    let c = r.host.calls();
    // the model files once, both servers stopped, both GPUs free before the one start
    assert_eq!(r.host.calls_like("restore"), ["restore A dense-stock"]);
    assert!(pos(&c, "stop A").is_some() && pos(&c, "stop B").is_some());
    assert_eq!(r.host.calls_like("gpu B").len(), 3);
    let start = pos(&c, "start A").unwrap();
    assert!(c[..start].iter().filter(|x| *x == "gpu B").count() == 3);
    assert!(pos(&c, "start B").is_none());
    assert_eq!(r.host.calls_like("chat"), ["chat A"]);
    // one upstream with 4 lanes: A1-A4, no B slots
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4"]);
    assert_eq!(
        r.plane.commits.lock().unwrap().last().unwrap().1,
        ["A".to_string()]
    );
    assert_eq!(r.host.st.lock().unwrap().running["A"].devices, ["A", "B"]);
}

#[tokio::test(start_paused = true)]
async fn switching_back_to_two_servers() {
    let r = rig(spec(tp(), CardSpec::off()));
    // the spanning server's memory on B goes with A's stop
    r.host
        .st
        .lock()
        .unwrap()
        .gpu_used
        .insert("B".into(), vec![15000, 100]);
    let j = r
        .orch
        .switch(req("merged-x2", spec(dense(), dense())))
        .await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    let c = r.host.calls();
    // B ran nothing of its own: only A is stopped; B starts once its GPU is free
    assert_eq!(r.host.calls_like("stop"), ["stop A"]);
    assert_eq!(r.host.calls_like("gpu B").len(), 2);
    assert!(pos(&c, "gpu B").unwrap() < pos(&c, "start B").unwrap());
    let mut chats = r.host.calls_like("chat");
    chats.sort();
    assert_eq!(chats, ["chat A", "chat B"]);
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "B1", "B2"]);
}

#[tokio::test(start_paused = true)]
async fn a_target_that_gives_a_card_twice_is_refused() {
    let r = rig(spec(ink(), ink()));
    let j = r.orch.switch(req("bad", spec(tp(), ink()))).await;
    assert_eq!(j.state, "refused");
    assert!(
        j.error
            .as_deref()
            .unwrap()
            .contains("card B is used by both A and B"),
        "{:?}",
        j.error
    );
    assert!(r.host.calls_like("stop").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_spanning_start_rolls_back_to_two_servers() {
    let r = rig(spec(ink(), ink()));
    r.host.st.lock().unwrap().exits.insert(
        ("A".into(), "dense-stock".into()),
        ("peer access failed".into(), None),
    );
    let j = r
        .orch
        .switch(req("merged-dense-tp", spec(tp(), CardSpec::off())))
        .await;
    assert_eq!(j.state, "rolled-back", "{}", steps(&j));
    let st = r.host.st.lock().unwrap();
    assert_eq!(st.running["A"], ink());
    assert_eq!(st.running["B"], ink());
    drop(st);
    let v = r.plane.view.lock().unwrap().clone();
    assert_eq!(v.slots, ["A1", "A2", "A3", "A4", "B1", "B2", "B3", "B4"]);
}

#[tokio::test(start_paused = true)]
async fn an_empty_layout_needs_allow_empty_and_an_admin_switch() {
    let off = || spec(CardSpec::off(), CardSpec::off());
    let r = rig(spec(tp(), CardSpec::off()));
    let j = r.orch.switch(req("cards-free", off())).await;
    assert_eq!(j.state, "refused", "{}", steps(&j));
    assert!(steps(&j).contains("would leave no card serving"));
    let mut od = req("cards-free", off());
    od.allow_empty = true;
    od.action = Action::OnDemand;
    let j = r.orch.switch(od).await;
    assert_eq!(
        j.state,
        "refused",
        "on demand never empties the cards: {}",
        steps(&j)
    );
    let mut ok = req("cards-free", off());
    ok.allow_empty = true;
    let j = r.orch.switch(ok).await;
    assert_eq!(j.state, "done", "{}", steps(&j));
    assert_eq!(r.host.calls_like("stop"), ["stop A"]);
}
