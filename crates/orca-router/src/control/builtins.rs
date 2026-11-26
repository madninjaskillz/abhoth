//! hostctl builtins (ROUTER-CONFIG.md 6.2): the safety-critical steps that are awkward in bash.
//! They run inside `abhoth hostctl` on the host (or in-process for a local server).

use std::time::Duration;

use serde_json::Value;

use super::exec::{Cancel, ExecError, ExecOutput};

/// Every builtin name hostctl knows.
pub const NAMES: &[&str] = &[
    "flock",
    "gpu-free-wait",
    "numa-room",
    "json-set",
    "restore-file",
    "oom-kills",
    "proc-scan",
    "secret-put",
    "docker-rm-robust",
    "telemetry",
    "compute-apps",
    "docker-up",
    "mounted",
    "gpus-visible",
    "file-exists",
    "read-json",
];

pub fn known(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Run a builtin. Filled in by the hostctl module.
pub async fn run(
    name: &str,
    args: &Value,
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    super::hostctl::run_builtin(name, args, stdin, timeout, cancel).await
}
