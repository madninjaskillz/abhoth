//! Local fallback slot store (`LocalSlotRegistry`): the file and lock PiG's slots.ts uses in its
//! local mode (`~/.pig/state/slots.json` + `slots.lock`), so OpenOrca and PiG on one machine
//! never take the same slot while the central registry is down. The file maps a slot name to
//! `{pid, owner, kind, since}`; entries whose process is gone or that are older than 12 h are
//! reaped on every read. Leases are tied to the process, so there is no TTL.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::registry::{SlotClaim, SlotClaimResult, SlotError, SlotRegistry};

const MAX_AGE_MS: i64 = 12 * 3600 * 1000;

pub type IsAlive = Box<dyn Fn(i64) -> bool + Send + Sync>;

pub struct LocalSlotRegistry {
    state_file: PathBuf,
    lock_file: PathBuf,
    slot_names: Vec<String>,
    is_alive: IsAlive,
    gate: Mutex<()>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn copy_of(slot: &str) -> String {
    slot.chars().next().map(String::from).unwrap_or_default()
}

fn owner_of(entry: &Value) -> Option<&str> {
    entry.get("owner").and_then(Value::as_str)
}

fn pid_of(entry: &Value) -> i64 {
    entry
        .get("pid")
        .and_then(Value::as_i64)
        .filter(|p| i32::try_from(*p).is_ok())
        .unwrap_or(-1)
}

impl LocalSlotRegistry {
    pub fn new(state_file: impl Into<PathBuf>, slot_names: Vec<String>) -> Self {
        Self::with_liveness(state_file, slot_names, Box::new(process_alive))
    }

    pub fn with_liveness(
        state_file: impl Into<PathBuf>,
        slot_names: Vec<String>,
        is_alive: IsAlive,
    ) -> Self {
        let state_file = state_file.into();
        let lock_file = state_file
            .parent()
            .unwrap_or(Path::new("."))
            .join("slots.lock");
        Self {
            state_file,
            lock_file,
            slot_names,
            is_alive,
            gate: Mutex::new(()),
        }
    }

    /// PiG's default location.
    pub fn default_state_file() -> PathBuf {
        crate::config::manager::user_profile()
            .join(".pig")
            .join("state")
            .join("slots.json")
    }

    fn with_lock<T>(&self, body: impl FnOnce(&mut Map<String, Value>) -> (T, bool)) -> T {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.state_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let locked = self.acquire_file_lock();
        let mut state = self.load();
        let (result, changed) = body(&mut state);
        if changed {
            self.save(&state);
        }
        if locked {
            let _ = std::fs::remove_file(&self.lock_file);
        }
        result
    }

    /// Exclusive-create lock file; a lock older than 3 s is stale. Gives up after 5 s.
    fn acquire_file_lock(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.lock_file)
            {
                Ok(_) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Windows reports a file being deleted as access denied; a real ACL
                    // problem would never clear, so give up after the deadline either way.
                    if Instant::now() > deadline {
                        return false;
                    }
                }
                Err(_) => {
                    if let Ok(modified) =
                        std::fs::metadata(&self.lock_file).and_then(|m| m.modified())
                        && SystemTime::now()
                            .duration_since(modified)
                            .unwrap_or_default()
                            > Duration::from_secs(3)
                    {
                        let _ = std::fs::remove_file(&self.lock_file);
                    }
                    if Instant::now() > deadline {
                        return false;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn load(&self) -> Map<String, Value> {
        let mut state: Map<String, Value> = std::fs::read_to_string(&self.state_file)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
            .and_then(|v| match v {
                Value::Object(o) => Some(o),
                _ => None,
            })
            .unwrap_or_default();
        let now = now_ms();
        let stale: Vec<String> = state
            .iter()
            .filter(|(slot, entry)| {
                let since = entry.get("since").and_then(Value::as_i64).unwrap_or(0);
                !self.slot_names.contains(slot)
                    || entry.is_null()
                    || !(self.is_alive)(pid_of(entry))
                    || now - since > MAX_AGE_MS
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            state.shift_remove(&k);
        }
        state
    }

    fn save(&self, state: &Map<String, Value>) {
        let tmp = PathBuf::from(format!(
            "{}.{}.tmp",
            self.state_file.display(),
            std::process::id()
        ));
        let text = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".into());
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_file);
        }
    }

    /// Free slots by preference: less-loaded copy first, then name order (A on ties).
    fn order(&self, state: &Map<String, Value>, avoid: &[String]) -> Vec<String> {
        let load = |copy: &str| {
            self.slot_names
                .iter()
                .filter(|s| copy_of(s) == copy && state.contains_key(*s))
                .count()
        };
        let mut free: Vec<String> = self
            .slot_names
            .iter()
            .filter(|s| {
                !state.contains_key(*s)
                    && !avoid.iter().any(|a| a.eq_ignore_ascii_case(&copy_of(s)))
            })
            .cloned()
            .collect();
        free.sort_by(|a, b| load(&copy_of(a)).cmp(&load(&copy_of(b))).then(a.cmp(b)));
        free
    }
}

#[async_trait]
impl SlotRegistry for LocalSlotRegistry {
    async fn claim(&self, claim: &SlotClaim) -> Result<SlotClaimResult, SlotError> {
        Ok(self.with_lock(|state| {
            for (slot, entry) in state.iter() {
                if owner_of(entry) == Some(claim.key.as_str()) {
                    return (SlotClaimResult::granted(slot, &copy_of(slot), None), false);
                }
            }
            let pick = if let Some(slot) = &claim.slot {
                (self.slot_names.contains(slot) && !state.contains_key(slot)).then(|| slot.clone())
            } else if let Some(copy) = &claim.copy {
                self.slot_names
                    .iter()
                    .find(|s| &copy_of(s) == copy && !state.contains_key(*s))
                    .cloned()
            } else {
                self.order(state, claim.avoid.as_deref().unwrap_or_default())
                    .into_iter()
                    .next()
            };
            let Some(pick) = pick else {
                return (SlotClaimResult::full(), false);
            };
            state.insert(
                pick.clone(),
                json!({"pid": claim.pid, "owner": claim.key, "kind": claim.kind, "since": now_ms()}),
            );
            (SlotClaimResult::granted(&pick, &copy_of(&pick), None), true)
        }))
    }

    async fn renew(&self, keys: &[String]) -> Result<Vec<String>, SlotError> {
        Ok(self.with_lock(|state| {
            let present: Vec<&str> = state.values().filter_map(owner_of).collect();
            let lost = keys
                .iter()
                .filter(|k| !present.contains(&k.as_str()))
                .cloned()
                .collect();
            (lost, false)
        }))
    }

    async fn release(&self, key: &str) -> Result<(), SlotError> {
        self.with_lock(|state| {
            let mine: Vec<String> = state
                .iter()
                .filter(|(_, e)| owner_of(e) == Some(key))
                .map(|(k, _)| k.clone())
                .collect();
            for slot in &mine {
                state.shift_remove(slot);
            }
            ((), !mine.is_empty())
        });
        Ok(())
    }

    async fn release_all(&self, _host: &str, pid: u32) -> Result<(), SlotError> {
        self.with_lock(|state| {
            let mine: Vec<String> = state
                .iter()
                .filter(|(_, e)| pid_of(e) == i64::from(pid))
                .map(|(k, _)| k.clone())
                .collect();
            for slot in &mine {
                state.shift_remove(slot);
            }
            ((), !mine.is_empty())
        });
        Ok(())
    }
}

/// Whether a process id is running (an inaccessible one counts as running).
pub fn process_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    if pid == i64::from(std::process::id()) {
        return true;
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => true,
        }
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_registry_writes_pig_format_and_reaps_dead_processes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("slots.json");
        std::fs::write(
            &file,
            r#"{"A1": {"pid": 999999, "owner": "main:old", "kind": "main", "since": 1},
                "A2": {"pid": 77, "owner": "pig-agent", "kind": "agent", "since": 9999999999999}}"#,
        )
        .unwrap();
        let names = ["A1", "A2", "B1", "B2"].map(String::from).to_vec();
        let local = LocalSlotRegistry::with_liveness(
            &file,
            names,
            Box::new(|pid| pid == 77 || pid == 4242),
        );
        let claim = |key: &str, copy: Option<&str>| SlotClaim {
            key: key.into(),
            owner: "me".into(),
            host: "box".into(),
            pid: 4242,
            label: "x".into(),
            kind: "agent".into(),
            copy: copy.map(String::from),
            ..SlotClaim::default()
        };
        let r = local.claim(&claim("k1", Some("A"))).await.unwrap();
        assert_eq!(r.slot.as_deref(), Some("A1"));
        let full = local.claim(&claim("k2", Some("A"))).await.unwrap();
        assert!(!full.ok);
        let any = local.claim(&claim("k2", None)).await.unwrap();
        assert_eq!(any.slot.as_deref(), Some("B1"));

        let json: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(json["A1"]["pid"], 4242);
        assert_eq!(json["A1"]["owner"], "k1");
        assert_eq!(json["A2"]["owner"], "pig-agent");

        local.release_all("box", 4242).await.unwrap();
        let json: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["A2"]
        );
        assert!(!dir.path().join("slots.lock").exists());
    }
}
