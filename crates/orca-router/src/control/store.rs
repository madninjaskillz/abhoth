//! `router.db`: the control plane's runtime state (ROUTER-CONFIG.md 4.1, D-RC2). Config says
//! what can exist; this database says what was chosen and what happened: the active layout per
//! server, card admin state, instances, jobs and their step journal, the config audit, OOM
//! kills, learned estimates, restores and per-boot ensure counters.
//!
//! SQLite (rusqlite 0.43, bundled, WAL), as D140's `abhoth.db`. Schema migrations are kept **per
//! part** in `schema_parts`, so each subsystem (this core, leases, budgets, accounts cache…)
//! migrates its own tables without fighting over one `user_version`:
//!
//! ```ignore
//! store.migrate_part("leases", &["CREATE TABLE leases (...)", "ALTER TABLE leases ADD ..."])?;
//! ```
//!
//! Calls are synchronous and short; async callers may use them directly (local file, WAL) or
//! wrap batches in `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::lifecycle::{Instance, InstanceState};
use super::schema::CardSpec;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// The core part's migrations, in order. Append only.
const CORE: &[&str] = &[
    // 1
    "CREATE TABLE active_layout (
        server TEXT PRIMARY KEY,
        layout TEXT NOT NULL,          -- a layout id, or 'custom' after a failed rollback
        cards_json TEXT NOT NULL DEFAULT '{}',   -- card -> CardSpec|null actually committed
        since INTEGER NOT NULL,
        set_by TEXT NOT NULL DEFAULT '',
        job_id TEXT
    );
    CREATE TABLE card_admin (
        card TEXT PRIMARY KEY,
        state TEXT NOT NULL,           -- online | offline | excluded
        by TEXT NOT NULL DEFAULT '',
        reason TEXT NOT NULL DEFAULT '',
        since INTEGER NOT NULL
    );
    CREATE TABLE instances (
        card TEXT PRIMARY KEY,
        server TEXT NOT NULL,
        engine TEXT NOT NULL DEFAULT '',
        spec_json TEXT,
        state TEXT NOT NULL,
        unhealthy INTEGER NOT NULL DEFAULT 0,
        since INTEGER NOT NULL,
        detail TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE jobs (
        id TEXT PRIMARY KEY,
        server TEXT NOT NULL,
        kind TEXT NOT NULL,            -- switch | offline | online | restart | reconcile | rollback
        target TEXT NOT NULL DEFAULT '',
        from_layout TEXT NOT NULL DEFAULT '',
        requested_by TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL,           -- queued | running | done | refused | rolled-back | failed | interrupted
        created INTEGER NOT NULL,
        started INTEGER,
        ended INTEGER,
        plan_json TEXT,
        error TEXT
    );
    CREATE INDEX jobs_server_created ON jobs(server, created);
    CREATE TABLE job_steps (
        job_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        name TEXT NOT NULL,
        card TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL,           -- running | ok | failed | skipped
        started INTEGER NOT NULL,
        ended INTEGER,
        detail TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (job_id, seq)
    );
    CREATE TABLE audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts INTEGER NOT NULL,
        who TEXT NOT NULL,             -- user id, key id, or 'file'
        action TEXT NOT NULL,          -- config.write | config.revert | config.file-edit | card.offline | ...
        old_rev TEXT NOT NULL DEFAULT '',
        new_rev TEXT NOT NULL DEFAULT '',
        diff TEXT NOT NULL DEFAULT '',
        reason TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE oom_events (
        ts INTEGER NOT NULL,
        server TEXT NOT NULL,
        job_id TEXT,
        line TEXT NOT NULL
    );
    CREATE TABLE estimates (
        engine TEXT NOT NULL,
        card TEXT NOT NULL,
        kind TEXT NOT NULL,            -- start | stop | restore_mib_per_s
        value REAL NOT NULL,
        samples INTEGER NOT NULL,
        PRIMARY KEY (engine, card, kind)
    );
    CREATE TABLE restores (
        ts INTEGER NOT NULL,
        server TEXT NOT NULL,
        file TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        seconds REAL NOT NULL,
        sha_ok INTEGER NOT NULL
    );
    CREATE TABLE ensure_tries (
        card TEXT NOT NULL,
        boot_id TEXT NOT NULL,
        tries INTEGER NOT NULL,
        last INTEGER NOT NULL,
        PRIMARY KEY (card, boot_id)
    );",
];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CardAdmin {
    Online,
    Offline,
    Excluded,
}

impl CardAdmin {
    pub fn as_str(self) -> &'static str {
        match self {
            CardAdmin::Online => "online",
            CardAdmin::Offline => "offline",
            CardAdmin::Excluded => "excluded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "online" => Some(Self::Online),
            "offline" => Some(Self::Offline),
            "excluded" => Some(Self::Excluded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CardAdminRow {
    pub card: String,
    pub state: CardAdmin,
    pub by: String,
    pub reason: String,
    pub since: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ActiveLayout {
    pub server: String,
    pub layout: String,
    /// card -> the spec committed (None = off).
    pub cards: indexmap::IndexMap<String, Option<CardSpec>>,
    pub since: i64,
    pub set_by: String,
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Refused,
    RolledBack,
    Failed,
    Interrupted,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Refused => "refused",
            JobState::RolledBack => "rolled-back",
            JobState::Failed => "failed",
            JobState::Interrupted => "interrupted",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Done,
            Self::Refused,
            Self::RolledBack,
            Self::Failed,
            Self::Interrupted,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }
    pub fn finished(self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JobRow {
    pub id: String,
    pub server: String,
    pub kind: String,
    pub target: String,
    pub from_layout: String,
    pub requested_by: String,
    pub state: JobState,
    pub created: i64,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub plan_json: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct StepRow {
    pub job_id: String,
    pub seq: i64,
    pub name: String,
    pub card: String,
    pub state: String,
    pub started: i64,
    pub ended: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AuditRow {
    pub id: i64,
    pub ts: i64,
    pub who: String,
    pub action: String,
    pub old_rev: String,
    pub new_rev: String,
    pub diff: String,
    pub reason: String,
}

pub type StoreResult<T> = Result<T, String>;

fn e(err: rusqlite::Error) -> String {
    err.to_string()
}

impl Store {
    /// Open (or create) `router.db` with WAL and the core migrations applied.
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|x| format!("{}: {x}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(e)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(e)?;
        Self::init(conn)
    }

    /// An in-memory database (tests).
    pub fn memory() -> StoreResult<Self> {
        Self::init(Connection::open_in_memory().map_err(e)?)
    }

    fn init(conn: Connection) -> StoreResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(e)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(e)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_parts (part TEXT PRIMARY KEY, version INTEGER NOT NULL)")
            .map_err(e)?;
        let s = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        s.migrate_part("core", CORE)?;
        Ok(s)
    }

    /// The connection, for other parts' own tables. Keep the guard short.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Apply `migrations[v..]` for `part`, each in a transaction. A database whose part version
    /// is newer than this build knows is refused.
    pub fn migrate_part(&self, part: &str, migrations: &[&str]) -> StoreResult<()> {
        let mut c = self.conn();
        let have: i64 = c
            .query_row(
                "SELECT version FROM schema_parts WHERE part = ?1",
                [part],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)?
            .unwrap_or(0);
        if have as usize > migrations.len() {
            return Err(format!(
                "router.db part '{part}' is at version {have}, newer than this build ({}); refusing to open",
                migrations.len()
            ));
        }
        for (i, sql) in migrations.iter().enumerate().skip(have as usize) {
            let tx = c.transaction().map_err(e)?;
            tx.execute_batch(sql)
                .map_err(|x| format!("migration {part} #{}: {x}", i + 1))?;
            tx.execute(
                "INSERT INTO schema_parts(part, version) VALUES (?1, ?2)
                 ON CONFLICT(part) DO UPDATE SET version = excluded.version",
                params![part, (i + 1) as i64],
            )
            .map_err(e)?;
            tx.commit().map_err(e)?;
        }
        Ok(())
    }

    // ── active layout ────────────────────────────────────────────────────────────────────────

    pub fn active_layout(&self, server: &str) -> StoreResult<Option<ActiveLayout>> {
        let c = self.conn();
        c.query_row(
            "SELECT server, layout, cards_json, since, set_by, job_id FROM active_layout WHERE server = ?1",
            [server],
            |r| {
                let cards: String = r.get(2)?;
                Ok(ActiveLayout {
                    server: r.get(0)?,
                    layout: r.get(1)?,
                    cards: serde_json::from_str(&cards).unwrap_or_default(),
                    since: r.get(3)?,
                    set_by: r.get(4)?,
                    job_id: r.get(5)?,
                })
            },
        )
        .optional()
        .map_err(e)
    }

    pub fn set_active_layout(&self, a: &ActiveLayout) -> StoreResult<()> {
        let cards = serde_json::to_string(&a.cards).map_err(|x| x.to_string())?;
        self.conn()
            .execute(
                "INSERT INTO active_layout(server, layout, cards_json, since, set_by, job_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(server) DO UPDATE SET layout = excluded.layout, cards_json = excluded.cards_json,
                   since = excluded.since, set_by = excluded.set_by, job_id = excluded.job_id",
                params![a.server, a.layout, cards, a.since, a.set_by, a.job_id],
            )
            .map_err(e)?;
        Ok(())
    }

    // ── card admin state ─────────────────────────────────────────────────────────────────────

    /// A card with no row is online.
    pub fn card_admin(&self, card: &str) -> StoreResult<CardAdminRow> {
        let c = self.conn();
        let row = c
            .query_row(
                "SELECT card, state, by, reason, since FROM card_admin WHERE card = ?1",
                [card],
                |r| {
                    let st: String = r.get(1)?;
                    Ok(CardAdminRow {
                        card: r.get(0)?,
                        state: CardAdmin::parse(&st).unwrap_or(CardAdmin::Online),
                        by: r.get(2)?,
                        reason: r.get(3)?,
                        since: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(e)?;
        Ok(row.unwrap_or(CardAdminRow {
            card: card.into(),
            state: CardAdmin::Online,
            by: String::new(),
            reason: String::new(),
            since: 0,
        }))
    }

    pub fn set_card_admin(&self, row: &CardAdminRow) -> StoreResult<()> {
        self.conn()
            .execute(
                "INSERT INTO card_admin(card, state, by, reason, since) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(card) DO UPDATE SET state = excluded.state, by = excluded.by,
                   reason = excluded.reason, since = excluded.since",
                params![row.card, row.state.as_str(), row.by, row.reason, row.since],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn offline_cards(&self) -> StoreResult<Vec<String>> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT card FROM card_admin WHERE state != 'online' ORDER BY card")
            .map_err(e)?;
        let rows = st.query_map([], |r| r.get(0)).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── instances ────────────────────────────────────────────────────────────────────────────

    pub fn put_instance(&self, i: &Instance) -> StoreResult<()> {
        let spec = i
            .spec
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_default());
        self.conn()
            .execute(
                "INSERT INTO instances(card, server, engine, spec_json, state, unhealthy, since, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(card) DO UPDATE SET server = excluded.server, engine = excluded.engine,
                   spec_json = excluded.spec_json, state = excluded.state, unhealthy = excluded.unhealthy,
                   since = excluded.since, detail = excluded.detail",
                params![i.card, i.server, i.engine, spec, i.state.as_str(), i.unhealthy, i.since, i.detail],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn instances(&self, server: Option<&str>) -> StoreResult<Vec<Instance>> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT card, server, engine, spec_json, state, unhealthy, since, detail FROM instances
                 WHERE ?1 IS NULL OR server = ?1 ORDER BY card",
            )
            .map_err(e)?;
        let rows = st
            .query_map([server], |r| {
                let spec: Option<String> = r.get(3)?;
                let state: String = r.get(4)?;
                Ok(Instance {
                    card: r.get(0)?,
                    server: r.get(1)?,
                    engine: r.get(2)?,
                    spec: spec.and_then(|s| serde_json::from_str(&s).ok()),
                    state: InstanceState::parse(&state).unwrap_or(InstanceState::Unknown),
                    unhealthy: r.get(5)?,
                    since: r.get(6)?,
                    detail: r.get(7)?,
                    restarted_in_job: false,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── jobs and the step journal ────────────────────────────────────────────────────────────

    pub fn insert_job(&self, j: &JobRow) -> StoreResult<()> {
        self.conn()
            .execute(
                "INSERT INTO jobs(id, server, kind, target, from_layout, requested_by, state, created, started, ended, plan_json, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    j.id, j.server, j.kind, j.target, j.from_layout, j.requested_by, j.state.as_str(),
                    j.created, j.started, j.ended, j.plan_json, j.error
                ],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn update_job_state(
        &self,
        id: &str,
        state: JobState,
        now: i64,
        error: Option<&str>,
    ) -> StoreResult<()> {
        let c = self.conn();
        let n = c
            .execute(
                "UPDATE jobs SET state = ?2,
                    started = CASE WHEN ?2 = 'running' AND started IS NULL THEN ?3 ELSE started END,
                    ended = CASE WHEN ?2 IN ('queued','running') THEN ended ELSE ?3 END,
                    error = COALESCE(?4, error)
                 WHERE id = ?1",
                params![id, state.as_str(), now, error],
            )
            .map_err(e)?;
        if n == 0 {
            return Err(format!("no job {id}"));
        }
        Ok(())
    }

    pub fn set_job_plan(&self, id: &str, plan_json: &str) -> StoreResult<()> {
        self.conn()
            .execute(
                "UPDATE jobs SET plan_json = ?2 WHERE id = ?1",
                params![id, plan_json],
            )
            .map_err(e)?;
        Ok(())
    }

    fn job_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
        let st: String = r.get(6)?;
        Ok(JobRow {
            id: r.get(0)?,
            server: r.get(1)?,
            kind: r.get(2)?,
            target: r.get(3)?,
            from_layout: r.get(4)?,
            requested_by: r.get(5)?,
            state: JobState::parse(&st).unwrap_or(JobState::Interrupted),
            created: r.get(7)?,
            started: r.get(8)?,
            ended: r.get(9)?,
            plan_json: r.get(10)?,
            error: r.get(11)?,
        })
    }

    const JOB_COLS: &'static str = "id, server, kind, target, from_layout, requested_by, state, created, started, ended, plan_json, error";

    pub fn job(&self, id: &str) -> StoreResult<Option<JobRow>> {
        let c = self.conn();
        c.query_row(
            &format!("SELECT {} FROM jobs WHERE id = ?1", Self::JOB_COLS),
            [id],
            Self::job_from,
        )
        .optional()
        .map_err(e)
    }

    /// Most recent first.
    pub fn jobs(&self, server: Option<&str>, limit: usize) -> StoreResult<Vec<JobRow>> {
        let c = self.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {} FROM jobs WHERE ?1 IS NULL OR server = ?1 ORDER BY created DESC, rowid DESC LIMIT ?2",
                Self::JOB_COLS
            ))
            .map_err(e)?;
        let rows = st
            .query_map(params![server, limit as i64], Self::job_from)
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Jobs that never ended (a crash mid-job): for crash recovery (5.7).
    pub fn unfinished_jobs(&self) -> StoreResult<Vec<JobRow>> {
        let c = self.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {} FROM jobs WHERE state IN ('queued','running') ORDER BY created",
                Self::JOB_COLS
            ))
            .map_err(e)?;
        let rows = st.query_map([], Self::job_from).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Journal a step as started; returns its sequence number.
    pub fn step_begin(&self, job_id: &str, name: &str, card: &str, now: i64) -> StoreResult<i64> {
        let c = self.conn();
        let seq: i64 = c
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM job_steps WHERE job_id = ?1",
                [job_id],
                |r| r.get(0),
            )
            .map_err(e)?;
        c.execute(
            "INSERT INTO job_steps(job_id, seq, name, card, state, started) VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
            params![job_id, seq, name, card, now],
        )
        .map_err(e)?;
        Ok(seq)
    }

    pub fn step_end(
        &self,
        job_id: &str,
        seq: i64,
        state: &str,
        now: i64,
        detail: &str,
    ) -> StoreResult<()> {
        self.conn()
            .execute(
                "UPDATE job_steps SET state = ?3, ended = ?4, detail = ?5 WHERE job_id = ?1 AND seq = ?2",
                params![job_id, seq, state, now, detail],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn steps(&self, job_id: &str) -> StoreResult<Vec<StepRow>> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT job_id, seq, name, card, state, started, ended, detail FROM job_steps
                 WHERE job_id = ?1 ORDER BY seq",
            )
            .map_err(e)?;
        let rows = st
            .query_map([job_id], |r| {
                Ok(StepRow {
                    job_id: r.get(0)?,
                    seq: r.get(1)?,
                    name: r.get(2)?,
                    card: r.get(3)?,
                    state: r.get(4)?,
                    started: r.get(5)?,
                    ended: r.get(6)?,
                    detail: r.get(7)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── audit ────────────────────────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn audit(
        &self,
        ts: i64,
        who: &str,
        action: &str,
        old_rev: &str,
        new_rev: &str,
        diff: &str,
        reason: &str,
    ) -> StoreResult<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO audit(ts, who, action, old_rev, new_rev, diff, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts, who, action, old_rev, new_rev, diff, reason],
        )
        .map_err(e)?;
        Ok(c.last_insert_rowid())
    }

    pub fn audit_rows(&self, limit: usize) -> StoreResult<Vec<AuditRow>> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT id, ts, who, action, old_rev, new_rev, diff, reason FROM audit ORDER BY id DESC LIMIT ?1")
            .map_err(e)?;
        let rows = st
            .query_map([limit as i64], |r| {
                Ok(AuditRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    who: r.get(2)?,
                    action: r.get(3)?,
                    old_rev: r.get(4)?,
                    new_rev: r.get(5)?,
                    diff: r.get(6)?,
                    reason: r.get(7)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── OOM kills, estimates, restores, ensure counters ──────────────────────────────────────

    pub fn record_oom(
        &self,
        ts: i64,
        server: &str,
        job_id: Option<&str>,
        line: &str,
    ) -> StoreResult<()> {
        self.conn()
            .execute(
                "INSERT INTO oom_events(ts, server, job_id, line) VALUES (?1, ?2, ?3, ?4)",
                params![ts, server, job_id, line],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn oom_events(&self, server: &str, since: i64) -> StoreResult<Vec<(i64, String)>> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT ts, line FROM oom_events WHERE server = ?1 AND ts >= ?2 ORDER BY ts")
            .map_err(e)?;
        let rows = st
            .query_map(params![server, since], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Fold a new sample into a learned value (exponential moving average, weight 0.3).
    pub fn learn_estimate(
        &self,
        engine: &str,
        card: &str,
        kind: &str,
        sample: f64,
    ) -> StoreResult<f64> {
        let c = self.conn();
        let old: Option<(f64, i64)> = c
            .query_row(
                "SELECT value, samples FROM estimates WHERE engine = ?1 AND card = ?2 AND kind = ?3",
                params![engine, card, kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(e)?;
        let (value, n) = match old {
            Some((v, n)) => (v * 0.7 + sample * 0.3, n + 1),
            None => (sample, 1),
        };
        c.execute(
            "INSERT INTO estimates(engine, card, kind, value, samples) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(engine, card, kind) DO UPDATE SET value = excluded.value, samples = excluded.samples",
            params![engine, card, kind, value, n],
        )
        .map_err(e)?;
        Ok(value)
    }

    pub fn estimate(&self, engine: &str, card: &str, kind: &str) -> StoreResult<Option<f64>> {
        self.conn()
            .query_row(
                "SELECT value FROM estimates WHERE engine = ?1 AND card = ?2 AND kind = ?3",
                params![engine, card, kind],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)
    }

    pub fn record_restore(
        &self,
        ts: i64,
        server: &str,
        file: &str,
        bytes: i64,
        seconds: f64,
        sha_ok: bool,
    ) -> StoreResult<()> {
        self.conn()
            .execute(
                "INSERT INTO restores(ts, server, file, bytes, seconds, sha_ok) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![ts, server, file, bytes, seconds, sha_ok],
            )
            .map_err(e)?;
        Ok(())
    }

    /// Increment and return the ensure tries for a card in this boot.
    pub fn bump_ensure(&self, card: &str, boot_id: &str, now: i64) -> StoreResult<u32> {
        let c = self.conn();
        c.execute(
            "INSERT INTO ensure_tries(card, boot_id, tries, last) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(card, boot_id) DO UPDATE SET tries = tries + 1, last = excluded.last",
            params![card, boot_id, now],
        )
        .map_err(e)?;
        c.query_row(
            "SELECT tries FROM ensure_tries WHERE card = ?1 AND boot_id = ?2",
            params![card, boot_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u32)
        .map_err(e)
    }

    pub fn ensure_tries(&self, card: &str, boot_id: &str) -> StoreResult<(u32, i64)> {
        Ok(self
            .conn()
            .query_row(
                "SELECT tries, last FROM ensure_tries WHERE card = ?1 AND boot_id = ?2",
                params![card, boot_id],
                |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?)),
            )
            .optional()
            .map_err(e)?
            .unwrap_or((0, 0)))
    }

    pub fn reset_ensure(&self, card: &str, boot_id: &str) -> StoreResult<()> {
        self.conn()
            .execute(
                "DELETE FROM ensure_tries WHERE card = ?1 AND boot_id = ?2",
                params![card, boot_id],
            )
            .map_err(e)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_per_part_and_refuse_newer() {
        let s = Store::memory().unwrap();
        s.migrate_part("leases", &["CREATE TABLE t1 (x INTEGER)"])
            .unwrap();
        s.migrate_part(
            "leases",
            &["CREATE TABLE t1 (x INTEGER)", "ALTER TABLE t1 ADD y TEXT"],
        )
        .unwrap();
        s.conn()
            .execute("INSERT INTO t1(x, y) VALUES (1, 'a')", [])
            .unwrap();
        assert!(
            s.migrate_part("leases", &["CREATE TABLE t1 (x INTEGER)"])
                .unwrap_err()
                .contains("newer")
        );
        // Re-running core is a no-op.
        s.migrate_part("core", CORE).unwrap();
    }

    #[test]
    fn file_db_reopens() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("router.db");
        {
            let s = Store::open(&p).unwrap();
            s.set_card_admin(&CardAdminRow {
                card: "B".into(),
                state: CardAdmin::Offline,
                by: "admin".into(),
                reason: "fan".into(),
                since: 5,
            })
            .unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.card_admin("B").unwrap().state, CardAdmin::Offline);
        assert_eq!(s.card_admin("A").unwrap().state, CardAdmin::Online);
        assert_eq!(s.offline_cards().unwrap(), vec!["B"]);
    }

    #[test]
    fn active_layout_and_instances_roundtrip() {
        let s = Store::memory().unwrap();
        assert!(s.active_layout("dt").unwrap().is_none());
        let spec = CardSpec {
            engine: "ninfer".into(),
            profile: "p".into(),
            model: "m".into(),
        };
        let mut cards = indexmap::IndexMap::new();
        cards.insert("A".to_string(), Some(spec.clone()));
        cards.insert("B".to_string(), None);
        let a = ActiveLayout {
            server: "dt".into(),
            layout: "x2".into(),
            cards,
            since: 1,
            set_by: "boot".into(),
            job_id: None,
        };
        s.set_active_layout(&a).unwrap();
        assert_eq!(s.active_layout("dt").unwrap().unwrap(), a);

        let i = Instance {
            server: "dt".into(),
            card: "A".into(),
            engine: "ninfer".into(),
            spec: Some(spec),
            state: InstanceState::Ready,
            since: 3,
            detail: "adopted".into(),
            ..Default::default()
        };
        s.put_instance(&i).unwrap();
        assert_eq!(s.instances(Some("dt")).unwrap(), vec![i.clone()]);
        assert_eq!(s.instances(None).unwrap().len(), 1);
        assert!(s.instances(Some("other")).unwrap().is_empty());
    }

    #[test]
    fn jobs_and_journal() {
        let s = Store::memory().unwrap();
        let j = JobRow {
            id: "j1".into(),
            server: "dt".into(),
            kind: "switch".into(),
            target: "flash-x2".into(),
            from_layout: "x2".into(),
            requested_by: "admin".into(),
            state: JobState::Queued,
            created: 10,
            started: None,
            ended: None,
            plan_json: None,
            error: None,
        };
        s.insert_job(&j).unwrap();
        s.update_job_state("j1", JobState::Running, 11, None)
            .unwrap();
        let a = s.step_begin("j1", "stop", "A", 12).unwrap();
        let b = s.step_begin("j1", "start", "A", 13).unwrap();
        assert_eq!((a, b), (1, 2));
        s.step_end("j1", a, "ok", 13, "").unwrap();
        assert_eq!(s.unfinished_jobs().unwrap().len(), 1);
        s.update_job_state("j1", JobState::Failed, 20, Some("health timeout"))
            .unwrap();
        let got = s.job("j1").unwrap().unwrap();
        assert_eq!(
            (got.started, got.ended, got.error.as_deref()),
            (Some(11), Some(20), Some("health timeout"))
        );
        assert!(s.unfinished_jobs().unwrap().is_empty());
        let steps = s.steps("j1").unwrap();
        assert_eq!(steps[0].state, "ok");
        assert_eq!(steps[1].state, "running");
        assert!(s.update_job_state("nope", JobState::Done, 1, None).is_err());
    }

    #[test]
    fn audit_estimates_ensure() {
        let s = Store::memory().unwrap();
        s.audit(1, "file", "config.file-edit", "aaa", "bbb", "-x\n+y", "")
            .unwrap();
        assert_eq!(s.audit_rows(10).unwrap()[0].new_rev, "bbb");
        assert_eq!(
            s.learn_estimate("ninfer", "A", "start", 70.0).unwrap(),
            70.0
        );
        let v = s.learn_estimate("ninfer", "A", "start", 80.0).unwrap();
        assert!((v - 73.0).abs() < 1e-9);
        assert_eq!(s.bump_ensure("A", "boot1", 5).unwrap(), 1);
        assert_eq!(s.bump_ensure("A", "boot1", 6).unwrap(), 2);
        assert_eq!(s.ensure_tries("A", "boot2").unwrap().0, 0);
    }
}
