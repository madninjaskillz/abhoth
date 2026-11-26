//! Windows backend: **design only** (SUPERVISOR.md 5.3, 15 Q16). Config validation refuses
//! `kind = "windows"` until it is built.
//!
//! Design:
//!
//! - **Reading:** LibreHardwareMonitor's web server (`http://127.0.0.1:8085/data.json`), a tree
//!   of hardware → sensors with `Text`, `Value` ("41.0 °C", "1200 RPM") and `SensorId`
//!   (`/gpu-nvidia/0/temperature/0`). Sources become `hwmon.lhm.<SensorId>`-style readings.
//! - **Writing:** FanControl (Rem0o) owns the curves. hostd writes the control temperature,
//!   as plain text with one decimal, into a file that FanControl's "File sensor" plugin reads
//!   (`sensor_file = 'C:\ProgramData\abhoth\fan-<id>.sensor'`), written atomically (temp file
//!   + rename). That makes it a **device-curve** backend: FanControl is the safety authority.
//! - **Fail-safe:** stop writing. FanControl's "sensor lost" handling (configure it to go to
//!   100 %) takes over when the file stops changing. On exit hostd deletes the file, which
//!   FanControl treats as a lost sensor at once.
//! - **NVML** on Windows: read-only (temperatures and power for the expressions).
//! - Shipped as a small `abhoth hostd` build for Windows that only does `windows` + `nvml`
//!   reads, run as a service.

use async_trait::async_trait;

use super::{BackendError, Caps, ChannelReading, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, FanMode};

pub struct WindowsDesignOnly {
    pub id: String,
}

#[async_trait]
impl FanBackend for WindowsDesignOnly {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Windows
    }
    fn caps(&self) -> Caps {
        Caps {
            mode: FanMode::DeviceCurve,
            writable: false,
            reads_rpm: false,
            failsafe: FailSafeOwner::Device,
        }
    }
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        Err(BackendError::Unsupported(
            "the windows backend (design only)",
        ))
    }
    async fn fail_safe(&self, _ch: &Channel) -> Result<(), BackendError> {
        Ok(())
    }
    async fn release(&self) -> Result<(), BackendError> {
        Ok(())
    }
}
