//! Records the runner keeps in the state dir: what is installed, what the admin approved, and
//! the handle of what is running (so a restarted router or bridge adopts it).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::MethodType;

/// What the admin approved for one install. Each item is an explicit click in the admin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Approvals {
    /// pip, uv, binary, git: code that runs on the host, not sandboxed.
    #[serde(default)]
    pub host_run: bool,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub host_network: bool,
    /// Approved host mount sources, as rendered (`/mnt/tank/sa3`).
    #[serde(default)]
    pub host_mounts: Vec<String>,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// The package comes from an untrusted store, and the admin installs it anyway.
    #[serde(default)]
    pub untrusted: bool,
    /// Who approved, for the audit (`admin:alice`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
}

impl Approvals {
    /// Everything approved (tests, and the CLI's `--yes`).
    pub fn all() -> Approvals {
        Approvals {
            host_run: true,
            privileged: true,
            host_network: true,
            host_mounts: vec!["*".into()],
            devices: vec!["*".into()],
            cap_add: vec!["*".into()],
            untrusted: true,
            by: String::new(),
        }
    }
    pub fn mount_ok(&self, source: &str) -> bool {
        self.host_mounts.iter().any(|m| m == "*" || m == source)
    }
    pub fn device_ok(&self, d: &str) -> bool {
        self.devices.iter().any(|m| m == "*" || m == d)
    }
    pub fn cap_ok(&self, c: &str) -> bool {
        self.cap_add.iter().any(|m| m == "*" || m == c)
    }
}

/// An installed package version on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    pub version: String,
    /// The index of the method used, in the manifest saved beside this record.
    pub method: usize,
    pub method_type: MethodType,
    /// `<state>/engines/<id>/<version>`.
    pub dir: PathBuf,
    /// The host OS it was installed for (`windows` decides `{{exe}}` and venv layout).
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venv: Option<PathBuf>,
    /// The unpacked binary dir, the git checkout, or (existing) the folder of the found program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<PathBuf>,
    /// existing: the found program itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
    /// The image it runs (docker, existing image), as pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// What was checked: `sha256:<hex> <url>`, `image <digest>`, `git <commit>`, `pip <line>`.
    #[serde(default)]
    pub pins: Vec<String>,
    /// The version an `existing` engine reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found_version: Option<String>,
    /// Unix seconds.
    pub at: i64,
    pub approvals: Approvals,
    /// The store it came from (`default`, a store URL id, or `my`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub store: String,
}

/// What is running, serialised so a restart can adopt it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Handle {
    Container {
        name: String,
    },
    Compose {
        project: String,
        file: PathBuf,
    },
    Process {
        pid: u32,
        /// The OS start time of the process (guards against PID reuse).
        started: u64,
        name: String,
    },
    /// An endpoint that runs outside the runner (`existing` with `detect.url`).
    External {
        url: String,
        name: String,
    },
}

impl Handle {
    pub fn name(&self) -> &str {
        match self {
            Handle::Container { name }
            | Handle::Process { name, .. }
            | Handle::External { name, .. } => name,
            Handle::Compose { project, .. } => project,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Starting,
    Running,
    Healthy,
    Unhealthy,
    /// Exited and waiting to be restarted.
    Backoff,
    Exited,
    /// Restarted too often; the runner gave up.
    Failed,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default)]
    pub restarts: u32,
    /// Unix seconds of the last start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl State {
    pub fn of(phase: Phase) -> State {
        State {
            phase,
            pid: None,
            restarts: 0,
            since: None,
            last_exit: None,
            notes: vec![],
        }
    }
}
