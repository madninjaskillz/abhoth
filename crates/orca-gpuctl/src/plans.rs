//! One-shot plans: `abhoth hostd boot` (replaces `boot.sh`'s clock part) and
//! `abhoth hostd failsafe` (replaces gpu-power's `failsafe.sh` dead-man cron).

use std::time::Duration;

use crate::config::CardPowerConfig;
use crate::nvml::{GpuCommand, GpuControl};

/// The dead-man threshold: a heartbeat older than this resets every managed card.
pub const FAILSAFE_STALE: Duration = Duration::from_secs(120);

/// PREINIT `hostd boot`: persistence, application clocks, power limits and `-rgc` on every
/// managed card, in that order (persistence first, or the driver drops later locks).
pub fn boot_plan<'a>(
    cards: impl IntoIterator<Item = (&'a str, &'a CardPowerConfig)>,
) -> Vec<(String, GpuCommand)> {
    let mut v = Vec::new();
    for (uuid, c) in cards {
        if c.persistence {
            v.push((uuid.to_string(), GpuCommand::SetPersistence { on: true }));
        }
        if let Some([mem, sm]) = c.app_clocks {
            v.push((
                uuid.to_string(),
                GpuCommand::SetAppClocks {
                    mem_mhz: mem,
                    sm_mhz: sm,
                },
            ));
        }
        if let Some(w) = c.power_limit_w {
            v.push((uuid.to_string(), GpuCommand::SetPowerLimit { watts: w }));
        }
        v.push((uuid.to_string(), GpuCommand::ResetGpuClocks));
    }
    v
}

/// What the failsafe cron does: nothing while the heartbeat is fresh; otherwise `-rgc` on every
/// managed card (a dead daemon must never leave a card at 135 MHz). `heartbeat_age` is `None`
/// when there is no heartbeat file at all, which also counts as stale.
pub fn failsafe_plan(
    heartbeat_age: Option<Duration>,
    managed_uuids: &[String],
) -> Vec<(String, GpuCommand)> {
    match heartbeat_age {
        Some(a) if a < FAILSAFE_STALE => Vec::new(),
        _ => managed_uuids
            .iter()
            .map(|u| (u.clone(), GpuCommand::ResetGpuClocks))
            .collect(),
    }
}

/// Runs a plan, continuing past failures (the fail-safe path tries every card). Returns the
/// failures as `uuid: error`.
pub fn run_plan(gpu: &mut dyn GpuControl, plan: &[(String, GpuCommand)]) -> Vec<String> {
    plan.iter()
        .filter_map(|(u, c)| gpu.apply(u, *c).err().map(|e| format!("{u}: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvml::FakeNvml;

    #[test]
    fn boot_replaces_boot_sh() {
        let a: CardPowerConfig = toml::from_str("persistence = true\npower_limit_w = 350").unwrap();
        let b: CardPowerConfig =
            toml::from_str("persistence = true\npower_limit_w = 250\napp_clocks = [877, 1380]")
                .unwrap();
        let plan = boot_plan([("GPU-a", &a), ("GPU-b", &b)]);
        let b_cmds: Vec<_> = plan
            .iter()
            .filter(|(u, _)| u == "GPU-b")
            .map(|(_, c)| *c)
            .collect();
        assert_eq!(
            b_cmds,
            [
                GpuCommand::SetPersistence { on: true },
                GpuCommand::SetAppClocks {
                    mem_mhz: 877,
                    sm_mhz: 1380
                },
                GpuCommand::SetPowerLimit { watts: 250 },
                GpuCommand::ResetGpuClocks
            ]
        );
        let mut f = FakeNvml::default()
            .with_card("GPU-a", 1597)
            .with_card("GPU-b", 1380);
        assert!(run_plan(&mut f, &plan).is_empty());
        assert_eq!(f.cards["GPU-b"].reading.app_sm_mhz, Some(1380));
    }

    #[test]
    fn failsafe_only_when_stale() {
        let m = vec!["GPU-a".to_string(), "GPU-b".to_string()];
        assert!(failsafe_plan(Some(Duration::from_secs(10)), &m).is_empty());
        assert_eq!(failsafe_plan(Some(Duration::from_secs(121)), &m).len(), 2);
        assert_eq!(failsafe_plan(None, &m).len(), 2);
        let mut f = FakeNvml::default().with_card("GPU-a", 1597);
        f.apply(
            "GPU-a",
            GpuCommand::LockGpuClocks {
                min_mhz: 135,
                max_mhz: 135,
            },
        )
        .unwrap();
        // GPU-b is missing: the failure is reported, GPU-a is still reset.
        let errs = run_plan(&mut f, &failsafe_plan(None, &m));
        assert_eq!(errs.len(), 1);
        assert_eq!(f.cards["GPU-a"].lock, None);
    }
}
