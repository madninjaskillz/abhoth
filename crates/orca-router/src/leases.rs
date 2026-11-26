//! The lease table behind the pig-slots surface (ROUTER-CONFIG.md section 8): a pure port of
//! pig-slots' `server.py` (claim, renew, release, list, reap) with the time passed in, so the
//! same code serves the `:8099` shim, the router's `/slots/...` paths and the control plane's
//! switch orchestration (withdraw or freeze slots, then check leases).
//!
//! Wire compatibility is tested against a transcript recorded from the real `server.py`
//! (`tests/fixtures/pigslots-golden.json`). Deliberate differences, all on error paths no client
//! takes:
//! - a JSON body that is not an object answers 400 `bad json` (pig-slots: a 500 with a Python
//!   error text);
//! - a `pid` that is not a number counts as 0 (pig-slots: a 500);
//! - slot names that do not start with `A` or `B` are allowed (pig-slots drops them): a slot's
//!   card is its first letter, so a third card `C` works.
//!
//! What is new and never on the pig-slots wire:
//! - the principal that claimed a lease (`principal`, written to `leases.json` only when set);
//! - **guard leases** (`kind: guard` through the native API only): no slot, a reason, renewed
//!   like any lease; while held they block switches and on-demand (section 8.3).

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// One slot lease, as pig-slots stores it in `leases.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub key: String,
    pub owner: String,
    pub host: String,
    pub pid: i64,
    pub label: String,
    pub kind: String,
    pub since: f64,
    pub renewed: f64,
    /// Who claimed it (user or key id), when known; never a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// A guard lease: blocks switches and on-demand while it is renewed (section 8.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuardLease {
    pub key: String,
    /// bench, video, maintenance, ...
    pub reason: String,
    pub owner: String,
    pub since: f64,
    pub renewed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// What the slots look like right now: the list, the freeze flag and the layout name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotView {
    pub slots: Vec<String>,
    pub frozen: bool,
    pub layout: Option<String>,
    pub reason: Option<String>,
    /// Cards always shown in `/list`'s `load` (pig-slots shows A and B even when one card's
    /// slots are withdrawn). Cards of `slots` and of held leases are added to these.
    pub cards: Vec<String>,
}

impl SlotView {
    /// Slots `{card}{n}` for `(card, lanes)` in the given order (`naming = "{card}{n}"`).
    pub fn from_lanes(cards: &[(String, u32)]) -> Vec<String> {
        let mut out = Vec::new();
        for (c, n) in cards {
            for i in 1..=*n {
                out.push(format!("{c}{i}"));
            }
        }
        out
    }
}

/// The card ("copy") of a slot: its first letter ("B1" -> "B").
pub fn copy_of(slot: &str) -> String {
    slot.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python's `str(x or default)` for a JSON value.
fn py_str(v: Option<&Value>, default: &str) -> String {
    match v {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) if s.is_empty() => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(false)) => default.to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => default.to_string(),
        Some(v) => v.to_string(),
    }
}

/// Python's `int(x or 0)`, lenient: numbers and numeric strings; anything else is 0.
fn py_int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The result of one call: HTTP status, JSON body, and whether the table changed (save it).
pub type Outcome = (u16, Value, bool);

/// Slot leases plus guard leases. Not thread-safe by itself: the owner wraps it in a mutex.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LeaseTable {
    pub leases: IndexMap<String, Lease>,
    pub guards: IndexMap<String, GuardLease>,
}

impl LeaseTable {
    /// Drop leases (and guard leases) not renewed within `ttl`. True when any went.
    pub fn reap(&mut self, now: f64, ttl: f64) -> bool {
        let before = self.leases.len() + self.guards.len();
        self.leases.retain(|_, l| now - l.renewed <= ttl);
        self.guards.retain(|_, g| now - g.renewed <= ttl);
        self.leases.len() + self.guards.len() != before
    }

    fn find_key(&self, key: &str) -> Option<String> {
        self.leases
            .iter()
            .find(|(_, l)| l.key == key)
            .map(|(s, _)| s.clone())
    }

    /// Free slots, least-loaded card first, then by name (pig-slots `order`).
    fn order(&self, view: &SlotView, avoid: &[String]) -> Vec<String> {
        let load = |c: &str| self.leases.keys().filter(|s| copy_of(s) == c).count();
        let mut free: Vec<&String> = view
            .slots
            .iter()
            .filter(|s| !self.leases.contains_key(*s) && !avoid.contains(&copy_of(s)))
            .collect();
        free.sort_by_key(|s| (load(&copy_of(s)), (*s).clone()));
        free.into_iter().cloned().collect()
    }

    /// `POST /claim`. `principal` is recorded with a new lease (never on the wire).
    pub fn claim(
        &mut self,
        view: &SlotView,
        b: &Map<String, Value>,
        now: f64,
        ttl: f64,
        principal: Option<&str>,
    ) -> Outcome {
        let key = py_str(b.get("key"), "");
        if key.is_empty() {
            return (400, json!({"ok": false, "reason": "key required"}), false);
        }
        let mut changed = self.reap(now, ttl);
        let res = if let Some(s) = self.find_key(&key) {
            if let Some(l) = self.leases.get_mut(&s) {
                l.renewed = now;
            }
            json!({"ok": true, "slot": s, "copy": copy_of(&s), "ttl": ttl, "existing": true})
        } else {
            let cards = self.cards_of(view);
            let want = b.get("slot").and_then(Value::as_str);
            let avoid: Vec<String> = str_list(b.get("avoid"))
                .into_iter()
                .filter(|c| cards.contains(c))
                .collect();
            let hard = b
                .get("copy")
                .and_then(Value::as_str)
                .filter(|c| cards.iter().any(|x| x == c));
            let cand: Vec<String> = match want {
                Some(w) if view.slots.iter().any(|s| s == w) => {
                    if self.leases.contains_key(w) {
                        vec![]
                    } else {
                        vec![w.to_string()]
                    }
                }
                _ => match hard {
                    Some(h) => view
                        .slots
                        .iter()
                        .filter(|s| copy_of(s) == h && !self.leases.contains_key(*s))
                        .cloned()
                        .collect(),
                    None => self.order(view, &avoid),
                },
            };
            if view.frozen {
                json!({"ok": false, "reason": "switching"})
            } else if let Some(s) = cand.first().cloned() {
                self.leases.insert(
                    s.clone(),
                    Lease {
                        key,
                        owner: clip(&py_str(b.get("owner"), "?"), 80),
                        host: clip(&py_str(b.get("host"), "?"), 80),
                        pid: py_int(b.get("pid")),
                        label: clip(&py_str(b.get("label"), ""), 80),
                        kind: if b.get("kind").and_then(Value::as_str) == Some("main") {
                            "main".into()
                        } else {
                            "agent".into()
                        },
                        since: now,
                        renewed: now,
                        principal: principal.map(str::to_string),
                    },
                );
                changed = true;
                json!({"ok": true, "slot": s, "copy": copy_of(&s), "ttl": ttl})
            } else {
                json!({"ok": false, "reason": "full"})
            }
        };
        (200, res, changed)
    }

    /// The cards a claim may name in `copy` / `avoid`: pig-slots accepts A and B always; here
    /// the view's cards plus the cards of its slots.
    fn cards_of(&self, view: &SlotView) -> Vec<String> {
        let mut v: Vec<String> = view.cards.clone();
        for s in &view.slots {
            let c = copy_of(s);
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    /// `POST /renew {keys}`: slot leases and guard leases alike.
    pub fn renew(&mut self, b: &Map<String, Value>, now: f64, ttl: f64) -> Outcome {
        let keys: Vec<String> = b
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|k| {
                        k.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| k.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let changed = self.reap(now, ttl);
        let (mut ok, mut lost) = (Vec::new(), Vec::new());
        for k in keys {
            if let Some(l) = self.leases.values_mut().find(|l| l.key == k) {
                l.renewed = now;
                ok.push(k);
            } else if let Some(g) = self.guards.get_mut(&k) {
                g.renewed = now;
                ok.push(k);
            } else {
                lost.push(k);
            }
        }
        (200, json!({"renewed": ok, "lost": lost}), changed)
    }

    /// `POST /release {key}` or `{host, pid}` (every lease of one client process).
    pub fn release(&mut self, b: &Map<String, Value>) -> Outcome {
        let key = b
            .get("key")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let host = b
            .get("host")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let pid = Some(py_int(b.get("pid"))).filter(|p| *p != 0);
        let gone: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, l)| {
                key.is_some_and(|k| l.key == k)
                    || (host.is_some_and(|h| l.host == h) && pid.is_some_and(|p| l.pid == p))
            })
            .map(|(s, _)| s.clone())
            .collect();
        for s in &gone {
            self.leases.shift_remove(s);
        }
        let mut changed = !gone.is_empty();
        if let Some(k) = key
            && self.guards.shift_remove(k).is_some()
        {
            changed = true;
        }
        (200, json!({"ok": true, "released": gone}), changed)
    }

    /// `GET /list`: `{ttl, slots, free, load, layout, frozen, orphans}`.
    pub fn list(&mut self, view: &SlotView, now: f64, ttl: f64) -> (Value, bool) {
        let changed = self.reap(now, ttl);
        let mut sm = Map::new();
        for s in &view.slots {
            sm.insert(
                s.clone(),
                match self.leases.get(s) {
                    Some(l) => json!({"key": l.key, "owner": l.owner, "host": l.host, "pid": l.pid,
                        "label": l.label, "kind": l.kind, "age_s": (now - l.since) as i64,
                        "idle_s": (now - l.renewed) as i64}),
                    None => Value::Null,
                },
            );
        }
        let mut load: BTreeMap<String, usize> = BTreeMap::new();
        for c in self.cards_of(view) {
            load.entry(c).or_default();
        }
        for s in self.leases.keys() {
            *load.entry(copy_of(s)).or_default() += 1;
        }
        let mut orphans: Vec<&String> = self
            .leases
            .keys()
            .filter(|s| !view.slots.contains(s))
            .collect();
        orphans.sort();
        let v = json!({
            "ttl": ttl,
            "slots": sm,
            "free": view.slots.iter().filter(|s| !self.leases.contains_key(*s)).collect::<Vec<_>>(),
            "load": load,
            "layout": view.layout,
            "frozen": view.frozen,
            "orphans": orphans,
        });
        (v, changed)
    }

    /// Live slot leases on `cards` or on cards the view does not know: what refuses a switch of
    /// those cards (layoutctl `leases()` filtered to the moving cards). A lease on a slot the
    /// view no longer lists (a withdrawn card's) is named `orphan`, as layoutctl does.
    pub fn held_on(
        &self,
        view: &SlotView,
        cards: &[String],
        now: f64,
        ttl: f64,
    ) -> Vec<(String, String)> {
        let all = self.cards_of(view);
        self.leases
            .iter()
            .filter(|(_, l)| now - l.renewed <= ttl)
            .filter(|(s, _)| {
                let c = copy_of(s);
                cards.contains(&c) || !all.contains(&c)
            })
            .map(|(s, l)| {
                let who = if view.slots.contains(s) {
                    format!("{} {}", l.owner, l.label).trim().to_string()
                } else {
                    "orphan".to_string()
                };
                (s.clone(), who)
            })
            .collect()
    }

    /// Claim or renew a guard lease (native API only).
    pub fn claim_guard(
        &mut self,
        key: &str,
        reason: &str,
        owner: &str,
        principal: Option<&str>,
        now: f64,
        ttl: f64,
    ) -> GuardLease {
        self.reap(now, ttl);
        let g = self
            .guards
            .entry(key.to_string())
            .or_insert_with(|| GuardLease {
                key: key.to_string(),
                reason: clip(reason, 80),
                owner: clip(if owner.is_empty() { "?" } else { owner }, 80),
                since: now,
                renewed: now,
                principal: principal.map(str::to_string),
            });
        g.renewed = now;
        g.clone()
    }

    /// Live guard leases (they block switches and on-demand).
    pub fn live_guards(&self, now: f64, ttl: f64) -> Vec<GuardLease> {
        self.guards
            .values()
            .filter(|g| now - g.renewed <= ttl)
            .cloned()
            .collect()
    }
}

// ── persistence in router.db (its own part, so migrations never clash) ─────────────────────

/// `router.db` part name and migrations for leases.
pub const STORE_PART: &str = "leases";
pub const MIGRATIONS: &[&str] = &[
    "CREATE TABLE leases(slot TEXT PRIMARY KEY, lease TEXT NOT NULL);
     CREATE TABLE guard_leases(key TEXT PRIMARY KEY, lease TEXT NOT NULL);",
];

/// Create or upgrade the lease tables.
pub fn store_migrate(store: &crate::control::store::Store) -> Result<(), String> {
    store.migrate_part(STORE_PART, MIGRATIONS)
}

/// Replace the stored leases with `t` (one transaction).
pub fn store_save(store: &crate::control::store::Store, t: &LeaseTable) -> Result<(), String> {
    let mut c = store.conn();
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM leases", [])
        .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM guard_leases", [])
        .map_err(|e| e.to_string())?;
    for (s, l) in &t.leases {
        let j = serde_json::to_string(l).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO leases(slot, lease) VALUES (?1, ?2)",
            rusqlite::params![s, j],
        )
        .map_err(|e| e.to_string())?;
    }
    for (k, g) in &t.guards {
        let j = serde_json::to_string(g).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO guard_leases(key, lease) VALUES (?1, ?2)",
            rusqlite::params![k, j],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

/// The stored leases (expired ones are reaped by the next call, as after a pig-slots restart).
pub fn store_load(store: &crate::control::store::Store) -> Result<LeaseTable, String> {
    let c = store.conn();
    let mut t = LeaseTable::default();
    let mut q = c
        .prepare("SELECT slot, lease FROM leases ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    let rows = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (s, j) = row.map_err(|e| e.to_string())?;
        if let Ok(l) = serde_json::from_str(&j) {
            t.leases.insert(s, l);
        }
    }
    let mut q = c
        .prepare("SELECT key, lease FROM guard_leases ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    let rows = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (k, j) = row.map_err(|e| e.to_string())?;
        if let Ok(g) = serde_json::from_str(&j) {
            t.guards.insert(k, g);
        }
    }
    Ok(t)
}
