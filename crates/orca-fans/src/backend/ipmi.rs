//! IPMI / BMC backend through `ipmitool` on `/dev/ipmi0` (Supermicro-style raw commands).
//!
//! **Read-only by default** (SUPERVISOR.md 15 Q14): `sdr` fan readings and the fan-mode GET
//! (`raw 0x30 0x45 0x00`). Writes (mode SET, zone duty) need `write = true` in config *and*
//! `ipmi_write = true` in hostd's limits file. When writing, the BMC is put in Full mode and
//! the zone duty is set; on stale input and on exit the mode goes back to `restore_mode`
//! (Optimal), so the BMC's own curve takes over.

use std::sync::Mutex;

use async_trait::async_trait;

use super::{BackendError, Caps, ChannelReading, CommandRunner, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, FanMode};

/// Supermicro fan modes.
pub fn parse_mode(s: &str) -> Result<u8, String> {
    match s.to_ascii_lowercase().as_str() {
        "standard" => Ok(0),
        "full" => Ok(1),
        "optimal" => Ok(2),
        "heavyio" | "heavy_io" => Ok(4),
        o => Err(format!(
            "unknown IPMI fan mode {o:?} (standard, full, optimal, heavyio)"
        )),
    }
}

pub fn mode_name(m: u8) -> &'static str {
    match m {
        0 => "standard",
        1 => "full",
        2 => "optimal",
        4 => "heavyio",
        _ => "unknown",
    }
}

pub struct Ipmi {
    id: String,
    runner: Box<dyn CommandRunner>,
    writable: bool,
    restore_mode: u8,
    /// True once hostd has changed the mode (so release knows to restore it).
    took_over: Mutex<bool>,
}

fn hex(b: u8) -> String {
    format!("0x{b:02x}")
}

/// Parses `ipmitool sdr` lines: `FAN1             | 1400 RPM          | ok`.
pub fn parse_sdr_fans(out: &str) -> Vec<(String, Option<f64>, String)> {
    out.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('|').map(str::trim).collect();
            if cols.len() < 3 {
                return None;
            }
            let (name, val, status) = (cols[0], cols[1], cols[2]);
            if val.ends_with("RPM") {
                let rpm = val.trim_end_matches("RPM").trim().parse().ok();
                Some((name.to_string(), rpm, status.to_string()))
            } else if name.to_ascii_uppercase().starts_with("FAN") && val == "no reading" {
                Some((name.to_string(), None, status.to_string()))
            } else {
                None
            }
        })
        .collect()
}

impl Ipmi {
    pub fn new(id: &str, runner: Box<dyn CommandRunner>, writable: bool, restore_mode: u8) -> Ipmi {
        Ipmi {
            id: id.into(),
            runner,
            writable,
            restore_mode,
            took_over: Mutex::new(false),
        }
    }

    async fn ipmitool(&self, args: &[&str]) -> Result<String, BackendError> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.runner
            .run("ipmitool", &a)
            .await
            .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))
    }

    pub async fn mode(&self) -> Result<u8, BackendError> {
        let out = self.ipmitool(&["raw", "0x30", "0x45", "0x00"]).await?;
        u8::from_str_radix(out.trim(), 16)
            .map_err(|_| BackendError::Io(format!("{}: bad fan mode reply {out:?}", self.id)))
    }

    async fn set_mode(&self, m: u8) -> Result<(), BackendError> {
        self.ipmitool(&["raw", "0x30", "0x45", "0x01", &hex(m)])
            .await
            .map(|_| ())
    }

    async fn restore(&self) -> Result<(), BackendError> {
        let took = *self.took_over.lock().unwrap_or_else(|e| e.into_inner());
        if took {
            self.set_mode(self.restore_mode).await?;
            *self.took_over.lock().unwrap_or_else(|e| e.into_inner()) = false;
        }
        Ok(())
    }
}

#[async_trait]
impl FanBackend for Ipmi {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Ipmi
    }
    fn caps(&self) -> Caps {
        Caps {
            mode: FanMode::HostCurve,
            writable: self.writable,
            reads_rpm: true,
            failsafe: FailSafeOwner::Bmc,
        }
    }

    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        if !self.writable {
            return Err(BackendError::ReadOnly);
        }
        let zone = ch
            .index()
            .filter(|z| *z <= 0xff)
            .ok_or_else(|| BackendError::NoChannel(ch.to_string()))? as u8;
        let took = *self.took_over.lock().unwrap_or_else(|e| e.into_inner());
        if !took {
            self.set_mode(1).await?;
            *self.took_over.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
        let d = duty.clamp(0.0, 100.0).round() as u8;
        self.ipmitool(&["raw", "0x30", "0x70", "0x66", "0x01", &hex(zone), &hex(d)])
            .await
            .map(|_| ())
    }

    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        let sdr = self.ipmitool(&["sdr"]).await?;
        let mode = self.mode().await.ok();
        Ok(parse_sdr_fans(&sdr)
            .into_iter()
            .map(|(name, rpm, status)| {
                let mut extra = vec![("status".to_string(), status)];
                if let Some(m) = mode {
                    extra.push(("mode".into(), mode_name(m).into()));
                }
                ChannelReading {
                    channel: name,
                    rpm,
                    duty: None,
                    failsafe: None,
                    stalled: None,
                    extra,
                }
            })
            .collect())
    }

    async fn fail_safe(&self, _ch: &Channel) -> Result<(), BackendError> {
        self.restore().await
    }

    async fn release(&self) -> Result<(), BackendError> {
        self.restore().await
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

    const SDR: &str = "CPU1 Temp        | 45 degrees C      | ok\nFAN1             | 1400 RPM          | ok\nFAN2             | no reading        | ns\nFANA             | 2100 RPM          | ok\n";

    #[tokio::test]
    async fn read_only_by_default() {
        let s = Arc::new(Scripted::default());
        s.on("ipmitool sdr", Ok(SDR));
        s.on("ipmitool raw 0x30 0x45 0x00", Ok(" 02\n"));
        let b = Ipmi::new("bmc", Box::new(Shared(s.clone())), false, 2);
        let r = b.read().await.unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].channel.as_str(), r[0].rpm), ("FAN1", Some(1400.0)));
        assert_eq!(r[1].rpm, None);
        assert!(r[0].extra.contains(&("mode".into(), "optimal".into())));
        assert_eq!(
            b.set_duty(&Channel::Index(0), 50.0).await,
            Err(BackendError::ReadOnly)
        );
        b.fail_safe(&Channel::Index(0)).await.unwrap();
        b.release().await.unwrap();
        // Only reads were ever run.
        assert!(
            s.calls()
                .iter()
                .all(|c| c == "ipmitool sdr" || c == "ipmitool raw 0x30 0x45 0x00")
        );
    }

    #[tokio::test]
    async fn writes_take_over_and_restore() {
        let s = Arc::new(Scripted::default());
        let b = Ipmi::new("bmc", Box::new(Shared(s.clone())), true, 2);
        b.set_duty(&Channel::Index(1), 45.0).await.unwrap();
        b.set_duty(&Channel::Index(1), 60.4).await.unwrap();
        b.fail_safe(&Channel::Index(1)).await.unwrap();
        b.release().await.unwrap();
        assert_eq!(
            s.calls(),
            vec![
                "ipmitool raw 0x30 0x45 0x01 0x01",
                "ipmitool raw 0x30 0x70 0x66 0x01 0x01 0x2d",
                "ipmitool raw 0x30 0x70 0x66 0x01 0x01 0x3c",
                "ipmitool raw 0x30 0x45 0x01 0x02",
            ]
        );
    }

    #[test]
    fn modes() {
        assert_eq!(parse_mode("Optimal"), Ok(2));
        assert!(parse_mode("turbo").is_err());
    }
}
