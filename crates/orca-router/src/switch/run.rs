//! The switch job (ROUTER-CONFIG.md 5.4), one per server at a time.

use std::sync::Arc;

use tokio::time::Instant;

use super::job::{JobRecord, JobSink, Step, now_iso};
use super::{
    CardSpec, Health, HostOps, OnMismatch, Plane, Spec, SwitchConfig, estimate, restart_set,
    slot_names,
};
use crate::guards::{Action, Caller};
use crate::leases::SlotView;

/// What to switch to and why.
#[derive(Debug, Clone)]
pub struct SwitchRequest {
    /// Layout name (a catalog layout or a composed one).
    pub layout: String,
    /// The target, already overlaid with offline cards (`engine = "off"`).
    pub target: Spec,
    /// The active layout's name, if the running cards are a named layout.
    pub active: Option<String>,
    pub requested_by: String,
    pub reason: String,
    /// Who asked (event-key grants may lift that event's reservations).
    pub caller: Caller,
    /// `Switch` for admin switches, `OnDemand` for on-demand ones (guards re-read here).
    pub action: Action,
}

enum Fail {
    /// Nothing was changed.
    Refused(String),
}

struct Run<'a> {
    job: JobRecord,
    t0: Instant,
    sink: &'a dyn JobSink,
}

impl Run<'_> {
    fn step(&mut self, msg: impl Into<String>) {
        let t = (self.t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let msg = msg.into();
        tracing::info!(job = %self.job.id, "[{t:7.1}s] {msg}");
        self.job.steps.push(Step { t, msg });
        self.sink.save(&self.job);
    }

    fn secs(&self) -> f64 {
        (self.t0.elapsed().as_secs_f64() * 10.0).round() / 10.0
    }
}

/// The orchestrator of one server.
pub struct Orchestrator<H: HostOps, P: Plane> {
    pub cfg: SwitchConfig,
    pub host: H,
    pub plane: P,
    pub jobs: Arc<dyn JobSink>,
    one: tokio::sync::Mutex<()>,
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl<H: HostOps, P: Plane> Orchestrator<H, P> {
    pub fn new(cfg: SwitchConfig, host: H, plane: P, jobs: Arc<dyn JobSink>) -> Self {
        Orchestrator {
            cfg,
            host,
            plane,
            jobs,
            one: tokio::sync::Mutex::new(()),
        }
    }

    /// Is a job running now?
    pub fn busy(&self) -> bool {
        self.one.try_lock().is_err()
    }

    fn view(
        &self,
        spec: &Spec,
        frozen: bool,
        layout: Option<&str>,
        reason: Option<String>,
    ) -> SlotView {
        SlotView {
            slots: slot_names(&self.cfg, spec),
            frozen,
            layout: layout.map(str::to_string),
            reason,
            cards: self.cfg.cards.clone(),
        }
    }

    fn serving(&self, spec: &Spec) -> Vec<String> {
        self.cfg
            .cards
            .iter()
            .filter(|c| spec.get(*c).is_some_and(|s| !s.is_off()))
            .cloned()
            .collect()
    }

    /// Run one switch to the end. Behaviours kept from `layoutctl.py switch`:
    /// - refused (nothing touched) on failed preconditions, guards (re-read for on-demand),
    ///   foreign GPU holders on a changing card, a failed model restore, held leases, the lock;
    /// - model files are restored from the archive **before** anything stops;
    /// - all cards moving: claims are frozen first; some: only their slots are withdrawn and
    ///   the other cards keep serving and leasing; leases are checked after the settle time;
    /// - gpu-power is held for the estimate + 120 s; the host locks are held while applying;
    /// - stop: retried until the instance is gone (a zombie container that takes a while to
    ///   die), then GPU memory under 1 GiB; sequential engines start first, one card at a
    ///   time, each healthy; health fails fast when the process exits (its log tail kept);
    /// - verify: a chat only on the cards that restarted, `/health` on the others; the clocks
    ///   of restarted cards (A must still reach 1597 MHz);
    /// - any failure rolls back to the previous spec with the same steps; OOM kills during
    ///   the job are recorded; a failed rollback ends `failed` with what the host shows, and
    ///   only healthy cards stay routable.
    pub async fn switch(&self, req: SwitchRequest) -> JobRecord {
        let _one = self.one.lock().await;
        let mut r = Run {
            job: JobRecord::new(&req.layout, &req.requested_by, &req.reason),
            t0: Instant::now(),
            sink: self.jobs.as_ref(),
        };
        r.sink.save(&r.job);
        match self.run(&mut r, &req).await {
            Ok(()) => {}
            Err(Fail::Refused(e)) => {
                r.job.state = "refused".into();
                r.job.error = Some(e.clone());
                r.job.ended = Some(now_iso());
                r.step(format!("refused: {e}"));
            }
        }
        self.jobs.finish(&r.job);
        r.job
    }

    async fn run(&self, r: &mut Run<'_>, req: &SwitchRequest) -> Result<(), Fail> {
        // 1. plan from what runs now
        let obs = self
            .host
            .observe()
            .await
            .map_err(|e| Fail::Refused(format!("cannot read the host's state: {e}")))?;
        let prev = obs.spec.clone();
        let prev_name = req.active.clone().filter(|_| !prev.is_empty());
        r.job.from = prev_name.clone();
        let moving = restart_set(&self.cfg, &prev, &req.target);
        r.job.estimate_seconds = Some(estimate(&self.cfg, &req.target, &moving));
        for c in &moving {
            if let Some(f) = obs.foreign.get(c).filter(|f| !f.is_empty()) {
                return Err(Fail::Refused(format!(
                    "card {c} is held by {} (not a layout instance)",
                    f.join(", ")
                )));
            }
        }
        self.host
            .preconditions(&req.target)
            .await
            .map_err(Fail::Refused)?;
        if self.serving(&req.target).is_empty() && !self.serving(&prev).is_empty() {
            return Err(Fail::Refused(format!(
                "{} would leave no card serving",
                req.layout
            )));
        }
        // 2. guards, with fresh data
        let blocks = self
            .plane
            .blocks(req.action, &moving, &req.caller, &req.layout);
        if !blocks.is_empty() {
            let why: Vec<String> = blocks.iter().map(|b| b.reason.clone()).collect();
            return Err(Fail::Refused(why.join("; ")));
        }
        // 3. model files first: nothing is stopped or frozen while they copy
        for c in &moving {
            if let Some(t) = req.target.get(c).filter(|t| !t.is_off()) {
                let msgs = self.host.restore_models(c, t).await.map_err(|e| {
                    Fail::Refused(format!("card {c}: model files could not be restored: {e}"))
                })?;
                for m in msgs {
                    r.step(m);
                }
            }
        }
        if moving.is_empty() {
            r.step(format!(
                "{} is already live: nothing to restart",
                req.layout
            ));
            self.plane
                .set_slots(self.view(&req.target, false, Some(&req.layout), None));
            self.plane
                .commit_routes(Some(&req.layout), &req.target, &self.serving(&req.target));
            r.job.state = "done".into();
            r.job.already = Some(true);
            r.job.seconds = Some(0.0);
            r.job.ended = Some(now_iso());
            r.job.live = Some(req.layout.clone());
            return Ok(());
        }
        // 4. withdraw or freeze, 5. then look at leases (race-free)
        let all = self.cfg.cards.iter().all(|c| moving.contains(c));
        let undo_view = self.view(&prev, false, prev_name.as_deref(), None);
        if all {
            self.plane.set_slots(self.view(
                &prev,
                true,
                prev_name.as_deref(),
                Some(format!("switching to {}", req.layout)),
            ));
        } else {
            let mut partial = prev.clone();
            for c in &moving {
                partial.insert(c.clone(), CardSpec::off());
            }
            self.plane.set_slots(self.view(
                &partial,
                false,
                prev_name.as_deref(),
                Some(format!(
                    "switching card(s) {} to {}",
                    moving.join(", "),
                    req.layout
                )),
            ));
        }
        tokio::time::sleep(self.cfg.lease_settle).await;
        let held = self.plane.held_on(&moving);
        if !held.is_empty() {
            self.plane.set_slots(undo_view);
            let h: Vec<String> = held.iter().map(|(s, w)| format!("{s} ({w})")).collect();
            return Err(Fail::Refused(format!(
                "pig-slots leases held: {}",
                h.join(", ")
            )));
        }
        let stay: Vec<String> = self
            .cfg
            .cards
            .iter()
            .filter(|c| !moving.contains(c))
            .cloned()
            .collect();
        r.step(format!(
            "pig-slots: no leases on card(s) {}{}; switching {} -> {} (estimate {}s)",
            moving.join(", "),
            if all {
                " (frozen)".to_string()
            } else {
                format!(
                    " (their slots withdrawn; {} keeps serving)",
                    stay.join(", ")
                )
            },
            prev_name.as_deref().unwrap_or("custom"),
            req.layout,
            r.job.estimate_seconds.unwrap_or(0)
        ));
        // 6. drain: moving cards leave routing
        self.plane.withdraw_routes(&moving);
        // gpu-power: full clocks for the verify chat and the first requests
        let hold = r.job.estimate_seconds.unwrap_or(0) + self.cfg.hold_extra_seconds;
        if let Err(e) = self.host.gpu_power_hold(hold).await {
            r.step(format!("gpu-power: could not hold ({e}); going on"));
        }
        // 7. lock
        if let Err(e) = self.host.lock().await {
            self.host.gpu_power_release().await;
            self.plane.set_slots(undo_view);
            self.plane
                .commit_routes(prev_name.as_deref(), &prev, &self.serving(&prev));
            return Err(Fail::Refused(format!("the host locks are held: {e}")));
        }
        r.step("holding the host locks (watchdog)");
        let k0 = self.host.oom_kills().await.unwrap_or_default();
        // 8-10. apply and verify; any failure rolls back
        let outcome = match self.apply(r, &prev, &req.target, &moving).await {
            Ok(restarted) => self.verify(r, &req.target, &restarted).await,
            Err(e) => Err(e),
        };
        let (state, final_name, final_spec): (&str, Option<String>, Option<Spec>) = match outcome {
            Ok(()) => ("done", Some(req.layout.clone()), Some(req.target.clone())),
            Err(e) => {
                r.job.error = Some(cut(&e, 800));
                r.step(format!("FAILED: {}", cut(&e, 300)));
                r.step(format!(
                    "rolling back to {}",
                    prev_name.as_deref().unwrap_or("the previous cards")
                ));
                match self.rollback(r, &prev).await {
                    Ok(()) => ("rolled-back", prev_name.clone(), Some(prev.clone())),
                    Err(e2) => {
                        r.job.rollback_error = Some(cut(&e2, 800));
                        r.step(format!("ROLLBACK FAILED: {}", cut(&e2, 300)));
                        let now = self.host.observe().await.ok().map(|o| o.spec);
                        let name = now.as_ref().and_then(|s| {
                            if *s == req.target {
                                Some(req.layout.clone())
                            } else if *s == prev {
                                prev_name.clone()
                            } else {
                                None
                            }
                        });
                        ("failed", name, now)
                    }
                }
            }
        };
        // 11. commit what runs
        match &final_spec {
            Some(spec) => {
                let ready = if state == "failed" {
                    let mut ok = Vec::new();
                    for c in self.serving(spec) {
                        if self.host.health(&c, &spec[&c]).await == Health::Ok {
                            ok.push(c);
                        }
                    }
                    ok
                } else {
                    self.serving(spec)
                };
                let mut routable = spec.clone();
                for c in self.cfg.cards.iter() {
                    if !ready.contains(c) {
                        routable.insert(c.clone(), CardSpec::off());
                    }
                }
                self.plane
                    .set_slots(self.view(&routable, false, final_name.as_deref(), None));
                self.plane
                    .commit_routes(final_name.as_deref(), spec, &ready);
            }
            None => {
                // the host cannot be read (SSH down): moving cards stay out, the rest serve
                let mut keep = prev.clone();
                for c in &moving {
                    keep.insert(c.clone(), CardSpec::off());
                }
                self.plane.set_slots(self.view(&keep, false, None, None));
                self.plane.commit_routes(None, &keep, &self.serving(&keep));
            }
        }
        // 12. post
        let kills = self.host.oom_kills().await.unwrap_or_default();
        let new: Vec<String> = kills.into_iter().filter(|k| !k0.contains(k)).collect();
        self.host.unlock().await;
        self.host.gpu_power_release().await;
        let secs = r.secs();
        r.job.state = state.into();
        r.job.seconds = Some(secs);
        r.job.ended = Some(now_iso());
        r.job.live = final_name.clone();
        let n = new.len();
        r.job.new_oom_kills = Some(new.into_iter().rev().take(5).rev().collect());
        r.step(format!(
            "{state}: live layout {} after {secs}s{}",
            final_name.as_deref().unwrap_or("custom"),
            if n > 0 {
                format!("; NEW OOM KILLS: {n}")
            } else {
                String::new()
            }
        ));
        Ok(())
    }

    async fn rollback(&self, r: &mut Run<'_>, prev: &Spec) -> Result<(), String> {
        let now = self
            .host
            .observe()
            .await
            .map_err(|e| format!("cannot read the host's state: {e}"))?;
        let back = restart_set(&self.cfg, &now.spec, prev);
        let restarted = self.apply(r, &now.spec, prev, &back).await?;
        self.verify(r, prev, &restarted).await
    }

    /// Stop the moving cards, wait for their GPU memory, start them. Returns the started cards.
    async fn apply(
        &self,
        r: &mut Run<'_>,
        cur: &Spec,
        target: &Spec,
        moving: &[String],
    ) -> Result<Vec<String>, String> {
        let off = CardSpec::off();
        for c in moving {
            let was = cur.get(c).unwrap_or(&off);
            if was.is_off() {
                continue;
            }
            r.step(format!("card {c}: stopping {} ({})", was.engine, was.model));
            let end = Instant::now() + self.cfg.stop_timeout;
            let mut tries = 0;
            loop {
                tries += 1;
                match self.host.stop_once(c, was).await {
                    Ok(true) => break,
                    Ok(false) if Instant::now() < end => {
                        tokio::time::sleep(self.cfg.stop_retry_every).await;
                    }
                    Ok(false) => {
                        return Err(format!(
                            "card {c}: the {} instance is still there after {} tries",
                            was.engine, tries
                        ));
                    }
                    Err(e) => return Err(format!("card {c}: stop failed: {e}")),
                }
            }
            if tries > 1 {
                r.step(format!("card {c}: gone after {tries} tries"));
            }
        }
        for c in moving {
            if target.get(c).is_none_or(|t| t.is_off()) {
                continue;
            }
            let end = Instant::now() + self.cfg.gpu_free_timeout;
            loop {
                let used = self
                    .host
                    .gpu_used_mib(c)
                    .await
                    .map_err(|e| format!("card {c}: cannot read GPU memory: {e}"))?;
                if used < self.cfg.gpu_free_below_mib {
                    break;
                }
                if Instant::now() >= end {
                    return Err(format!(
                        "card {c} still has {used} MiB in use after stopping its server"
                    ));
                }
                tokio::time::sleep(self.cfg.stop_retry_every).await;
            }
        }
        let starts: Vec<String> = moving
            .iter()
            .filter(|c| target.get(*c).is_some_and(|t| !t.is_off()))
            .cloned()
            .collect();
        let (seq, par): (Vec<String>, Vec<String>) = starts
            .iter()
            .cloned()
            .partition(|c| self.cfg.rules(&target[c].engine).sequential);
        for c in &seq {
            self.start_one(r, c, &target[c]).await?;
            self.wait_healthy(r, std::slice::from_ref(c), target)
                .await?;
        }
        for c in &par {
            self.start_one(r, c, &target[c]).await?;
        }
        self.wait_healthy(r, &par, target).await?;
        Ok(starts)
    }

    async fn start_one(&self, r: &mut Run<'_>, c: &str, t: &CardSpec) -> Result<(), String> {
        for m in self
            .host
            .before_start(c, t)
            .await
            .map_err(|e| format!("card {c}: before-start hook failed: {e}"))?
        {
            r.step(m);
        }
        r.step(format!(
            "card {c}: starting {} ({}, {} lanes, {})",
            t.engine, t.model, t.lanes, t.profile
        ));
        self.host
            .start(c, t)
            .await
            .map_err(|e| format!("card {c}: start failed: {e}"))
    }

    async fn wait_healthy(
        &self,
        r: &mut Run<'_>,
        cards: &[String],
        spec: &Spec,
    ) -> Result<(), String> {
        let t0 = Instant::now();
        let end = t0 + self.cfg.health_timeout;
        let mut pending: Vec<String> = cards.to_vec();
        while !pending.is_empty() {
            let mut left = Vec::new();
            for c in &pending {
                match self.host.health(c, &spec[c]).await {
                    Health::Ok => r.step(format!(
                        "card {c}: healthy after {:.0}s",
                        t0.elapsed().as_secs_f64()
                    )),
                    Health::Exited(tail) => {
                        return Err(format!(
                            "card {c}: exited while starting: {}",
                            cut(&tail, 600)
                        ));
                    }
                    Health::NotYet(_) => left.push(c.clone()),
                }
            }
            pending = left;
            if pending.is_empty() {
                break;
            }
            if Instant::now() >= end {
                return Err(format!(
                    "not healthy after {}s: cards {}",
                    self.cfg.health_timeout.as_secs(),
                    pending.join(", ")
                ));
            }
            tokio::time::sleep(self.cfg.health_every).await;
        }
        Ok(())
    }

    /// The chat only on restarted cards (a busy card that kept running would queue it behind
    /// long generations), `/health` on the others; then clocks of the restarted cards.
    async fn verify(
        &self,
        r: &mut Run<'_>,
        spec: &Spec,
        restarted: &[String],
    ) -> Result<(), String> {
        let mut bad = Vec::new();
        for c in self.serving(spec) {
            let s = &spec[&c];
            if restarted.contains(&c) {
                let t0 = Instant::now();
                match tokio::time::timeout(self.cfg.verify_timeout, self.host.verify_chat(&c, s))
                    .await
                {
                    Ok(Ok(())) => r.step(format!(
                        "card {c}: healthy, chat ok in {:.1}s",
                        t0.elapsed().as_secs_f64()
                    )),
                    Ok(Err(e)) => {
                        r.step(format!("card {c}: chat: {e}"));
                        bad.push(format!("{c}: chat: {e}"));
                    }
                    Err(_) => {
                        let e = format!(
                            "chat timed out after {}s",
                            self.cfg.verify_timeout.as_secs()
                        );
                        r.step(format!("card {c}: {e}"));
                        bad.push(format!("{c}: {e}"));
                    }
                }
            } else {
                match self.host.health(&c, s).await {
                    Health::Ok => r.step(format!("card {c}: healthy")),
                    h => {
                        let d = format!("{h:?}");
                        r.step(format!("card {c}: {d}"));
                        bad.push(format!("{c}: {d}"));
                    }
                }
            }
        }
        for c in restarted {
            let Some(want) = self.cfg.max_sm_mhz.get(c).copied() else {
                continue;
            };
            match self.host.clocks(c).await {
                Ok(k) if k.max_sm_mhz >= want => {
                    r.step(format!("card {c}: clocks ok (max {} MHz)", k.max_sm_mhz));
                }
                Ok(k) => {
                    let m = format!(
                        "card {c}: clocks capped at {} MHz (expected {want})",
                        k.max_sm_mhz
                    );
                    match self.cfg.clock_mismatch {
                        OnMismatch::Warn => r.step(format!("{m}; warning only")),
                        OnMismatch::Fail => {
                            r.step(m.clone());
                            bad.push(m);
                        }
                    }
                }
                Err(e) => r.step(format!("card {c}: clocks unreadable ({e})")),
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(format!("verify failed: {}", bad.join("; ")))
        }
    }
}
