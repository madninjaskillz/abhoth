//! The fan loop: one policy evaluation per fan per tick, producing backend actions.
//! Pure and synchronous (time is passed in), so the same code runs in hostd, in shadow mode
//! and in the parity replays. [`execute`] carries the actions out against backends.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::backend::{BackendError, FanBackend};
use crate::config::{
    BackendConfig, BackendKind, Channel, ConfigError, FanMode, FanSpec, FansConfig, ProfileConfig,
};
use crate::limits::{FanLimits, MAX_OVERRIDE_TTL_S};
use crate::policy::{self, Effective, HostAction, HostCurve, Override, Target};
use crate::push::{DevicePush, Push, Step};

/// The `fans` part of hostd's `policy.put` (SUPERVISOR.md 2.4), compiled from `[fans.*]`,
/// `[fan_backends.*]`, `[fan_profiles.*]` and `[fan_defaults]`. Secrets stay references.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FansPolicy {
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
    #[serde(default)]
    pub fans: Vec<FanSpec>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

impl FansPolicy {
    pub fn compile(
        cfg: &FansConfig,
        limits: &FanLimits,
        may_write: &dyn Fn(BackendKind) -> bool,
    ) -> Result<FansPolicy, ConfigError> {
        let fans = cfg.resolve(limits, may_write)?;
        Ok(FansPolicy {
            backends: cfg.fan_backends.clone(),
            fans,
            profiles: cfg.fan_profiles.clone(),
        })
    }

    /// hostd's check of a received policy against its own limits: the whole policy is refused
    /// on the first violation.
    pub fn validate(
        &self,
        limits: &FanLimits,
        may_write: &dyn Fn(BackendKind) -> bool,
    ) -> Result<(), String> {
        for f in &self.fans {
            let b = self
                .backends
                .get(&f.backend)
                .ok_or_else(|| format!("fans.{}: backend {:?} is missing", f.id, f.backend))?;
            limits
                .check_fan(f, b, may_write)
                .map_err(|e| format!("fans.{}: {e}", f.id))?;
            crate::config::Curve::validate(&f.policy.curve.0)
                .map_err(|e| format!("fans.{}: {e}", f.id))?;
            if let Some(p) = &f.profile
                && !self.profiles.contains_key(p)
            {
                return Err(format!("fans.{}: profile {p:?} is missing", f.id));
            }
        }
        for (id, p) in &self.profiles {
            if p.duty.is_some() && !p.ttl.is_some_and(|t| t.as_secs_f64() <= MAX_OVERRIDE_TTL_S) {
                return Err(format!("fan_profiles.{id}: a fixed duty needs a ttl ≤ 1 h"));
            }
        }
        Ok(())
    }
}

/// A sensor value with the timestamp of its oldest input (monotonic seconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub value: f64,
    pub ts: f64,
}

/// The evaluated inputs of one fan. `None` = the expression is missing (never 0).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FanInputs {
    pub temp: Option<Sample>,
    pub load: Option<Sample>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Push { fan: usize, push: Push },
    SetDuty { fan: usize, duty: f64 },
    FailSafe { fan: usize },
}

/// One fan's evaluation this tick (for logs, the dashboard and shadow mode).
#[derive(Debug, Clone, PartialEq)]
pub struct FanStatus {
    pub fan: String,
    /// What the policy wants (for device fans: what the device curve should produce).
    pub target: Target,
    pub action: Option<Action>,
    /// The input is missing or stale.
    pub missing: bool,
}

struct FanState {
    spec: FanSpec,
    device: Option<DevicePush>,
    host: Option<HostCurve>,
    ovr: Option<Override>,
}

pub struct FanLoop {
    fans: Vec<FanState>,
    profiles: BTreeMap<String, ProfileConfig>,
    /// Profile chosen by the calendar, HA or the UI, with its expiry.
    active_profile: Option<(String, f64)>,
}

impl FanLoop {
    pub fn new(policy: &FansPolicy) -> FanLoop {
        let fans = policy
            .fans
            .iter()
            .map(|s| FanState {
                spec: s.clone(),
                device: (s.mode == FanMode::DeviceCurve).then(|| DevicePush::new(s.send)),
                host: (s.mode == FanMode::HostCurve).then(|| HostCurve::new(s.stale)),
                ovr: None,
            })
            .collect();
        FanLoop {
            fans,
            profiles: policy.profiles.clone(),
            active_profile: None,
        }
    }

    pub fn specs(&self) -> impl Iterator<Item = &FanSpec> {
        self.fans.iter().map(|f| &f.spec)
    }

    pub fn spec(&self, fan: usize) -> &FanSpec {
        &self.fans[fan].spec
    }

    /// Selects a profile for every fan until `until` (monotonic seconds; capped at 1 h from
    /// `now`). `None` returns to each fan's configured default.
    pub fn set_profile(
        &mut self,
        name: Option<&str>,
        now: f64,
        until: Option<f64>,
    ) -> Result<(), String> {
        match name {
            None => self.active_profile = None,
            Some(n) => {
                let p = self
                    .profiles
                    .get(n)
                    .ok_or_else(|| format!("unknown fan profile {n:?}"))?;
                let cap = now + MAX_OVERRIDE_TTL_S;
                let ttl_end = p.ttl.map(|t| now + t.as_secs_f64());
                let end = [until, ttl_end].into_iter().flatten().fold(cap, f64::min);
                self.active_profile = Some((n.to_string(), end));
            }
        }
        Ok(())
    }

    /// A duty override for one fan, TTL ≤ 1 h. Floors and caps still apply.
    pub fn set_override(
        &mut self,
        fan: &str,
        duty: f64,
        now: f64,
        ttl_s: f64,
    ) -> Result<(), String> {
        if !(0.0..=MAX_OVERRIDE_TTL_S).contains(&ttl_s) {
            return Err("override ttl must be 0-3600 s".into());
        }
        let f = self
            .fans
            .iter_mut()
            .find(|f| f.spec.id == fan)
            .ok_or_else(|| format!("unknown fan {fan:?}"))?;
        f.ovr = Some(Override {
            duty: duty.clamp(0.0, 100.0),
            until: now + ttl_s,
        });
        Ok(())
    }

    fn effective(&self, f: &FanState, now: f64) -> Effective {
        let name = match &self.active_profile {
            Some((n, until)) if now < *until => Some(n.as_str()),
            _ => f.spec.profile.as_deref(),
        };
        let p = name.and_then(|n| self.profiles.get(n));
        // A device fan cannot take a fixed duty from a profile: the device runs its own curve.
        let p = p.filter(|p| !(f.spec.mode == FanMode::DeviceCurve && p.duty.is_some()));
        policy::apply_profile(&f.spec.policy, p)
    }

    /// One tick. `inputs(i, spec)` evaluates fan `i`'s expressions.
    pub fn step(
        &mut self,
        now: f64,
        inputs: &mut dyn FnMut(usize, &FanSpec) -> FanInputs,
    ) -> Vec<FanStatus> {
        let mut out = Vec::with_capacity(self.fans.len());
        for i in 0..self.fans.len() {
            let inp = inputs(i, &self.fans[i].spec);
            let eff = self.effective(&self.fans[i], now);
            let f = &mut self.fans[i];
            if f.ovr.is_some_and(|o| now >= o.until) {
                f.ovr = None;
            }
            let load = inp.load.map(|s| s.value);
            if let Some(dp) = f.device.as_mut() {
                let stale_in = f.spec.stale_input.as_secs_f64();
                let temp = inp
                    .temp
                    .filter(|s| now - s.ts <= stale_in && s.value.is_finite())
                    .map(|s| s.value);
                let target = policy::target(&eff, temp, load, None, now);
                let step = dp.step(now, temp, load, eff.policy.load_gate.as_ref());
                let action = match step {
                    Step::Send(push) => Some(Action::Push { fan: i, push }),
                    _ => None,
                };
                out.push(FanStatus {
                    fan: f.spec.id.clone(),
                    target,
                    action,
                    missing: temp.is_none(),
                });
            } else if let Some(h) = f.host.as_mut() {
                let temp = inp.temp.map(|s| s.value);
                let age = inp.temp.map(|s| now - s.ts);
                let (a, target) = h.step(now, &eff, temp, age, load, f.ovr);
                let action = match a {
                    HostAction::SetDuty(d) => Action::SetDuty { fan: i, duty: d },
                    HostAction::FailSafe => Action::FailSafe { fan: i },
                };
                out.push(FanStatus {
                    fan: f.spec.id.clone(),
                    target,
                    action: Some(action),
                    missing: target.reason == policy::Reason::FailSafe,
                });
            }
        }
        out
    }

    /// Records the outcome of a [`Action::Push`].
    pub fn push_result(&mut self, fan: usize, push: &Push, ok: bool, now: f64) {
        if let Some(dp) = self.fans[fan].device.as_mut() {
            if ok {
                dp.sent_ok(push, now);
            } else {
                dp.send_failed(now);
            }
        }
    }
}

/// Carries out one action. Device pushes report back to the loop through the returned result.
pub async fn execute(
    lp: &FanLoop,
    action: &Action,
    backends: &BTreeMap<String, Arc<dyn FanBackend>>,
) -> Result<(), BackendError> {
    let fan = match action {
        Action::Push { fan, .. } | Action::SetDuty { fan, .. } | Action::FailSafe { fan } => *fan,
    };
    let spec = lp.spec(fan);
    let b = backends
        .get(&spec.backend)
        .ok_or_else(|| BackendError::Io(format!("backend {} not open", spec.backend)))?;
    let ch: &Channel = &spec.channel;
    match action {
        Action::Push { push, .. } => b.push_inputs(ch, &push.temp, push.load.as_deref()).await,
        Action::SetDuty { duty, .. } => match b.set_duty(ch, *duty).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // A host-curve fan whose duty cannot be written goes to its safe state at once
                // (full or hardware auto); the error is still reported for the alert.
                let _ = b.fail_safe(ch).await;
                Err(e)
            }
        },
        Action::FailSafe { .. } => b.fail_safe(ch).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Fake;
    use crate::backend::{Shadow, Write};

    const CFG: &str = r#"
[fan_backends.esp]
kind = "esphome"
url = "http://esp.invalid"
auth = { kind = "digest", user = "fans", password = "secret:esp" }
mode = "act"

[fan_backends.mb]
kind = "hwmon"
chip = "nct6798"
on_exit = "auto"

[fan_backends.bmc]
kind = "ipmi"

[fan_defaults]
load_gate    = { lo = 52, hi = 64 }
curve        = [[45, 30], [78, 100]]
feed_forward = { lo = 20, hi = 90, max_duty = 80 }
min_duty     = 30
max_temp     = 85
ramp_down    = "2%/2s"
stale_input  = "3s"
send         = { temp_delta = 1.0, load_delta = 10, min_gap = "1s", heartbeat = "10s" }

[fans.duct0]
backend = "esp"
channel = 0
temp = "sensor.ctrl_B"
load = "gpu.B.power_pct"

[fans.cpu]
backend = "mb"
channel = 2
temp = "hwmon.k10temp.Tctl"
load_gate = { lo = 0, hi = 1 }
load = "host.cpu"
min_duty = 40
stale = "5s"

[fan_profiles.quiet]
curve = [[50, 25], [80, 85]]
[fan_profiles.full]
duty = 100
ttl = "1h"
"#;

    fn all(_: BackendKind) -> bool {
        true
    }
    fn no_ipmi(k: BackendKind) -> bool {
        k != BackendKind::Ipmi
    }

    fn compile(text: &str) -> Result<FansPolicy, ConfigError> {
        FansPolicy::compile(
            &FansConfig::from_toml(text).unwrap(),
            &FanLimits::default(),
            &no_ipmi,
        )
    }

    #[test]
    fn compiles_round_trips_and_validates() {
        let p = compile(CFG).unwrap();
        assert_eq!(p.fans.len(), 2);
        assert_eq!(p.fans[0].id, "cpu");
        assert_eq!(p.fans[0].mode, FanMode::HostCurve);
        assert_eq!(p.fans[1].mode, FanMode::DeviceCurve);
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("hunter"));
        assert!(json.contains("secret:esp"));
        let back: FansPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        back.validate(&FanLimits::default(), &no_ipmi).unwrap();
        // A stricter hostd refuses the whole policy.
        let strict = FanLimits {
            min_duty: 35.0,
            ..Default::default()
        };
        assert!(
            back.validate(&strict, &no_ipmi)
                .unwrap_err()
                .contains("min_duty")
        );
    }

    #[test]
    fn config_refusals() {
        let cases = [
            ("max_temp     = 85", "max_temp     = 95", "max_control_temp"),
            ("min_duty     = 30", "min_duty     = 20", "below"),
            (
                "[fan_backends.bmc]\nkind = \"ipmi\"",
                "[fan_backends.bmc]\nkind = \"ipmi\"\nwrite = true",
                "IPMI",
            ),
            ("stale = \"5s\"", "stale = \"60s\"", "longer"),
            ("channel = 2\n", "channel = 2\nbogus = 1\n", "bogus"),
            ("kind = \"hwmon\"", "kind = \"windows\"", "design only"),
            (
                "ramp_down    = \"2%/2s\"",
                "ramp_down    = \"2/2s\"",
                "ramp_down",
            ),
            ("ttl = \"1h\"", "ttl = \"2h\"", "ttl"),
            (
                "curve        = [[45, 30], [78, 100]]",
                "curve        = [[45, 30], [90, 100]]",
                "above max_temp",
            ),
        ];
        for (from, to, want) in cases {
            assert!(CFG.contains(from), "{from}");
            let text = CFG.replacen(from, to, 1);
            let err = match FansConfig::from_toml(&text) {
                Err(e) => e.to_string(),
                Ok(c) => FansPolicy::compile(&c, &FanLimits::default(), &no_ipmi)
                    .unwrap_err()
                    .to_string(),
            };
            assert!(err.contains(want), "{to}: {err}");
        }
        // With IPMI writes allowed by the limits, write = true is accepted.
        let text = CFG.replacen("kind = \"ipmi\"", "kind = \"ipmi\"\nwrite = true", 1);
        FansPolicy::compile(
            &FansConfig::from_toml(&text).unwrap(),
            &FanLimits::default(),
            &all,
        )
        .unwrap();
    }

    fn inputs(t: f64, load: Option<f64>, ts: f64) -> FanInputs {
        FanInputs {
            temp: Some(Sample { value: t, ts }),
            load: load.map(|l| Sample { value: l, ts }),
        }
    }

    #[test]
    fn device_and_host_fans_in_one_loop() {
        let p = compile(CFG).unwrap();
        let mut lp = FanLoop::new(&p);
        let st = lp.step(0.0, &mut |_, _| inputs(60.0, Some(50.0), 0.0));
        // cpu (host): ramps down from 100 % only over time; the gate (0-1 °C) passes the load.
        assert_eq!(
            st[0].action,
            Some(Action::SetDuty {
                fan: 0,
                duty: 100.0
            })
        );
        // duct0 (device): first push, load gated by 60 °C → 50 × 8/12.
        match &st[1].action {
            Some(Action::Push { fan: 1, push }) => {
                assert_eq!(
                    (push.temp.as_str(), push.load.as_deref()),
                    ("60.0", Some("33.3"))
                );
                lp.push_result(1, push, true, 0.0);
            }
            a => panic!("{a:?}"),
        }
        // Inputs go stale (sample 6 s old): device fan sends nothing, host fan fails safe.
        let st = lp.step(6.0, &mut |_, _| inputs(60.0, Some(50.0), 0.0));
        assert_eq!(st[0].action, Some(Action::FailSafe { fan: 0 }));
        assert!(st[1].missing && st[1].action.is_none());
    }

    #[test]
    fn profiles_overrides_and_caps() {
        let p = compile(CFG).unwrap();
        let mut lp = FanLoop::new(&p);
        lp.set_profile(Some("full"), 0.0, None).unwrap();
        let st = lp.step(0.0, &mut |_, _| inputs(40.0, None, 0.0));
        assert_eq!(
            st[0].action,
            Some(Action::SetDuty {
                fan: 0,
                duty: 100.0
            })
        );
        // The full profile does not apply to the device fan (it runs its own curve).
        assert_eq!(st[1].target.duty, 30.0);
        // Profiles expire within an hour whatever was asked.
        lp.set_profile(Some("quiet"), 0.0, Some(1e9)).unwrap();
        assert!(lp.set_profile(Some("nope"), 0.0, None).is_err());
        assert!(lp.set_override("cpu", 0.0, 0.0, 7200.0).is_err());
        lp.set_override("cpu", 0.0, 0.0, 60.0).unwrap();
        let st = lp.step(100.0, &mut |_, _| inputs(40.0, None, 100.0));
        // Override 0 % → floored at the fan's min_duty 40 (after ramping down from 100).
        let mut duty = 0.0;
        for k in 0..100 {
            let s = lp.step(101.0 + k as f64, &mut |_, _| {
                inputs(40.0, None, 101.0 + k as f64)
            });
            if let Some(Action::SetDuty { duty: d, .. }) = s[0].action {
                duty = d;
            }
        }
        assert_eq!(duty, 40.0);
        let _ = st;
        // max_temp beats the override.
        lp.set_override("cpu", 0.0, 300.0, 60.0).unwrap();
        let s = lp.step(301.0, &mut |_, _| inputs(85.0, None, 301.0));
        assert_eq!(
            s[0].action,
            Some(Action::SetDuty {
                fan: 0,
                duty: 100.0
            })
        );
    }

    #[tokio::test]
    async fn failed_duty_write_falls_back_to_failsafe() {
        struct Flaky(Fake);
        #[async_trait::async_trait]
        impl FanBackend for Flaky {
            fn id(&self) -> &str {
                "mb"
            }
            fn kind(&self) -> BackendKind {
                BackendKind::Hwmon
            }
            fn caps(&self) -> crate::backend::Caps {
                self.0.caps()
            }
            async fn set_duty(&self, _c: &Channel, _d: f64) -> Result<(), BackendError> {
                Err(BackendError::Io("EIO".into()))
            }
            async fn read(&self) -> Result<Vec<crate::backend::ChannelReading>, BackendError> {
                Ok(vec![])
            }
            async fn fail_safe(&self, c: &Channel) -> Result<(), BackendError> {
                self.0.fail_safe(c).await
            }
            async fn release(&self) -> Result<(), BackendError> {
                Ok(())
            }
        }
        let p = compile(CFG).unwrap();
        let lp = FanLoop::new(&p);
        let flaky = Arc::new(Flaky(Fake::new("mb", FanMode::HostCurve)));
        let mut map: BTreeMap<String, Arc<dyn FanBackend>> = BTreeMap::new();
        map.insert("mb".into(), flaky.clone());
        assert!(
            execute(&lp, &Action::SetDuty { fan: 0, duty: 50.0 }, &map)
                .await
                .is_err()
        );
        assert_eq!(
            flaky.0.writes(),
            vec![Write::FailSafe {
                channel: "2".into()
            }]
        );
    }

    #[tokio::test]
    async fn shadow_never_writes() {
        let p = compile(CFG).unwrap();
        let mut lp = FanLoop::new(&p);
        let esp = Arc::new(Fake::new("esp", FanMode::DeviceCurve));
        let mb = Arc::new(Fake::new("mb", FanMode::HostCurve));
        struct ArcB(Arc<Fake>);
        #[async_trait::async_trait]
        impl FanBackend for ArcB {
            fn id(&self) -> &str {
                self.0.id()
            }
            fn kind(&self) -> BackendKind {
                self.0.kind()
            }
            fn caps(&self) -> crate::backend::Caps {
                self.0.caps()
            }
            async fn push_inputs(
                &self,
                c: &Channel,
                t: &str,
                l: Option<&str>,
            ) -> Result<(), BackendError> {
                self.0.push_inputs(c, t, l).await
            }
            async fn set_duty(&self, c: &Channel, d: f64) -> Result<(), BackendError> {
                self.0.set_duty(c, d).await
            }
            async fn read(&self) -> Result<Vec<crate::backend::ChannelReading>, BackendError> {
                self.0.read().await
            }
            async fn fail_safe(&self, c: &Channel) -> Result<(), BackendError> {
                self.0.fail_safe(c).await
            }
            async fn release(&self) -> Result<(), BackendError> {
                self.0.release().await
            }
        }
        let shadow_esp = Arc::new(Shadow::new(
            Box::new(ArcB(esp.clone())) as Box<dyn FanBackend>
        ));
        let shadow_mb = Arc::new(Shadow::new(
            Box::new(ArcB(mb.clone())) as Box<dyn FanBackend>
        ));
        let mut map: BTreeMap<String, Arc<dyn FanBackend>> = BTreeMap::new();
        map.insert("esp".into(), shadow_esp.clone());
        map.insert("mb".into(), shadow_mb.clone());
        for t in 0..20 {
            let now = t as f64 * 0.5;
            let st = lp.step(now, &mut |_, _| inputs(50.0 + t as f64, Some(80.0), now));
            for s in st {
                if let Some(a) = s.action {
                    let r = execute(&lp, &a, &map).await;
                    if let Action::Push { fan, push } = &a {
                        lp.push_result(*fan, push, r.is_ok(), now);
                    }
                }
            }
        }
        assert!(
            esp.writes().is_empty() && mb.writes().is_empty(),
            "nothing reaches the hardware"
        );
        let log = shadow_esp.take_log();
        assert!(
            matches!(&log[0], Write::PushInputs { channel, temp, .. } if channel == "0" && temp == "50.0")
        );
        assert!(!shadow_mb.take_log().is_empty());
    }
}
