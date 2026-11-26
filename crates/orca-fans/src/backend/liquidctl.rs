//! liquidctl backend (Corsair Commander, NZXT Kraken / Smart Device …) through the
//! `liquidctl` CLI with JSON output: `liquidctl --match <m> status --json` to read,
//! `liquidctl --match <m> set <channel> speed <duty>` to write.
//!
//! Host curve: hostd computes the duty. On stale input and on exit the channel is set to
//! 100 % (`on_exit = "full"`, the default for liquidctl) — these controllers have no
//! "automatic" mode to hand back to unless a device profile was uploaded, which
//! [`Liquidctl::set_profile`] does (a temperature/duty profile against the liquid sensor,
//! stored on the device, which then keeps working without hostd).

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::{BackendError, Caps, ChannelReading, CommandRunner, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, FanMode, OnExit};

/// A device-side profile: (temperature, duty) points.
type Profile = Vec<(f64, f64)>;

pub struct Liquidctl {
    id: String,
    matcher: String,
    runner: Box<dyn CommandRunner>,
    writable: bool,
    on_exit: OnExit,
    /// Channel → a device profile to restore on exit (if one was configured).
    profiles: Mutex<Vec<(String, Profile)>>,
    touched: Mutex<Vec<String>>,
}

impl Liquidctl {
    pub fn new(
        id: &str,
        matcher: &str,
        runner: Box<dyn CommandRunner>,
        writable: bool,
        on_exit: OnExit,
    ) -> Liquidctl {
        Liquidctl {
            id: id.into(),
            matcher: matcher.into(),
            runner,
            writable,
            on_exit,
            profiles: Mutex::new(Vec::new()),
            touched: Mutex::new(Vec::new()),
        }
    }

    async fn lc(&self, args: Vec<String>) -> Result<String, BackendError> {
        let mut a = vec!["--match".to_string(), self.matcher.clone()];
        a.extend(args);
        self.runner
            .run("liquidctl", &a)
            .await
            .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))
    }

    fn channel_name(ch: &Channel) -> Result<String, BackendError> {
        let s = ch.to_string();
        if s.is_empty()
            || !s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(BackendError::NoChannel(s));
        }
        Ok(s)
    }

    /// Uploads a device-side profile `[(temp, duty)…]` (against the liquid temperature) and
    /// remembers it as the channel's exit state.
    pub async fn set_profile(
        &self,
        ch: &Channel,
        points: &[(f64, f64)],
    ) -> Result<(), BackendError> {
        if !self.writable {
            return Err(BackendError::ReadOnly);
        }
        let c = Self::channel_name(ch)?;
        let mut args = vec!["set".to_string(), c.clone(), "speed".to_string()];
        for (t, d) in points {
            args.push(format!("{}", t.round() as i64));
            args.push(format!("{}", d.clamp(0.0, 100.0).round() as i64));
        }
        self.lc(args).await?;
        let mut p = self.profiles.lock().unwrap_or_else(|e| e.into_inner());
        p.retain(|(n, _)| *n != c);
        p.push((c, points.to_vec()));
        Ok(())
    }

    async fn safe(&self, c: &str) -> Result<(), BackendError> {
        let profile = self
            .profiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(n, _)| n == c)
            .map(|(_, p)| p.clone());
        match (self.on_exit, profile) {
            (OnExit::Auto, Some(p)) => self.set_profile(&Channel::Name(c.into()), &p).await,
            _ => self
                .lc(vec!["set".into(), c.into(), "speed".into(), "100".into()])
                .await
                .map(|_| ()),
        }
    }
}

/// Parses `liquidctl status --json`: a list of devices, each with `status: [{key, value, unit}]`.
pub fn parse_status(out: &str) -> Result<Vec<ChannelReading>, String> {
    let v: Value = serde_json::from_str(out).map_err(|e| format!("liquidctl status: {e}"))?;
    let mut fans: Vec<ChannelReading> = Vec::new();
    for dev in v.as_array().ok_or("liquidctl status: not a list")? {
        let Some(st) = dev.get("status").and_then(Value::as_array) else {
            continue;
        };
        for item in st {
            let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
            let val = item.get("value");
            let unit = item.get("unit").and_then(Value::as_str).unwrap_or_default();
            let lower = key.to_ascii_lowercase();
            // "Fan 1 speed" (rpm), "Fan 1 duty" (%), "Pump speed" …
            let (name, what) = if let Some(n) = lower.strip_suffix(" speed") {
                (n.to_string(), "rpm")
            } else if let Some(n) = lower.strip_suffix(" duty") {
                (n.to_string(), "duty")
            } else {
                if let Some(first) = fans.first_mut() {
                    first.extra.push((
                        key.to_string(),
                        format!("{} {unit}", val.map(|v| v.to_string()).unwrap_or_default()),
                    ));
                }
                continue;
            };
            let chan = name.replace(' ', "");
            let idx = match fans.iter().position(|f| f.channel == chan) {
                Some(i) => i,
                None => {
                    fans.push(ChannelReading {
                        channel: chan.clone(),
                        ..Default::default()
                    });
                    fans.len() - 1
                }
            };
            let num = val.and_then(Value::as_f64);
            if what == "rpm" {
                fans[idx].rpm = num;
            } else {
                fans[idx].duty = num;
            }
        }
    }
    Ok(fans)
}

#[async_trait]
impl FanBackend for Liquidctl {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Liquidctl
    }
    fn caps(&self) -> Caps {
        Caps {
            mode: FanMode::HostCurve,
            writable: self.writable,
            reads_rpm: true,
            failsafe: FailSafeOwner::Host,
        }
    }

    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        if !self.writable {
            return Err(BackendError::ReadOnly);
        }
        let c = Self::channel_name(ch)?;
        {
            let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
            if !t.contains(&c) {
                t.push(c.clone());
            }
        }
        let d = duty.clamp(0.0, 100.0).round() as i64;
        self.lc(vec!["set".into(), c, "speed".into(), d.to_string()])
            .await
            .map(|_| ())
    }

    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        let out = self.lc(vec!["status".into(), "--json".into()]).await?;
        parse_status(&out).map_err(BackendError::Io)
    }

    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        if !self.writable {
            return Ok(());
        }
        self.safe(&Self::channel_name(ch)?).await
    }

    async fn release(&self) -> Result<(), BackendError> {
        let t = std::mem::take(&mut *self.touched.lock().unwrap_or_else(|e| e.into_inner()));
        for c in t {
            self.safe(&c).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_runner::Scripted;
    use super::*;
    use std::sync::Arc;

    struct Shared(Arc<Scripted>);
    #[async_trait]
    impl CommandRunner for Shared {
        async fn run(&self, p: &str, a: &[String]) -> Result<String, String> {
            self.0.run(p, a).await
        }
    }

    #[test]
    fn status_json() {
        let out = r#"[{"bus":"hid","address":"x","description":"Corsair Commander Pro","status":[
            {"key":"Fan 1 speed","value":1220,"unit":"rpm"},{"key":"Fan 1 duty","value":40,"unit":"%"},
            {"key":"Fan 2 speed","value":0,"unit":"rpm"},{"key":"Temperature 1","value":31.5,"unit":"°C"}]}]"#;
        let r = parse_status(out).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].channel.as_str(), r[0].rpm, r[0].duty),
            ("fan1", Some(1220.0), Some(40.0))
        );
        assert!(r[0].extra.iter().any(|(k, _)| k == "Temperature 1"));
    }

    #[tokio::test]
    async fn set_and_release_full() {
        let s = Arc::new(Scripted::default());
        let b = Liquidctl::new(
            "aio",
            "Commander",
            Box::new(Shared(s.clone())),
            true,
            OnExit::Full,
        );
        b.set_duty(&Channel::Name("fan1".into()), 42.4)
            .await
            .unwrap();
        b.release().await.unwrap();
        assert_eq!(
            s.calls(),
            vec![
                "liquidctl --match Commander set fan1 speed 42",
                "liquidctl --match Commander set fan1 speed 100"
            ]
        );
        assert!(
            b.set_duty(&Channel::Name("fan1; rm".into()), 1.0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn profile_is_the_exit_state_with_auto() {
        let s = Arc::new(Scripted::default());
        let b = Liquidctl::new(
            "aio",
            "Kraken",
            Box::new(Shared(s.clone())),
            true,
            OnExit::Auto,
        );
        b.set_profile(&Channel::Name("fan".into()), &[(25.0, 30.0), (40.0, 100.0)])
            .await
            .unwrap();
        b.set_duty(&Channel::Name("fan".into()), 50.0)
            .await
            .unwrap();
        b.fail_safe(&Channel::Name("fan".into())).await.unwrap();
        assert_eq!(
            s.calls().last().unwrap(),
            "liquidctl --match Kraken set fan speed 25 30 40 100"
        );
    }
}
