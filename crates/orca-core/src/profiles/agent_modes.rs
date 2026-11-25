//! How the main agent works, chosen separately from the inference profile (`AgentModes`):
//! "auto", "hands-on", "orchestrator" (agent.leadMode) or "peer" (agent.peerMode, which
//! overrides the lead mode). A profile's own mode is its default; a project's choice replaces it.
//! Bridge profiles (agent.engine claude / codex) have no mode.

use serde_json::{Value, json};

use super::catalog::ProfileDefinition;

pub const LEAD_MODE_AUTO: &str = "auto";
pub const LEAD_MODE_HANDS_ON: &str = "hands-on";
pub const LEAD_MODE_ORCHESTRATOR: &str = "orchestrator";
pub const PEER: &str = "peer";

/// Every mode, in picker order.
pub const ALL: [&str; 4] = [
    LEAD_MODE_AUTO,
    LEAD_MODE_HANDS_ON,
    LEAD_MODE_ORCHESTRATOR,
    PEER,
];

/// The canonical spelling of a mode name, or `None` when it isn't one.
pub fn normalize(mode: Option<&str>) -> Option<&'static str> {
    let mode = mode?.trim();
    if mode.is_empty() {
        return None;
    }
    ALL.into_iter().find(|m| m.eq_ignore_ascii_case(mode))
}

/// "Unknown mode 'x' (one of auto, hands-on, orchestrator, peer)."
pub fn unknown_mode_message(mode: &str) -> String {
    format!("Unknown mode '{mode}' (one of {}).", ALL.join(", "))
}

/// The config paths a mode sets: peer -> agent.peerMode=true; otherwise the lead mode and
/// peerMode=false. Errors with [`unknown_mode_message`] for an unknown mode.
pub fn overrides(mode: &str) -> Result<Vec<(String, Value)>, String> {
    let m = normalize(Some(mode)).ok_or_else(|| unknown_mode_message(mode))?;
    Ok(if m == PEER {
        vec![("agent.peerMode".into(), json!(true))]
    } else {
        vec![
            ("agent.leadMode".into(), json!(m)),
            ("agent.peerMode".into(), json!(false)),
        ]
    })
}

/// The bench arm settings ("a.b=value") of a mode.
pub fn bench_sets(mode: &str) -> Vec<String> {
    if normalize(Some(mode)) == Some(PEER) {
        vec!["agent.peerMode=true".into()]
    } else {
        vec!["agent.peerMode=false".into()]
    }
}

/// A profile's built-in mode (its own `mode`, else its client agent.peerMode / agent.leadMode),
/// "auto" when it sets none.
pub fn default_of(profile: Option<&ProfileDefinition>) -> &'static str {
    if let Some(own) = normalize(profile.and_then(|p| p.mode.as_deref())) {
        return own;
    }
    let Some(client) = profile.map(|p| &p.client) else {
        return LEAD_MODE_AUTO;
    };
    if client.get("agent.peerMode").and_then(Value::as_bool) == Some(true) {
        return PEER;
    }
    if let Some(m) = normalize(client.get("agent.leadMode").and_then(Value::as_str))
        && m != PEER
    {
        return m;
    }
    LEAD_MODE_AUTO
}

/// A bridge profile (its turns run on another CLI): no mode applies.
pub fn is_bridge(profile: Option<&ProfileDefinition>) -> bool {
    profile
        .and_then(|p| p.client.get("agent.engine"))
        .and_then(Value::as_str)
        .is_some_and(|e| e != "native" && e != "pi")
}

/// The mode of a results.jsonl row: peer when its sets turned peer mode on, else its lead mode.
pub fn of_result(lead_mode: Option<&str>, sets: &[&str]) -> &'static str {
    if sets
        .iter()
        .any(|s| s.replace(' ', "").eq_ignore_ascii_case("agent.peerMode=true"))
    {
        return PEER;
    }
    normalize(lead_mode).unwrap_or(LEAD_MODE_AUTO)
}
