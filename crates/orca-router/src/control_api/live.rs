//! The admin API over the real control plane: `router.toml` ([`crate::control::load`],
//! [`crate::control::validate`]) and `router.db` ([`crate::control::store`]).
//!
//! Reads (state, config, jobs, audit, routes) and config saves (check, backup, atomic write,
//! audit) are here. Switches, plans, leases and card offline/online belong to the orchestrator
//! and slots (C5-C7); until those are plugged in with [`ConfigBackend::with_actions`] those
//! calls answer 501 and change nothing. Telemetry is filled by the same hook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::broadcast;

use super::model::*;
use super::{ApiError, ApiResult, ControlBackend, Event};
use crate::control::load::{self, Loaded};
use crate::control::schema::{ColdPolicy, ControlConfig, LayoutCard, Target};
use crate::control::store::Store;
use crate::control::validate::{self, Issue, Level};

/// What the orchestrator, slots and telemetry add (plugged in by their owners).
#[async_trait::async_trait]
pub trait Actions: Send + Sync {
    /// Fill live data into a state built from config + store (telemetry, in-flight, leases).
    fn decorate(&self, _state: &mut StateView) {}
    fn leases(&self) -> LeasesView {
        LeasesView::default()
    }
    async fn plan(&self, server: &str, layout: &str) -> ApiResult<Plan>;
    async fn switch(&self, server: &str, layout: &str, who: &str) -> ApiResult<Job>;
    async fn cancel_job(&self, id: &str, who: &str) -> ApiResult<Job>;
    async fn set_card(&self, card: &str, state: &str, reason: &str, who: &str) -> ApiResult<Job>;
    async fn release_lease(&self, key: &str, who: &str) -> ApiResult<()>;
}

pub struct ConfigBackend {
    main: PathBuf,
    backups: PathBuf,
    store: Arc<Store>,
    loaded: Mutex<Option<Loaded>>,
    error: Mutex<Option<String>>,
    tx: broadcast::Sender<Event>,
    actions: Option<Arc<dyn Actions>>,
    /// Serialises saves (check + write + audit as one step).
    save_lock: tokio::sync::Mutex<()>,
}

fn not_built() -> ApiError {
    ApiError::new(
        501,
        "the orchestrator is not plugged in yet; nothing was changed",
    )
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn problem(i: &Issue) -> Problem {
    Problem {
        path: i.path.clone(),
        line: i.line.map(|l| l as u32),
        message: i.message.clone(),
        severity: match i.level {
            Level::Error => "error",
            Level::Warning => "warning",
        }
        .into(),
    }
}

/// A line diff (`-old`/`+new`), enough for the audit and the save dialog.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // LCS table (config files are small).
    let (n, m) = (a.len(), b.len());
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, String::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < m && (i == n || t[i][j + 1] > t[i + 1][j]) {
            out.push_str(&format!("+{}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{}\n", a[i]));
            i += 1;
        }
    }
    out
}

fn target_text(t: &Target) -> String {
    let base = if let Some(p) = &t.pool {
        match &t.engine {
            Some(e) => format!("pool:{p} ({e})"),
            None => format!("pool:{p}"),
        }
    } else if let Some(c) = &t.card {
        format!("card:{c}")
    } else if let Some(u) = &t.upstream {
        format!("upstream:{u}/{}", t.model.clone().unwrap_or_default())
    } else {
        format!("model:{}", t.model.clone().unwrap_or_default())
    };
    if t.fallback {
        format!("{base} (fallback)")
    } else {
        base
    }
}

fn cold_text(c: &ColdPolicy) -> &'static str {
    match c {
        ColdPolicy::Reject => "reject",
        ColdPolicy::Fallback => "fallback",
        ColdPolicy::OnDemand => "on_demand",
    }
}

fn spec_text(c: &LayoutCard) -> String {
    match c {
        LayoutCard::Word(w) => w.clone(),
        LayoutCard::Spec(s) if !s.devices.is_empty() => format!(
            "{} / {} / {} (over {})",
            s.engine,
            s.profile,
            s.model,
            s.devices.join("+")
        ),
        LayoutCard::Spec(s) => format!("{} / {} / {}", s.engine, s.profile, s.model),
    }
}

impl ConfigBackend {
    /// `main`: router.toml; `state_dir`: where `backups/` goes.
    pub fn new(main: &Path, state_dir: &Path, store: Arc<Store>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        let me = Arc::new(Self {
            main: main.to_path_buf(),
            backups: state_dir.join("backups"),
            store,
            loaded: Mutex::new(None),
            error: Mutex::new(None),
            tx,
            actions: None,
            save_lock: tokio::sync::Mutex::new(()),
        });
        me.reload();
        me
    }

    pub fn with_actions(
        main: &Path,
        state_dir: &Path,
        store: Arc<Store>,
        actions: Arc<dyn Actions>,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        let me = Arc::new(Self {
            main: main.to_path_buf(),
            backups: state_dir.join("backups"),
            store,
            loaded: Mutex::new(None),
            error: Mutex::new(None),
            tx,
            actions: Some(actions),
            save_lock: tokio::sync::Mutex::new(()),
        });
        me.reload();
        me
    }

    /// Publish an event (the orchestrator's job steps, telemetry ticks).
    pub fn publish(&self, e: Event) {
        let _ = self.tx.send(e);
    }

    /// Re-read the file (watcher, SIGHUP); an invalid file keeps the old config.
    pub fn reload(&self) {
        match load::load(&self.main) {
            Ok(l) => {
                let errs: Vec<String> = validate::validate_loaded(&l)
                    .iter()
                    .filter(|i| i.level == Level::Error)
                    .map(|i| i.to_string())
                    .collect();
                if errs.is_empty() {
                    let rev = l.rev.clone();
                    // The bridges' overlay and the media routes read the saved file too.
                    if let Some(h) = crate::bridges::installed() {
                        h.set_base(Arc::new(l.config.clone()));
                    }
                    *self.loaded.lock().unwrap() = Some(l);
                    *self.error.lock().unwrap() = None;
                    self.publish(Event {
                        kind: "config",
                        data: json!({"rev": rev}),
                    });
                } else {
                    *self.error.lock().unwrap() = Some(errs.join("\n"));
                }
            }
            Err(issues) => {
                *self.error.lock().unwrap() = Some(
                    issues
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
    }

    fn config_of(&self) -> Option<(ControlConfig, String)> {
        self.loaded
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| (l.config.clone(), l.rev.clone()))
    }

    fn main_text(&self) -> String {
        std::fs::read_to_string(&self.main).unwrap_or_default()
    }

    fn backup(&self, old_text: &str, old_rev: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.backups).map_err(|e| format!("backups: {e}"))?;
        let name = format!(
            "router.toml.bak-{}-{old_rev}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        std::fs::write(self.backups.join(name), old_text).map_err(|e| format!("backup: {e}"))?;
        // keep the last 100
        if let Ok(rd) = std::fs::read_dir(&self.backups) {
            let mut v: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("router.toml.bak-"))
                })
                .collect();
            v.sort();
            if v.len() > 100 {
                for p in &v[..v.len() - 100] {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ControlBackend for ConfigBackend {
    fn state(&self) -> StateView {
        let mut st = StateView {
            config_error: self.error.lock().unwrap().clone(),
            updated: now_ms(),
            ..Default::default()
        };
        let Some((c, rev)) = self.config_of() else {
            return st;
        };
        st.rev = rev;
        st.mode = format!("{:?}", c.control.mode).to_lowercase();
        let instances = self.store.instances(None).unwrap_or_default();
        for (sid, s) in &c.servers {
            let active = self.store.active_layout(sid).ok().flatten();
            let cards = c
                .cards
                .iter()
                .filter(|(_, card)| &card.server == sid)
                .map(|(cid, card)| {
                    let admin = self.store.card_admin(cid).ok();
                    let inst = instances
                        .iter()
                        .find(|i| &i.card == cid && &i.server == sid);
                    let own = inst.filter(|i| !i.engine.is_empty());
                    // A card another card's instance spans (tensor parallel) shows that one.
                    let holder = instances.iter().find(|i| {
                        &i.server == sid
                            && &i.card != cid
                            && !i.engine.is_empty()
                            && i.spec
                                .as_ref()
                                .is_some_and(|s| s.devices.iter().any(|d| d == cid))
                    });
                    let held_by = own.is_none().then_some(holder).flatten();
                    CardView {
                        id: cid.clone(),
                        server: sid.clone(),
                        gpu_uuid: card.gpu_uuid.clone(),
                        label: card.label.clone(),
                        managed: card.managed,
                        admin_state: if card.managed {
                            admin
                                .as_ref()
                                .map(|a| a.state.as_str().to_string())
                                .unwrap_or_else(|| "online".into())
                        } else {
                            "unmanaged".into()
                        },
                        admin_note: admin
                            .as_ref()
                            .filter(|a| !a.reason.is_empty())
                            .map(|a| format!("{} ({})", a.reason, a.by))
                            .or_else(|| {
                                held_by.map(|h| {
                                    format!("held by card {} (one server over both)", h.card)
                                })
                            }),
                        engine: own.or(held_by).map(|i| EngineView {
                            engine: i.engine.clone(),
                            profile: i
                                .spec
                                .as_ref()
                                .map(|s| s.profile.clone())
                                .unwrap_or_default(),
                            model: i.spec.as_ref().map(|s| s.model.clone()).unwrap_or_default(),
                            served_id: String::new(),
                            state: if held_by.is_some_and(|h| std::ptr::eq(h, i)) {
                                format!("held by {}", i.card)
                            } else {
                                i.state.as_str().to_string()
                            },
                            since: i.since * 1000,
                            healthy: !i.unhealthy,
                        }),
                        telemetry: None,
                        lanes: 0,
                        inflight: 0,
                        queued: 0,
                        leases: 0,
                        on_demand: card.on_demand,
                        foreign: vec![],
                        device: if card.is_cpu() { "cpu" } else { "gpu" }.into(),
                        ram_gib: card.ram_gib,
                        cores: card.cores,
                    }
                })
                .collect();
            st.servers.push(ServerView {
                id: sid.clone(),
                title: if s.title.is_empty() {
                    sid.clone()
                } else {
                    s.title.clone()
                },
                reachability: "unknown".into(),
                exec: format!("{:?}", s.exec).to_lowercase(),
                active_layout: active.as_ref().map(|a| a.layout.clone()),
                drift: false,
                hostctl_ok: true,
                cards,
            });
            for (lid, l) in &c.layouts {
                if l.hidden {
                    continue;
                }
                let on_server = l
                    .cards
                    .keys()
                    .any(|k| c.cards.get(k).is_some_and(|cd| &cd.server == sid));
                if !on_server {
                    continue;
                }
                st.layouts.push(LayoutView {
                    id: lid.clone(),
                    title: l.title.clone(),
                    server: sid.clone(),
                    mode: l.mode.clone().unwrap_or_default(),
                    cards: l
                        .cards
                        .iter()
                        .map(|(k, v)| (k.clone(), spec_text(v)))
                        .collect::<BTreeMap<_, _>>(),
                    active: active.as_ref().is_some_and(|a| &a.layout == lid),
                    source: l.source.clone().unwrap_or_else(|| "config".into()),
                });
            }
        }
        for (gid, g) in &c.guards {
            st.guards.push(GuardView {
                id: gid.clone(),
                kind: serde_json::to_value(g.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                active: false,
                stale: false,
                blocks: g
                    .blocks
                    .iter()
                    .filter_map(|b| serde_json::to_value(b).ok()?.as_str().map(str::to_string))
                    .collect(),
                reason: None,
                retry_seconds: g.retry_seconds,
            });
        }
        if let Ok(jobs) = self.store.jobs(None, 20)
            && let Some(j) = jobs.iter().find(|j| !j.state.finished())
        {
            let steps = self.store.steps(&j.id).unwrap_or_default();
            st.job = Some(JobSummary {
                id: j.id.clone(),
                server: j.server.clone(),
                kind: j.kind.clone(),
                target: j.target.clone(),
                state: j.state.as_str().into(),
                step: steps.len() as u32,
                steps: 12,
                title: steps.last().map(|s| s.name.clone()).unwrap_or_default(),
                started: j.started.unwrap_or(j.created) * 1000,
                estimate_seconds: 0,
            });
        }
        if let Some(a) = &self.actions {
            a.decorate(&mut st);
        }
        st
    }

    fn config(&self) -> ConfigDoc {
        let rev = self
            .loaded
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.rev.clone())
            .unwrap_or_default();
        ConfigDoc {
            rev,
            path: self.main.display().to_string(),
            text: self.main_text(),
            error: self.error.lock().unwrap().clone(),
        }
    }

    fn jobs(&self, limit: usize) -> Vec<Job> {
        self.store
            .jobs(None, limit)
            .unwrap_or_default()
            .into_iter()
            .map(|j| self.job(&j.id).unwrap_or_default())
            .collect()
    }

    fn job(&self, id: &str) -> Option<Job> {
        let j = self.store.job(id).ok()??;
        let steps = self.store.steps(id).unwrap_or_default();
        Some(Job {
            id: j.id.clone(),
            server: j.server.clone(),
            kind: j.kind.clone(),
            from: (!j.from_layout.is_empty()).then(|| j.from_layout.clone()),
            target: j.target.clone(),
            requested_by: j.requested_by.clone(),
            state: j.state.as_str().into(),
            started: j.started.unwrap_or(j.created) * 1000,
            ended: j.ended.map(|e| e * 1000),
            error: j.error.clone(),
            steps: steps
                .iter()
                .enumerate()
                .map(|(i, s)| JobStep {
                    n: i as u32 + 1,
                    title: if s.card.is_empty() {
                        s.name.clone()
                    } else {
                        format!("{} ({})", s.name, s.card)
                    },
                    state: s.state.clone(),
                    seconds: s.ended.map(|e| (e - s.started) as f64).unwrap_or(0.0),
                    output: (!s.detail.is_empty()).then(|| s.detail.clone()),
                })
                .collect(),
        })
    }

    fn audit(&self, limit: usize, before: Option<i64>) -> Vec<AuditRow> {
        self.store
            .audit_rows(limit + before.map_or(0, |_| 1000))
            .unwrap_or_default()
            .into_iter()
            .filter(|a| before.is_none_or(|b| a.id < b))
            .take(limit)
            .map(|a| AuditRow {
                id: a.id,
                time: a.ts * 1000,
                who: a.who,
                target: if a.action.starts_with("config") {
                    "router.toml".into()
                } else {
                    String::new()
                },
                action: a.action,
                rev_from: (!a.old_rev.is_empty()).then_some(a.old_rev),
                rev_to: (!a.new_rev.is_empty()).then_some(a.new_rev),
                diff: (!a.diff.is_empty()).then_some(a.diff),
                reason: (!a.reason.is_empty()).then_some(a.reason),
            })
            .collect()
    }

    fn leases(&self) -> LeasesView {
        self.actions
            .as_ref()
            .map(|a| a.leases())
            .unwrap_or_default()
    }

    fn routes(&self) -> Vec<RouteView> {
        let Some((c, _)) = self.config_of() else {
            return vec![];
        };
        c.routes
            .iter()
            .map(|(name, r)| {
                let base = r
                    .same_as
                    .as_ref()
                    .and_then(|s| c.routes.get(s))
                    .unwrap_or(r);
                RouteView {
                    name: name.clone(),
                    targets: base.targets.iter().map(target_text).collect(),
                    cold: cold_text(r.cold.as_ref().unwrap_or(&c.route_defaults.cold)).into(),
                    fallback: r.fallback.clone(),
                    access: r
                        .access
                        .clone()
                        .unwrap_or_else(|| c.route_defaults.access.clone()),
                    adapters: r
                        .adapters
                        .clone()
                        .unwrap_or_else(|| c.route_defaults.adapters.clone()),
                    hidden: r.hidden,
                    same_as: r.same_as.clone(),
                    serving_now: vec![],
                    allow_user_switch: r.allow_user_switch,
                    model: base
                        .targets
                        .iter()
                        .find(|t| t.upstream.is_none() && t.pool.is_none() && t.card.is_none())
                        .and_then(|t| t.model.clone()),
                    vision: crate::control::resolve::route_vision(&c, r),
                }
            })
            .collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn check_config(&self, text: &str) -> CheckResult {
        let current = self.main_text();
        let diff = line_diff(&current, text);
        match load::load_with(&self.main, text.to_string()) {
            Err(issues) => CheckResult {
                ok: false,
                problems: issues.iter().map(problem).collect(),
                diff,
                ..Default::default()
            },
            Ok(l) => {
                let issues = validate::validate_loaded(&l);
                let ok = !validate::has_errors(&issues);
                let mut consequences = Vec::new();
                if let Some((old, _)) = self.config_of() {
                    let changed_routes: Vec<&String> = l
                        .config
                        .routes
                        .iter()
                        .filter(|(k, v)| old.routes.get(*k) != Some(*v))
                        .map(|(k, _)| k)
                        .chain(
                            old.routes
                                .keys()
                                .filter(|k| !l.config.routes.contains_key(*k)),
                        )
                        .collect();
                    if !changed_routes.is_empty() {
                        consequences.push(format!(
                            "routes changed for new requests: {}",
                            changed_routes
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    for (sid, _) in &l.config.servers {
                        if let Ok(Some(a)) = self.store.active_layout(sid)
                            && old.layouts.get(&a.layout) != l.config.layouts.get(&a.layout)
                        {
                            consequences.push(format!(
                                "the active layout {} of {sid} changed: it shows as drift with a plan; nothing restarts",
                                a.layout
                            ));
                        }
                    }
                }
                CheckResult {
                    ok,
                    problems: issues.iter().map(problem).collect(),
                    diff,
                    consequences,
                    plan: None,
                    rev: Some(l.rev),
                }
            }
        }
    }

    async fn save_config(
        &self,
        text: &str,
        if_match: &str,
        reason: &str,
        who: &str,
    ) -> ApiResult<CheckResult> {
        let _g = self.save_lock.lock().await;
        // The file may have been edited by hand since the last load: compare with disk.
        let disk = self.main_text();
        let disk_rev = load::load_with(&self.main, disk.clone())
            .map(|l| l.rev)
            .unwrap_or_default();
        if if_match != disk_rev {
            return Err(ApiError {
                status: 412,
                message: "the file changed since you loaded it; reload and merge".into(),
                extra: Some(json!({"currentRev": disk_rev})),
            });
        }
        let check = self.check_config(text).await;
        if !check.ok {
            return Err(ApiError {
                status: 422,
                message: "the config has errors; nothing was saved".into(),
                extra: Some(json!({"problems": check.problems})),
            });
        }
        self.backup(&disk, &disk_rev)
            .map_err(|e| ApiError::new(500, format!("{e}; nothing was saved")))?;
        let tmp = self.main.with_extension("toml.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &self.main))
            .map_err(|e| ApiError::new(500, format!("could not write the config: {e}")))?;
        let new_rev = check.rev.clone().unwrap_or_default();
        let _ = self.store.audit(
            chrono::Utc::now().timestamp(),
            who,
            "config.save",
            &disk_rev,
            &new_rev,
            &check.diff,
            reason,
        );
        self.reload();
        Ok(check)
    }

    async fn plan(&self, server: &str, layout: &str) -> ApiResult<Plan> {
        match &self.actions {
            Some(a) => a.plan(server, layout).await,
            None => Err(not_built()),
        }
    }
    async fn switch(&self, server: &str, layout: &str, who: &str) -> ApiResult<Job> {
        match &self.actions {
            Some(a) => a.switch(server, layout, who).await,
            None => Err(not_built()),
        }
    }
    async fn cancel_job(&self, id: &str, who: &str) -> ApiResult<Job> {
        match &self.actions {
            Some(a) => a.cancel_job(id, who).await,
            None => Err(not_built()),
        }
    }
    async fn set_card(&self, card: &str, state: &str, reason: &str, who: &str) -> ApiResult<Job> {
        match &self.actions {
            Some(a) => a.set_card(card, state, reason, who).await,
            None => Err(not_built()),
        }
    }
    async fn release_lease(&self, key: &str, who: &str) -> ApiResult<()> {
        match &self.actions {
            Some(a) => a.release_lease(key, who).await,
            None => Err(not_built()),
        }
    }

    fn record(
        &self,
        who: &str,
        action: &str,
        target: &str,
        old_rev: &str,
        new_rev: &str,
        diff: &str,
        reason: &str,
    ) {
        let reason = if target.is_empty() {
            reason.to_string()
        } else if reason.is_empty() {
            target.to_string()
        } else {
            format!("{target}: {reason}")
        };
        let _ = self.store.audit(
            chrono::Utc::now().timestamp(),
            who,
            action,
            old_rev,
            new_rev,
            diff,
            &reason,
        );
        self.publish(Event {
            kind: "audit",
            data: json!({"who": who, "action": action}),
        });
    }

    /// Edits one route in the file with `toml_edit` (comments and order kept), through the same
    /// check, backup and audit as a full save.
    async fn edit_route(&self, name: &str, edit: &RouteEdit, who: &str) -> ApiResult<RouteView> {
        let text = self.main_text();
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| ApiError::new(500, format!("router.toml does not parse: {e}")))?;
        let routes = doc
            .get_mut("routes")
            .and_then(|r| r.as_table_like_mut())
            .ok_or_else(|| {
                ApiError::new(
                    404,
                    "no routes in the main file (included files are edited in the Config page)",
                )
            })?;
        let route = routes
            .get_mut(name)
            .and_then(|r| r.as_table_like_mut())
            .ok_or_else(|| ApiError::new(404, "no such route in the main file"))?;
        if let Some(c) = &edit.cold {
            if !["reject", "fallback", "on_demand"].contains(&c.as_str()) {
                return Err(ApiError::new(
                    422,
                    "cold must be reject, fallback or on_demand",
                ));
            }
            route.insert("cold", toml_edit::value(c.as_str()));
        }
        if let Some(f) = &edit.fallback {
            if f.is_empty() {
                route.remove("fallback");
            } else {
                let mut a = toml_edit::Array::new();
                for x in f {
                    a.push(x.as_str());
                }
                route.insert("fallback", toml_edit::value(a));
            }
        }
        if let Some(a) = &edit.access {
            route.insert("access", toml_edit::value(a.as_str()));
        }
        if let Some(h) = edit.hidden {
            if h {
                route.insert("hidden", toml_edit::value(true));
            } else {
                route.remove("hidden");
            }
        }
        if let Some(u) = edit.allow_user_switch {
            if u {
                route.insert("allow_user_switch", toml_edit::value(true));
            } else {
                route.remove("allow_user_switch");
            }
        }
        let new_text = doc.to_string();
        let rev = self.config().rev;
        let disk_rev = load::load_with(&self.main, text.clone())
            .map(|l| l.rev)
            .unwrap_or(rev);
        match self
            .save_config(&new_text, &disk_rev, &format!("route {name}"), who)
            .await
        {
            Ok(_) => self
                .routes()
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| ApiError::new(500, "route vanished after save")),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/control/sample.toml");

    fn setup() -> (tempfile::TempDir, Arc<ConfigBackend>) {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("router.toml");
        std::fs::write(&main, FIXTURE).unwrap();
        let store = Arc::new(Store::memory().unwrap());
        let b = ConfigBackend::new(&main, d.path(), store);
        (d, b)
    }

    #[test]
    fn diffs_are_minimal() {
        assert_eq!(line_diff("a\nb\nc\n", "a\nx\nc\n"), "-b\n+x\n");
        assert_eq!(line_diff("a\n", "a\nb\n"), "+b\n");
    }

    #[tokio::test]
    async fn a_held_card_shows_the_instance_that_spans_it() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("router.toml");
        let text = format!(
            "{FIXTURE}
[layouts.dense-tp]
title = \"Dense over both\"
             cards = {{ A = {{ engine = \"ninfer\", profile = \"4lane\", model = \"dense\", devices = [\"A\", \"B\"] }}, B = \"held\" }}
"
        );
        std::fs::write(&main, text).unwrap();
        let store = Arc::new(Store::memory().unwrap());
        let spec = crate::control::schema::CardSpec {
            engine: "ninfer".into(),
            profile: "4lane".into(),
            model: "dense".into(),
            devices: vec!["A".into(), "B".into()],
        };
        store
            .put_instance(&crate::control::lifecycle::Instance {
                server: "gpu".into(),
                card: "A".into(),
                engine: "ninfer".into(),
                spec: Some(spec),
                state: crate::control::lifecycle::InstanceState::Ready,
                ..Default::default()
            })
            .unwrap();
        store
            .put_instance(&crate::control::lifecycle::Instance::off("gpu", "B"))
            .unwrap();
        let b = ConfigBackend::new(&main, d.path(), store);
        let st = b.state();
        assert!(st.config_error.is_none(), "{:?}", st.config_error);
        let card = |id: &str| {
            st.servers[0]
                .cards
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .unwrap()
        };
        let a = card("A").engine.unwrap();
        assert_eq!((a.model.as_str(), a.state.as_str()), ("dense", "ready"));
        let bv = card("B");
        let be = bv.engine.unwrap();
        assert_eq!(
            (be.model.as_str(), be.state.as_str()),
            ("dense", "held by A")
        );
        assert!(bv.admin_note.unwrap().contains("held by card A"));
        let l = st.layouts.iter().find(|l| l.id == "dense-tp").unwrap();
        assert_eq!(l.cards["A"], "ninfer / 4lane / dense (over A+B)");
        assert_eq!(l.cards["B"], "held");
    }

    #[tokio::test]
    async fn the_sample_loads_into_state_and_routes() {
        let (_d, b) = setup();
        let st = b.state();
        assert!(st.config_error.is_none(), "{:?}", st.config_error);
        assert!(!st.servers.is_empty() && !st.servers[0].cards.is_empty());
        assert!(!st.layouts.is_empty());
        assert!(!b.routes().is_empty());
        assert_eq!(b.config().rev, st.rev);
    }

    #[tokio::test]
    async fn saves_check_back_up_write_and_audit() {
        let (d, b) = setup();
        let doc = b.config();
        let bad = format!(
            "{}\n[upstreams.zz]\nkind = \"openai\"\nbase_url = \"https://x\"\nkey = \"sk-abc123456789\"\n",
            doc.text
        );
        let chk = b.check_config(&bad).await;
        assert!(!chk.ok);
        assert!(
            chk.problems
                .iter()
                .any(|p| p.path.starts_with("upstreams.zz") && p.severity == "error"),
            "{:?}",
            chk.problems
        );
        assert_eq!(
            b.save_config(&bad, &doc.rev, "", "t")
                .await
                .unwrap_err()
                .status,
            422
        );
        assert_eq!(
            b.save_config(&doc.text, "nope", "", "t")
                .await
                .unwrap_err()
                .status,
            412
        );
        let edited = doc.text.replacen("version = 1", "version = 1\n# edited", 1);
        let r = b
            .save_config(&edited, &doc.rev, "why", "web:admin#1")
            .await
            .unwrap();
        assert!(r.diff.contains("+# edited"));
        assert!(
            std::fs::read_to_string(d.path().join("router.toml"))
                .unwrap()
                .contains("# edited")
        );
        assert_eq!(
            std::fs::read_dir(d.path().join("backups")).unwrap().count(),
            1
        );
        let a = b.audit(5, None);
        assert_eq!(
            (
                a[0].action.as_str(),
                a[0].who.as_str(),
                a[0].reason.as_deref()
            ),
            ("config.save", "web:admin#1", Some("why"))
        );
    }

    #[tokio::test]
    async fn route_edits_keep_comments_and_are_validated() {
        let (d, b) = setup();
        let r = b.routes();
        let name = r[0].name.clone();
        let before = std::fs::read_to_string(d.path().join("router.toml")).unwrap();
        let comments = before
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .count();
        let out = b
            .edit_route(
                &name,
                &RouteEdit {
                    access: Some("admins".into()),
                    ..Default::default()
                },
                "t",
            )
            .await
            .unwrap();
        assert_eq!(out.access, "admins");
        let after = std::fs::read_to_string(d.path().join("router.toml")).unwrap();
        assert_eq!(
            after
                .lines()
                .filter(|l| l.trim_start().starts_with('#'))
                .count(),
            comments
        );
        // on_demand on a pool route: the validator refuses, nothing written
        let pool = b
            .routes()
            .into_iter()
            .find(|r| r.targets.iter().any(|t| t.starts_with("pool:")));
        if let Some(p) = pool {
            let e = b
                .edit_route(
                    &p.name,
                    &RouteEdit {
                        cold: Some("on_demand".into()),
                        ..Default::default()
                    },
                    "t",
                )
                .await
                .unwrap_err();
            assert_eq!(e.status, 422);
        }
    }

    #[tokio::test]
    async fn orchestrator_calls_are_refused_until_plugged_in() {
        let (_d, b) = setup();
        assert_eq!(b.plan("x", "y").await.unwrap_err().status, 501);
    }
}
