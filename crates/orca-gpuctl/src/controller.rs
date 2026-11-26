//! The per-card clock state machine hostd runs (SUPERVISOR.md 6.2-6.4).
//!
//! ```text
//!            idle for `after`, no hold, nothing in flight
//!   Serving ───────────────────────────────────────────▶ Low (-lgc 135,135)
//!      ▲                                                  │
//!      └──── wake: router admission · sniff · util ───────┘  (re-applies the serving policy,
//!            · procs · hold · admin · assert · NVML error      cap override included)
//! ```
//!
//! - **Serving** applies the card's `serving` policy: `unlocked` is `-rgc` (boost allowed, card A),
//!   `unlocked` with a cap or a cap override is `-lgc floor,cap`, `pin` is `-lgc n,n`, `range`
//!   is `-lgc lo,hi`.
//! - **Assert** (start, boot, engine `Ready`, admin, every `every:` period) re-checks persistence,
//!   power limit, app clocks and the hardware max clock, and re-applies the serving clocks.
//! - **Drift**: a Serving card that is busy, cool and not power/thermal limited, yet runs below
//!   its expected max for `drift.sustained`, raises `clock_drift` (and is fixed per
//!   `on_mismatch`).
//! - **External lock removal** while Low (something ran `-rgc`) is re-locked; three times in
//!   ten minutes and the card stands down for fifteen (gpu-power parity).
//!
//! Time is a `Duration` since any fixed origin, supplied by the caller, so tests drive it.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use serde::Serialize;

use crate::config::{self, CardPowerConfig, OnMismatch, Reassert, Serving, WakeSource};
use crate::limits::{self, Limits};
use crate::nvml::{GpuCommand, GpuControl, GpuReading, reasons};

const VERIFY_EVERY: Duration = Duration::from_secs(2);
const EXT_RESET_WINDOW: Duration = Duration::from_secs(600);
const EXT_RESET_MAX: usize = 3;
const BACKOFF: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Full control.
    On,
    /// Decide and report, never touch the hardware (gpu-power's dry-run).
    DryRun,
    /// No idle clock-down; clocks are left as they are.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Serving,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertReason {
    /// hostd started: apply serving from the live state (replaces gpu-power's startup reset).
    Start,
    Boot,
    EngineReady,
    Periodic,
    DriverReload,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    ClockMismatch,
    ClockDrift,
    ClockDriftCleared,
    LockRemovedExternally,
    BackingOff,
    CommandFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alert {
    pub kind: AlertKind,
    pub card: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssertOutcome {
    pub card: String,
    pub reason: AssertReason,
    pub applied: Vec<GpuCommand>,
    pub mismatches: Vec<String>,
    pub ok: bool,
    /// For the job step, e.g. "A unlocked (boost to 1597), PL 350 W ✓".
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ClockEvent {
    Lowered {
        card: String,
        lock_mhz: u32,
        idle_for_s: u64,
        dry_run: bool,
    },
    Woke {
        card: String,
        source: WakeSource,
        detail: String,
        dry_run: bool,
    },
    Applied {
        card: String,
        cmd: GpuCommand,
    },
    CapSet {
        card: String,
        cap_mhz: Option<u32>,
        ttl_s: u64,
    },
    CapExpired {
        card: String,
    },
    Asserted(AssertOutcome),
    Alert(Alert),
}

#[derive(Debug, Clone)]
pub struct CardState {
    pub id: String,
    pub uuid: String,
    pub cfg: CardPowerConfig,
    pub phase: Phase,
    /// The hardware is actually locked low by us (false in dry-run).
    pub applied_low: bool,
    pub low_since: Option<Duration>,
    pub last_work: Duration,
    /// reason -> expiry
    pub holds: BTreeMap<String, Duration>,
    /// (cap MHz, expiry)
    pub cap: Option<(u32, Duration)>,
    pub inflight: u32,
    pub engine_ports: Vec<u16>,
    next_periodic: Option<Duration>,
    drift_since: Option<Duration>,
    pub drift_alerted: bool,
    ext_resets: VecDeque<Duration>,
    pub backoff_until: Duration,
    next_verify: Duration,
    pub last_reading: Option<GpuReading>,
    pub last_assert: Option<AssertOutcome>,
    pub wakes: BTreeMap<&'static str, u64>,
    pub low_count: u64,
    pub low_secs: f64,
    pub saved_wh: f64,
    baseline_w: Option<f64>,
    last_sample: Option<Duration>,
}

impl CardState {
    fn new(id: String, uuid: String, cfg: CardPowerConfig, now: Duration) -> Self {
        let next_periodic = cfg.periodic().map(|p| now + p);
        CardState {
            id,
            uuid,
            cfg,
            phase: Phase::Serving,
            applied_low: false,
            low_since: None,
            last_work: now,
            holds: BTreeMap::new(),
            cap: None,
            inflight: 0,
            engine_ports: Vec::new(),
            next_periodic,
            drift_since: None,
            drift_alerted: false,
            ext_resets: VecDeque::new(),
            backoff_until: Duration::ZERO,
            next_verify: Duration::ZERO,
            last_reading: None,
            last_assert: None,
            wakes: BTreeMap::new(),
            low_count: 0,
            low_secs: 0.0,
            saved_wh: 0.0,
            baseline_w: None,
            last_sample: None,
        }
    }

    fn active_cap(&self, now: Duration) -> Option<u32> {
        self.cap.filter(|(_, until)| *until > now).map(|(c, _)| c)
    }

    pub fn hold_reasons(&self, now: Duration) -> Vec<&str> {
        self.holds
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(r, _)| r.as_str())
            .collect()
    }

    /// The clock command for serving right now (policy plus any cap override).
    pub fn serving_command(&self, now: Duration) -> GpuCommand {
        limits::serving_command(self.cfg.serving, self.cfg.floor(), self.active_cap(now))
    }

    /// The clock a busy, cool, unthrottled card should reach under the current policy.
    pub fn expected_max(&self, now: Duration) -> Option<u32> {
        let hw = self
            .cfg
            .expect_max_sm_mhz
            .or(self.last_reading.as_ref().and_then(|r| r.max_sm_mhz))?;
        let policy = match self.cfg.serving {
            Serving::Unlocked { cap } => cap,
            Serving::Pin(p) => Some(p),
            Serving::Range(_, hi) => Some(hi),
        };
        Some(
            [Some(hw), policy, self.active_cap(now)]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(hw),
        )
    }

    fn util_idle(&self) -> u32 {
        self.cfg.idle().map_or(0, |i| i.util_at_most)
    }

    fn describe_policy(&self, now: Duration) -> String {
        let cap = self
            .active_cap(now)
            .map(|c| format!(", capped {c} until override expires"))
            .unwrap_or_default();
        let base = match self.cfg.serving {
            Serving::Unlocked { cap: None } => match self.expected_max(now) {
                Some(m) => format!("unlocked (boost to {m})"),
                None => "unlocked".into(),
            },
            Serving::Unlocked { cap: Some(c) } => format!("unlocked, cap {c}"),
            Serving::Pin(p) => format!("pinned {p}"),
            Serving::Range(lo, hi) => format!("locked {lo}-{hi}"),
        };
        format!("{base}{cap}")
    }
}

/// The status row for `GET /api/v1/supervisor/cards` and hostd `status`.
#[derive(Debug, Clone, Serialize)]
pub struct CardStatus {
    pub card: String,
    pub uuid: String,
    pub serving: &'static str,
    pub phase: Phase,
    pub applied_low: bool,
    pub holds: Vec<String>,
    pub cap_mhz: Option<u32>,
    pub cap_offered: bool,
    pub inflight: u32,
    pub expected_max_mhz: Option<u32>,
    pub idle_for_s: u64,
    pub drift: bool,
    pub backing_off: bool,
    pub last_assert: Option<String>,
    pub wakes: BTreeMap<&'static str, u64>,
    pub low_count: u64,
    pub low_secs: u64,
    pub saved_wh: f64,
    pub reading: Option<GpuReading>,
}

pub struct Controller {
    pub mode: Mode,
    cards: BTreeMap<String, CardState>,
    out: Vec<ClockEvent>,
}

impl Controller {
    /// `cards`: (card id, GPU UUID, config). Every card starts Serving with its idle timer at
    /// `now`; call [`Controller::start`] to apply the policy.
    pub fn new(
        cards: impl IntoIterator<Item = (String, String, CardPowerConfig)>,
        mode: Mode,
        now: Duration,
    ) -> Self {
        Controller {
            mode,
            cards: cards
                .into_iter()
                .map(|(id, uuid, cfg)| (id.clone(), CardState::new(id, uuid, cfg, now)))
                .collect(),
            out: Vec::new(),
        }
    }

    pub fn card(&self, id: &str) -> Option<&CardState> {
        self.cards.get(id)
    }

    pub fn cards(&self) -> impl Iterator<Item = &CardState> {
        self.cards.values()
    }

    fn take(&mut self) -> Vec<ClockEvent> {
        std::mem::take(&mut self.out)
    }

    fn ids(&self, sel: &str) -> Vec<String> {
        if sel == "all" {
            self.cards.keys().cloned().collect()
        } else {
            self.cards.keys().filter(|k| *k == sel).cloned().collect()
        }
    }

    /// Sends one command, honouring the mode. Returns whether it reached the hardware cleanly.
    fn send(&mut self, gpu: &mut dyn GpuControl, id: &str, cmd: GpuCommand) -> bool {
        if self.mode != Mode::On {
            return false;
        }
        let uuid = self.cards[id].uuid.clone();
        match gpu.apply(&uuid, cmd) {
            Ok(()) => {
                self.out.push(ClockEvent::Applied {
                    card: id.to_string(),
                    cmd,
                });
                true
            }
            Err(e) => {
                self.alert(
                    AlertKind::CommandFailed,
                    id,
                    format!("{}: {e}", cmd.describe()),
                );
                false
            }
        }
    }

    fn alert(&mut self, kind: AlertKind, card: &str, message: String) {
        self.out.push(ClockEvent::Alert(Alert {
            kind,
            card: card.to_string(),
            message,
        }));
    }

    /// hostd start: every card to its serving policy (a previous instance may have left one low).
    pub fn start(&mut self, gpu: &mut dyn GpuControl, now: Duration) -> Vec<ClockEvent> {
        for id in self.ids("all") {
            let c = self.cards.get_mut(&id).expect("card");
            c.phase = Phase::Serving;
            c.applied_low = false;
            c.last_work = now;
            self.assert_inner(gpu, &id, AssertReason::Start, now);
        }
        self.take()
    }

    /// Reads every card from the driver, feeds the samples and runs the timers.
    pub fn poll(&mut self, gpu: &mut dyn GpuControl, now: Duration) -> Vec<ClockEvent> {
        for id in self.ids("all") {
            let uuid = self.cards[&id].uuid.clone();
            let r = gpu.read(&uuid);
            self.sample_inner(gpu, &id, now, r.ok());
        }
        self.tick_inner(gpu, now);
        self.take()
    }

    /// One sample of one card (`None`: the driver could not be read).
    pub fn sample(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        now: Duration,
        reading: Option<GpuReading>,
    ) -> Vec<ClockEvent> {
        if self.cards.contains_key(card) {
            self.sample_inner(gpu, card, now, reading);
        }
        self.take()
    }

    fn sample_inner(
        &mut self,
        gpu: &mut dyn GpuControl,
        id: &str,
        now: Duration,
        reading: Option<GpuReading>,
    ) {
        let util = reading.as_ref().and_then(|r| r.util_pct);
        let Some(util) = util else {
            // Never act blind: an unreadable card goes back to serving and its idle timer restarts.
            self.cards.get_mut(id).expect("card").last_work = now;
            self.wake_inner(gpu, id, WakeSource::Error, "NVML unreadable", now);
            return;
        };
        let r = reading.expect("reading");
        let mode = self.mode;
        let c = self.cards.get_mut(id).expect("card");
        let dt = c
            .last_sample
            .map_or(0.0, |t| now.saturating_sub(t).as_secs_f64().min(5.0));
        c.last_sample = Some(now);
        // Power bookkeeping (gpu-power's saved Wh).
        if let Some(p) = r.power_draw_w {
            match c.phase {
                Phase::Serving if util <= c.util_idle() => {
                    c.baseline_w = Some(c.baseline_w.map_or(p, |b| b * 0.9 + p * 0.1));
                }
                Phase::Low => {
                    c.low_secs += dt;
                    if c.applied_low
                        && let Some(b) = c.baseline_w
                    {
                        c.saved_wh += (b - p).max(0.0) * dt / 3600.0;
                    }
                }
                Phase::Serving => {}
            }
        }
        c.last_reading = Some(r.clone());
        let busy = util > c.util_idle();
        if busy {
            c.last_work = now;
        }
        if busy && c.phase == Phase::Low {
            self.wake_inner(gpu, id, WakeSource::Util, &format!("{util}%"), now);
            return;
        }
        // Low: has something else removed our lock?
        let c = self.cards.get_mut(id).expect("card");
        if c.phase == Phase::Low && c.applied_low && now >= c.next_verify {
            c.next_verify = now + VERIFY_EVERY;
            let lock = c
                .cfg
                .idle()
                .map_or(config::DEFAULT_FLOOR_MHZ, |i| i.lock_mhz);
            if r.sm_mhz.is_some_and(|s| s > lock + 50) {
                while c
                    .ext_resets
                    .front()
                    .is_some_and(|t| now.saturating_sub(*t) > EXT_RESET_WINDOW)
                {
                    c.ext_resets.pop_front();
                }
                c.ext_resets.push_back(now);
                if c.ext_resets.len() >= EXT_RESET_MAX {
                    c.ext_resets.clear();
                    c.backoff_until = now + BACKOFF;
                    self.alert(
                        AlertKind::BackingOff,
                        id,
                        format!("our idle lock keeps being removed by something else; standing down for {}", config::format_duration(BACKOFF)),
                    );
                    self.wake_inner(gpu, id, WakeSource::Hold, "backoff", now);
                } else {
                    let sm = r.sm_mhz.unwrap_or(0);
                    self.alert(
                        AlertKind::LockRemovedExternally,
                        id,
                        format!("idle lock removed externally (reads {sm} MHz); re-applied"),
                    );
                    self.send(
                        gpu,
                        id,
                        GpuCommand::LockGpuClocks {
                            min_mhz: lock,
                            max_mhz: lock,
                        },
                    );
                }
            }
            return;
        }
        if mode == Mode::On && c.phase == Phase::Serving {
            self.check_drift(gpu, id, &r, now);
        }
    }

    fn check_drift(&mut self, gpu: &mut dyn GpuControl, id: &str, r: &GpuReading, now: Duration) {
        let c = self.cards.get_mut(id).expect("card");
        let d = c.cfg.drift.clone();
        let (Some(util), Some(sm), Some(expected)) = (r.util_pct, r.sm_mhz, c.expected_max(now))
        else {
            return;
        };
        let busy = util >= d.busy_util_at_least;
        let cool = r.hottest_c().is_some_and(|t| t < d.cool_below_c);
        let excused = r.event_reasons.is_some_and(|x| x & reasons::EXCUSED != 0);
        if !busy {
            c.drift_since = None;
            return;
        }
        if sm + d.tolerance_mhz >= expected {
            c.drift_since = None;
            if c.drift_alerted {
                c.drift_alerted = false;
                self.alert(
                    AlertKind::ClockDriftCleared,
                    id,
                    format!("back at {sm} MHz (expected {expected})"),
                );
            }
            return;
        }
        if !cool || excused {
            c.drift_since = None;
            return;
        }
        let since = *c.drift_since.get_or_insert(now);
        if now.saturating_sub(since) >= d.sustained && !c.drift_alerted {
            c.drift_alerted = true;
            let fix = c.cfg.on_mismatch() == OnMismatch::Fix;
            let cmd = c.serving_command(now);
            let temp = r.hottest_c().unwrap_or(0);
            self.alert(
                AlertKind::ClockDrift,
                id,
                format!(
                    "busy ({util}%) and cool ({temp} °C) but at {sm} MHz, expected {expected}; {}",
                    if fix {
                        "re-applying the serving policy"
                    } else {
                        "on_mismatch is not fix, leaving it"
                    }
                ),
            );
            if fix {
                self.send(gpu, id, cmd);
            }
        }
    }

    /// Runs the timers: hold and cap expiry, periodic re-assert, idle clock-down.
    pub fn tick(&mut self, gpu: &mut dyn GpuControl, now: Duration) -> Vec<ClockEvent> {
        self.tick_inner(gpu, now);
        self.take()
    }

    fn tick_inner(&mut self, gpu: &mut dyn GpuControl, now: Duration) {
        for id in self.ids("all") {
            let c = self.cards.get_mut(&id).expect("card");
            c.holds.retain(|_, until| *until > now);
            if let Some((_, until)) = c.cap
                && until <= now
            {
                c.cap = None;
                self.out.push(ClockEvent::CapExpired { card: id.clone() });
                if self.cards[&id].phase == Phase::Serving {
                    let cmd = self.cards[&id].serving_command(now);
                    self.send(gpu, &id, cmd);
                }
            }
            let c = self.cards.get_mut(&id).expect("card");
            if let (Some(p), Some(due)) = (c.cfg.periodic(), c.next_periodic)
                && now >= due
            {
                c.next_periodic = Some(now + p);
                self.assert_inner(gpu, &id, AssertReason::Periodic, now);
            }
            let c = &self.cards[&id];
            let held = !c.hold_reasons(now).is_empty();
            if held && c.phase == Phase::Low {
                let why = c.hold_reasons(now).join(",");
                self.wake_inner(gpu, &id, WakeSource::Hold, &why, now);
                continue;
            }
            let Some(idle) = c.cfg.idle().cloned() else {
                continue;
            };
            if c.phase == Phase::Serving
                && self.mode != Mode::Off
                && !held
                && c.inflight == 0
                && c.backoff_until <= now
                && now.saturating_sub(c.last_work) >= idle.after
            {
                self.go_low(gpu, &id, idle.lock_mhz, now);
            }
        }
    }

    fn go_low(&mut self, gpu: &mut dyn GpuControl, id: &str, lock: u32, now: Duration) {
        let idle_for = now.saturating_sub(self.cards[id].last_work).as_secs();
        let dry = self.mode != Mode::On;
        if !dry
            && !self.send(
                gpu,
                id,
                GpuCommand::LockGpuClocks {
                    min_mhz: lock,
                    max_mhz: lock,
                },
            )
        {
            // gpu-power: cannot lock -> leave at max and retry after the back-off.
            let c = self.cards.get_mut(id).expect("card");
            c.backoff_until = now + BACKOFF;
            let cmd = c.serving_command(now);
            self.send(gpu, id, cmd);
            return;
        }
        let c = self.cards.get_mut(id).expect("card");
        c.phase = Phase::Low;
        c.applied_low = !dry;
        c.low_since = Some(now);
        c.low_count += 1;
        c.next_verify = now + VERIFY_EVERY;
        c.drift_since = None;
        self.out.push(ClockEvent::Lowered {
            card: id.to_string(),
            lock_mhz: lock,
            idle_for_s: idle_for,
            dry_run: dry,
        });
    }

    /// Work (or a hold) for a card: restart its idle timer and, if it is low, restore its serving
    /// policy. The router calls this with [`WakeSource::Router`] at admission.
    pub fn wake(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        source: WakeSource,
        detail: &str,
        now: Duration,
    ) -> Vec<ClockEvent> {
        for id in self.ids(card) {
            if matches!(
                source,
                WakeSource::Router
                    | WakeSource::Sniff
                    | WakeSource::Util
                    | WakeSource::Procs
                    | WakeSource::Admin
            ) {
                self.cards.get_mut(&id).expect("card").last_work = now;
            }
            self.wake_inner(gpu, &id, source, detail, now);
        }
        self.take()
    }

    fn wake_inner(
        &mut self,
        gpu: &mut dyn GpuControl,
        id: &str,
        source: WakeSource,
        detail: &str,
        now: Duration,
    ) {
        let c = &self.cards[id];
        if c.phase != Phase::Low {
            return;
        }
        if source.configurable() && c.cfg.idle().is_some_and(|i| !i.wake.contains(&source)) {
            return;
        }
        let applied = c.applied_low;
        let cmd = c.serving_command(now);
        if applied {
            self.send(gpu, id, cmd);
        }
        let c = self.cards.get_mut(id).expect("card");
        c.phase = Phase::Serving;
        c.applied_low = false;
        c.low_since = None;
        *c.wakes.entry(source.as_str()).or_default() += 1;
        self.out.push(ClockEvent::Woke {
            card: id.to_string(),
            source,
            detail: detail.to_string(),
            dry_run: !applied,
        });
    }

    /// The router's in-flight count for a card: while it is above zero the card never idles.
    pub fn set_inflight(&mut self, card: &str, n: u32, now: Duration) {
        if let Some(c) = self.cards.get_mut(card) {
            c.inflight = n;
            if n > 0 {
                c.last_work = now;
            }
        }
    }

    /// The ports an engine on this card listens on (the sniffer watches them plus `sniff_ports`).
    pub fn set_engine_ports(&mut self, card: &str, ports: Vec<u16>) {
        if let Some(c) = self.cards.get_mut(card) {
            c.engine_ports = ports;
        }
    }

    /// Port -> cards for the sniffer, for cards whose idle policy wakes on `sniff`.
    pub fn sniff_map(&self) -> BTreeMap<u16, Vec<String>> {
        let mut m: BTreeMap<u16, Vec<String>> = BTreeMap::new();
        for c in self.cards.values() {
            let Some(i) = c.cfg.idle().filter(|i| i.wake.contains(&WakeSource::Sniff)) else {
                continue;
            };
            for p in i.sniff_ports.iter().chain(&c.engine_ports) {
                let v = m.entry(*p).or_default();
                if !v.contains(&c.id) {
                    v.push(c.id.clone());
                }
            }
        }
        m
    }

    /// `hold {card|all, ttl, reason}`: no idle clock-down until it expires or is released.
    pub fn hold(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        reason: &str,
        ttl: Duration,
        now: Duration,
    ) -> Result<Vec<ClockEvent>, String> {
        if ttl.is_zero() || ttl > config::MAX_TTL {
            return Err(format!(
                "hold ttl must be between 1s and {}",
                config::format_duration(config::MAX_TTL)
            ));
        }
        let ids = self.ids(card);
        if ids.is_empty() {
            return Err(format!("unknown card {card}"));
        }
        for id in ids {
            self.cards
                .get_mut(&id)
                .expect("card")
                .holds
                .insert(reason.to_string(), now + ttl);
            self.wake_inner(gpu, &id, WakeSource::Hold, reason, now);
        }
        Ok(self.take())
    }

    pub fn release(&mut self, card: &str, reason: &str) {
        for id in self.ids(card) {
            self.cards.get_mut(&id).expect("card").holds.remove(reason);
        }
    }

    /// `override {card, clock_cap, ttl}` (the old HA "GPU Max Clock"). `cap = None` clears it.
    /// Refused on cards where the cap is not offered (card A), outside the limits, or with a
    /// TTL over the maximum.
    pub fn override_cap(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        cap: Option<u32>,
        ttl: Duration,
        limits: Option<&Limits>,
        now: Duration,
    ) -> Result<Vec<ClockEvent>, String> {
        let c = self
            .cards
            .get(card)
            .ok_or_else(|| format!("unknown card {card}"))?;
        if cap.is_some() && !c.cfg.cap_offered() {
            return Err(format!(
                "the clock cap is not offered for card {card} (allow_cap = false, serving {})",
                c.cfg.serving.kind()
            ));
        }
        match limits {
            Some(l) if cap.is_some() => l.check_ttl(ttl)?,
            _ if cap.is_some() && (ttl.is_zero() || ttl > config::MAX_TTL) => {
                return Err("cap ttl must be between 1s and 1h".into());
            }
            _ => {}
        }
        if let Some(m) = cap {
            if m < c.cfg.floor() {
                return Err(format!(
                    "cap {m} MHz is below the floor {} MHz",
                    c.cfg.floor()
                ));
            }
            let cmd = limits::serving_command(c.cfg.serving, c.cfg.floor(), Some(m));
            if let Some(l) = limits {
                l.check_command(&c.uuid, cmd)?;
            }
        }
        let c = self.cards.get_mut(card).expect("card");
        c.cap = cap.map(|m| (m, now + ttl));
        c.drift_since = None;
        c.drift_alerted = false;
        self.out.push(ClockEvent::CapSet {
            card: card.to_string(),
            cap_mhz: cap,
            ttl_s: ttl.as_secs(),
        });
        if self.cards[card].phase == Phase::Serving {
            let cmd = self.cards[card].serving_command(now);
            self.send(gpu, card, cmd);
        }
        Ok(self.take())
    }

    /// An engine on the card became `Ready` (switch step 10, reconcile): wake it and assert its
    /// policy. `None` when the card is unknown or does not re-assert on `engine_ready`.
    pub fn engine_ready(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        now: Duration,
    ) -> (Option<AssertOutcome>, Vec<ClockEvent>) {
        let Some(c) = self.cards.get_mut(card) else {
            return (None, Vec::new());
        };
        c.last_work = now;
        if !c.cfg.reasserts_on(&Reassert::EngineReady) {
            return (None, Vec::new());
        }
        let o = self.assert_inner(gpu, card, AssertReason::EngineReady, now);
        (Some(o), self.take())
    }

    /// `assert {card}`: re-check and fix clocks, power limit and persistence now.
    pub fn assert(
        &mut self,
        gpu: &mut dyn GpuControl,
        card: &str,
        reason: AssertReason,
        now: Duration,
    ) -> (Option<AssertOutcome>, Vec<ClockEvent>) {
        if !self.cards.contains_key(card) {
            return (None, Vec::new());
        }
        let o = self.assert_inner(gpu, card, reason, now);
        (Some(o), self.take())
    }

    fn assert_inner(
        &mut self,
        gpu: &mut dyn GpuControl,
        id: &str,
        reason: AssertReason,
        now: Duration,
    ) -> AssertOutcome {
        if matches!(reason, AssertReason::EngineReady | AssertReason::Admin) {
            self.wake_inner(gpu, id, WakeSource::Assert, "assert", now);
        }
        let cfg = self.cards[id].cfg.clone();
        let uuid = self.cards[id].uuid.clone();
        // Periodic checks follow on_mismatch; every other reason applies the policy.
        let fix = reason != AssertReason::Periodic || cfg.on_mismatch() == OnMismatch::Fix;
        let forced = matches!(
            reason,
            AssertReason::Start | AssertReason::Boot | AssertReason::DriverReload
        );
        let Ok(r) = gpu.read(&uuid) else {
            let o = AssertOutcome {
                card: id.into(),
                reason,
                applied: Vec::new(),
                mismatches: vec!["cannot read the card".into()],
                ok: false,
                summary: format!("{id} unreadable ✗"),
            };
            self.alert(
                AlertKind::ClockMismatch,
                id,
                "assert: cannot read the card".into(),
            );
            self.out.push(ClockEvent::Asserted(o.clone()));
            return o;
        };
        // (command, mismatch if any)
        let mut want: Vec<(GpuCommand, Option<String>)> = Vec::new();
        if cfg.persistence {
            want.push((
                GpuCommand::SetPersistence { on: true },
                (r.persistence != Some(true)).then(|| "persistence mode is off".into()),
            ));
        }
        if let Some(w) = cfg.power_limit_w {
            let m = match r.power_limit_w {
                Some(got) if (got - f64::from(w)).abs() > 0.5 => {
                    Some(format!("power limit is {got:.0} W, expected {w} W"))
                }
                None => Some("power limit unreadable".into()),
                _ => None,
            };
            want.push((GpuCommand::SetPowerLimit { watts: w }, m));
        }
        if let Some([mem, sm]) = cfg.app_clocks {
            let show = |v: Option<u32>| v.map_or("?".to_string(), |v| v.to_string());
            let m = (r.app_mem_mhz != Some(mem) || r.app_sm_mhz != Some(sm)).then(|| {
                format!(
                    "app clocks are {},{}, expected {mem},{sm}",
                    show(r.app_mem_mhz),
                    show(r.app_sm_mhz)
                )
            });
            want.push((
                GpuCommand::SetAppClocks {
                    mem_mhz: mem,
                    sm_mhz: sm,
                },
                m,
            ));
        }
        let mut applied = Vec::new();
        // (mismatch, fixed)
        let mut found: Vec<(String, bool)> = Vec::new();
        for (cmd, m) in want {
            let sent = ((m.is_some() && fix) || forced) && self.send(gpu, id, cmd);
            if sent {
                applied.push(cmd);
            }
            if let Some(m) = m {
                found.push((m, sent));
            }
        }
        if let (Some(exp), Some(max)) = (cfg.expect_max_sm_mhz, r.max_sm_mhz)
            && max < exp
        {
            found.push((format!("max SM clock is {max} MHz, expected {exp}"), false));
        }
        // The clock policy itself cannot be read back from the driver, so it is re-applied.
        if self.cards[id].phase == Phase::Serving && fix {
            let cmd = self.cards[id].serving_command(now);
            if self.send(gpu, id, cmd) {
                applied.push(cmd);
            }
        }
        for (m, fixed) in &found {
            let msg = if *fixed {
                format!("{m} (fixed)")
            } else {
                m.clone()
            };
            self.alert(AlertKind::ClockMismatch, id, msg);
        }
        let unresolved: Vec<&str> = found
            .iter()
            .filter(|(_, f)| !f)
            .map(|(m, _)| m.as_str())
            .collect();
        let ok = unresolved.is_empty()
            || (reason == AssertReason::Periodic && cfg.on_mismatch() == OnMismatch::Warn);
        let c = self.cards.get_mut(id).expect("card");
        c.last_reading = Some(r);
        let mut parts = vec![format!("{id} {}", c.describe_policy(now))];
        if let Some(w) = cfg.power_limit_w {
            parts.push(format!("PL {w} W"));
        }
        if let Some([m, s]) = cfg.app_clocks {
            parts.push(format!("app {m},{s}"));
        }
        if c.phase == Phase::Low {
            parts.push("idle-low".into());
        }
        if self.mode != Mode::On {
            parts.push(format!("{:?} mode, nothing applied", self.mode).to_lowercase());
        }
        let mark = if unresolved.is_empty() {
            "✓".to_string()
        } else {
            format!("✗ {}", unresolved.join("; "))
        };
        let o = AssertOutcome {
            card: id.into(),
            reason,
            applied,
            mismatches: found.into_iter().map(|(m, _)| m).collect(),
            ok,
            summary: format!("{} {mark}", parts.join(", ")),
        };
        self.cards.get_mut(id).expect("card").last_assert = Some(o.clone());
        self.out.push(ClockEvent::Asserted(o.clone()));
        o
    }

    /// Changes the mode. Every card that is low (really or virtually) goes back to serving first,
    /// as gpu-power does on a mode change.
    pub fn set_mode(
        &mut self,
        gpu: &mut dyn GpuControl,
        mode: Mode,
        now: Duration,
    ) -> Vec<ClockEvent> {
        if mode != self.mode {
            for id in self.ids("all") {
                self.wake_inner(gpu, &id, WakeSource::Hold, "mode change", now);
            }
            self.mode = mode;
        }
        self.take()
    }

    /// UUIDs this daemon holds low right now (persisted for the dead-man cron, gpu-power's
    /// `low.json`).
    pub fn lowered_uuids(&self) -> Vec<String> {
        self.cards
            .values()
            .filter(|c| c.applied_low)
            .map(|c| c.uuid.clone())
            .collect()
    }

    pub fn status(&self, now: Duration) -> Vec<CardStatus> {
        self.cards
            .values()
            .map(|c| CardStatus {
                card: c.id.clone(),
                uuid: c.uuid.clone(),
                serving: c.cfg.serving.kind(),
                phase: c.phase,
                applied_low: c.applied_low,
                holds: c.hold_reasons(now).into_iter().map(String::from).collect(),
                cap_mhz: c.active_cap(now),
                cap_offered: c.cfg.cap_offered(),
                inflight: c.inflight,
                expected_max_mhz: c.expected_max(now),
                idle_for_s: now.saturating_sub(c.last_work).as_secs(),
                drift: c.drift_alerted,
                backing_off: c.backoff_until > now,
                last_assert: c.last_assert.as_ref().map(|a| a.summary.clone()),
                wakes: c.wakes.clone(),
                low_count: c.low_count,
                low_secs: c.low_secs as u64,
                saved_wh: (c.saved_wh * 1000.0).round() / 1000.0,
                reading: c.last_reading.clone(),
            })
            .collect()
    }
}
