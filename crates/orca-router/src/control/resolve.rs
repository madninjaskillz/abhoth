//! Resolving a (card, spec) into variables and commands (ROUTER-CONFIG.md 4.5). Filled in with C4.

use super::driver::Cmd;
use super::schema::{CardSpec, ControlConfig};

/// The start commands of `spec` on `card` (validation renders these for every layout card).
pub fn start_commands(
    _c: &ControlConfig,
    _card: &str,
    _spec: &CardSpec,
) -> Result<Vec<Cmd>, String> {
    Ok(Vec::new())
}
