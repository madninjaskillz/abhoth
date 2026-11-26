//! Boot readiness, crash recovery and reconcile (ROUTER-CONFIG.md 5.6, 5.7).
//!
//! - **Boot readiness:** the server's `ready` hooks (docker up, mounts, GPUs visible, the
//!   middleware settled) must all pass before anything is started or judged missing.
//! - **Crash recovery:** jobs that were queued or running when the router stopped are marked
//!   `interrupted` and audited. Nothing is rolled back blindly: reconcile then looks at what
//!   really runs.
//! - **Reconcile:** compare what runs (observe) with what the active layout wants. Matching
//!   cards are adopted without a restart. A card that differs is reported, and when
//!   `switch_on_boot` allows it, the caller's [`Ensure`] is asked to switch, at most
//!   `tries_per_boot` times per card with the configured backoff. Reconcile **never stops an
//!   instance by itself**, and a card running something it cannot name is left alone
//!   (`unknown`) for an admin.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use super::driver::Cmd;
use super::exec::{Cancel, Executor};
use super::schema::ControlConfig;
use super::store::{JobState, Store};
use crate::switch::{HostOps, wiring};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Run the server's `ready` hooks until all pass or `ready_timeout_seconds` runs out.
pub async fn wait_ready(
    c: &ControlConfig,
    server: &str,
    exec: &dyn Executor,
    every: Duration,
    cancel: &Cancel,
) -> Result<(), String> {
    let s = c
        .servers
        .get(server)
        .ok_or_else(|| format!("no server {server}"))?;
    let end = std::time::Instant::now() + Duration::from_secs(s.ready_timeout_seconds);
    // A hook that keeps failing is retried less often (doubling, at most 2 minutes), so one
    // stuck check does not fill the host's audit log; it is logged when its text changes.
    let mut wait = every;
    let mut last = String::new();
    loop {
        let mut failing = Vec::new();
        for h in &s.ready {
            let timeout = Duration::from_secs(h.timeout.unwrap_or(30));
            let cmd = if let Some(b) = &h.builtin {
                let mut args = serde_json::Map::new();
                for (k, v) in &h.params {
                    args.insert(k.clone(), v.clone());
                }
                if b == "gpus-visible" && !args.contains_key("uuids") {
                    let uuids: Vec<String> = c
                        .cards
                        .values()
                        .filter(|x| x.server == server)
                        .map(|x| x.gpu_uuid.clone())
                        .collect();
                    args.insert("uuids".into(), serde_json::json!(uuids));
                }
                Cmd::builtin(
                    b,
                    serde_json::Value::Object(args),
                    b == "docker-up",
                    timeout,
                )
            } else if let Some(name) = &h.command {
                let Some(text) = s.templates.get(name) else {
                    failing.push(format!("{name}: no such template"));
                    continue;
                };
                Cmd::shell(&format!("template:{name}"), text.clone(), false, timeout)
            } else {
                continue;
            };
            match exec.run(cmd.request(server), cancel).await {
                Ok(o) if o.ok() => {}
                Ok(o) => failing.push(format!("{}: {}", cmd.id, o.summary())),
                Err(e) => failing.push(format!("{}: {e}", cmd.id)),
            }
        }
        if failing.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= end || cancel.is_cancelled() {
            return Err(format!("{server} not ready: {}", failing.join("; ")));
        }
        let now = failing.join("; ");
        if now != last {
            tracing::info!("{server} not ready yet: {now}");
            last = now;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        tokio::select! {
            _ = tokio::time::sleep(wait.min(left)) => {}
            _ = cancel.cancelled() => return Err("cancelled".into()),
        }
        wait = (wait * 2).min(Duration::from_secs(120).max(every));
    }
}

/// Mark jobs a previous router left unfinished as interrupted. Returns their ids.
pub fn recover_jobs(store: &Store) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for j in store.unfinished_jobs()? {
        store.update_job_state(
            &j.id,
            JobState::Interrupted,
            now_ms(),
            Some("the router stopped while the job ran"),
        )?;
        store.audit(
            now_ms(),
            "router",
            "job.interrupted",
            "",
            "",
            &j.id,
            &format!("{} {} -> {}", j.kind, j.from_layout, j.target),
        )?;
        ids.push(j.id);
    }
    Ok(ids)
}

/// What one card looks like to reconcile.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CardVerdict {
    /// Runs what the layout wants (adopted as is).
    InSync,
    /// Runs something else, or nothing, while the layout wants something.
    Differs { want: String, have: String },
    /// Runs an instance the config cannot name: left alone.
    Unknown { have: String },
    /// Offline by the admin: not judged.
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub server: String,
    pub layout: Option<String>,
    pub cards: Vec<(String, CardVerdict)>,
    /// Foreign GPU holders per card (a switch of that card would be refused).
    pub foreign: Vec<(String, Vec<String>)>,
    /// What reconcile did: `ensure <layout> (try n)`, or why it did not.
    pub actions: Vec<String>,
}

impl Report {
    pub fn in_sync(&self) -> bool {
        self.cards
            .iter()
            .all(|(_, v)| matches!(v, CardVerdict::InSync | CardVerdict::Offline))
    }
}

/// Asks the orchestrator to bring the server to a layout (an `ensure` job).
#[async_trait]
pub trait Ensure: Send + Sync {
    async fn ensure(&self, server: &str, layout: &str) -> Result<(), String>;
}

fn show(s: &crate::switch::CardSpec) -> String {
    if s.is_off() {
        "off".into()
    } else {
        format!("{}/{}/{}", s.engine, s.profile, s.model)
    }
}

/// One reconcile pass. `boot_id` scopes the try counters to this router boot.
pub async fn reconcile_once<H: HostOps>(
    c: &ControlConfig,
    server: &str,
    host: &H,
    store: &Store,
    ensure: Option<&dyn Ensure>,
    boot_id: &str,
) -> Result<Report, String> {
    let active = store.active_layout(server)?.map(|a| a.layout).or_else(|| {
        c.servers
            .get(server)
            .and_then(|s| s.state.as_ref())
            .and_then(|s| s.initial_layout.clone())
    });
    let offline = store.offline_cards()?;
    let observed = host.observe().await.map_err(|e| e.0)?;
    let mut rep = Report {
        server: server.into(),
        layout: active.clone(),
        cards: Vec::new(),
        foreign: observed.foreign.clone().into_iter().collect(),
        actions: Vec::new(),
    };
    let Some(layout) = active else {
        rep.actions
            .push("no active layout: nothing to compare".into());
        return Ok(rep);
    };
    let Some(target) = wiring::target_spec(c, server, &layout, &offline) else {
        rep.actions.push(format!(
            "active layout {layout} is not in the config: left alone"
        ));
        return Ok(rep);
    };
    let mut differing = Vec::new();
    for (card, want) in &target {
        let have = observed
            .spec
            .get(card)
            .cloned()
            .unwrap_or_else(crate::switch::CardSpec::off);
        let v = if offline.contains(card) {
            CardVerdict::Offline
        } else if have.detail.contains_key("unknown") {
            CardVerdict::Unknown {
                have: format!("{} ({})", have.engine, have.detail["unknown"]),
            }
        } else if have == *want {
            CardVerdict::InSync
        } else {
            differing.push(card.clone());
            CardVerdict::Differs {
                want: show(want),
                have: show(&have),
            }
        };
        rep.cards.push((card.clone(), v));
    }
    let r = &c.control.reconcile;
    if differing.is_empty() {
        for (card, _) in &rep.cards {
            store.reset_ensure(card, boot_id)?;
        }
        return Ok(rep);
    }
    let Some(ensure) =
        ensure.filter(|_| r.switch_on_boot && c.control.mode == super::schema::ControlMode::Act)
    else {
        rep.actions.push(format!(
            "cards {} differ from {layout}: reported only (observe mode or switch_on_boot off)",
            differing.join(", ")
        ));
        return Ok(rep);
    };
    // Backoff: the try count of the most-tried differing card decides.
    let mut tries = 0;
    let mut last = 0;
    for card in &differing {
        let (n, at) = store.ensure_tries(card, boot_id)?;
        if n >= tries {
            tries = n;
            last = at;
        }
    }
    if tries >= r.tries_per_boot {
        rep.actions.push(format!(
            "cards {} differ from {layout}: {tries} tries this boot, giving up until an admin acts",
            differing.join(", ")
        ));
        return Ok(rep);
    }
    if tries > 0 {
        let wait = r
            .backoff_seconds
            .get(tries as usize - 1)
            .or(r.backoff_seconds.last())
            .copied()
            .unwrap_or(60) as i64
            * 1000;
        if now_ms() - last < wait {
            rep.actions.push(format!(
                "cards {} differ from {layout}: next try after the backoff",
                differing.join(", ")
            ));
            return Ok(rep);
        }
    }
    for card in &differing {
        store.bump_ensure(card, boot_id, now_ms())?;
    }
    let res = ensure.ensure(server, &layout).await;
    rep.actions.push(match res {
        Ok(()) => format!("ensure {layout} (try {})", tries + 1),
        Err(e) => format!("ensure {layout} (try {}) failed: {e}", tries + 1),
    });
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::exec::{ExecOutput, FakeExecutor};
    use crate::control::validate::tests::sample;
    use crate::switch::fake::FakeHost;
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<String>>);
    #[async_trait]
    impl Ensure for Rec {
        async fn ensure(&self, server: &str, layout: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("{server}:{layout}"));
            Err("boom".into())
        }
    }

    fn act(mut c: ControlConfig) -> ControlConfig {
        c.control.mode = crate::control::schema::ControlMode::Act;
        c.control.reconcile.backoff_seconds = vec![0];
        c
    }

    #[tokio::test]
    async fn adopts_matching_cards_and_retries_others_a_bounded_number_of_times() {
        let c = act(sample());
        let store = Store::memory().unwrap();
        store
            .set_active_layout(&crate::control::store::ActiveLayout {
                server: "gpu".into(),
                layout: "dense-x2".into(),
                cards: Default::default(),
                since: 0,
                set_by: "test".into(),
                job_id: None,
            })
            .unwrap();
        let want = wiring::target_spec(&c, "gpu", "dense-x2", &[]).unwrap();
        // In sync: nothing to do.
        let host = FakeHost::with(want.clone());
        let ens = Rec(Mutex::new(vec![]));
        let r = reconcile_once(&c, "gpu", &host, &store, Some(&ens), "boot1")
            .await
            .unwrap();
        assert!(r.in_sync(), "{r:?}");
        assert!(ens.0.lock().unwrap().is_empty());
        assert!(
            !host.calls().iter().any(|c| c.starts_with("stop")),
            "reconcile never stops"
        );
        // Card B runs nothing: ensure is asked, up to tries_per_boot times.
        let mut half = want.clone();
        half.insert("B".into(), crate::switch::CardSpec::off());
        let host = FakeHost::with(half);
        for _ in 0..5 {
            let r = reconcile_once(&c, "gpu", &host, &store, Some(&ens), "boot1")
                .await
                .unwrap();
            assert!(!r.in_sync());
        }
        assert_eq!(
            ens.0.lock().unwrap().len() as u32,
            c.control.reconcile.tries_per_boot
        );
        // Observe mode only reports.
        let mut obs = c.clone();
        obs.control.mode = crate::control::schema::ControlMode::Observe;
        let r = reconcile_once(&obs, "gpu", &host, &store, Some(&ens), "boot2")
            .await
            .unwrap();
        assert!(r.actions[0].contains("reported only"));
    }

    #[tokio::test]
    async fn unknown_instances_are_left_alone() {
        let c = act(sample());
        let store = Store::memory().unwrap();
        let mut running = wiring::target_spec(&c, "gpu", "dense-x2", &[]).unwrap();
        let mut odd = crate::switch::CardSpec::new("ninfer", "", "", 0);
        odd.detail.insert("unknown".into(), "no label".into());
        running.insert("A".into(), odd);
        store
            .set_active_layout(&crate::control::store::ActiveLayout {
                server: "gpu".into(),
                layout: "dense-x2".into(),
                cards: Default::default(),
                since: 0,
                set_by: "t".into(),
                job_id: None,
            })
            .unwrap();
        let r = reconcile_once(&c, "gpu", &FakeHost::with(running), &store, None, "b")
            .await
            .unwrap();
        assert!(matches!(r.cards[0].1, CardVerdict::Unknown { .. }));
        assert!(r.actions.is_empty());
    }

    #[tokio::test]
    async fn readiness_and_recovery() {
        let c = sample();
        let fake = FakeExecutor::new();
        fake.on(
            "docker-up",
            vec![
                Ok(ExecOutput::fail(1, "down")),
                Ok(ExecOutput::ok_with("27")),
            ],
        );
        let mut c2 = c.clone();
        c2.servers.get_mut("gpu").unwrap().ready = vec![crate::control::schema::Hook {
            builtin: Some("docker-up".into()),
            ..Default::default()
        }];
        wait_ready(&c2, "gpu", &fake, Duration::from_millis(10), &Cancel::new())
            .await
            .unwrap();
        assert_eq!(
            fake.labels().iter().filter(|l| *l == "docker-up").count(),
            2
        );
        c2.servers.get_mut("gpu").unwrap().ready_timeout_seconds = 0;
        fake.on("docker-up", vec![Ok(ExecOutput::fail(1, "down"))]);
        let fake2 = FakeExecutor::new();
        fake2.on("docker-up", vec![Ok(ExecOutput::fail(1, "down"))]);
        assert!(
            wait_ready(
                &c2,
                "gpu",
                &fake2,
                Duration::from_millis(10),
                &Cancel::new()
            )
            .await
            .unwrap_err()
            .contains("not ready")
        );

        let store = Store::memory().unwrap();
        assert!(recover_jobs(&store).unwrap().is_empty());
    }
}
