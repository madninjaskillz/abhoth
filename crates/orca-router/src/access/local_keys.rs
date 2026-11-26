//! The router's own admin keys (D151): `abk_…` keys kept in `router.db`, so a router that runs
//! without the Abhoth web server still has admins. Only each key's SHA-256 is stored; the key
//! itself is shown once, when it is made (`abhoth router admin-key create`, or the admin page).
//!
//! A local key is always an admin key. It is checked before the web server's keys (no network),
//! and a revoked one stops working at once.

use std::sync::Arc;

use orca_core::router_link::{API_KEY_PREFIX, Role, key_digest};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::keys::Identity;
use crate::control::store::Store;

const MIGRATIONS: &[&str] = &["CREATE TABLE router_keys (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        digest TEXT NOT NULL UNIQUE,
        prefix TEXT NOT NULL,
        created INTEGER NOT NULL,
        created_by TEXT NOT NULL,
        last_used INTEGER,
        revoked INTEGER
    )"];

/// The scope a local key's [`Identity`] carries (its audit label is `router-key:<name>`).
pub const LOCAL_SCOPE: &str = "router-local";

/// One key as the admin page lists it (never the key or its hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalKey {
    pub id: i64,
    pub name: String,
    /// The first characters, to tell keys apart ("abk_3f9a1c").
    pub prefix: String,
    pub created: i64,
    pub created_by: String,
    pub last_used: Option<i64>,
    pub revoked: Option<i64>,
}

/// A key just made: the only time the key itself exists outside the caller.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewKey {
    #[serde(flatten)]
    pub info: LocalKey,
    pub key: String,
}

#[derive(Clone)]
pub struct LocalKeys {
    store: Arc<Store>,
}

/// A key name: 1-64 characters, letters, digits, `-`, `_`, `.`, `@` and spaces.
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return Err("a key name has 1 to 64 characters".into());
    }
    if !n
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ' '))
    {
        return Err("a key name may use letters, digits, spaces and - _ . @".into());
    }
    Ok(n.to_string())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl LocalKeys {
    pub fn open(store: Arc<Store>) -> Result<Self, String> {
        store.migrate_part("router_keys", MIGRATIONS)?;
        Ok(Self { store })
    }

    /// Make a key named `name`; `by` is who asked (the audit label, or "cli").
    pub fn create(&self, name: &str, by: &str) -> Result<NewKey, String> {
        let name = check_name(name)?;
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|e| format!("no randomness: {e}"))?;
        let secret: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let key = format!("{API_KEY_PREFIX}{secret}");
        let prefix: String = key.chars().take(API_KEY_PREFIX.len() + 6).collect();
        let created = now();
        let id = {
            let c = self.store.conn();
            c.execute(
                "INSERT INTO router_keys(name, digest, prefix, created, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, key_digest(&key), prefix, created, by],
            )
            .map_err(|e| e.to_string())?;
            c.last_insert_rowid()
        };
        Ok(NewKey {
            info: LocalKey {
                id,
                name,
                prefix,
                created,
                created_by: by.to_string(),
                last_used: None,
                revoked: None,
            },
            key,
        })
    }

    pub fn list(&self) -> Result<Vec<LocalKey>, String> {
        let c = self.store.conn();
        let mut st = c
            .prepare(
                "SELECT id, name, prefix, created, created_by, last_used, revoked FROM router_keys ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok(LocalKey {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    prefix: r.get(2)?,
                    created: r.get(3)?,
                    created_by: r.get(4)?,
                    last_used: r.get(5)?,
                    revoked: r.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// Keys that still work.
    pub fn active_count(&self) -> usize {
        self.list()
            .map(|v| v.iter().filter(|k| k.revoked.is_none()).count())
            .unwrap_or(0)
    }

    /// Revoke key `id` (by id, or by name when `id` is not a number). Ok(name) of the key.
    pub fn revoke(&self, id_or_name: &str) -> Result<String, String> {
        let c = self.store.conn();
        let row: Option<(i64, String)> = match id_or_name.trim().parse::<i64>() {
            Ok(id) => c
                .query_row(
                    "SELECT id, name FROM router_keys WHERE id = ?1 AND revoked IS NULL",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional(),
            Err(_) => c
                .query_row(
                    "SELECT id, name FROM router_keys WHERE name = ?1 AND revoked IS NULL ORDER BY id DESC",
                    [id_or_name.trim()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional(),
        }
        .map_err(|e| e.to_string())?;
        let Some((id, name)) = row else {
            return Err("no such key (or it is revoked already)".into());
        };
        c.execute(
            "UPDATE router_keys SET revoked = ?2 WHERE id = ?1",
            params![id, now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(name)
    }

    /// The admin a key digest belongs to, if it is a local key that still works. Marks it used
    /// (at most once a minute, to keep writes down).
    pub fn check_digest(&self, digest: &str) -> Option<Identity> {
        let c = self.store.conn();
        let row: Option<(i64, String, Option<i64>)> = c
            .query_row(
                "SELECT id, name, last_used FROM router_keys WHERE digest = ?1 AND revoked IS NULL",
                [digest],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (id, name, last) = row?;
        let t = now();
        if last.is_none_or(|l| t - l >= 60) {
            let _ = c.execute(
                "UPDATE router_keys SET last_used = ?2 WHERE id = ?1",
                params![id, t],
            );
        }
        Some(identity(id, &name))
    }

    /// [`Self::check_digest`] for a key.
    pub fn check(&self, key: &str) -> Option<Identity> {
        if !key.starts_with(API_KEY_PREFIX) || key.len() < API_KEY_PREFIX.len() + 16 {
            return None;
        }
        self.check_digest(&key_digest(key))
    }
}

/// The identity of local key `id` named `name`: an admin.
pub fn identity(id: i64, name: &str) -> Identity {
    Identity {
        user_id: 0,
        name: name.to_string(),
        role: Role::Admin,
        key_id: id,
        key_name: name.to_string(),
        scopes: vec![LOCAL_SCOPE.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> LocalKeys {
        LocalKeys::open(Arc::new(Store::memory().unwrap())).unwrap()
    }

    #[test]
    fn a_key_is_shown_once_and_stored_as_a_hash() {
        let k = keys();
        let n = k.create("ops laptop", "cli").unwrap();
        assert!(n.key.starts_with("abk_") && n.key.len() == 4 + 48);
        assert_eq!(n.info.prefix, n.key[..10]);
        let dump: String = {
            let c = k.store.conn();
            c.query_row(
                "SELECT name || digest || prefix FROM router_keys",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!dump.contains(&n.key), "the key itself is never stored");
        let id = k.check(&n.key).unwrap();
        assert_eq!((id.role, id.name.as_str()), (Role::Admin, "ops laptop"));
        assert!(k.check("abk_0000000000000000000000000000").is_none());
        assert!(k.check("sk-other").is_none());
        assert!(k.list().unwrap()[0].last_used.is_some());
    }

    #[test]
    fn revoked_keys_stop_at_once() {
        let k = keys();
        let a = k.create("a", "cli").unwrap();
        let b = k.create("b", "cli").unwrap();
        assert_eq!(k.active_count(), 2);
        assert_eq!(k.revoke(&a.info.id.to_string()).unwrap(), "a");
        assert!(k.check(&a.key).is_none());
        assert!(k.check(&b.key).is_some());
        assert_eq!(k.revoke("b").unwrap(), "b");
        assert!(k.revoke("b").is_err());
        assert_eq!(k.active_count(), 0);
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("").is_err());
        assert!(check_name("x;rm").is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
        assert_eq!(check_name(" ops@pc ").unwrap(), "ops@pc");
    }
}
