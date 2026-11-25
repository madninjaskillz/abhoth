//! Tool execution policies (`IToolExecutionPolicy`): checks that refuse a call before it runs.
//! Evaluated in registration order: input size (native calls only), plan mode, sandbox mode,
//! then the orchestrator-Lead restriction (`LeadModePolicy`, its static part).
//!
//! A refusal is the call's result as is (no `ERROR: ` prefix), is not recorded in the tool
//! history and does not count towards retry detection, as in C#.

use orca_abstractions::ToolRiskLevel;

use super::registry::ToolRegistry;
use crate::config::OrcaConfig;

/// `CliConstants.ToolArgsMaxChars`.
pub const TOOL_ARGS_MAX_CHARS: usize = 1_000_000;

/// `ToolBlockResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    pub message: String,
    pub is_error: bool,
    /// The blocked tool's risk level (plan / sandbox refusals).
    pub risk_level: Option<String>,
}

fn risk_name(r: ToolRiskLevel) -> &'static str {
    match r {
        ToolRiskLevel::ReadOnly => "ReadOnly",
        ToolRiskLevel::Moderate => "Moderate",
        ToolRiskLevel::Dangerous => "Dangerous",
    }
}

/// `InputSizePolicy`: arguments over the limit (UTF-16 units) are refused.
pub fn check_input_size(args_json: &str) -> Option<ToolBlock> {
    let len = args_json.encode_utf16().count();
    (len > TOOL_ARGS_MAX_CHARS).then(|| ToolBlock {
        message: format!(
            "Error: Tool arguments exceed maximum size ({len} chars, limit {TOOL_ARGS_MAX_CHARS})."
        ),
        is_error: true,
        risk_level: None,
    })
}

/// `PlanModePolicy`: only read-only tools run in plan mode. (Read-only sub-agent spawns are
/// allowed in C#; that exception arrives with the agent tools.)
pub fn check_plan_mode(
    plan_mode: bool,
    registry: &ToolRegistry,
    tool_name: &str,
) -> Option<ToolBlock> {
    if !plan_mode {
        return None;
    }
    let Some(tool) = registry.resolve(tool_name) else {
        return Some(ToolBlock {
            message: format!("PLAN MODE: Unknown tool '{tool_name}' is blocked in plan mode."),
            is_error: true,
            risk_level: None,
        });
    };
    if tool.risk_level() == ToolRiskLevel::ReadOnly {
        return None;
    }
    let risk = risk_name(tool.risk_level());
    Some(ToolBlock {
        message: format!(
            "PLAN MODE: Tool '{tool_name}' is blocked in plan mode (risk: {risk}). Describe what you would do with this tool in your plan text instead."
        ),
        is_error: true,
        risk_level: Some(risk.into()),
    })
}

/// `SandboxModePolicy`: only read-only tools run in sandbox mode.
pub fn check_sandbox_mode(
    config: &OrcaConfig,
    registry: &ToolRegistry,
    tool_name: &str,
) -> Option<ToolBlock> {
    if !config.sandbox_mode {
        return None;
    }
    let Some(tool) = registry.resolve(tool_name) else {
        return Some(ToolBlock {
            message: format!(
                "SANDBOX MODE: Unknown tool '{tool_name}' is blocked in sandbox mode."
            ),
            is_error: true,
            risk_level: None,
        });
    };
    if tool.risk_level() == ToolRiskLevel::ReadOnly {
        return None;
    }
    let risk = risk_name(tool.risk_level());
    Some(ToolBlock {
        message: format!(
            "SANDBOX MODE: Tool '{tool_name}' is blocked in sandbox mode (risk: {risk}). Only read-only tools are available."
        ),
        is_error: true,
        risk_level: Some(risk.into()),
    })
}

/// `LeadModePolicy.WriteTools`.
pub const LEAD_WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "write_file",
    "edit_file",
    "multi_edit",
    "delete_file",
    "move_file",
    "copy_file",
    "rename_file",
    "mkdir",
    "create_directory",
    "apply_patch",
    "git_commit",
    "git_checkout",
    "git_stash",
    "git_push",
    "git_pull",
    "git_branch",
    "generate_image",
    "edit_image",
    "resize_image",
    "crop_image",
];

/// `LeadModePolicy.ShellTools`.
pub const LEAD_SHELL_TOOLS: &[&str] = &[
    "bash",
    "start_background_process",
    "get_process_output",
    "stop_process",
    "list_processes",
    "job",
];

/// `AgentConfig.IsOrchestratorLead`: `agent.leadMode` "orchestrator" (never in peer mode).
pub fn is_orchestrator_lead(config: &OrcaConfig) -> bool {
    !config.agent.peer_mode
        && config
            .agent
            .lead_mode
            .trim()
            .eq_ignore_ascii_case("orchestrator")
}

fn contains_ignore_case(set: &[&str], name: &str) -> bool {
    set.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// `LeadModePolicy.IsDenied`: the orchestrator Lead may not change files (nor use the shell
/// unless `agent.leadBash`).
pub fn is_denied_for_lead(config: &OrcaConfig, tool_name: &str) -> bool {
    is_orchestrator_lead(config)
        && (contains_ignore_case(LEAD_WRITE_TOOLS, tool_name)
            || (!config.agent.lead_bash && contains_ignore_case(LEAD_SHELL_TOOLS, tool_name)))
}

/// `LeadModePolicy.CheckBlocked`, orchestrator part. (The per-request auto lead mode arrives
/// with the agent tools.)
pub fn check_lead_mode(config: &OrcaConfig, tool_name: &str) -> Option<ToolBlock> {
    is_denied_for_lead(config, tool_name).then(|| ToolBlock {
        message: format!(
            "You are the orchestrator: you cannot use {tool_name}. Every change to files, even a one-line fix, goes to a worker through the subagent tool (send fix-ups to the agent that wrote the code with a follow-up)."
        ),
        is_error: true,
        risk_level: None,
    })
}

/// All policies in C# registration order. `native` enables the input size check.
pub fn check_blocked(
    config: &OrcaConfig,
    registry: &ToolRegistry,
    plan_mode: bool,
    native: bool,
    tool_name: &str,
    args_json: &str,
) -> Option<ToolBlock> {
    if native && let Some(b) = check_input_size(args_json) {
        return Some(b);
    }
    check_plan_mode(plan_mode, registry, tool_name)
        .or_else(|| check_sandbox_mode(config, registry, tool_name))
        .or_else(|| check_lead_mode(config, tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_size_policy_blocks_oversized_args() {
        assert!(check_input_size("{}").is_none());
        let big = "x".repeat(TOOL_ARGS_MAX_CHARS + 1);
        let b = check_input_size(&big).unwrap();
        assert_eq!(
            b.message,
            format!(
                "Error: Tool arguments exceed maximum size ({} chars, limit 1000000).",
                TOOL_ARGS_MAX_CHARS + 1
            )
        );
        assert!(b.is_error);
    }

    #[test]
    fn plan_mode_off_allows_everything() {
        let r = ToolRegistry::new();
        assert!(check_plan_mode(false, &r, "anything").is_none());
    }

    #[test]
    fn plan_mode_blocks_unknown_tools() {
        let r = ToolRegistry::new();
        let b = check_plan_mode(true, &r, "nope").unwrap();
        assert_eq!(
            b.message,
            "PLAN MODE: Unknown tool 'nope' is blocked in plan mode."
        );
        assert_eq!(b.risk_level, None);
    }

    #[test]
    fn sandbox_off_allows_and_on_blocks_unknown() {
        let r = ToolRegistry::new();
        let mut c = OrcaConfig::default();
        assert!(check_sandbox_mode(&c, &r, "x").is_none());
        c.sandbox_mode = true;
        assert_eq!(
            check_sandbox_mode(&c, &r, "x").unwrap().message,
            "SANDBOX MODE: Unknown tool 'x' is blocked in sandbox mode."
        );
    }

    #[test]
    fn orchestrator_lead_cannot_write_and_shell_follows_lead_bash() {
        let mut c = OrcaConfig::default();
        assert!(!is_denied_for_lead(&c, "write_file"));
        c.agent.lead_mode = "Orchestrator".into();
        assert!(is_denied_for_lead(&c, "WRITE_FILE"));
        assert!(!is_denied_for_lead(&c, "bash"));
        c.agent.lead_bash = false;
        assert!(is_denied_for_lead(&c, "bash"));
        assert!(!is_denied_for_lead(&c, "read_file"));
        c.agent.peer_mode = true;
        assert!(!is_denied_for_lead(&c, "write_file"));
    }
}
