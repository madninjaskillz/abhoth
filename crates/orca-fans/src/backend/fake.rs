//! In-memory backend for tests and `abhoth supervisor fans test`: records every write,
//! returns scripted readings, and can be told to fail.

use std::sync::Mutex;

use async_trait::async_trait;

use super::{BackendError, Caps, ChannelReading, FailSafeOwner, FanBackend, Write};
use crate::config::{BackendKind, Channel, FanMode};

pub struct Fake {
    pub id: String,
    pub mode: FanMode,
    pub writes: Mutex<Vec<Write>>,
    pub readings: Mutex<Vec<ChannelReading>>,
    pub fail: Mutex<bool>,
}

impl Fake {
    pub fn new(id: &str, mode: FanMode) -> Fake {
        Fake {
            id: id.into(),
            mode,
            writes: Mutex::new(Vec::new()),
            readings: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
        }
    }

    pub fn set_failing(&self, f: bool) {
        *self.fail.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    pub fn writes(&self) -> Vec<Write> {
        self.writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn rec(&self, w: Write) -> Result<(), BackendError> {
        if *self.fail.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(BackendError::Io(format!("{}: injected failure", self.id)));
        }
        self.writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(w);
        Ok(())
    }
}

#[async_trait]
impl FanBackend for Fake {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Fake
    }
    fn caps(&self) -> Caps {
        let failsafe = if self.mode == FanMode::DeviceCurve {
            FailSafeOwner::Device
        } else {
            FailSafeOwner::Host
        };
        Caps {
            mode: self.mode,
            writable: true,
            reads_rpm: true,
            failsafe,
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
        })
    }
    async fn set_duty(&self, ch: &Channel, duty: f64) -> Result<(), BackendError> {
        self.rec(Write::SetDuty {
            channel: ch.to_string(),
            duty,
        })
    }
    async fn set_setting(&self, entity: &str, value: f64) -> Result<(), BackendError> {
        self.rec(Write::SetSetting {
            entity: entity.into(),
            value,
        })
    }
    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        Ok(self
            .readings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone())
    }
    async fn fail_safe(&self, ch: &Channel) -> Result<(), BackendError> {
        self.rec(Write::FailSafe {
            channel: ch.to_string(),
        })
    }
    async fn release(&self) -> Result<(), BackendError> {
        self.rec(Write::Release)
    }
}
