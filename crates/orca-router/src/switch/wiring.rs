//! Wiring the orchestrator to the control plane's config (`router.toml`) and `router.db`:
//! switch settings and target specs from the schema, the guard board from `[guards]`, and a
//! job sink that writes `router.db` (jobs, steps) plus the legacy job files for the dash.

use std::sync::{Arc, Mutex};

use super::job::{FileJobSink, JobRecord, JobSink};
use super::{CardSpec, EngineRules, OnMismatch, Spec, SwitchConfig};
use crate::control::schema::{self, ControlConfig, GuardAction, GuardKind, StartOrder};
use crate::control::store::{JobRow, JobState, Store};
use crate::guards::{Action, CalendarEntry, GuardBoard, Scope, When};

/// The switch settings of `server` from the config.
pub fn switch_config(cfg: &ControlConfig, server: &str) -> SwitchConfig {
    let cards: Vec<String> = cfg
        .managed_cards(server)
        .into_iter()
        .map(String::from)
        .collect();
    let mut out = SwitchConfig::deepthought_like(&[]);
    out.server = server.to_string();
    out.cards = cards.clone();
    out.engines.clear();
    let mut stop_timeout = 0;
    let mut gpu_timeout = 0;
    let mut health_timeout = 0;
    let mut verify_timeout = 0;
    for (id, e) in &cfg.engines {
        let est = e
            .start_estimate_seconds
            .as_ref()
            .and_then(|p| cards.iter().filter_map(|c| p.get(c)).max().copied())
            .unwrap_or(75);
        out.engines.insert(
            id.clone(),
            EngineRules {
                sequential: e.start_order == StartOrder::Sequential,
                share: e.share.clone(),
                restart_if_peer_starts: e.restart_if_peer_starts.clone(),
                start_estimate_seconds: est,
            },
        );
        stop_timeout = stop_timeout.max(e.stop.timeout_seconds);
        gpu_timeout = gpu_timeout.max(e.stop.gpu_free_timeout_seconds);
        out.gpu_free_below_mib = e.stop.gpu_free_below_mib;
        health_timeout = health_timeout.max(e.health.timeout_seconds);
        verify_timeout = verify_timeout.max(e.verify.timeout_seconds);
    }
    let secs = std::time::Duration::from_secs;
    if stop_timeout > 0 {
        out.stop_timeout = secs(stop_timeout);
    }
    if gpu_timeout > 0 {
        out.gpu_free_timeout = secs(gpu_timeout);
    }
    if health_timeout > 0 {
        out.health_timeout = secs(health_timeout);
    }
    if verify_timeout > 0 {
        out.verify_timeout = secs(verify_timeout);
    }
    out.lease_settle = std::time::Duration::from_millis(cfg.slots.settle_ms);
    for c in &cards {
        if let Some(k) = cfg.cards.get(c).and_then(|c| c.clocks.as_ref()) {
            if let Some(m) = k.max_sm_mhz {
                out.max_sm_mhz.insert(c.clone(), m);
            }
            if k.on_mismatch == schema::ClockMismatch::Fail {
                out.clock_mismatch = OnMismatch::Fail;
            }
        }
    }
    if let Some(gp) = cfg.servers.get(server).and_then(|s| s.gpu_power.as_ref()) {
        out.hold_extra_seconds = gp.hold_extra_seconds;
    }
    out
}

/// A layout's target spec on `server`'s managed cards, with offline cards off. `None` for an
/// unknown layout.
pub fn target_spec(
    cfg: &ControlConfig,
    server: &str,
    layout: &str,
    offline: &[String],
) -> Option<Spec> {
    let cards = cfg.resolve_layout(layout)?;
    let mut out = Spec::new();
    for c in cfg.managed_cards(server) {
        let s = cards
            .iter()
            .find(|(k, _)| k == c)
            .and_then(|(_, s)| s.clone())
            .filter(|_| !offline.iter().any(|o| o == c));
        out.insert(
            c.to_string(),
            s.map(|s| spec_of(cfg, &s)).unwrap_or_else(CardSpec::off),
        );
    }
    Some(out)
}

/// The orchestrator's card spec for a schema card spec (lanes from the engine profile).
pub fn spec_of(cfg: &ControlConfig, s: &schema::CardSpec) -> CardSpec {
    let lanes = cfg
        .engines
        .get(&s.engine)
        .and_then(|e| e.profiles.get(&s.profile))
        .map(|p| p.lanes)
        .unwrap_or(1);
    CardSpec::new(&s.engine, &s.profile, &s.model, lanes)
}

fn action(a: GuardAction) -> Action {
    match a {
        GuardAction::Switch => Action::Switch,
        GuardAction::OnDemand => Action::OnDemand,
        GuardAction::Ensure => Action::Ensure,
        GuardAction::Offline => Action::Offline,
        GuardAction::Online => Action::Online,
    }
}

/// `HH:MM-HH:MM` -> minutes from midnight.
pub fn parse_window(w: &str) -> Option<(u32, u32)> {
    let (a, b) = w.split_once('-')?;
    let m = |t: &str| -> Option<u32> {
        let (h, mm) = t.trim().split_once(':')?;
        let (h, mm): (u32, u32) = (h.parse().ok()?, mm.parse().ok()?);
        (h < 24 && mm < 60).then_some(h * 60 + mm)
    };
    Some((m(a)?, m(b)?))
}

/// The guard board of `server` from `[guards]`: schedules become calendar entries, probes
/// (`process`, `json_file`, `file_exists`, `http`) become detectors the host probe reports
/// to. `lease` and `admin_hold` guards are evaluated from leases and holds at check time.
pub fn guard_board(
    cfg: &ControlConfig,
    server: &str,
) -> (GuardBoard, Arc<crate::guards::Calendar>) {
    let board = GuardBoard::default();
    let cal = Arc::new(crate::guards::Calendar::default());
    for (id, g) in &cfg.guards {
        if g.server.as_deref().is_some_and(|s| s != server) {
            continue;
        }
        let blocks: Vec<Action> = g.blocks.iter().copied().map(action).collect();
        match g.kind {
            GuardKind::Schedule => {
                for w in &g.windows {
                    if let Some((s, e)) = parse_window(w) {
                        cal.entries.lock().unwrap().push(CalendarEntry {
                            id: id.clone(),
                            reason: format!("{w} ({id}): no switches"),
                            blocks: blocks.clone(),
                            scope: Scope::All,
                            when: When::DailyLondon {
                                start_min: s,
                                end_min: e,
                            },
                            event: None,
                        });
                    }
                }
            }
            GuardKind::Process | GuardKind::JsonFile | GuardKind::FileExists | GuardKind::Http => {
                board.declare(id, blocks, Scope::All, g.retry_seconds);
            }
            GuardKind::Lease | GuardKind::AdminHold => {}
        }
    }
    board.add_guard(cal.clone());
    (board, cal)
}

fn epoch_of(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// Jobs into `router.db` (rows and steps), and the legacy job files while the mirror is on.
pub struct StoreJobSink {
    pub store: Arc<Store>,
    pub server: String,
    pub legacy: Option<FileJobSink>,
    written: Mutex<std::collections::HashMap<String, usize>>,
}

impl StoreJobSink {
    pub fn new(store: Arc<Store>, server: &str, legacy: Option<FileJobSink>) -> Self {
        StoreJobSink {
            store,
            server: server.to_string(),
            legacy,
            written: Mutex::new(Default::default()),
        }
    }

    fn row(&self, j: &JobRecord) -> JobRow {
        JobRow {
            id: j.id.clone(),
            server: self.server.clone(),
            kind: "switch".into(),
            target: j.layout.clone(),
            from_layout: j.from.clone().unwrap_or_default(),
            requested_by: j.requested_by.clone(),
            state: JobState::parse(&j.state).unwrap_or(JobState::Running),
            created: epoch_of(&j.created),
            started: Some(epoch_of(&j.started)),
            ended: j.ended.as_deref().map(epoch_of),
            plan_json: None,
            error: j.error.clone(),
        }
    }
}

impl JobSink for StoreJobSink {
    fn save(&self, j: &JobRecord) {
        let mut w = self.written.lock().unwrap();
        let done = w.entry(j.id.clone()).or_insert_with(|| {
            if let Err(e) = self.store.insert_job(&self.row(j)) {
                tracing::warn!("router.db: job {}: {e}", j.id);
            }
            0
        });
        let base = epoch_of(&j.started);
        for s in &j.steps[*done..] {
            let at = base + s.t as i64;
            if let Ok(seq) = self.store.step_begin(&j.id, &s.msg, "", at) {
                let _ = self.store.step_end(&j.id, seq, "done", at, "");
            }
        }
        *done = j.steps.len();
        drop(w);
        if let Some(l) = &self.legacy {
            l.save(j);
        }
    }

    fn finish(&self, j: &JobRecord) {
        self.save(j);
        let r = self.row(j);
        if let Err(e) = self.store.update_job_state(
            &j.id,
            r.state,
            r.ended.unwrap_or(r.created),
            r.error.as_deref(),
        ) {
            tracing::warn!("router.db: job {}: {e}", j.id);
        }
        if let Some(l) = &self.legacy {
            l.finish(j);
        }
    }
}

/// The data plane's on-demand settings that the control plane owns: the gate follows
/// `control.enabled` (keys required, hourly limits, `x-abhoth-no-switch`), and cards that are
/// unmanaged or not `on_demand = true` are never picked. With the flag off nothing changes.
pub fn apply_on_demand(cfg: &ControlConfig, server: &str, od: &mut crate::config::OnDemandConfig) {
    if !cfg.control.enabled {
        return;
    }
    let s = &cfg.on_demand;
    od.control_gate = true;
    od.require_key = true;
    od.max_switches_per_hour = s.max_switches_per_hour;
    od.max_switches_per_principal_per_hour = s.max_switches_per_principal_per_hour;
    od.queue_seconds = s.queue_seconds;
    od.min_stay_seconds = s.min_stay_seconds;
    od.idle_grace_seconds = s.idle_grace_seconds;
    od.switch_timeout_seconds = s.switch_timeout_seconds;
    od.failure_hold_seconds = s.failure_hold_seconds;
    od.allow_both_cards = s.allow_both_cards;
    od.stream_note = s.stream_note.clone();
    od.exclude_cards = cfg
        .cards
        .iter()
        .filter(|(_, c)| c.server == server && !(c.managed && c.on_demand))
        .map(|(k, _)| k.clone())
        .collect();
}
