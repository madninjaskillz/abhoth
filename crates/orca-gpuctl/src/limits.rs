//! The limits file (`$APPS/abhoth/etc/hostd-limits.toml`, root-owned 0644; SUPERVISOR.md 2.3).
//!
//! It bounds what a (possibly compromised) supervisor may ask hostd to do with clocks and power.
//! A policy outside it is refused in full and the old one stays; every command hostd sends to the
//! driver also goes through [`LimitedControl`], so nothing reaches the hardware outside it.
//!
//! ```toml
//! max_ttl = "1h"
//!
//! [gpus."GPU-…"]                 # card A
//! label         = "A"
//! actions       = ["persistence", "power_limit", "app_clocks", "lock_clocks", "reset_clocks"]
//! clock_mhz     = [135, 1597]    # every -lgc lo,hi must lie inside: a floor and a cap
//! power_limit_w = [250, 350]     # -pl must lie inside
//! ```
//!
//! `-rgc` (reset) is the fail-safe direction and is allowed on every listed card whatever
//! `actions` says. Cards that are not listed are never touched.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{self, CardPowerConfig, Serving};
use crate::nvml::{GpuCommand, GpuControl, GpuError, GpuReading};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "one_hour", with = "dur")]
    pub max_ttl: Duration,
    #[serde(default)]
    pub gpus: BTreeMap<String, GpuLimits>,
}

fn one_hour() -> Duration {
    config::MAX_TTL
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLimits {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
    pub clock_mhz: [u32; 2],
    #[serde(default)]
    pub power_limit_w: Option<[u32; 2]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Persistence,
    PowerLimit,
    AppClocks,
    LockClocks,
    ResetClocks,
}

impl Limits {
    pub fn parse(text: &str) -> Result<Limits, String> {
        let l: Limits = toml::from_str(text).map_err(|e| format!("limits file: {e}"))?;
        if l.max_ttl > config::MAX_TTL {
            return Err(format!(
                "limits file: max_ttl may not exceed {}",
                config::format_duration(config::MAX_TTL)
            ));
        }
        for (u, g) in &l.gpus {
            if g.clock_mhz[0] > g.clock_mhz[1] {
                return Err(format!("limits file: {u}: clock_mhz is backwards"));
            }
            if let Some([a, b]) = g.power_limit_w
                && a > b
            {
                return Err(format!("limits file: {u}: power_limit_w is backwards"));
            }
        }
        Ok(l)
    }

    /// A stable hash for `hello` (hex of a simple FNV-1a over the canonical JSON).
    pub fn hash(&self) -> String {
        let s = serde_json::to_string(self).unwrap_or_default();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    /// Checks one command against the limits. `Err` carries the reason.
    pub fn check_command(&self, uuid: &str, cmd: GpuCommand) -> Result<(), String> {
        let g = self
            .gpus
            .get(uuid)
            .ok_or_else(|| format!("{uuid} is not in the limits file"))?;
        let need = |a: Action| {
            if g.actions.contains(&a) {
                Ok(())
            } else {
                Err(format!("{uuid}: action {a:?} not allowed"))
            }
        };
        let [lo, hi] = g.clock_mhz;
        match cmd {
            GpuCommand::ResetGpuClocks => Ok(()),
            GpuCommand::SetPersistence { .. } => need(Action::Persistence),
            GpuCommand::SetAppClocks { sm_mhz, .. } => {
                need(Action::AppClocks)?;
                if sm_mhz > hi {
                    return Err(format!("{uuid}: app clock {sm_mhz} MHz above the cap {hi}"));
                }
                Ok(())
            }
            GpuCommand::SetPowerLimit { watts } => {
                need(Action::PowerLimit)?;
                let [a, b] = g
                    .power_limit_w
                    .ok_or_else(|| format!("{uuid}: no power_limit_w range in the limits file"))?;
                if watts < a || watts > b {
                    return Err(format!("{uuid}: power limit {watts} W outside {a}-{b} W"));
                }
                Ok(())
            }
            GpuCommand::LockGpuClocks { min_mhz, max_mhz } => {
                need(Action::LockClocks)?;
                if min_mhz > max_mhz {
                    return Err(format!("{uuid}: lock {min_mhz},{max_mhz} is backwards"));
                }
                if min_mhz < lo || max_mhz > hi {
                    return Err(format!(
                        "{uuid}: lock {min_mhz},{max_mhz} outside {lo}-{hi} MHz"
                    ));
                }
                Ok(())
            }
        }
    }

    /// Checks a whole policy: every card and every command it could produce. All reasons are
    /// returned; one reason refuses the whole policy.
    pub fn validate_policy<'a>(
        &self,
        cards: impl IntoIterator<Item = (&'a str, &'a CardPowerConfig)>,
    ) -> Result<(), Vec<String>> {
        let mut why = Vec::new();
        for (uuid, c) in cards {
            if let Err(e) = c.check() {
                why.push(format!("{uuid}: {e}"));
            }
            for cmd in possible_commands(c) {
                if let Err(e) = self.check_command(uuid, cmd) {
                    why.push(e);
                }
            }
        }
        why.dedup();
        if why.is_empty() { Ok(()) } else { Err(why) }
    }

    pub fn check_ttl(&self, ttl: Duration) -> Result<(), String> {
        if ttl.is_zero() || ttl > self.max_ttl {
            return Err(format!(
                "ttl must be between 1s and {}",
                config::format_duration(self.max_ttl)
            ));
        }
        Ok(())
    }
}

/// Every driver command a card's policy can lead to (boot, serving, idle).
pub fn possible_commands(c: &CardPowerConfig) -> Vec<GpuCommand> {
    let mut v = Vec::new();
    if c.persistence {
        v.push(GpuCommand::SetPersistence { on: true });
    }
    if let Some(w) = c.power_limit_w {
        v.push(GpuCommand::SetPowerLimit { watts: w });
    }
    if let Some([mem, sm]) = c.app_clocks {
        v.push(GpuCommand::SetAppClocks {
            mem_mhz: mem,
            sm_mhz: sm,
        });
    }
    v.push(serving_command(c.serving, c.floor(), None));
    if let Some(i) = c.idle() {
        v.push(GpuCommand::LockGpuClocks {
            min_mhz: i.lock_mhz,
            max_mhz: i.lock_mhz,
        });
    }
    v
}

/// The clock command for a serving policy, with an optional cap override on top.
pub fn serving_command(s: Serving, floor: u32, cap_override: Option<u32>) -> GpuCommand {
    let lock = |lo: u32, hi: u32| GpuCommand::LockGpuClocks {
        min_mhz: lo.min(hi),
        max_mhz: hi,
    };
    match (s, cap_override) {
        (Serving::Unlocked { cap: None }, None) => GpuCommand::ResetGpuClocks,
        (Serving::Unlocked { cap }, o) => {
            lock(floor, cap.into_iter().chain(o).min().unwrap_or(u32::MAX))
        }
        (Serving::Pin(p), None) => lock(p, p),
        (Serving::Pin(p), Some(o)) => lock(p.min(o), p.min(o)),
        (Serving::Range(lo, hi), o) => lock(lo, o.map_or(hi, |o| hi.min(o))),
    }
}

/// A driver wrapper that refuses anything outside the limits before it reaches the hardware.
pub struct LimitedControl<'l, G> {
    pub limits: &'l Limits,
    pub inner: G,
}

impl<G: GpuControl> GpuControl for LimitedControl<'_, G> {
    fn apply(&mut self, uuid: &str, cmd: GpuCommand) -> Result<(), GpuError> {
        self.limits
            .check_command(uuid, cmd)
            .map_err(|e| GpuError(format!("refused by limits: {e}")))?;
        self.inner.apply(uuid, cmd)
    }
    fn read(&mut self, uuid: &str) -> Result<GpuReading, GpuError> {
        self.inner.read(uuid)
    }
}

mod dur {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::config::format_duration(*d))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        crate::config::parse_duration(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvml::FakeNvml;

    const LIMITS: &str = r#"
        [gpus.GPU-a]
        label = "A"
        actions = ["persistence", "power_limit", "lock_clocks", "reset_clocks"]
        clock_mhz = [135, 1597]
        power_limit_w = [250, 350]
        [gpus.GPU-b]
        actions = ["persistence", "power_limit", "app_clocks", "lock_clocks"]
        clock_mhz = [135, 1380]
        power_limit_w = [150, 250]
    "#;

    fn card(s: &str) -> CardPowerConfig {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn floors_and_caps() {
        let l = Limits::parse(LIMITS).unwrap();
        assert_eq!(l.max_ttl, Duration::from_secs(3600));
        assert!(
            l.check_command(
                "GPU-a",
                GpuCommand::LockGpuClocks {
                    min_mhz: 135,
                    max_mhz: 1597
                }
            )
            .is_ok()
        );
        assert!(
            l.check_command(
                "GPU-a",
                GpuCommand::LockGpuClocks {
                    min_mhz: 100,
                    max_mhz: 1597
                }
            )
            .is_err()
        );
        assert!(
            l.check_command(
                "GPU-a",
                GpuCommand::LockGpuClocks {
                    min_mhz: 135,
                    max_mhz: 1700
                }
            )
            .is_err()
        );
        assert!(
            l.check_command("GPU-a", GpuCommand::SetPowerLimit { watts: 400 })
                .is_err()
        );
        assert!(
            l.check_command("GPU-a", GpuCommand::SetPowerLimit { watts: 200 })
                .is_err()
        );
        assert!(
            l.check_command(
                "GPU-a",
                GpuCommand::SetAppClocks {
                    mem_mhz: 877,
                    sm_mhz: 1380
                }
            )
            .is_err(),
            "action not allowed"
        );
        assert!(
            l.check_command("GPU-c", GpuCommand::ResetGpuClocks)
                .is_err(),
            "unlisted card"
        );
        // Reset is the fail-safe: allowed even when not in actions (card B).
        assert!(l.check_command("GPU-b", GpuCommand::ResetGpuClocks).is_ok());
    }

    #[test]
    fn policy_refused_in_full() {
        let l = Limits::parse(LIMITS).unwrap();
        let a = card(
            r#"persistence = true
            power_limit_w = 350
            serving = "unlocked"
            idle = { lock_mhz = 135 }"#,
        );
        let b = card("power_limit_w = 250\napp_clocks = [877, 1380]\nserving = { pin = 1500 }");
        assert!(l.validate_policy([("GPU-a", &a)]).is_ok());
        let why = l
            .validate_policy([("GPU-a", &a), ("GPU-b", &b)])
            .unwrap_err();
        assert!(
            why.iter().any(|w| w.contains("1500,1500 outside")),
            "{why:?}"
        );
        let raised = card("power_limit_w = 400");
        assert!(l.validate_policy([("GPU-a", &raised)]).is_err());
    }

    #[test]
    fn limited_control_never_reaches_hardware() {
        let l = Limits::parse(LIMITS).unwrap();
        let mut fake = FakeNvml::default().with_card("GPU-a", 1597);
        let mut g = LimitedControl {
            limits: &l,
            inner: &mut fake,
        };
        assert!(
            g.apply(
                "GPU-a",
                GpuCommand::LockGpuClocks {
                    min_mhz: 50,
                    max_mhz: 50
                }
            )
            .is_err()
        );
        assert!(g.apply("GPU-a", GpuCommand::ResetGpuClocks).is_ok());
        assert_eq!(fake.log.len(), 1);
    }

    #[test]
    fn ttl_and_parse_errors() {
        let l = Limits::parse(LIMITS).unwrap();
        assert!(l.check_ttl(Duration::from_secs(600)).is_ok());
        assert!(l.check_ttl(Duration::from_secs(7200)).is_err());
        assert!(l.check_ttl(Duration::ZERO).is_err());
        assert!(Limits::parse("max_ttl = \"2h\"").is_err());
        assert!(Limits::parse("[gpus.x]\nclock_mhz = [1500, 135]").is_err());
        assert_eq!(l.hash(), Limits::parse(LIMITS).unwrap().hash());
    }

    #[test]
    fn serving_commands() {
        assert_eq!(
            serving_command(Serving::Unlocked { cap: None }, 135, None),
            GpuCommand::ResetGpuClocks
        );
        assert_eq!(
            serving_command(Serving::Unlocked { cap: None }, 135, Some(1400)),
            GpuCommand::LockGpuClocks {
                min_mhz: 135,
                max_mhz: 1400
            }
        );
        assert_eq!(
            serving_command(Serving::Unlocked { cap: Some(1500) }, 135, Some(1600)),
            GpuCommand::LockGpuClocks {
                min_mhz: 135,
                max_mhz: 1500
            }
        );
        assert_eq!(
            serving_command(Serving::Pin(1597), 135, None),
            GpuCommand::LockGpuClocks {
                min_mhz: 1597,
                max_mhz: 1597
            }
        );
        assert_eq!(
            serving_command(Serving::Range(1200, 1500), 135, Some(1300)),
            GpuCommand::LockGpuClocks {
                min_mhz: 1200,
                max_mhz: 1300
            }
        );
    }
}
