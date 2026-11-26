//! Job records in layoutctl's shape: `state/jobs/<id>.json` with timed steps, plus one line per
//! job in `history.jsonl` (the dash and the profiles shim read both).

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Seconds since the job started, one decimal.
    pub t: f64,
    pub msg: String,
}

/// A switch job. Field names and states are layoutctl's: `running`, `done`, `rolled-back`,
/// `failed`, `refused`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    pub layout: String,
    pub profile: Option<String>,
    pub requested_by: String,
    pub reason: String,
    pub state: String,
    pub created: String,
    pub started: String,
    pub steps: Vec<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_oom_kills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already: Option<bool>,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl JobRecord {
    pub fn new(layout: &str, requested_by: &str, reason: &str) -> Self {
        let id = format!("{}-{layout}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        let t = now_iso();
        JobRecord {
            id,
            layout: layout.to_string(),
            profile: None,
            requested_by: requested_by.to_string(),
            reason: reason.to_string(),
            state: "running".into(),
            created: t.clone(),
            started: t,
            steps: Vec::new(),
            from: None,
            estimate_seconds: None,
            ended: None,
            seconds: None,
            new_oom_kills: None,
            live: None,
            error: None,
            rollback_error: None,
            already: None,
        }
    }

    /// The `history.jsonl` line (layoutctl's key set).
    pub fn history_line(&self) -> Value {
        json!({"id": self.id, "layout": self.layout, "from": self.from, "profile": self.profile,
            "requestedBy": self.requested_by, "state": self.state, "seconds": self.seconds,
            "estimateSeconds": self.estimate_seconds, "error": self.error,
            "started": self.started, "ended": self.ended})
    }
}

/// Where job records go.
pub trait JobSink: Send + Sync {
    fn save(&self, job: &JobRecord);
    fn finish(&self, job: &JobRecord);
}

/// `<dir>/jobs/<id>.json` and `<dir>/history.jsonl`, written atomically per file.
pub struct FileJobSink {
    pub dir: PathBuf,
}

impl JobSink for FileJobSink {
    fn save(&self, job: &JobRecord) {
        let d = self.dir.join("jobs");
        let _ = std::fs::create_dir_all(&d);
        let p = d.join(format!("{}.json", job.id));
        let tmp = p.with_extension("json.tmp");
        if let Ok(t) = serde_json::to_string_pretty(job)
            && std::fs::write(&tmp, t).is_ok()
        {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    fn finish(&self, job: &JobRecord) {
        self.save(job);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("history.jsonl"))
        {
            let _ = writeln!(f, "{}", job.history_line());
        }
    }
}

/// In memory (tests, and the jobs API's recent list).
#[derive(Default)]
pub struct MemJobSink {
    pub saves: Mutex<u64>,
    pub done: Mutex<Vec<JobRecord>>,
}

impl JobSink for MemJobSink {
    fn save(&self, _job: &JobRecord) {
        *self.saves.lock().unwrap() += 1;
    }
    fn finish(&self, job: &JobRecord) {
        self.done.lock().unwrap().push(job.clone());
    }
}
