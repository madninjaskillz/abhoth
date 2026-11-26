//! Engine drivers (ROUTER-CONFIG.md 4.6): how an engine instance is started, stopped, observed
//! and its logs read. A driver is **pure**: it turns a resolved instance into commands
//! ([`Cmd`]) and parses status output. The lifecycle manager runs the commands through an
//! [`super::exec::Executor`], so plans can show every command, the allowlist export can list
//! them, and tests can check them without a host.
//!
//! Every driver must report the running spec (`layouts.spec` label for docker/compose, a
//! sidecar file for systemd/process, the status template's JSON for `template`) so reconcile can
//! compare what runs with what the active layout wants.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::exec::{ExecOutput, ExecRequest, Op};
use super::schema::{CardSpec, DriverKind};
use super::template::Vars;

/// One command a driver wants run. `id` is the allowlist id: stable for a given text, so a
/// strict host can pin it (`docker-run:ninfer:A:3f2a9c1b`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Cmd {
    pub id: String,
    pub op: Op,
    pub root: bool,
    #[serde(with = "secs")]
    pub timeout: Duration,
    /// A failure of this command does not fail the step (`docker rm -f` of a missing container).
    #[serde(default)]
    pub allow_fail: bool,
}

mod secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

impl Cmd {
    /// A shell command; the id gets a short hash of the text appended so a strict allowlist pins
    /// exactly this text.
    pub fn shell(id_base: &str, text: String, root: bool, timeout: Duration) -> Self {
        let id = format!("{id_base}:{}", short_hash(&text));
        Self {
            id: id.clone(),
            op: Op::Command {
                id,
                text,
                params: BTreeMap::new(),
            },
            root,
            timeout,
            allow_fail: false,
        }
    }
    pub fn builtin(name: &str, args: Value, root: bool, timeout: Duration) -> Self {
        Self {
            id: name.into(),
            op: Op::Builtin {
                name: name.into(),
                args,
            },
            root,
            timeout,
            allow_fail: false,
        }
    }
    pub fn allow_fail(mut self) -> Self {
        self.allow_fail = true;
        self
    }
    pub fn request(&self, server: &str) -> ExecRequest {
        ExecRequest {
            server: server.into(),
            op: self.op.clone(),
            timeout: self.timeout,
            root: self.root,
            stdin: None,
        }
    }
    /// The command text (empty for builtins).
    pub fn text(&self) -> &str {
        match &self.op {
            Op::Command { text, .. } => text,
            Op::Builtin { .. } | Op::Hostd { .. } => "",
        }
    }
}

/// First 8 hex digits of SHA-256.
pub fn short_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(text.as_bytes());
    d.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Identifies an instance without needing its spec (stop, status, logs).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct InstanceRef {
    pub server: String,
    pub card: String,
    pub engine: String,
    /// Container name, compose container, systemd unit or pid-file stem, rendered.
    pub name: String,
    /// The base variables (card, card_lc, gpu_uuid, port, var:*, …).
    #[serde(skip)]
    pub vars: Vars,
}

/// Everything needed to start one instance: the 4.5 variables plus the rendered pieces.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Resolved {
    pub iref: InstanceRef,
    pub spec: CardSpec,
    /// The engine's own argv (prefix + rendered options + suffix), key flag included per
    /// `key_delivery` (argv mode shows the key masked as `{key}` until the executor fills it).
    pub args: Vec<String>,
    pub image: Option<String>,
    pub port: u16,
    pub served_id: String,
    pub lanes: u32,
    pub ctx: Option<u64>,
    /// `-e K=V` (sorted as layoutctl renders them).
    pub env: Vec<(String, String)>,
    /// `-v host:container[:ro]`.
    pub mounts: Vec<String>,
    /// `--label k=v` in order.
    pub labels: Vec<(String, String)>,
    /// Extra `docker run` words (cpuset, runtime, user, devices…), rendered.
    pub docker: Vec<String>,
    pub entrypoint: Option<String>,
    /// The `layouts.spec` label: sorted JSON of the card spec.
    pub spec_json: String,
    /// Model file on the host (for restore checks), when the engine loads a file.
    pub model_file: Option<String>,
}

/// What a status command found.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct Observed {
    /// The container/unit/process exists at all.
    pub exists: bool,
    pub running: bool,
    /// The spec label (or sidecar) parsed, when it is a known card spec.
    pub spec: Option<CardSpec>,
    /// The raw label/sidecar value (unknown shapes are kept for display).
    pub spec_raw: Option<Value>,
    pub started_at: Option<String>,
    pub exit_code: Option<i32>,
    pub oom_killed: bool,
    /// Free text: status string, parse notes.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError(pub String);

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DriverError {}

impl From<super::template::TplError> for DriverError {
    fn from(e: super::template::TplError) -> Self {
        DriverError(e.to_string())
    }
}

/// See the module docs.
pub trait EngineDriver: Send + Sync {
    fn kind(&self) -> DriverKind;
    /// Commands that start the instance (after `prepare`/`before_start` hooks have run).
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError>;
    /// Commands that stop it (waiting for GPU memory is the lifecycle's job, not the driver's).
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError>;
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError>;
    fn parse_status(&self, i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError>;
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError>;
}
