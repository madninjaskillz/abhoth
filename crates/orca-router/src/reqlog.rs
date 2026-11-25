//! The JSONL request log (ROUTER.md SHOULD "Request log"): one line per request, no bodies.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use serde_json::Value;

#[derive(Default)]
pub struct RequestLog {
    inner: Mutex<Option<(String, File)>>,
}

impl RequestLog {
    pub fn open(path: Option<&str>) -> Self {
        let l = Self::default();
        l.reopen(path);
        l
    }

    /// Point the log at `path` (reload); same path keeps the open file.
    pub fn reopen(&self, path: Option<&str>) {
        let mut g = self.inner.lock().unwrap();
        match path.filter(|p| !p.is_empty()) {
            None => *g = None,
            Some(p) if g.as_ref().is_some_and(|(cur, _)| cur == p) => {}
            Some(p) => {
                if let Some(dir) = std::path::Path::new(p).parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                *g = match OpenOptions::new().create(true).append(true).open(p) {
                    Ok(f) => Some((p.to_string(), f)),
                    Err(e) => {
                        tracing::warn!("request log {p}: {e}");
                        None
                    }
                };
            }
        }
    }

    pub fn write(&self, line: &Value) {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, f)) = g.as_mut() {
            let mut s = line.to_string();
            s.push('\n');
            let _ = f.write_all(s.as_bytes());
        }
    }
}
