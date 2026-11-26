//! The calendar's part of `router.db` (SUPERVISOR.md 8.1, 9, 10.2): what happened, never what
//! is planned (that is `calendar.toml`). Migrated as its own part, `calendar`.
//!
//! - `cal_overrides`: one occurrence skipped or ended early by an admin;
//! - `cal_runs`: live detection runs (survive a restart, so an extension is not lost);
//! - `cal_dynamic`: runtime one-off entries (the bench queue's), with their expiry;
//! - `event_keys`: `abe_` keys, SHA-256 only;
//! - `event_key_uses`: every use of an event key and its decision (the audit);
//! - `alerts`: one row per alert identity (first, last, count, state).

use rusqlite::{OptionalExtension, params};

use crate::control::store::{Store, StoreResult};
use orca_calendar::keys::KeyRecord;
use orca_calendar::live::Run;

const MIGRATIONS: &[&str] = &["CREATE TABLE cal_overrides (
        occurrence TEXT PRIMARY KEY,
        entry TEXT NOT NULL,
        kind TEXT NOT NULL,
        at INTEGER NOT NULL,
        who TEXT NOT NULL,
        reason TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE cal_runs (
        entry TEXT NOT NULL,
        start INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        ongoing INTEGER NOT NULL,
        detail TEXT,
        PRIMARY KEY (entry, start)
    );
    CREATE TABLE cal_dynamic (
        id TEXT PRIMARY KEY,
        entry_json TEXT NOT NULL,
        source TEXT NOT NULL,
        created INTEGER NOT NULL,
        expires INTEGER NOT NULL
    );
    CREATE TABLE event_keys (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        event TEXT NOT NULL,
        prefix TEXT NOT NULL,
        key_hash TEXT NOT NULL UNIQUE,
        owner TEXT NOT NULL,
        created INTEGER NOT NULL,
        revoked INTEGER,
        retire_at INTEGER,
        last_used INTEGER
    );
    CREATE INDEX event_keys_event ON event_keys(event);
    CREATE TABLE event_key_uses (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts INTEGER NOT NULL,
        key_id INTEGER NOT NULL,
        event TEXT NOT NULL,
        occurrence TEXT NOT NULL DEFAULT '',
        action TEXT NOT NULL,
        decision TEXT NOT NULL,
        status INTEGER NOT NULL,
        detail TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX event_key_uses_event ON event_key_uses(event, ts);
    CREATE TABLE alerts (
        identity TEXT PRIMARY KEY,
        rule TEXT NOT NULL,
        state TEXT NOT NULL,
        last INTEGER NOT NULL,
        json TEXT NOT NULL
    );"];

fn e(x: rusqlite::Error) -> String {
    x.to_string()
}

/// One use of an event key (the audit row).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeyUse {
    pub ts: i64,
    pub key_id: i64,
    pub event: String,
    pub occurrence: String,
    pub action: String,
    pub decision: String,
    pub status: i64,
    pub detail: String,
}

pub struct CalStore<'a>(pub &'a Store);

impl CalStore<'_> {
    pub fn migrate(&self) -> StoreResult<()> {
        self.0.migrate_part("calendar", MIGRATIONS)
    }

    // ── overrides ────────────────────────────────────────────────────────────────────────────

    pub fn set_override(
        &self,
        occurrence: &str,
        entry: &str,
        kind: &str,
        at: i64,
        who: &str,
        reason: &str,
    ) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "INSERT INTO cal_overrides(occurrence, entry, kind, at, who, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(occurrence) DO UPDATE SET kind = excluded.kind, at = excluded.at, who = excluded.who, reason = excluded.reason",
                params![occurrence, entry, kind, at, who, reason],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn clear_override(&self, occurrence: &str) -> StoreResult<bool> {
        Ok(self
            .0
            .conn()
            .execute(
                "DELETE FROM cal_overrides WHERE occurrence = ?1",
                [occurrence],
            )
            .map_err(e)?
            > 0)
    }

    /// `(occurrence, kind, at)`.
    pub fn overrides(&self) -> StoreResult<Vec<(String, String, i64)>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT occurrence, kind, at FROM cal_overrides")
            .map_err(e)?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── runs ─────────────────────────────────────────────────────────────────────────────────

    pub fn put_run(&self, r: &Run) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "INSERT INTO cal_runs(entry, start, last_seen, ongoing, detail) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(entry, start) DO UPDATE SET last_seen = excluded.last_seen, ongoing = excluded.ongoing, detail = excluded.detail",
                params![r.entry, r.start, r.last_seen, r.ongoing as i64, r.detail],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn runs(&self, since: i64) -> StoreResult<Vec<Run>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT entry, start, last_seen, ongoing, detail FROM cal_runs WHERE last_seen >= ?1 OR ongoing = 1 ORDER BY start")
            .map_err(e)?;
        let rows = st
            .query_map([since], |r| {
                Ok(Run {
                    entry: r.get(0)?,
                    start: r.get(1)?,
                    last_seen: r.get(2)?,
                    ongoing: r.get::<_, i64>(3)? != 0,
                    detail: r.get(4)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    pub fn prune_runs(&self, before: i64) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "DELETE FROM cal_runs WHERE ongoing = 0 AND last_seen < ?1",
                [before],
            )
            .map_err(e)?;
        Ok(())
    }

    // ── dynamic entries ──────────────────────────────────────────────────────────────────────

    pub fn put_dynamic(
        &self,
        id: &str,
        entry_json: &str,
        source: &str,
        created: i64,
        expires: i64,
    ) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "INSERT INTO cal_dynamic(id, entry_json, source, created, expires) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET entry_json = excluded.entry_json, expires = excluded.expires",
                params![id, entry_json, source, created, expires],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn delete_dynamic(&self, id: &str) -> StoreResult<bool> {
        Ok(self
            .0
            .conn()
            .execute("DELETE FROM cal_dynamic WHERE id = ?1", [id])
            .map_err(e)?
            > 0)
    }

    /// `(id, entry_json, source, expires)` not yet expired at `now`.
    pub fn dynamic(&self, now: i64) -> StoreResult<Vec<(String, String, String, i64)>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT id, entry_json, source, expires FROM cal_dynamic WHERE expires > ?1 ORDER BY created")
            .map_err(e)?;
        let rows = st
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    // ── event keys ───────────────────────────────────────────────────────────────────────────

    pub fn insert_key(
        &self,
        event: &str,
        prefix: &str,
        hash: &str,
        owner: &str,
        created: i64,
    ) -> StoreResult<i64> {
        let c = self.0.conn();
        c.execute(
            "INSERT INTO event_keys(event, prefix, key_hash, owner, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event, prefix, hash, owner, created],
        )
        .map_err(e)?;
        Ok(c.last_insert_rowid())
    }

    fn key_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRecord> {
        Ok(KeyRecord {
            id: r.get(0)?,
            event: r.get(1)?,
            prefix: r.get(2)?,
            owner: r.get(3)?,
            created: r.get(4)?,
            revoked: r.get(5)?,
            retire_at: r.get(6)?,
            last_used: r.get(7)?,
        })
    }

    pub fn key_by_hash(&self, hash: &str) -> StoreResult<Option<KeyRecord>> {
        self.0
            .conn()
            .query_row(
                "SELECT id, event, prefix, owner, created, revoked, retire_at, last_used FROM event_keys WHERE key_hash = ?1",
                [hash],
                Self::key_row,
            )
            .optional()
            .map_err(e)
    }

    pub fn keys(&self, event: Option<&str>) -> StoreResult<Vec<KeyRecord>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT id, event, prefix, owner, created, revoked, retire_at, last_used FROM event_keys WHERE ?1 IS NULL OR event = ?1 ORDER BY id DESC")
            .map_err(e)?;
        let rows = st.query_map([event], Self::key_row).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    pub fn revoke_key(&self, id: i64, at: i64) -> StoreResult<bool> {
        Ok(self
            .0
            .conn()
            .execute(
                "UPDATE event_keys SET revoked = ?2 WHERE id = ?1 AND revoked IS NULL",
                params![id, at],
            )
            .map_err(e)?
            > 0)
    }

    pub fn retire_key(&self, id: i64, at: i64) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "UPDATE event_keys SET retire_at = ?2 WHERE id = ?1 AND revoked IS NULL AND (retire_at IS NULL OR retire_at > ?2)",
                params![id, at],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn touch_key(&self, id: i64, now: i64) {
        let _ = self.0.conn().execute(
            "UPDATE event_keys SET last_used = ?2 WHERE id = ?1 AND (last_used IS NULL OR last_used < ?2 - 30)",
            params![id, now],
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn key_use(
        &self,
        ts: i64,
        key_id: i64,
        event: &str,
        occurrence: &str,
        action: &str,
        decision: &str,
        status: u16,
        detail: &str,
    ) -> StoreResult<()> {
        self.0
            .conn()
            .execute(
                "INSERT INTO event_key_uses(ts, key_id, event, occurrence, action, decision, status, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![ts, key_id, event, occurrence, action, decision, status, detail],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn key_uses(&self, event: &str, limit: usize) -> StoreResult<Vec<KeyUse>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT ts, key_id, event, occurrence, action, decision, status, detail FROM event_key_uses WHERE event = ?1 ORDER BY id DESC LIMIT ?2")
            .map_err(e)?;
        let rows = st
            .query_map(params![event, limit as i64], |r| {
                Ok(KeyUse {
                    ts: r.get(0)?,
                    key_id: r.get(1)?,
                    event: r.get(2)?,
                    occurrence: r.get(3)?,
                    action: r.get(4)?,
                    decision: r.get(5)?,
                    status: r.get(6)?,
                    detail: r.get(7)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Denied-outside-window uses of a key since `since` (the leak alert, 9.6).
    pub fn outside_window_uses(&self, key_id: i64, since: i64) -> StoreResult<i64> {
        self.0
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM event_key_uses WHERE key_id = ?1 AND ts >= ?2 AND decision = 'outside_window'",
                params![key_id, since],
                |r| r.get(0),
            )
            .map_err(e)
    }

    // ── alerts ───────────────────────────────────────────────────────────────────────────────

    pub fn put_alert(&self, a: &orca_alerts::Alert) -> StoreResult<()> {
        let json = serde_json::to_string(a).map_err(|x| x.to_string())?;
        let state = serde_json::to_value(a.state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        self.0
            .conn()
            .execute(
                "INSERT INTO alerts(identity, rule, state, last, json) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(identity) DO UPDATE SET rule = excluded.rule, state = excluded.state, last = excluded.last, json = excluded.json",
                params![a.identity, a.rule, state, a.last, json],
            )
            .map_err(e)?;
        Ok(())
    }

    pub fn alerts(&self, since: i64) -> StoreResult<Vec<orca_alerts::Alert>> {
        let c = self.0.conn();
        let mut st = c
            .prepare("SELECT json FROM alerts WHERE last >= ?1 OR state != 'resolved' ORDER BY last DESC")
            .map_err(e)?;
        let rows = st
            .query_map([since], |r| r.get::<_, String>(0))
            .map_err(e)?;
        let mut out = Vec::new();
        for j in rows {
            if let Ok(a) = serde_json::from_str(&j.map_err(e)?) {
                out.push(a);
            }
        }
        Ok(out)
    }
}
