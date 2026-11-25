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
