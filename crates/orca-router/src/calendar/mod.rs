//! The Supervisor's calendar in the router (SUPERVISOR.md 8, 9; SV12/SV13): `orca-calendar`
//! wired to `calendar.toml`, `router.db`, the guard board and the admin API.
//!
//! - [`CalendarService`] is a [`Guard`]: the active occurrences' blocks become
//!   [`Reservation`]s with `event` set, so a caller holding that event's key (a grant) gets
//!   through, and nobody else does. Card reservations block [`Action::Use`] of the card.
//! - Live detection: the host probe's `/proc` scan ([`CalendarService::report_processes`],
//!   the same matcher as the `video` guard) and other signals ([`CalendarService::report`]).
//! - Event keys: issued, rotated and revoked here (SHA-256 in `router.db`, shown once),
//!   checked by [`CalendarService::authorize`] (window, scopes, the 9.5 limits), every use
//!   audited, and turned into a guard-board [`Caller`] by [`CalendarService::caller`].
//! - Edits go to `calendar.toml` with `toml_edit` (comments kept), a backup and an audit row.
//!   Runtime one-offs (the bench queue's) live in `router.db` with their expiry.
//!
//! Off unless `[calendar] enabled = true` *and* the control plane is on.

pub mod api;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use indexmap::IndexMap;
use orca_calendar::effects::Target;
use orca_calendar::entry::EntryConfig;
use orca_calendar::keys::{self, KeyAction, KeyDecision, KeyPolicy, KeyRecord, KeyScope};
use orca_calendar::now::{BlockAction, NowState};
use orca_calendar::{Calendar, CalendarConfig, Issue, Live, Occurrence, Overrides, Safety};
use serde::Serialize;
use serde_json::json;

use crate::control::schema::ControlConfig;
use crate::control::store::Store;
use crate::guards::{Action, Caller, EventGrant, EventScope, Guard, Reservation, Scope};
use store::CalStore;

/// The installed calendar (one per process, set when the control plane starts with
/// `calendar.enabled`). The admin API and the switch side find it here.
static INSTALLED: OnceLock<Arc<CalendarService>> = OnceLock::new();

pub fn installed() -> Option<&'static Arc<CalendarService>> {
    INSTALLED.get()
}

pub fn install(s: Arc<CalendarService>) -> Result<(), String> {
    INSTALLED
        .set(s)
        .map_err(|_| "the calendar is already installed".to_string())
}

/// Opens and installs the calendar when `[calendar] enabled = true` (both control-plane
/// starts call this before the guard board is built). Returns ", calendar on (N entries)" or "".
pub fn start(
    cfg: &ControlConfig,
    main: &Path,
    origin: &std::collections::BTreeMap<String, PathBuf>,
    store: Arc<Store>,
) -> Result<String, String> {
    let file = origin
        .get("calendar.entries")
        .or(origin.get("calendar.enabled"))
        .cloned();
    match CalendarService::from_control(cfg, main, file.as_deref(), store)? {
        Some(svc) => {
            let n = svc.entries().len();
            install(svc)?;
            Ok(format!(", calendar on ({n} entries)"))
        }
        None => Ok(String::new()),
    }
}

fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Inner {
    file_text: String,
    file_cfg: CalendarConfig,
    /// runtime one-offs from `router.db`: id -> (entry, source, expires)
    dynamic: IndexMap<String, (EntryConfig, String, i64)>,
    cal: Calendar,
    issues: Vec<Issue>,
    rev: String,
    ov: Overrides,
}

pub struct CalendarService {
    pub server: String,
    default_tz: String,
    file: PathBuf,
    backups: PathBuf,
    store: Arc<Store>,
    safety: Safety,
    inner: RwLock<Inner>,
    live: Mutex<Live>,
}

fn rev_of(text: &str) -> String {
    keys::hash(text)[..12].to_string()
}

/// The 9.5 limits from the control plane's config: unmanaged and non-on-demand cards.
pub fn safety_of(cfg: &ControlConfig) -> Safety {
    Safety {
        excluded_cards: cfg
            .cards
            .iter()
            .filter(|(_, c)| !c.managed || !c.on_demand)
            .map(|(id, _)| id.clone())
            .collect(),
        excluded_models: Vec::new(),
        unsafe_layouts: Vec::new(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub id: String,
    #[serde(flatten)]
    pub entry: EntryConfig,
    /// `daily 07:00`, `cron 0 7 * * 0`
    pub schedule: String,
    pub effects_text: Vec<String>,
    /// `file` (calendar.toml) or the runtime source (`bench`)
    pub origin: String,
    pub frequent: bool,
    /// the entry did not compile (its errors are in `issues`)
    pub broken: bool,
    pub keys: Vec<KeyRecord>,
}

impl CalendarService {
    /// Open the calendar: `file` is `calendar.toml` (missing = empty).
    pub fn open(
        server: &str,
        default_tz: &str,
        file: &Path,
        state_dir: &Path,
        store: Arc<Store>,
        safety: Safety,
    ) -> Result<Arc<Self>, String> {
        CalStore(&store).migrate()?;
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let file_cfg = CalendarConfig::from_toml(&text)?;
        let now = now_epoch();
        let mut live = Live::default();
        live.runs = CalStore(&store).runs(now - orca_calendar::live::KEEP_SECONDS)?;
        let s = Arc::new(Self {
            server: server.to_string(),
            default_tz: default_tz.to_string(),
            file: file.to_path_buf(),
            backups: state_dir.join("backups"),
            store,
            safety,
            inner: RwLock::new(Inner {
                rev: rev_of(&text),
                file_text: text,
                file_cfg,
                dynamic: IndexMap::new(),
                cal: Calendar::compile(&CalendarConfig::default(), default_tz, &Safety::default())
                    .0,
                issues: vec![],
                ov: Overrides::default(),
            }),
            live: Mutex::new(live),
        });
        s.refresh()?;
        Ok(s)
    }

    /// From the control plane's config: `None` when `calendar.enabled` is off. The calendar
    /// file is the include that defines `[calendar]`, else `calendar.toml` next to `main`.
    pub fn from_control(
        cfg: &ControlConfig,
        main: &Path,
        origin_file: Option<&Path>,
        store: Arc<Store>,
    ) -> Result<Option<Arc<Self>>, String> {
        let c = CalendarConfig::from_raw(&cfg.calendar)?;
        if !c.enabled {
            return Ok(None);
        }
        let file = origin_file.map(Path::to_path_buf).unwrap_or_else(|| {
            main.parent()
                .unwrap_or(Path::new("."))
                .join("calendar.toml")
        });
        let server = cfg
            .servers
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "gpu".into());
        let state_dir = PathBuf::from(&cfg.router.state_dir);
        Self::open(
            &server,
            &cfg.router.timezone,
            &file,
            &state_dir,
            store,
            safety_of(cfg),
        )
        .map(Some)
    }

    /// Recompile from the file text held, the dynamic entries in `router.db` and overrides.
    pub fn refresh(&self) -> Result<(), String> {
        let now = now_epoch();
        let st = CalStore(&self.store);
        let mut dynamic = IndexMap::new();
        for (id, j, source, exp) in st.dynamic(now)? {
            if let Ok(e) = serde_json::from_str::<EntryConfig>(&j) {
                dynamic.insert(id, (e, source, exp));
            }
        }
        let ov = orca_calendar::overrides_from(&st.overrides()?);
        let mut g = self.inner.write().unwrap();
        let mut cfg = g.file_cfg.clone();
        for (id, (e, _, _)) in &dynamic {
            cfg.entries.insert(id.clone(), e.clone());
        }
        let (cal, issues) = Calendar::compile(&cfg, &self.default_tz, &self.safety);
        g.cal = cal;
        g.issues = issues;
        g.dynamic = dynamic;
        g.ov = ov;
        Ok(())
    }

    /// Re-read `calendar.toml` (after a config reload or an outside edit).
    pub fn reload_file(&self) -> Result<(), String> {
        let text = std::fs::read_to_string(&self.file).unwrap_or_default();
        let cfg = CalendarConfig::from_toml(&text)?;
        {
            let mut g = self.inner.write().unwrap();
            g.rev = rev_of(&text);
            g.file_text = text;
            g.file_cfg = cfg;
        }
        self.refresh()
    }

    pub fn rev(&self) -> String {
        self.inner.read().unwrap().rev.clone()
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        self.inner.read().unwrap().cal.tz
    }

    pub fn issues(&self) -> Vec<Issue> {
        self.inner.read().unwrap().issues.clone()
    }

    pub fn now_state(&self, now: i64) -> NowState {
        let g = self.inner.read().unwrap();
        let live = self.live.lock().unwrap();
        g.cal.now(now, &live, &g.ov)
    }

    pub fn occurrences(&self, from: i64, to: i64, now: i64) -> Vec<Occurrence> {
        let g = self.inner.read().unwrap();
        let live = self.live.lock().unwrap();
        g.cal.occurrences(from, to, &live, &g.ov, now)
    }

    pub fn overlaps(&self, from: i64, to: i64) -> Vec<orca_calendar::now::Overlap> {
        self.inner.read().unwrap().cal.overlaps(from, to)
    }

    /// The entries as the UI lists them (file entries first, then runtime ones).
    pub fn entries(&self) -> Vec<EntryView> {
        let g = self.inner.read().unwrap();
        let keys = CalStore(&self.store).keys(None).unwrap_or_default();
        let mut out = Vec::new();
        let all = g
            .file_cfg
            .entries
            .iter()
            .map(|(id, e)| (id, e, "file".to_string()))
            .chain(
                g.dynamic
                    .iter()
                    .map(|(id, (e, src, _))| (id, e, src.clone())),
            );
        for (id, e, origin) in all {
            let compiled = g.cal.entry(id);
            out.push(EntryView {
                id: id.clone(),
                entry: e.clone(),
                schedule: compiled
                    .map(|c| {
                        let mut s = c.when.describe();
                        if let Some(a) = &e.after {
                            s.push_str(&format!(", after {a}"));
                        }
                        s
                    })
                    .unwrap_or_default(),
                effects_text: compiled
                    .map(|c| c.effects.iter().map(|x| x.describe()).collect())
                    .unwrap_or_default(),
                origin,
                frequent: compiled.is_some_and(|c| c.frequent_info()),
                broken: compiled.is_none() && e.enabled,
                keys: keys.iter().filter(|k| &k.event == id).cloned().collect(),
            });
        }
        out
    }

    // ── detection ────────────────────────────────────────────────────────────────────────────

    /// A reading for one entry's signal.
    pub fn report(&self, entry: &str, at: i64, present: bool, detail: Option<String>) {
        let change = self.live.lock().unwrap().report(entry, at, present, detail);
        let st = CalStore(&self.store);
        use orca_calendar::live::Change;
        match change {
            Some(Change::Started(r)) | Some(Change::Rejoined(r)) | Some(Change::Ended(r)) => {
                if let Err(e) = st.put_run(&r) {
                    tracing::warn!("router.db: calendar run: {e}");
                }
            }
            None => {
                // Persist the last sighting now and then, so a restart keeps the extension.
                if present {
                    let live = self.live.lock().unwrap();
                    if let Some(r) = live.runs_of(entry).filter(|r| r.ongoing).last()
                        && (r.last_seen - r.start) % 60 < 10
                    {
                        let _ = st.put_run(r);
                    }
                }
            }
        }
    }

    /// The host probe's process scan (`(pid, argv)` from `/proc/*/cmdline`): every entry
    /// with `detect.process` gets a reading.
    pub fn report_processes(&self, procs: &[(u32, Vec<String>)], at: i64) {
        let watch: Vec<(String, crate::guards::ProcessMatch, String)> = {
            let g = self.inner.read().unwrap();
            g.cal
                .entries
                .values()
                .filter_map(|e| {
                    let p = e.cfg.detect.as_ref()?.process.as_ref()?;
                    let script = p.argv_any.clone();
                    let label_default = p.label_default.clone().unwrap_or_default();
                    let want = p.label.clone().unwrap_or_else(|| label_default.clone());
                    Some((
                        e.id.clone(),
                        crate::guards::ProcessMatch {
                            exe: ["bash", "sh", "dash", script.as_str()]
                                .map(String::from)
                                .to_vec(),
                            script,
                            label_arg: p.label_arg.clone().unwrap_or_default(),
                            label_default,
                        },
                        want,
                    ))
                })
                .collect()
        };
        for (entry, m, want) in watch {
            let runs = crate::guards::video_runs(procs, &m);
            let pids: Vec<String> = runs
                .iter()
                .filter(|(_, l)| want.is_empty() || *l == want)
                .map(|(p, _)| p.to_string())
                .collect();
            let present = !pids.is_empty();
            self.report(
                &entry,
                at,
                present,
                present.then(|| format!("pid {}", pids.join(", "))),
            );
        }
    }

    // ── checks ───────────────────────────────────────────────────────────────────────────────

    /// `calendar check --effect block_switch --card B`: the reasons it is blocked now (empty =
    /// free).
    pub fn check(&self, action: BlockAction, card: Option<&str>, now: i64) -> Vec<String> {
        let st = self.now_state(now);
        st.blocks
            .iter()
            .filter(|b| {
                (b.action == action
                    || (action == BlockAction::OnDemand && b.action == BlockAction::Switch))
                    && match card {
                        None => true,
                        Some(c) => b.target.covers_card(c, Some(&self.server)),
                    }
            })
            .map(|b| b.reason.clone())
            .collect()
    }

    // ── event keys ───────────────────────────────────────────────────────────────────────────

    fn audit(&self, who: &str, action: &str, detail: &str) {
        let _ = self
            .store
            .audit(now_epoch(), who, action, "", "", detail, "");
    }

    /// Issue a key for `event` (an admin, the owner). Returns the secret: shown once.
    pub fn issue_key(&self, event: &str, who: &str) -> Result<(KeyRecord, String), String> {
        {
            let g = self.inner.read().unwrap();
            if !g.cal.entries.contains_key(event) {
                return Err(format!("no usable entry '{event}'"));
            }
        }
        let k = keys::generate()?;
        let now = now_epoch();
        let st = CalStore(&self.store);
        let id = st.insert_key(event, &k.prefix, &k.hash, who, now)?;
        self.audit(
            who,
            "calendar.key.issue",
            &format!("{event}: key {id} ({})", k.prefix),
        );
        let rec = st
            .keys(Some(event))?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or("the key vanished")?;
        Ok((rec, k.secret))
    }

    /// Rotate: a new key now; the live old ones retire at the end of the current or next
    /// occurrence (+ grace), or in 24 h, whichever is sooner.
    pub fn rotate_key(&self, event: &str, who: &str) -> Result<(KeyRecord, String, i64), String> {
        let now = now_epoch();
        let retire = {
            let g = self.inner.read().unwrap();
            let live = self.live.lock().unwrap();
            keys::retire_time(&g.cal, event, now, &live, &g.ov)
        };
        let st = CalStore(&self.store);
        for k in st.keys(Some(event))?.into_iter().filter(|k| k.live(now)) {
            st.retire_key(k.id, retire)?;
        }
        let (rec, secret) = self.issue_key(event, who)?;
        self.audit(
            who,
            "calendar.key.rotate",
            &format!("{event}: old keys retire at {retire}"),
        );
        Ok((rec, secret, retire))
    }

    pub fn revoke_key(&self, id: i64, who: &str) -> Result<bool, String> {
        let done = CalStore(&self.store).revoke_key(id, now_epoch())?;
        if done {
            self.audit(who, "calendar.key.revoke", &format!("key {id}"));
        }
        Ok(done)
    }

    /// The scopes an event's key policy grants (empty for an unknown event).
    pub fn key_scopes(&self, event: &str) -> Vec<KeyScope> {
        let g = self.inner.read().unwrap();
        g.cal
            .entry(event)
            .map(|e| e.key.clone().unwrap_or_default().scopes)
            .unwrap_or_default()
    }

    pub fn keys(&self, event: Option<&str>) -> Vec<KeyRecord> {
        CalStore(&self.store).keys(event).unwrap_or_default()
    }

    pub fn key_uses(&self, event: &str, limit: usize) -> Vec<store::KeyUse> {
        CalStore(&self.store)
            .key_uses(event, limit)
            .unwrap_or_default()
    }

    /// The stored record of an `abe_` secret.
    pub fn lookup(&self, secret: &str) -> Option<KeyRecord> {
        if !keys::looks_like_event_key(secret) {
            return None;
        }
        CalStore(&self.store)
            .key_by_hash(&keys::hash(secret))
            .ok()
            .flatten()
    }

    /// May the holder of `secret` do `action` now? Every call is audited (9.6). `None` record
    /// = not an event key we know (401).
    pub fn authorize(&self, secret: &str, action: &KeyAction, owner_enabled: bool) -> KeyDecision {
        let now = now_epoch();
        let Some(rec) = self.lookup(secret) else {
            return KeyDecision::Deny {
                status: 401,
                reason: "not a valid event key".into(),
                next: None,
            };
        };
        let d = {
            let g = self.inner.read().unwrap();
            let live = self.live.lock().unwrap();
            let st = g.cal.now(now, &live, &g.ov);
            keys::authorize(
                &g.cal,
                &st,
                &live,
                &g.ov,
                &rec,
                owner_enabled,
                action,
                &self.safety,
                now,
            )
        };
        let cs = CalStore(&self.store);
        cs.touch_key(rec.id, now);
        let (decision, status, occ, detail) = match &d {
            KeyDecision::Allow { occurrence, .. } => {
                ("allow", 200u16, occurrence.clone(), String::new())
            }
            KeyDecision::Deny { status, reason, .. } => (
                if reason.starts_with("event key outside its window") {
                    "outside_window"
                } else {
                    "deny"
                },
                *status,
                String::new(),
                reason.clone(),
            ),
        };
        let what = match action {
            KeyAction::Chat { route, .. } => format!("chat {route}"),
            KeyAction::UseCard { card } => format!("use {card}"),
            KeyAction::Lease { card } => format!("lease {card}"),
            KeyAction::Switch { layout, probe } => {
                format!("switch {layout}{}", if *probe { " (probe)" } else { "" })
            }
            KeyAction::CardState { card, online } => {
                format!("{} {card}", if *online { "online" } else { "offline" })
            }
            KeyAction::HoldClocks { card, ttl } => format!("hold {card} {ttl}s"),
            KeyAction::FanProfile { profile } => format!("fan {profile}"),
            KeyAction::OnDemand { route, card, .. } => format!("on_demand {route} {card}"),
        };
        let _ = cs.key_use(
            now, rec.id, &rec.event, &occ, &what, decision, status, &detail,
        );
        // Control actions also go to the main audit table with the event, key and owner.
        let control = !matches!(action, KeyAction::Chat { .. } | KeyAction::UseCard { .. });
        if control && decision == "allow" {
            self.audit(
                &format!("event:{} key {} (owner {})", rec.event, rec.id, rec.owner),
                &format!("calendar.key.{}", action.name()),
                &what,
            );
        }
        d
    }

    /// How many times a key was refused outside its window in the last 10 minutes (the
    /// `event_key_outside_window` alert fires above 3).
    pub fn outside_window_recent(&self, key_id: i64) -> i64 {
        CalStore(&self.store)
            .outside_window_uses(key_id, now_epoch() - 600)
            .unwrap_or(0)
    }

    /// The guard-board caller for an event key used now: grants for its event (and the lower
    /// ones it may lift), with scopes mapped to the board's. `None` outside the window.
    pub fn caller(&self, secret: &str) -> Option<Caller> {
        let now = now_epoch();
        let rec = self.lookup(secret)?;
        if !rec.live(now) {
            return None;
        }
        let g = self.inner.read().unwrap();
        let live = self.live.lock().unwrap();
        keys::window(&g.cal, &rec.event, now, &live, &g.ov).ok()?;
        let policy = g
            .cal
            .entry(&rec.event)
            .and_then(|e| e.key.clone())
            .unwrap_or_default();
        let st = g.cal.now(now, &live, &g.ov);
        let scopes = board_scopes(&policy);
        Some(Caller {
            principal: Some(format!("event:{} key {}", rec.event, rec.id)),
            grants: g
                .cal
                .liftable(&rec.event, &st)
                .into_iter()
                .map(|event| EventGrant {
                    event,
                    scopes: scopes.clone(),
                })
                .collect(),
        })
    }

    // ── edits ────────────────────────────────────────────────────────────────────────────────

    fn backup(&self, text: &str, rev: &str) {
        if std::fs::create_dir_all(&self.backups).is_ok() {
            let name = format!(
                "calendar.toml.bak-{}-{rev}",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            );
            let _ = std::fs::write(self.backups.join(name), text);
        }
    }

    /// Check an edit without saving: the entry's issues, its next 10 occurrences and the
    /// overlaps it would make over the next 8 weeks.
    pub fn preview(&self, id: &str, e: &EntryConfig) -> serde_json::Value {
        let g = self.inner.read().unwrap();
        let mut cfg = g.file_cfg.clone();
        for (k, (d, _, _)) in &g.dynamic {
            cfg.entries.insert(k.clone(), d.clone());
        }
        cfg.entries.insert(id.to_string(), e.clone());
        let (cal, issues) = Calendar::compile(&cfg, &self.default_tz, &self.safety);
        let now = now_epoch();
        let live = self.live.lock().unwrap();
        let next = cal.next_of(id, now, 10, &live, &g.ov);
        let overlaps: Vec<_> = cal
            .overlaps(now, now + 56 * 86_400)
            .into_iter()
            .filter(|o| o.a == id || o.b == id)
            .collect();
        let issues: Vec<Issue> = issues
            .into_iter()
            .filter(|i| {
                i.path.starts_with(&format!("calendar.entries.{id}"))
                    || i.path.starts_with("calendar.timezone")
            })
            .collect();
        json!({
            "id": id,
            "issues": issues,
            "next": next,
            "overlaps": overlaps,
            "effects": cal.entry(id).map(|c| c.effects.iter().map(|x| x.describe()).collect::<Vec<_>>()).unwrap_or_default(),
            "schedule": cal.entry(id).map(|c| c.when.describe()).unwrap_or_default(),
        })
    }

    /// Create or replace an entry in `calendar.toml`. Errors refuse the save; warnings and
    /// overlaps are returned. `if_rev`: the file rev the editor saw (412 if it changed).
    pub fn save_entry(
        &self,
        id: &str,
        e: Option<&EntryConfig>,
        if_rev: Option<&str>,
        who: &str,
        reason: &str,
    ) -> Result<serde_json::Value, (u16, String)> {
        if !orca_calendar::valid_id(id) {
            return Err((
                400,
                "ids are lower-case letters, digits, '-' and '_'".into(),
            ));
        }
        let (old_text, old_rev) = {
            let g = self.inner.read().unwrap();
            if g.dynamic.contains_key(id) {
                return Err((
                    409,
                    format!(
                        "'{id}' is a runtime entry ({}); end it instead",
                        g.dynamic[id].1
                    ),
                ));
            }
            (g.file_text.clone(), g.rev.clone())
        };
        if let Some(r) = if_rev
            && r != old_rev
        {
            return Err((
                412,
                format!("calendar.toml changed (rev {old_rev}); reload and edit again"),
            ));
        }
        let new_text = edit_entry(&old_text, id, e).map_err(|m| (400, m))?;
        let cfg = CalendarConfig::from_toml(&new_text).map_err(|m| (400, m))?;
        let (_, issues) = Calendar::compile(&cfg, &self.default_tz, &self.safety);
        let mine: Vec<Issue> = issues
            .into_iter()
            .filter(|i| i.path.starts_with(&format!("calendar.entries.{id}")) || e.is_none())
            .collect();
        if let Some(err) = mine.iter().find(|i| i.is_error()) {
            return Err((422, err.to_string()));
        }
        self.backup(&old_text, &old_rev);
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.file.with_extension("toml.tmp");
        std::fs::write(&tmp, &new_text).map_err(|x| (500, format!("{}: {x}", tmp.display())))?;
        std::fs::rename(&tmp, &self.file)
            .map_err(|x| (500, format!("{}: {x}", self.file.display())))?;
        let new_rev = rev_of(&new_text);
        let _ = self.store.audit(
            now_epoch(),
            who,
            if e.is_some() {
                "calendar.save"
            } else {
                "calendar.delete"
            },
            &old_rev,
            &new_rev,
            &format!("calendar.entries.{id}"),
            reason,
        );
        self.reload_file().map_err(|m| (500, m))?;
        let now = now_epoch();
        let overlaps: Vec<_> = self
            .overlaps(now, now + 56 * 86_400)
            .into_iter()
            .filter(|o| o.a == id || o.b == id)
            .collect();
        Ok(json!({"rev": new_rev, "warnings": mine, "overlaps": overlaps}))
    }

    /// Skip a planned occurrence, end a running one now (`cancel`), or undo (`restore`).
    pub fn occurrence_action(
        &self,
        occurrence: &str,
        action: &str,
        who: &str,
        reason: &str,
    ) -> Result<(), (u16, String)> {
        let entry = occurrence
            .split('@')
            .next()
            .filter(|e| !e.is_empty())
            .ok_or((400, "bad occurrence id".to_string()))?;
        let st = CalStore(&self.store);
        let now = now_epoch();
        match action {
            "skip" => st.set_override(occurrence, entry, "skip", now, who, reason),
            "cancel" => st.set_override(occurrence, entry, "cancel", now, who, reason),
            "restore" => st.clear_override(occurrence).map(|_| ()),
            _ => return Err((400, "action: skip, cancel or restore".into())),
        }
        .map_err(|m| (500, m))?;
        self.audit(who, &format!("calendar.occurrence.{action}"), occurrence);
        self.refresh().map_err(|m| (500, m))
    }

    // ── bench protection (15 Q10) ────────────────────────────────────────────────────────────

    /// The bench queue starts a run: a runtime one-off entry for its duration (+ 30 min) that
    /// blocks on-demand, holds the cards' clocks, reserves them and two lanes, and has a key
    /// with `use_reserved` and `switch` limited to the bench layouts. Returns the entry id and
    /// the key (for the bench run's requests).
    pub fn bench_begin(
        &self,
        run: &str,
        cards: &[String],
        layouts: &[String],
        seconds: i64,
        who: &str,
    ) -> Result<(String, String), String> {
        let slug: String = run
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(40)
            .collect();
        let id = format!("bench-{}", slug.trim_matches('-'));
        let now = now_epoch();
        let local = orca_calendar::recur::to_local(self.tz(), now);
        let mut effects = vec![json!({"block_on_demand": "all"})];
        for c in cards {
            effects.push(json!({"hold_clocks": c}));
            effects.push(json!({"reserve_card": c, "on_reserve": "wait"}));
        }
        let e = EntryConfig {
            title: format!("Bench run {run}"),
            owner: Some(who.to_string()),
            at: Some(local.format("%Y-%m-%dT%H:%M:%S").to_string()),
            rrule: None,
            start: None,
            cron: None,
            ics: None,
            duration: Some(orca_calendar::dur::format(seconds.max(60) + 1800)),
            tz: None,
            lead: None,
            tail: None,
            detect: None,
            after: None,
            effects,
            priority: orca_calendar::Priority::High,
            key: Some(KeyPolicy {
                scopes: vec![
                    KeyScope::Chat,
                    KeyScope::UseReserved,
                    KeyScope::Lease,
                    KeyScope::Switch,
                    KeyScope::HoldClocks,
                ],
                layouts: layouts.to_vec(),
                grace_before: "1m".into(),
                grace_after: "5m".into(),
                ..Default::default()
            }),
            enabled: true,
            until: None,
            exdates: vec![],
            source: Some("bench".into()),
        };
        let j = serde_json::to_string(&e).map_err(|x| x.to_string())?;
        CalStore(&self.store).put_dynamic(
            &id,
            &j,
            "bench",
            now,
            now + seconds.max(60) + 1800 + 3600,
        )?;
        self.refresh()?;
        let (_, secret) = self.issue_key(&id, who)?;
        self.audit(who, "calendar.bench.begin", &id);
        Ok((id, secret))
    }

    /// The bench run ended: its occurrence ends now and its keys are revoked.
    pub fn bench_end(&self, id: &str, who: &str) -> Result<(), String> {
        let occ: Vec<String> = self
            .occurrences(now_epoch() - 86_400, now_epoch() + 1, now_epoch())
            .into_iter()
            .filter(|o| o.entry == id)
            .map(|o| o.id)
            .collect();
        for o in occ {
            let _ = self.occurrence_action(&o, "cancel", who, "bench finished");
        }
        let st = CalStore(&self.store);
        for k in st.keys(Some(id))? {
            st.revoke_key(k.id, now_epoch())?;
        }
        self.audit(who, "calendar.bench.end", id);
        Ok(())
    }

    // ── imports ──────────────────────────────────────────────────────────────────────────────

    /// TrueNAS cron jobs (`cronjob.query` JSON) as entries. `apply`: write the ones not in
    /// the calendar yet; else only show them.
    pub fn import_cron(
        &self,
        jobs: &serde_json::Value,
        who: &str,
        apply: bool,
    ) -> Result<serde_json::Value, (u16, String)> {
        let imp = orca_calendar::cron_import::import(jobs, &self.server, who);
        let have: Vec<String> = self
            .inner
            .read()
            .unwrap()
            .file_cfg
            .entries
            .keys()
            .cloned()
            .collect();
        let new: Vec<_> = imp.iter().filter(|i| !have.contains(&i.id)).collect();
        if apply {
            for i in &new {
                self.save_entry(
                    &i.id,
                    Some(&i.entry),
                    None,
                    who,
                    &format!("import-cron (cron job {})", i.cron_id),
                )?;
            }
        }
        Ok(json!({
            "found": imp.iter().map(|i| json!({"id": i.id, "kind": i.kind, "cron_id": i.cron_id, "title": i.entry.title, "exists": have.contains(&i.id)})).collect::<Vec<_>>(),
            "written": if apply { new.len() } else { 0 },
            "toml": orca_calendar::emit::entries("calendar import-cron", new.iter().map(|i| (&i.id, &i.entry))),
        }))
    }

    /// An `.ics` text imported once: every VEVENT becomes an entry with `effects`.
    pub fn import_ics(
        &self,
        text: &str,
        effects: &[serde_json::Value],
        priority: orca_calendar::Priority,
        who: &str,
        apply: bool,
    ) -> Result<serde_json::Value, (u16, String)> {
        let (events, mut issues) = orca_calendar::ics::parse(text);
        let tz = self.tz();
        let entries = orca_calendar::ics::to_entries(
            &events,
            "ics",
            "ics:import",
            effects,
            priority,
            tz,
            &mut issues,
        );
        if apply {
            for (id, e) in &entries {
                let mut e = e.clone();
                e.owner = Some(who.to_string());
                self.save_entry(id, Some(&e), None, who, "import-ics")?;
            }
        }
        Ok(json!({
            "entries": entries.keys().collect::<Vec<_>>(),
            "issues": issues,
            "toml": orca_calendar::emit::entries("calendar import-ics", entries.iter()),
        }))
    }
}

/// The event-key scopes as the guard board knows them.
pub fn board_scopes(p: &KeyPolicy) -> Vec<EventScope> {
    let mut out = Vec::new();
    for s in &p.scopes {
        match s {
            KeyScope::UseReserved => out.push(EventScope::UseReserved),
            KeyScope::Switch | KeyScope::OnDemand => {
                let only = if p.all_layouts {
                    None
                } else {
                    Some(p.layouts.clone())
                };
                let e = EventScope::SwitchLayout(only);
                if !out.contains(&e) {
                    out.push(e);
                }
            }
            KeyScope::CardState => out.push(EventScope::CardOnline),
            _ => {}
        }
    }
    out
}

fn action_of(a: BlockAction) -> Action {
    match a {
        BlockAction::Switch => Action::Switch,
        BlockAction::OnDemand => Action::OnDemand,
        BlockAction::Ensure => Action::Ensure,
        BlockAction::Use => Action::Use,
    }
}

impl Guard for CalendarService {
    fn id(&self) -> &str {
        "calendar"
    }

    fn reservations(&self, now: f64) -> Vec<Reservation> {
        let now = now as i64;
        let st = self.now_state(now);
        st.blocks
            .into_iter()
            .filter_map(|b| {
                let scope = match &b.target {
                    Target::All => Scope::All,
                    Target::Server(s) if *s == self.server => Scope::All,
                    Target::Server(_) => return None,
                    Target::Card(c) => Scope::Cards(vec![c.clone()]),
                    // Route-level on-demand blocks are asked by route (api `check`), not here.
                    Target::Route(_) => return None,
                };
                Some(Reservation {
                    source: format!("calendar:{}", b.entry),
                    reason: b.reason,
                    blocks: vec![action_of(b.action)],
                    scope,
                    until: Some(b.until as f64),
                    retry_seconds: ((b.until - now).max(60)) as u64,
                    event: Some(b.entry),
                })
            })
            .collect()
    }
}

/// Replace (or remove) `[calendar.entries.<id>]` in a `calendar.toml` text, keeping every
/// other line and comment.
pub fn edit_entry(text: &str, id: &str, e: Option<&EntryConfig>) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut =
        text.parse().map_err(|x| format!("calendar.toml: {x}"))?;
    let cal = doc
        .entry("calendar")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let cal = cal.as_table_mut().ok_or("`calendar` is not a table")?;
    if !cal.contains_key("enabled") && cal.is_empty() {
        cal.set_implicit(true);
    }
    let entries = cal
        .entry("entries")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let entries = entries
        .as_table_mut()
        .ok_or("`calendar.entries` is not a table")?;
    entries.set_implicit(true);
    match e {
        None => {
            entries.remove(id);
        }
        Some(e) => {
            let snippet: toml_edit::DocumentMut = orca_calendar::emit::entry(id, e)
                .parse()
                .map_err(|x| format!("internal: {x}"))?;
            let item = snippet["calendar"]["entries"][id].clone();
            let old_decor = entries
                .get(id)
                .and_then(|i| i.as_table())
                .map(|t| t.decor().clone());
            entries.insert(id, item);
            if let (Some(d), Some(t)) = (
                old_decor,
                entries.get_mut(id).and_then(|i| i.as_table_mut()),
            ) {
                *t.decor_mut() = d;
            }
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests;
