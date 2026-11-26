//! NVIDIA GPU fans through NVML (`nvmlDeviceSetFanSpeed_v2` / `nvmlDeviceSetDefaultFanSpeed_v2`,
//! driver ≥ 520), for cards with their own fans (the Quadro M4000 here). `nvidia-settings`
//! needs X and is not used.
//!
//! The NVML calls are behind [`NvmlFans`]: the concrete adapter lives with the NVML wrapper in
//! `orca-gpuctl` (SUPERVISOR.md 2.2), so this crate stays free of FFI (`unsafe` is forbidden
//! workspace-wide). Fail-safe: `SetDefaultFanSpeed_v2` hands the fan back to the GPU's own
//! curve (`on_exit = "auto"`, the default) or 100 % (`"full"`). The driver's thermal slowdown
//! and shutdown stay active whatever hostd does.

use std::sync::Mutex;

use async_trait::async_trait;

use super::{BackendError, Caps, ChannelReading, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, FanMode, OnExit};

/// The NVML fan calls for one GPU.
pub trait NvmlFans: Send + Sync {
    fn num_fans(&self) -> Result<u32, String>;
    /// Current target speed, percent.
    fn fan_speed(&self, fan: u32) -> Result<u32, String>;
    fn set_fan_speed(&self, fan: u32, pct: u32) -> Result<(), String>;
    fn set_default_fan_speed(&self, fan: u32) -> Result<(), String>;
}

pub struct Nvml {
    id: String,
    gpu: Box<dyn NvmlFans>,
    writable: bool,
    on_exit: OnExit,
    touched: Mutex<Vec<u32>>,
}

impl Nvml {
    pub fn new(id: &str, gpu: Box<dyn NvmlFans>, writable: bool, on_exit: OnExit) -> Nvml {
        Nvml {
            id: id.into(),
            gpu,
            writable,
            on_exit,
            touched: Mutex::new(Vec::new()),
        }
    }

    fn fan(&self, ch: &Channel) -> Result<u32, BackendError> {
        let n = ch
            .index()
            .ok_or_else(|| BackendError::NoChannel(ch.to_string()))?;
        let max = self
            .gpu
            .num_fans()
            .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))?;
        if n >= max {
            return Err(BackendError::NoChannel(ch.to_string()));
        }
        Ok(n)
    }

    fn safe(&self, n: u32) -> Result<(), BackendError> {
        let r = match self.on_exit {
            OnExit::Auto => self.gpu.set_default_fan_speed(n),
            OnExit::Full => self.gpu.set_fan_speed(n, 100),
        };
        r.map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))
    }
}

#[async_trait]
impl FanBackend for Nvml {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Nvml
    }
    fn caps(&self) -> Caps {
        Caps {
            mode: FanMode::HostCurve,
            writable: self.writable,
            reads_rpm: false,
            failsafe: FailSafeOwner::Driver,
        }
    }

    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        if !self.writable {
            return Err(BackendError::ReadOnly);
        }
        let n = self.fan(ch)?;
        {
            let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
            if !t.contains(&n) {
                t.push(n);
            }
        }
        self.gpu
            .set_fan_speed(n, duty.clamp(0.0, 100.0).round() as u32)
            .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))
    }

    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        let n = self
            .gpu
            .num_fans()
            .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))?;
        Ok((0..n)
            .map(|i| ChannelReading {
                channel: i.to_string(),
                duty: self.gpu.fan_speed(i).ok().map(f64::from),
                ..Default::default()
            })
            .collect())
    }

    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        if !self.writable {
            return Ok(());
        }
        let n = self.fan(ch)?;
        self.safe(n)
    }

    async fn release(&self) -> Result<(), BackendError> {
        let t = std::mem::take(&mut *self.touched.lock().unwrap_or_else(|e| e.into_inner()));
        for n in t {
            self.safe(n)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeGpu {
        calls: Mutex<Vec<String>>,
    }
    impl NvmlFans for Arc<FakeGpu> {
        fn num_fans(&self) -> Result<u32, String> {
            Ok(1)
        }
        fn fan_speed(&self, _f: u32) -> Result<u32, String> {
            Ok(42)
        }
        fn set_fan_speed(&self, f: u32, p: u32) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("set {f} {p}"));
            Ok(())
        }
        fn set_default_fan_speed(&self, f: u32) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("default {f}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn set_then_default_on_exit() {
        let g = Arc::new(FakeGpu::default());
        let b = Nvml::new("cardD", Box::new(g.clone()), true, OnExit::Auto);
        b.set_duty(&Channel::Index(0), 55.4).await.unwrap();
        assert!(matches!(
            b.set_duty(&Channel::Index(1), 50.0).await,
            Err(BackendError::NoChannel(_))
        ));
        assert_eq!(b.read().await.unwrap()[0].duty, Some(42.0));
        b.release().await.unwrap();
        assert_eq!(*g.calls.lock().unwrap(), vec!["set 0 55", "default 0"]);
        // Left on auto (not writable): nothing is ever written.
        let g2 = Arc::new(FakeGpu::default());
        let ro = Nvml::new("cardD", Box::new(g2.clone()), false, OnExit::Auto);
        assert_eq!(
            ro.set_duty(&Channel::Index(0), 50.0).await,
            Err(BackendError::ReadOnly)
        );
        ro.fail_safe(&Channel::Index(0)).await.unwrap();
        assert!(g2.calls.lock().unwrap().is_empty());
    }
}
