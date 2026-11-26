//! `abhoth hostctl`: the host side of execution (ROUTER-CONFIG.md 6.2). Filled in with C3.

use std::time::Duration;

use serde_json::Value;

use super::exec::{Cancel, ExecError, ExecOutput};

pub async fn run_builtin(
    name: &str,
    _args: &Value,
    _stdin: Option<&[u8]>,
    _timeout: Duration,
    _cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    Err(ExecError::Refused(format!(
        "builtin `{name}` is not available yet"
    )))
}
