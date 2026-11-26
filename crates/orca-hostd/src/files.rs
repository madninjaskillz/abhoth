//! hostd's files: the last-known-good policy (state dir), the heartbeat and the lowered-cards
//! file (run dir). Writes are atomic (temp file + rename) so a crash never leaves half a file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::PolicyDoc;

/// Where hostd keeps things. Defaults follow SUPERVISOR.md 2.3 with `$APPS/abhoth`.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$APPS/abhoth/etc/hostd-limits.toml`.
    pub limits: PathBuf,
    /// `$APPS/abhoth/state/hostd` (last-known-good policy).
    pub state_dir: PathBuf,
    /// `/run/abhoth` (socket, heartbeat, lowered cards).
    pub run_dir: PathBuf,
}

impl Paths {
    pub fn socket(&self) -> PathBuf {
        self.run_dir.join("hostd.sock")
    }
    pub fn heartbeat(&self) -> PathBuf {
        self.run_dir.join("hostd.heartbeat")
    }
    pub fn lowered(&self) -> PathBuf {
        self.run_dir.join("hostd.lowered.json")
    }
    /// `secret:<name>` files: `etc/secrets/<name>` next to the limits file (root-only).
    pub fn secrets_dir(&self) -> PathBuf {
        self.limits.parent().unwrap_or(Path::new(".")).join("secrets")
    }
    pub fn policy(&self) -> PathBuf {
        self.state_dir.join("policy.json")
    }
}

/// Writes `bytes` to `path` atomically.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("hostd")));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Saves the policy in force (last-known-good).
pub fn save_policy(paths: &Paths, p: &PolicyDoc) -> std::io::Result<()> {
    let j = serde_json::to_vec_pretty(p).map_err(std::io::Error::other)?;
    write_atomic(&paths.policy(), &j)
}

/// Loads the last-known-good policy, if any. A corrupt file is an error (not silently ignored).
pub fn load_policy(paths: &Paths) -> Result<Option<PolicyDoc>, String> {
    match std::fs::read(paths.policy()) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| format!("{}: {e}", paths.policy().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", paths.policy().display())),
    }
}

/// Writes the heartbeat: the current time in ms since the epoch.
pub fn write_heartbeat(paths: &Paths, now_ms: i64) -> std::io::Result<()> {
    write_atomic(&paths.heartbeat(), format!("{now_ms}\n").as_bytes())
}

/// Age of the heartbeat, or `None` if missing or unreadable (which counts as stale).
pub fn heartbeat_age(paths: &Paths, now_ms: i64) -> Option<Duration> {
    let t: i64 = std::fs::read_to_string(paths.heartbeat()).ok()?.trim().parse().ok()?;
    Some(Duration::from_millis((now_ms - t).max(0) as u64))
}

/// Writes the UUIDs of cards hostd has locked low (for humans and the failsafe).
pub fn write_lowered(paths: &Paths, uuids: &[String]) -> std::io::Result<()> {
    let j = serde_json::to_vec(uuids).map_err(std::io::Error::other)?;
    write_atomic(&paths.lowered(), &j)
}

pub fn read_lowered(paths: &Paths) -> Vec<String> {
    std::fs::read(paths.lowered()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths { limits: d.path().join("l.toml"), state_dir: d.path().join("state"), run_dir: d.path().join("run") };
        assert_eq!(load_policy(&p).unwrap(), None);
        let pol = PolicyDoc {
            rev: 3,
            cards: Default::default(),
            sensors: Default::default(),
            fans: Default::default(),
            power: Default::default(),
        };
        save_policy(&p, &pol).unwrap();
        assert_eq!(load_policy(&p).unwrap(), Some(pol));
        assert_eq!(heartbeat_age(&p, 10_000), None);
        write_heartbeat(&p, 4_000).unwrap();
        assert_eq!(heartbeat_age(&p, 10_000), Some(Duration::from_secs(6)));
        write_lowered(&p, &["GPU-a".into()]).unwrap();
        assert_eq!(read_lowered(&p), vec!["GPU-a".to_string()]);
        std::fs::write(p.policy(), b"{bad").unwrap();
        assert!(load_policy(&p).is_err());
    }
}
