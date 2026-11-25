//! `ToolExposure`: which registered tools are advertised to the model. Everything stays
//! executable; this trims the per-request schema payload to a core set.

use orca_core::config::OrcaConfig;

/// The everyday set: file I/O, search, shell, core git, orchestration, interaction. Excluded by
/// default: LSP/MCP (advertised automatically when their configs enable them), image
/// generation (config-gated), archive/network/env utilities, redundant file aliases and rarely
/// model-driven git remotes, all reachable via `tools.extraTools` or `tools.extendedTools`.
const CORE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "multi_edit",
    "delete_file",
    "move_file",
    "mkdir",
    "glob",
    "grep",
    "list_directory",
    "bash",
    "start_background_process",
    "get_process_output",
    "stop_process",
    "list_processes",
    "git_status",
    "git_diff",
    "git_log",
    "git_commit",
    "git_branch",
    "git_checkout",
    "web_fetch",
    "web_search",
    "github",
    "think",
    "task_list",
    "print",
    "ask_user",
    "spawn_agent",
    "spawn_agents",
    "report_progress",
];

/// `PiLeadModeTool.ToolName`: registered only in `agent.leadMode` "auto".
const LEAD_MODE_TOOL: &str = "lead_mode";

/// `FilterAdvertised` for one tool name. The Pi profile's own list is not ported yet (its
/// tools are not either), so under it every tool is advertised.
pub fn is_advertised(config: &OrcaConfig, name: &str) -> bool {
    if config.tool_profile.eq_ignore_ascii_case("pi") || config.tools.extended_tools {
        return true;
    }
    if CORE_TOOLS.iter().any(|n| n.eq_ignore_ascii_case(name))
        || config
            .tools
            .extra_tools
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
    {
        return true;
    }
    if name == LEAD_MODE_TOOL {
        return true;
    }
    // Opt-in categories advertise themselves when their configs are enabled.
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("mcp_") {
        return true;
    }
    if lower.starts_with("lsp_") {
        return config.lsp.enabled;
    }
    if matches!(
        name,
        "generate_image" | "edit_image" | "resize_image" | "crop_image"
    ) {
        return config.image_gen.enabled;
    }
    false
}
