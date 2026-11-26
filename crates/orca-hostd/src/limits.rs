//! The root-owned limits file (SUPERVISOR.md 2.3): `$APPS/abhoth/etc/hostd-limits.toml`.
//!
//! It bounds everything a (possibly compromised) supervisor can ask of hostd. A `policy.put`,
//! `override` or `hold` outside these limits is refused in full. The file is read at start;
//! hostd refuses to start if it is missing, unparseable, or (on Unix, as root) not owned by
//! root or writable by group/other.
//!
//! One file, three owners: `[gpus.*]` and `max_ttl` are `orca_gpuctl::Limits` (clocks and
//! power), `[fans]` is `orca_fans::FanLimits`, `[backends]` is hostd's.
//!
//! ```toml
//! max_ttl = "1h"
//!
//! [fans]                       # orca_fans::FanLimits
//! min_duty = 30                # %: no fan below this, in any mode or profile
//! max_control_temp = 85        # °C: no fan's max_temp above this
//! max_stale_s = 30             # s: no fan's stale timeout longer than this
//!
//! [backends]
//! write = ["esphome", "hwmon", "nvml"]   # fan backend kinds that may write; ipmi stays read-only
//!
//! [gpus."GPU-…"]               # orca_gpuctl::limits::GpuLimits; a card not listed is never touched
//! label         = "A"
//! actions       = ["persistence", "power_limit", "app_clocks", "lock_clocks", "reset_clocks"]
//! clock_mhz     = [135, 1597]
//! power_limit_w = [250, 350]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use orca_fans::FanLimits;
use orca_gpuctl::limits::GpuLimits;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendLimits {
    /// Fan backend kinds that may write (`esphome`, `hwmon`, `ipmi`, `liquidctl`, `nvml`,
    /// `windows`). Anything else is read-only. IPMI is read-only unless listed (15 Q14).
    #[serde(default)]
    pub write: BTreeSet<String>,
}

/// The whole file, as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    #[serde(default)]
    max_ttl: Option<String>,
    #[serde(default)]
    fans: FanLimits,
    #[serde(default)]
    backends: BackendLimits,
    #[serde(default)]
    gpus: BTreeMap<String, GpuLimits>,
}

/// The parsed limits.
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// Clocks and power, and the TTL bound for holds and overrides.
    pub gpu: orca_gpuctl::Limits,
    pub fans: FanLimits,
    pub backends: BackendLimits,
}

#[derive(Debug, thiserror::Error)]
pub enum LimitsError {
    #[error("limits file {path}: {err}")]
    Read { path: String, err: std::io::Error },
    #[error("limits file {path}: {err}")]
    Parse { path: String, err: String },
    #[error("limits file {path}: {why}")]
    Unsafe { path: String, why: String },
}

/// A loaded limits file and the hash of its bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedLimits {
    pub limits: Limits,
    /// Hex sha256 of the file, reported in `hello` and `status`.
    pub hash: String,
}

impl Limits {
    /// Parses limits text and checks that the limits themselves are sane.
    pub fn parse(text: &str) -> Result<Limits, String> {
        let f: LimitsFile = toml::from_str(text).map_err(|e| e.to_string())?;
        // Re-serialise the GPU part and let orca-gpuctl apply its own checks.
        #[derive(Serialize)]
        struct GpuPart<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            max_ttl: &'a Option<String>,
            gpus: &'a BTreeMap<String, GpuLimits>,
        }
        let gtext = toml::to_string(&GpuPart {
            max_ttl: &f.max_ttl,
            gpus: &f.gpus,
        })
        .map_err(|e| e.to_string())?;
        let gpu = orca_gpuctl::Limits::parse(&gtext)?;
        let fl = &f.fans;
        if !(0.0..=100.0).contains(&fl.min_duty) {
            return Err("fans.min_duty must be 0-100".into());
        }
        if !(30.0..=110.0).contains(&fl.max_control_temp) {
            return Err("fans.max_control_temp must be 30-110 °C".into());
        }
        if !(1.0..=120.0).contains(&fl.max_stale_s) {
            return Err("fans.max_stale_s must be 1-120 s".into());
        }
        Ok(Limits {
            gpu,
            fans: f.fans,
            backends: f.backends,
        })
    }

    /// May this fan backend kind write?
    pub fn backend_writes(&self, kind: &str) -> bool {
        self.backends.write.contains(kind)
    }

    /// Checks a TTL for a hold or override.
    pub fn check_ttl(&self, ttl: Duration) -> Result<(), String> {
        self.gpu.check_ttl(ttl)
    }

    /// The UUID of a labelled GPU, if the limits list it.
    pub fn uuid_for_label(&self, label: &str) -> Option<&str> {
        self.gpu
            .gpus
            .iter()
            .find(|(_, g)| g.label.as_deref() == Some(label))
            .map(|(u, _)| u.as_str())
    }
}

/// Reads, checks ownership (Unix: owned by root, not group/other writable) and parses.
pub fn load(path: &Path, require_root_owned: bool) -> Result<LoadedLimits, LimitsError> {
    let p = path.display().to_string();
    let bytes = std::fs::read(path).map_err(|err| LimitsError::Read {
        path: p.clone(),
        err,
    })?;
    if require_root_owned {
        check_owner(path).map_err(|why| LimitsError::Unsafe {
            path: p.clone(),
            why,
        })?;
    }
    let text = String::from_utf8(bytes.clone()).map_err(|e| LimitsError::Parse {
        path: p.clone(),
        err: e.to_string(),
    })?;
    let limits = Limits::parse(&text).map_err(|err| LimitsError::Parse { path: p, err })?;
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(LoadedLimits { limits, hash })
}

#[cfg(unix)]
fn check_owner(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if m.uid() != 0 {
        return Err("must be owned by root".into());
    }
    if m.mode() & 0o022 != 0 {
        return Err("must not be group- or world-writable".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
max_ttl = "1h"
[fans]
min_duty = 30
max_control_temp = 85
max_stale_s = 30
[backends]
write = ["esphome"]
[gpus."GPU-aaaa"]
label = "A"
actions = ["persistence", "power_limit", "lock_clocks", "reset_clocks"]
clock_mhz = [135, 1597]
power_limit_w = [250, 350]
"#;

    #[test]
    fn parses_sample() {
        let l = Limits::parse(SAMPLE).unwrap();
        assert_eq!(l.uuid_for_label("A"), Some("GPU-aaaa"));
        assert!(l.backend_writes("esphome") && !l.backend_writes("ipmi"));
        assert_eq!(l.fans.min_duty, 30.0);
        assert!(l.check_ttl(Duration::from_secs(3600)).is_ok());
        assert!(l.check_ttl(Duration::from_secs(3601)).is_err());
        assert!(l.check_ttl(Duration::ZERO).is_err());
        // Defaults when sections are left out.
        assert!(Limits::parse("").is_ok());
    }

    #[test]
    fn rejects_bad_limits() {
        assert!(Limits::parse("max_ttl = \"2h\"").is_err());
        assert!(
            Limits::parse("[fans]\nmin_duty = 130\nmax_control_temp = 85\nmax_stale_s = 30")
                .is_err()
        );
        assert!(Limits::parse("surprise = true").is_err());
        assert!(Limits::parse("[gpus.x]\nclock_mhz = [2000, 100]").is_err());
    }

    #[test]
    fn load_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.toml");
        std::fs::write(&p, SAMPLE).unwrap();
        let l = load(&p, false).unwrap();
        assert_eq!(l.hash.len(), 64);
    }
}
