//! Ported from `ToolSignatureFormatterTests.Format_AllRegisteredTools_ProduceNonEmptySingleLineSignatures`.

use orca_core::agent::prompts::format_signature;
use orca_core::config::OrcaConfig;

#[test]
fn format_all_registered_tools_produce_non_empty_single_line_signatures() {
    // Every real tool schema must render without panicking, on one line.
    let registry = orca_cli::build_registry(&OrcaConfig::default());
    assert!(!registry.all().is_empty());
    // The C# test covers the file, shell and utility tools; spawn_agent's description has a
    // line break before its first ". ", so its text-mode signature spans lines there too.
    for tool in registry
        .all()
        .iter()
        .filter(|t| !orca_core::orchestration::activity::is_agent_tool(t.name()))
    {
        let sig = format_signature(
            tool.name(),
            Some(tool.description()),
            &tool.parameter_schema(),
        );
        assert!(sig.starts_with(&format!("{}(", tool.name())), "{sig}");
        assert!(!sig.contains('\n'), "{sig}");
    }
}

/// The registry keeps the C# order (plugin DLLs by file name, tool classes by name), and the
/// advertised subset is the C# default list minus the tools not ported yet.
#[test]
fn registry_order_matches_csharp_plugin_order() {
    let registry = orca_cli::build_registry(&OrcaConfig::default());
    let names = registry.names();
    let expected = [
        "report_progress",
        "spawn_agent",
        "spawn_agents",
        "archive",
        "apply_patch",
        "cd",
        "copy_file",
        "create_directory",
        "delete_file",
        "edit_file",
        "glob",
        "grep",
        "list_directory",
        "mkdir",
        "move_file",
        "multi_edit",
        "read_file",
        "rename_file",
        "write_file",
        "git_branch",
        "git_checkout",
        "git_commit",
        "git_diff",
        "git_log",
        "git_pull",
        "git_push",
        "git_stash",
        "git_status",
        "github",
        "crop_image",
        "edit_image",
        "generate_image",
        "resize_image",
        "ask_user",
        "lsp_call_hierarchy",
        "lsp_diagnostics",
        "lsp_document_symbols",
        "lsp_find_references",
        "lsp_go_to_definition",
        "lsp_hover",
        "lsp_workspace_symbols",
        "network_diagnostics",
        "bash",
        "get_process_output",
        "list_processes",
        "start_background_process",
        "stop_process",
        "env",
        "print",
        "task_list",
        "think",
        "web_fetch",
        "web_search",
    ];
    assert_eq!(names, expected);
    let config = OrcaConfig::default();
    let advertised: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|n| orca_core::agent::tool_exposure::is_advertised(n, &config))
        .collect();
    assert_eq!(
        advertised,
        [
            "report_progress",
            "spawn_agent",
            "spawn_agents",
            "delete_file",
            "edit_file",
            "glob",
            "grep",
            "list_directory",
            "mkdir",
            "move_file",
            "multi_edit",
            "read_file",
            "write_file",
            "git_branch",
            "git_checkout",
            "git_commit",
            "git_diff",
            "git_log",
            "git_status",
            "github",
            "ask_user",
            "bash",
            "get_process_output",
            "list_processes",
            "start_background_process",
            "stop_process",
            "print",
            "task_list",
            "think",
            "web_fetch",
            "web_search",
        ]
    );
}
