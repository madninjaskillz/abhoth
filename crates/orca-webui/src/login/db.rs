//! `UserDb`: the web UI's accounts in SQLite (`<home>/abhoth.db`, D140): users, browser
//! sessions, API keys, 2FA recovery codes and the admin audit log. Secrets that are only ever
//! compared (session cookies, API keys, recovery codes) are stored as SHA-256 hashes; passwords
//! as Argon2id (see [`super::password`]). Migrations are numbered by `PRAGMA user_version`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The migrations, in order; `user_version` is the number applied.
const MIGRATIONS: &[&str] = &[
    // 1: accounts, sessions, API keys, recovery codes, audit, meta.
    "CREATE TABLE users(
        id INTEGER PRIMARY KEY,
        username TEXT NOT NULL,
        username_key TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        is_admin INTEGER NOT NULL DEFAULT 0,
        disabled INTEGER NOT NULL DEFAULT 0,
        totp_secret TEXT,
        totp_enabled INTEGER NOT NULL DEFAULT 0,
        totp_last_step INTEGER NOT NULL DEFAULT 0,
        totp_pending TEXT,
        created INTEGER NOT NULL,
        last_login INTEGER
    );
    CREATE TABLE sessions(
        id INTEGER PRIMARY KEY,
        token_hash BLOB NOT NULL UNIQUE,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        stage TEXT NOT NULL,
        csrf TEXT NOT NULL,
        created INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        ip TEXT,
        user_agent TEXT,
        desktop INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX sessions_user ON sessions(user_id);
    CREATE TABLE api_keys(
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        prefix TEXT NOT NULL,
        key_hash BLOB NOT NULL UNIQUE,
        created INTEGER NOT NULL,
        last_used INTEGER
    );
    CREATE TABLE recovery_codes(
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        code_hash BLOB NOT NULL,
        used INTEGER
    );
    CREATE INDEX recovery_user ON recovery_codes(user_id);
    CREATE TABLE audit(
        id INTEGER PRIMARY KEY,
        time INTEGER NOT NULL,
        event TEXT NOT NULL,
        actor_id INTEGER,
        actor TEXT,
        target TEXT,
        ip TEXT,
        detail TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
];

/// SHA-256 of a high-entropy secret (a cookie, an API key, a recovery code).
pub fn secret_hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// The lookup key of a username (unique regardless of case).
pub fn username_key(name: &str) -> String {
    name.trim().to_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
    pub admin: bool,
    pub disabled: bool,
    #[serde(rename = "totp")]
    pub totp_enabled: bool,
    #[serde(skip)]
    pub totp_secret: Option<String>,
    #[serde(skip)]
    pub totp_last_step: i64,
    #[serde(skip)]
    pub totp_pending: Option<String>,
    pub created: i64,
    pub last_login: Option<i64>,
}

impl User {
    /// The desktop's local admin (D142): no password yet, so it signs in only through the
    /// desktop's per-launch token until one is set.
    pub fn local_only(&self) -> bool {
        self.password_hash.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub stage: String,
    pub csrf: String,
    pub created: i64,
    pub last_seen: i64,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub desktop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    pub id: i64,
    #[serde(skip)]
    pub user_id: i64,
    pub name: String,
    pub prefix: String,
    pub created: i64,
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    pub time: i64,
    pub event: String,
    pub actor: Option<String>,
    pub target: Option<String>,
    pub ip: Option<String>,
    pub detail: Value,
}

/// Why a user could not be created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreateError {
    #[error("That username is taken.")]
    Taken,
    /// Setup: a user exists already.
    #[error("The admin account exists already.")]
    NotFirst,
    #[error("{0}")]
    Db(String),
}

fn db_err(e: rusqlite::Error) -> String {
    e.to_string()
}

pub struct UserDb {
    conn: Mutex<Connection>,
    path: PathBuf,
}

const USER_COLS: &str = "id, username, password_hash, is_admin, disabled, totp_secret, totp_enabled, totp_last_step, totp_pending, created, last_login";

fn user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        admin: r.get::<_, i64>(3)? != 0,
        disabled: r.get::<_, i64>(4)? != 0,
        totp_secret: r.get(5)?,
        totp_enabled: r.get::<_, i64>(6)? != 0,
        totp_last_step: r.get(7)?,
        totp_pending: r.get(8)?,
        created: r.get(9)?,
        last_login: r.get(10)?,
    })
}

const SESSION_COLS: &str = "id, user_id, stage, csrf, created, last_seen, ip, user_agent, desktop";

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        user_id: r.get(1)?,
        stage: r.get(2)?,
        csrf: r.get(3)?,
        created: r.get(4)?,
        last_seen: r.get(5)?,
        ip: r.get(6)?,
        user_agent: r.get(7)?,
        desktop: r.get::<_, i64>(8)? != 0,
    })
}

impl UserDb {
    /// Open (creating it and its folder) and migrate. The file is private to this OS user on
    /// Unix.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Self::init(conn, path.to_path_buf())
    }

    /// An in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        Self::init(
            Connection::open_in_memory().map_err(db_err)?,
            PathBuf::from(":memory:"),
        )
    }

    fn init(conn: Connection, path: PathBuf) -> Result<Self, String> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        // WAL lets `abhoth admin ...` write while the server runs; the result is a mode name.
        let _: Option<String> = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(db_err)?;
        let db = Self {
            conn: Mutex::new(conn),
            path,
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err)?;
        if version as usize > MIGRATIONS.len() {
            return Err(format!(
                "{} was written by a newer {} (schema {version}); upgrade to use it.",
                self.path.display(),
                orca_core::brand::PRODUCT_NAME
            ));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction().map_err(db_err)?;
            tx.execute_batch(sql).map_err(db_err)?;
            tx.execute_batch(&format!("PRAGMA user_version={}", i + 1))
                .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0)
    }

    // ---- meta ----

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        );
    }

    // ---- users ----

    /// Users who can sign in with a password (not the desktop's local-only admin).
    pub fn password_user_count(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM users WHERE password_hash <> ''",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    pub fn user_count(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Insert a user. With `only_if_empty`, the insert happens only while the table is empty,
    /// in the same statement (two concurrent setups cannot both win).
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        admin: bool,
        only_if_empty: bool,
        now: i64,
    ) -> Result<User, CreateError> {
        let conn = self.conn.lock().unwrap();
        let sql = if only_if_empty {
            "INSERT INTO users(username, username_key, password_hash, is_admin, created)
             SELECT ?1, ?2, ?3, ?4, ?5 WHERE NOT EXISTS (SELECT 1 FROM users)"
        } else {
            "INSERT INTO users(username, username_key, password_hash, is_admin, created)
             VALUES(?1, ?2, ?3, ?4, ?5)"
        };
        let n = conn
            .execute(
                sql,
                params![
                    username.trim(),
                    username_key(username),
                    password_hash,
                    admin as i64,
                    now
                ],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CreateError::Taken
                }
                e => CreateError::Db(e.to_string()),
            })?;
        if n == 0 {
            return Err(CreateError::NotFirst);
        }
        let id = conn.last_insert_rowid();
        conn.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE id=?1"),
            [id],
            user_row,
        )
        .map_err(|e| CreateError::Db(e.to_string()))
    }

    pub fn user(&self, id: i64) -> Option<User> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {USER_COLS} FROM users WHERE id=?1"),
                [id],
                user_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn user_by_name(&self, name: &str) -> Option<User> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {USER_COLS} FROM users WHERE username_key=?1"),
                [username_key(name)],
                user_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn users(&self) -> Vec<User> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut st) = conn.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY id")) else {
            return vec![];
        };
        st.query_map([], user_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// The owner of everything made before accounts existed (projects without an owner, the
    /// launch token): the first admin that is not disabled.
    pub fn first_admin(&self) -> Option<User> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                &format!(
                    "SELECT {USER_COLS} FROM users WHERE is_admin=1 AND disabled=0 ORDER BY id LIMIT 1"
                ),
                [],
                user_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn active_admin_count(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM users WHERE is_admin=1 AND disabled=0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    fn exec(&self, sql: &str, p: impl rusqlite::Params) -> bool {
        self.conn
            .lock()
            .unwrap()
            .execute(sql, p)
            .is_ok_and(|n| n > 0)
    }

    pub fn set_password_hash(&self, id: i64, hash: &str) -> bool {
        self.exec(
            "UPDATE users SET password_hash=?2 WHERE id=?1",
            params![id, hash],
        )
    }

    pub fn set_admin(&self, id: i64, admin: bool) -> bool {
        self.exec(
            "UPDATE users SET is_admin=?2 WHERE id=?1",
            params![id, admin as i64],
        )
    }

    pub fn set_disabled(&self, id: i64, disabled: bool) -> bool {
        self.exec(
            "UPDATE users SET disabled=?2 WHERE id=?1",
            params![id, disabled as i64],
        )
    }

    pub fn touch_login(&self, id: i64, now: i64) {
        self.exec(
            "UPDATE users SET last_login=?2 WHERE id=?1",
            params![id, now],
        );
    }

    /// A secret shown for setup, not yet confirmed.
    pub fn set_totp_pending(&self, id: i64, secret: Option<&str>) -> bool {
        self.exec(
            "UPDATE users SET totp_pending=?2 WHERE id=?1",
            params![id, secret],
        )
    }

    /// 2FA on with `secret` (`step`: the step of the code that confirmed it).
    pub fn enable_totp(&self, id: i64, secret: &str, step: i64) -> bool {
        self.exec(
            "UPDATE users SET totp_secret=?2, totp_enabled=1, totp_last_step=?3, totp_pending=NULL WHERE id=?1",
            params![id, secret, step],
        )
    }

    /// Record the step of a code just used, only when it is newer (a code works once, also
    /// against two concurrent requests).
    pub fn use_totp_step(&self, id: i64, step: i64) -> bool {
        self.exec(
            "UPDATE users SET totp_last_step=?2 WHERE id=?1 AND totp_last_step < ?2",
            params![id, step],
        )
    }

    pub fn disable_totp(&self, id: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE users SET totp_secret=NULL, totp_enabled=0, totp_last_step=0, totp_pending=NULL WHERE id=?1",
                [id],
            )
            .unwrap_or(0);
        let _ = conn.execute("DELETE FROM recovery_codes WHERE user_id=?1", [id]);
        n > 0
    }

    pub fn delete_user(&self, id: i64) -> bool {
        self.exec("DELETE FROM users WHERE id=?1", [id])
    }

    /// The legacy single account (`account.json`) imported as the first admin, with its
    /// PBKDF2 hash (re-hashed with Argon2id at the next sign-in) and its 2FA.
    pub fn import_legacy(
        &self,
        account: &super::accounts::Account,
        now: i64,
    ) -> Result<User, CreateError> {
        let user = self.create_user(&account.username, &account.password_hash, true, true, now)?;
        if account.totp_confirmed
            && let Some(secret) = &account.totp_secret
        {
            self.enable_totp(user.id, secret, account.totp_last_step);
        }
        Ok(self.user(user.id).unwrap_or(user))
    }

    // ---- sessions ----

    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        token: &str,
        user_id: i64,
        stage: &str,
        csrf: &str,
        now: i64,
        ip: Option<&str>,
        user_agent: Option<&str>,
        desktop: bool,
    ) -> Option<Session> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions(token_hash, user_id, stage, csrf, created, last_seen, ip, user_agent, desktop)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)",
            params![
                secret_hash(token),
                user_id,
                stage,
                csrf,
                now,
                ip,
                user_agent.map(|u| u.chars().take(200).collect::<String>()),
                desktop as i64
            ],
        )
        .ok()?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"),
            [id],
            session_row,
        )
        .ok()
    }

    pub fn session_by_token(&self, token: &str) -> Option<Session> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE token_hash=?1"),
                [secret_hash(token)],
                session_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn sessions_of(&self, user_id: i64) -> Vec<Session> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut st) = conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE user_id=?1 AND stage='ok' ORDER BY last_seen DESC"
        )) else {
            return vec![];
        };
        st.query_map([user_id], session_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    pub fn touch_session(&self, id: i64, now: i64) {
        self.exec(
            "UPDATE sessions SET last_seen=?2 WHERE id=?1",
            params![id, now],
        );
    }

    pub fn delete_session(&self, id: i64) -> bool {
        self.exec("DELETE FROM sessions WHERE id=?1", [id])
    }

    /// End a user's sessions, all or all but `keep`.
    pub fn delete_sessions_of(&self, user_id: i64, keep: Option<i64>) -> usize {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM sessions WHERE user_id=?1 AND id IS NOT ?2",
                params![user_id, keep],
            )
            .unwrap_or(0)
    }

    /// Drop sessions past their limits: signed-in ones `idle`/`absolute` seconds, half-signed-in
    /// ones `pending` seconds after they began. Desktop-window sessions end with the process
    /// that made them: a desktop process (`desktop_since`: its start) drops older ones.
    pub fn purge_sessions(
        &self,
        now: i64,
        idle: i64,
        absolute: i64,
        pending: i64,
        desktop_since: Option<i64>,
    ) {
        let _ = self.conn.lock().unwrap().execute(
            "DELETE FROM sessions WHERE
               (desktop=0 AND stage='ok' AND (?1 - created > ?3 OR ?1 - last_seen > ?2))
               OR (desktop=0 AND stage<>'ok' AND ?1 - created > ?4)
               OR (desktop=1 AND ?5 IS NOT NULL AND created < ?5)",
            params![now, idle, absolute, pending, desktop_since],
        );
    }

    // ---- API keys ----

    pub fn create_api_key(
        &self,
        user_id: i64,
        name: &str,
        key: &str,
        prefix: &str,
        now: i64,
    ) -> Option<ApiKey> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO api_keys(user_id, name, prefix, key_hash, created) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![user_id, name, prefix, secret_hash(key), now],
        )
        .ok()?;
        Some(ApiKey {
            id: conn.last_insert_rowid(),
            user_id,
            name: name.into(),
            prefix: prefix.into(),
            created: now,
            last_used: None,
        })
    }

    pub fn api_key_by_secret(&self, key: &str) -> Option<ApiKey> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, user_id, name, prefix, created, last_used FROM api_keys WHERE key_hash=?1",
                [secret_hash(key)],
                |r| {
                    Ok(ApiKey {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        name: r.get(2)?,
                        prefix: r.get(3)?,
                        created: r.get(4)?,
                        last_used: r.get(5)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn api_keys_of(&self, user_id: i64) -> Vec<ApiKey> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut st) = conn.prepare(
            "SELECT id, user_id, name, prefix, created, last_used FROM api_keys WHERE user_id=?1 ORDER BY id",
        ) else {
            return vec![];
        };
        st.query_map([user_id], |r| {
            Ok(ApiKey {
                id: r.get(0)?,
                user_id: r.get(1)?,
                name: r.get(2)?,
                prefix: r.get(3)?,
                created: r.get(4)?,
                last_used: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn touch_api_key(&self, id: i64, now: i64) {
        self.exec(
            "UPDATE api_keys SET last_used=?2 WHERE id=?1 AND (last_used IS NULL OR last_used < ?2 - 60)",
            params![id, now],
        );
    }

    /// Revoke one of `user_id`'s keys.
    pub fn delete_api_key(&self, user_id: i64, id: i64) -> bool {
        self.exec(
            "DELETE FROM api_keys WHERE id=?1 AND user_id=?2",
            params![id, user_id],
        )
    }

    // ---- recovery codes ----

    /// Replace the user's recovery codes with these (hashed).
    pub fn set_recovery_codes(&self, user_id: i64, codes: &[String]) {
        let mut conn = self.conn.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        let _ = tx.execute("DELETE FROM recovery_codes WHERE user_id=?1", [user_id]);
        for c in codes {
            let _ = tx.execute(
                "INSERT INTO recovery_codes(user_id, code_hash) VALUES(?1, ?2)",
                params![user_id, secret_hash(&super::recovery::normalize(c))],
            );
        }
        let _ = tx.commit();
    }

    /// Use a recovery code: true once per code.
    pub fn use_recovery_code(&self, user_id: i64, code: &str, now: i64) -> bool {
        self.exec(
            "UPDATE recovery_codes SET used=?3 WHERE user_id=?1 AND code_hash=?2 AND used IS NULL",
            params![user_id, secret_hash(&super::recovery::normalize(code)), now],
        )
    }

    pub fn recovery_codes_left(&self, user_id: i64) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM recovery_codes WHERE user_id=?1 AND used IS NULL",
                [user_id],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    // ---- audit ----

    #[allow(clippy::too_many_arguments)]
    pub fn audit(
        &self,
        now: i64,
        event: &str,
        actor_id: Option<i64>,
        actor: Option<&str>,
        target: Option<&str>,
        ip: Option<&str>,
        detail: &Value,
    ) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO audit(time, event, actor_id, actor, target, ip, detail) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![now, event, actor_id, actor, target, ip, detail.to_string()],
        );
    }

    /// The newest entries first, older than `before` when given.
    pub fn audit_entries(&self, limit: i64, before: Option<i64>) -> Vec<AuditEntry> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut st) = conn.prepare(
            "SELECT id, time, event, actor, target, ip, detail FROM audit
             WHERE ?2 IS NULL OR id < ?2 ORDER BY id DESC LIMIT ?1",
        ) else {
            return vec![];
        };
        st.query_map(params![limit.clamp(1, 1000), before], |r| {
            let detail: String = r.get(6)?;
            Ok(AuditEntry {
                id: r.get(0)?,
                time: r.get(1)?,
                event: r.get(2)?,
                actor: r.get(3)?,
                target: r.get(4)?,
                ip: r.get(5)?,
                detail: serde_json::from_str(&detail).unwrap_or(Value::Null),
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a").join("abhoth.db");
        let db = UserDb::open(&p).unwrap();
        assert_eq!(db.schema_version(), MIGRATIONS.len() as i64);
        db.create_user("Ann", "h", true, false, 1).unwrap();
        drop(db);
        let db = UserDb::open(&p).unwrap();
        assert_eq!(db.user_count(), 1);
        db.conn
            .lock()
            .unwrap()
            .execute_batch("PRAGMA user_version=99")
            .unwrap();
        drop(db);
        let e = UserDb::open(&p).err().unwrap();
        assert!(e.contains("newer"), "{e}");
    }

    #[test]
    fn usernames_are_unique_regardless_of_case_and_setup_wins_once() {
        let db = UserDb::open_in_memory().unwrap();
        let a = db.create_user("Ann", "h", true, true, 1).unwrap();
        assert!(a.admin);
        assert_eq!(
            db.create_user("bob", "h", true, true, 1),
            Err(CreateError::NotFirst)
        );
        assert_eq!(
            db.create_user(" ANN ", "h", false, false, 1),
            Err(CreateError::Taken)
        );
        assert_eq!(db.user_by_name("ann").unwrap().id, a.id);
        assert_eq!(db.first_admin().unwrap().id, a.id);
    }

    #[test]
    fn concurrent_setups_have_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abhoth.db");
        UserDb::open(&p).unwrap();
        let wins: usize = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    let db = UserDb::open(&p).unwrap();
                    db.create_user(&format!("u{i}"), "h", true, true, 1).is_ok() as usize
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .sum();
        assert_eq!(wins, 1);
        assert_eq!(UserDb::open(&p).unwrap().user_count(), 1);
    }

    #[test]
    fn secrets_are_stored_hashed_and_deleting_a_user_cascades() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abhoth.db");
        let db = UserDb::open(&p).unwrap();
        let u = db.create_user("ann", "h", false, false, 1).unwrap();
        db.create_session(
            "cookie-secret-value",
            u.id,
            "ok",
            "csrf",
            1,
            None,
            None,
            false,
        )
        .unwrap();
        db.create_api_key(u.id, "ci", "abk_key-secret-value", "abk_key", 1)
            .unwrap();
        db.set_recovery_codes(u.id, &["aaaaa-bbbbb".into()]);
        assert!(db.session_by_token("cookie-secret-value").is_some());
        assert!(db.api_key_by_secret("abk_key-secret-value").is_some());
        assert!(db.use_recovery_code(u.id, "AAAAA BBBBB", 2));
        assert!(!db.use_recovery_code(u.id, "aaaaa-bbbbb", 3));
        drop(db);
        let bytes = std::fs::read(&p).unwrap_or_default();
        let wal = std::fs::read(p.with_extension("db-wal")).unwrap_or_default();
        for raw in [&bytes, &wal] {
            let text = String::from_utf8_lossy(raw);
            assert!(!text.contains("cookie-secret-value"));
            assert!(!text.contains("key-secret-value"));
        }
        let db = UserDb::open(&p).unwrap();
        assert!(db.delete_user(u.id));
        assert!(db.session_by_token("cookie-secret-value").is_none());
        assert!(db.api_key_by_secret("abk_key-secret-value").is_none());
    }

    #[test]
    fn sessions_expire_by_idle_absolute_and_pending_limits() {
        let db = UserDb::open_in_memory().unwrap();
        let u = db.create_user("ann", "h", false, false, 0).unwrap();
        db.create_session("a", u.id, "ok", "c", 0, None, None, false);
        db.create_session("b", u.id, "totp", "c", 0, None, None, false);
        let c = db
            .create_session("c", u.id, "ok", "c", 0, None, None, false)
            .unwrap();
        db.touch_session(c.id, 90);
        db.purge_sessions(100, 50, 1000, 60, None);
        assert!(db.session_by_token("a").is_none(), "idle");
        assert!(db.session_by_token("b").is_none(), "pending");
        assert!(db.session_by_token("c").is_some());
        db.purge_sessions(1001, 5000, 1000, 60, None);
        assert!(db.session_by_token("c").is_none(), "absolute");
    }

    #[test]
    fn a_totp_step_is_used_once() {
        let db = UserDb::open_in_memory().unwrap();
        let u = db.create_user("ann", "h", false, false, 0).unwrap();
        db.enable_totp(u.id, "SECRET", 10);
        assert!(!db.use_totp_step(u.id, 10));
        assert!(db.use_totp_step(u.id, 11));
        assert!(!db.use_totp_step(u.id, 11));
        db.set_recovery_codes(u.id, &["x".into()]);
        db.disable_totp(u.id);
        assert_eq!(db.recovery_codes_left(u.id), 0);
        assert!(!db.user(u.id).unwrap().totp_enabled);
    }

    #[test]
    fn the_audit_log_pages_newest_first() {
        let db = UserDb::open_in_memory().unwrap();
        for i in 0..5 {
            db.audit(
                i,
                "login.ok",
                None,
                Some("ann"),
                None,
                Some("10.0.0.1"),
                &serde_json::json!({"n": i}),
            );
        }
        let first = db.audit_entries(2, None);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].detail["n"], 4);
        let next = db.audit_entries(10, Some(first[1].id));
        assert_eq!(next.len(), 3);
        assert_eq!(next[0].detail["n"], 2);
    }
}
