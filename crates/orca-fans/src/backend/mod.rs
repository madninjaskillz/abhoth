//! The pluggable fan backends (SUPERVISOR.md 5.3).
//!
//! | kind | mode | fail-safe owner |
//! |---|---|---|
//! | [`esphome`] | device curve (hostd pushes temp + load) | the device: no temp for `stale_seconds` → 100 % |
//! | [`hwmon`] | host curve (`pwmN`) | hostd: chip auto (`pwmN_enable = 2`) or full on exit/stale |
//! | [`ipmi`] | host curve (BMC zones), **read-only** unless the limits allow writes | the BMC: mode restored (Optimal) |
//! | [`liquidctl`] | host curve (fixed duties) | hostd: 100 % on exit/stale |
//! | [`nvml`] | host curve (GPU fans) | the driver: `SetDefaultFanSpeed_v2` |
//! | [`windows`] | design only | FanControl's own "sensor lost" handling |
//! | [`fake`] | either | — (tests) |
//!
//! Writes never happen in shadow mode: [`Shadow`] wraps any backend, passes reads through and
//! records the writes it swallowed.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::config::{BackendKind, Channel, FanMode};

pub mod esphome;
pub mod fake;
pub mod hwmon;
pub mod ipmi;
pub mod liquidctl;
pub mod nvml;
pub mod windows;

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum BackendError {
    #[error("{0} is not supported by this backend")]
    Unsupported(&'static str),
    #[error("backend is read-only (writes are not allowed by config or the limits file)")]
    ReadOnly,
    #[error("unknown channel {0}")]
    NoChannel(String),
    #[error("{0}")]
    Io(String),
}

/// Who keeps the fan safe when hostd is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailSafeOwner {
    Device,
    Host,
    Bmc,
    Driver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub mode: FanMode,
    pub writable: bool,
    pub reads_rpm: bool,
    pub failsafe: FailSafeOwner,
}

/// A reading from a backend for one channel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelReading {
    pub channel: String,
    pub rpm: Option<f64>,
    /// Applied duty, 0-100 %.
    pub duty: Option<f64>,
    /// The device reports this fan (or the device) in failsafe.
    pub failsafe: Option<bool>,
    pub stalled: Option<bool>,
    /// Any other value worth showing (mode, FF duty, liquid temp …).
    pub extra: Vec<(String, String)>,
}

/// A write a backend would perform, as recorded by [`Shadow`] and the fake backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    PushInputs {
        channel: String,
        temp: String,
        load: Option<String>,
    },
    SetDuty {
        channel: String,
        duty: f64,
    },
    SetSetting {
        entity: String,
        value: f64,
    },
    FailSafe {
        channel: String,
    },
    Release,
}

#[async_trait]
pub trait FanBackend: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> BackendKind;
    fn caps(&self) -> Caps;

    /// Device-curve backends: push the control temperature and (first) the load.
    async fn push_inputs(
        &self,
        _ch: &Channel,
        _temp: &str,
        _load: Option<&str>,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("push_inputs"))
    }

    /// Host-curve backends: apply a duty (0-100 %).
    async fn set_duty(&self, _ch: &Channel, _duty: f64) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("set_duty"))
    }

    /// Device settings (the ESP curve numbers); only through a [`crate::esp::DeviceSync`] plan.
    async fn set_setting(&self, _entity: &str, _value: f64) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("set_setting"))
    }

    /// Current readings for every channel the backend knows.
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError>;

    /// Input stale or loop failing: put this channel into its safe state (full speed or
    /// hardware auto). Device-curve backends do nothing: they stop being fed and the device
    /// fails safe on its own.
    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError>;

    /// hostd is exiting: hand every touched channel back to the hardware.
    async fn release(&self) -> Result<(), BackendError>;
}

/// Shadow mode: reads pass through; writes are recorded and never reach the hardware.
pub struct Shadow<B: ?Sized> {
    pub inner: Box<B>,
    pub log: Mutex<Vec<Write>>,
}

impl<B: FanBackend + ?Sized> Shadow<B> {
    pub fn new(inner: Box<B>) -> Shadow<B> {
        Shadow {
            inner,
            log: Mutex::new(Vec::new()),
        }
    }

    fn rec(&self, w: Write) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push(w);
    }

    pub fn take_log(&self) -> Vec<Write> {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl<B: FanBackend + ?Sized> FanBackend for Shadow<B> {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn kind(&self) -> BackendKind {
        self.inner.kind()
    }
    fn caps(&self) -> Caps {
        Caps {
            writable: false,
            ..self.inner.caps()
        }
    }
    async fn push_inputs(
        &self,
        ch: &Channel,
        temp: &str,
        load: Option<&str>,
    ) -> Result<(), BackendError> {
        self.rec(Write::PushInputs {
            channel: ch.to_string(),
            temp: temp.into(),
            load: load.map(Into::into),
        });
        Ok(())
    }
    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        self.rec(Write::SetDuty {
            channel: ch.to_string(),
            duty,
        });
        Ok(())
    }
    async fn set_setting(&self, entity: &str, value: f64) -> Result<(), BackendError> {
        self.rec(Write::SetSetting {
            entity: entity.into(),
            value,
        });
        Ok(())
    }
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        self.inner.read().await
    }
    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        self.rec(Write::FailSafe {
            channel: ch.to_string(),
        });
        Ok(())
    }
    async fn release(&self) -> Result<(), BackendError> {
        self.rec(Write::Release);
        Ok(())
    }
}

/// Runs external programs (ipmitool, liquidctl). A trait so tests can script the output.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs `program args…` and returns stdout, or an error with stderr / the exit status.
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String>;
}

/// The real runner: no shell, fixed program names, a timeout.
pub struct ProcessRunner {
    pub timeout: std::time::Duration,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        ProcessRunner {
            timeout: std::time::Duration::from_secs(10),
        }
    }
}

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let fut = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| format!("{program}: timed out after {:?}", self.timeout))?
            .map_err(|e| format!("{program}: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("{program}: {} {}", out.status, err.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
pub(crate) mod test_runner {
    use super::*;

    /// Scripted runner: returns canned outputs by the joined command line, records calls.
    #[derive(Default)]
    pub struct Scripted {
        pub outputs: Mutex<Vec<(String, Result<String, String>)>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Scripted {
        pub fn on(&self, cmd: &str, out: Result<&str, &str>) {
            self.outputs
                .lock()
                .unwrap()
                .push((cmd.into(), out.map(Into::into).map_err(Into::into)));
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for Scripted {
        async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
            let line = std::iter::once(program.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(line.clone());
            for (k, v) in self.outputs.lock().unwrap().iter() {
                if *k == line {
                    return v.clone();
                }
            }
            Ok(String::new())
        }
    }
}
