//! `ToolExposure.FilterAdvertised`: which registered tools are advertised to the model.
//! Everything stays executable; this trims the per-request schema payload to a core set.

use crate::config::OrcaConfig;

/// The everyday set: file I/O, search, shell, core git, orchestration, interaction.
/// Excluded by default: LSP/MCP (advertised when their configs enable them), image generation
/// (config-gated), archive/network/env utilities, redundant file aliases (create_directory,
/// rename_file, copy_file, cd, apply_patch) and the git remotes (push/pull/stash); all
/// reachable through `tools.extraTools` or `tools.extendedTools`.
pub const CORE_TOOLS: &[&str] = &[
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

/// `PiLeadModeTool.ToolName`: registered only in `agent.leadMode` "auto", then advertised.
const LEAD_MODE_TOOL: &str = "lead_mode";

/// Whether the tool `name` is advertised under `config` (the OpenOrca profile; the Pi
/// profile's own list is not ported yet, so it advertises everything).
pub fn is_advertised(name: &str, config: &OrcaConfig) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_tools_advertised_and_aliases_hidden() {
        let mut config = OrcaConfig::default();
        assert!(is_advertised("read_file", &config));
        assert!(is_advertised("TASK_LIST", &config));
        assert!(!is_advertised("cd", &config));
        assert!(!is_advertised("apply_patch", &config));
        assert!(!is_advertised("archive", &config));
        assert!(is_advertised("mcp_server_tool", &config));
        assert!(!is_advertised("lsp_hover", &config));
        config.tools.extra_tools = vec!["Apply_Patch".into()];
        assert!(is_advertised("apply_patch", &config));
        config.tools.extended_tools = true;
        assert!(is_advertised("archive", &config));
    }
}
