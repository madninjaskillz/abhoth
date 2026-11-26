//! Linux hwmon PWM backend: `/sys/class/hwmon/hwmonN/{name, pwmK, pwmK_enable, fanK_input}`.
//!
//! The chip is found by its `name` (hwmonN numbers change between boots). On the first write
//! to a channel the original `pwmK_enable` is recorded and the channel is switched to manual
//! (1). On `fail_safe` and `release` the channel goes back to the chip's automatic mode (the
//! recorded value if it was automatic, else 2) or, with `on_exit = "full"`, to 255. If the
//! chip refuses the automatic mode, full speed is written instead.
//!
//! A SIGKILL or a hard hang of hostd leaves the last duty: hostd's watchdog and the
//! `hostd failsafe` cron cover that (SUPERVISOR.md 5.3), and a `min_duty` of 40 % or more is
//! recommended for hwmon fans.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use super::{BackendError, Caps, ChannelReading, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, FanMode, OnExit};

pub struct Hwmon {
    id: String,
    dir: PathBuf,
    on_exit: OnExit,
    writable: bool,
    /// channel → original pwmK_enable.
    touched: Mutex<BTreeMap<u32, Option<String>>>,
}

fn io(e: impl std::fmt::Display) -> BackendError {
    BackendError::Io(e.to_string())
}

/// 0-100 % → 0-255, rounded.
pub fn duty_to_pwm(duty: f64) -> u8 {
    (duty.clamp(0.0, 100.0) * 255.0 / 100.0).round() as u8
}

impl Hwmon {
    /// Finds the chip under `root` (normally `/sys/class/hwmon`).
    pub fn open(
        id: &str,
        root: &Path,
        chip: &str,
        on_exit: OnExit,
        writable: bool,
    ) -> Result<Hwmon, BackendError> {
        let mut found = Vec::new();
        for e in std::fs::read_dir(root).map_err(|e| io(format!("{}: {e}", root.display())))? {
            let p = e.map_err(io)?.path();
            if std::fs::read_to_string(p.join("name"))
                .map(|n| n.trim() == chip)
                .unwrap_or(false)
            {
                found.push(p);
            }
        }
        match found.len() {
            1 => Ok(Hwmon {
                id: id.into(),
                dir: found.remove(0),
                on_exit,
                writable,
                touched: Mutex::new(BTreeMap::new()),
            }),
            0 => Err(io(format!("no hwmon chip named {chip:?}"))),
            _ => Err(io(format!(
                "{} hwmon chips are named {chip:?}; cannot tell them apart",
                found.len()
            ))),
        }
    }

    fn file(&self, f: String) -> PathBuf {
        self.dir.join(f)
    }

    fn write(&self, f: String, v: &str) -> Result<(), BackendError> {
        std::fs::write(self.file(f.clone()), v).map_err(|e| io(format!("{}: {f}: {e}", self.id)))
    }

    fn read_s(&self, f: String) -> Option<String> {
        std::fs::read_to_string(self.file(f))
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn channel(&self, ch: &Channel) -> Result<u32, BackendError> {
        let n = ch
            .index()
            .ok_or_else(|| BackendError::NoChannel(ch.to_string()))?;
        if !self.file(format!("pwm{n}")).exists() {
            return Err(BackendError::NoChannel(ch.to_string()));
        }
        Ok(n)
    }

    fn channels(&self) -> Vec<u32> {
        let mut v: Vec<u32> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("pwm").and_then(|r| r.parse().ok())
            })
            .collect();
        v.sort();
        v
    }

    fn safe(&self, n: u32, orig: Option<&str>) -> Result<(), BackendError> {
        if self.on_exit == OnExit::Full {
            self.write(format!("pwm{n}_enable"), "1")?;
            return self.write(format!("pwm{n}"), "255");
        }
        let auto = match orig {
            Some(o) if o != "0" && o != "1" => o.to_string(),
            _ => "2".to_string(),
        };
        if self.write(format!("pwm{n}_enable"), &auto).is_err() {
            // The chip has no automatic mode we can select: full speed is the safe state.
            let _ = self.write(format!("pwm{n}_enable"), "1");
            return self.write(format!("pwm{n}"), "255");
        }
        Ok(())
    }
}

#[async_trait]
impl FanBackend for Hwmon {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Hwmon
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
        let n = self.channel(ch)?;
        {
            let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
            t.entry(n)
                .or_insert_with(|| self.read_s(format!("pwm{n}_enable")));
        }
        if self.read_s(format!("pwm{n}_enable")).as_deref() != Some("1") {
            self.write(format!("pwm{n}_enable"), "1")?;
        }
        self.write(format!("pwm{n}"), &duty_to_pwm(duty).to_string())
    }

    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        Ok(self
            .channels()
            .into_iter()
            .map(|n| {
                let mut extra = Vec::new();
                if let Some(e) = self.read_s(format!("pwm{n}_enable")) {
                    extra.push(("pwm_enable".into(), e));
                }
                ChannelReading {
                    channel: n.to_string(),
                    rpm: self
                        .read_s(format!("fan{n}_input"))
                        .and_then(|s| s.parse().ok()),
                    duty: self
                        .read_s(format!("pwm{n}"))
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|p| (p * 100.0 / 255.0 * 10.0).round() / 10.0),
                    failsafe: None,
                    stalled: None,
                    extra,
                }
            })
            .collect())
    }

    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        if !self.writable {
            return Ok(());
        }
        let n = self.channel(ch)?;
        let orig = self
            .touched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&n)
            .cloned()
            .flatten();
        self.safe(n, orig.as_deref())
    }

    async fn release(&self) -> Result<(), BackendError> {
        let touched = std::mem::take(&mut *self.touched.lock().unwrap_or_else(|e| e.into_inner()));
        let mut first_err = None;
        for (n, orig) in touched {
            if let Err(e) = self.safe(n, orig.as_deref()) {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(root: &Path, dir: &str, name: &str, enable: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        for n in 1..=2 {
            std::fs::write(d.join(format!("pwm{n}")), "128\n").unwrap();
            std::fs::write(d.join(format!("pwm{n}_enable")), format!("{enable}\n")).unwrap();
            std::fs::write(d.join(format!("fan{n}_input")), "900\n").unwrap();
        }
    }

    fn rd(root: &Path, f: &str) -> String {
        std::fs::read_to_string(root.join(f))
            .unwrap()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn write_then_restore_auto() {
        let t = tempfile::tempdir().unwrap();
        chip(t.path(), "hwmon0", "k10temp", "2");
        chip(t.path(), "hwmon3", "nct6798", "5");
        let h = Hwmon::open("mb", t.path(), "nct6798", OnExit::Auto, true).unwrap();
        h.set_duty(&Channel::Index(2), 50.0).await.unwrap();
        assert_eq!(rd(t.path(), "hwmon3/pwm2_enable"), "1");
        assert_eq!(rd(t.path(), "hwmon3/pwm2"), "128");
        h.set_duty(&Channel::Index(2), 100.0).await.unwrap();
        assert_eq!(rd(t.path(), "hwmon3/pwm2"), "255");
        let r = h.read().await.unwrap();
        assert_eq!(r[1].rpm, Some(900.0));
        assert_eq!(r[1].duty, Some(100.0));
        // Release restores the chip's own smart-fan mode (5), untouched channels stay.
        h.release().await.unwrap();
        assert_eq!(rd(t.path(), "hwmon3/pwm2_enable"), "5");
        assert_eq!(rd(t.path(), "hwmon3/pwm1_enable"), "5");
        assert!(matches!(
            h.set_duty(&Channel::Index(7), 50.0).await,
            Err(BackendError::NoChannel(_))
        ));
    }

    #[tokio::test]
    async fn on_exit_full_and_manual_origin() {
        let t = tempfile::tempdir().unwrap();
        chip(t.path(), "hwmon1", "it8686", "1");
        let h = Hwmon::open("mb", t.path(), "it8686", OnExit::Auto, true).unwrap();
        h.set_duty(&Channel::Index(1), 40.0).await.unwrap();
        // The original mode was manual: auto (2) is chosen, not "1".
        h.fail_safe(&Channel::Index(1)).await.unwrap();
        assert_eq!(rd(t.path(), "hwmon1/pwm1_enable"), "2");
        let f = Hwmon::open("mb", t.path(), "it8686", OnExit::Full, true).unwrap();
        f.set_duty(&Channel::Index(1), 40.0).await.unwrap();
        f.fail_safe(&Channel::Index(1)).await.unwrap();
        assert_eq!(rd(t.path(), "hwmon1/pwm1"), "255");
        assert_eq!(rd(t.path(), "hwmon1/pwm1_enable"), "1");
    }

    #[tokio::test]
    async fn read_only_and_lookup_errors() {
        let t = tempfile::tempdir().unwrap();
        chip(t.path(), "hwmon1", "it8686", "2");
        chip(t.path(), "hwmon2", "it8686", "2");
        assert!(Hwmon::open("x", t.path(), "it8686", OnExit::Auto, true).is_err());
        assert!(Hwmon::open("x", t.path(), "nope", OnExit::Auto, true).is_err());
        let t = tempfile::tempdir().unwrap();
        chip(t.path(), "hwmon1", "it8686", "2");
        let h = Hwmon::open("x", t.path(), "it8686", OnExit::Auto, false).unwrap();
        assert_eq!(
            h.set_duty(&Channel::Index(1), 50.0).await,
            Err(BackendError::ReadOnly)
        );
        assert_eq!(rd(t.path(), "hwmon1/pwm1"), "128");
    }

    #[test]
    fn pwm_scale() {
        assert_eq!(duty_to_pwm(0.0), 0);
        assert_eq!(duty_to_pwm(30.0), 77);
        assert_eq!(duty_to_pwm(100.0), 255);
        assert_eq!(duty_to_pwm(150.0), 255);
    }
}
