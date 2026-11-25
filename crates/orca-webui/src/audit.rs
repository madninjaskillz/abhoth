//! `AuditLog`: append-only JSON lines (sign-ins, prompts, terminal opens, commits, file writes,
//! projects added or removed, clones). No secrets are ever written.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

/// Who made a request, as the audit log records it.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub ip: Option<String>,
    pub user: Option<String>,
}

pub struct AuditLog {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: Option<PathBuf>) -> Self {
        if let Some(dir) = path.as_deref().and_then(Path::parent)
            && !dir.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// One line: time, event, ip, user, then `fields` (an object) in order.
    pub fn write(&self, caller: Option<&Caller>, event: &str, fields: Value) {
        let Some(path) = &self.path else { return };
        let mut entry = Map::new();
        entry.insert("time".into(), Value::String(crate::util::utc_now_iso()));
        entry.insert("event".into(), Value::String(event.into()));
        entry.insert(
            "ip".into(),
            caller
                .and_then(|c| c.ip.clone())
                .map_or(Value::Null, Value::String),
        );
        entry.insert(
            "user".into(),
            caller
                .and_then(|c| c.user.clone())
                .map_or(Value::Null, Value::String),
        );
        if let Value::Object(f) = fields {
            for (k, v) in f {
                entry.insert(k, v);
            }
        }
        let line = Value::Object(entry).to_string();
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // The audit log must not take a request down.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The first `max` characters (UTF-16 units), for prompts in the log.
    pub fn clip(text: &str, max: usize) -> String {
        let units: Vec<u16> = text.encode_utf16().collect();
        if units.len() <= max {
            text.to_string()
        } else {
            format!("{}…", String::from_utf16_lossy(&units[..max]))
        }
    }
}
