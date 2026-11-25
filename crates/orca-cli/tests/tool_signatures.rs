//! Ported from `ToolSignatureFormatterTests.Format_AllRegisteredTools_ProduceNonEmptySingleLineSignatures`.

use orca_core::agent::prompts::format_signature;
use orca_core::config::OrcaConfig;

#[test]
fn format_all_registered_tools_produce_non_empty_single_line_signatures() {
    // Every real tool schema must render without panicking, on one line.
    let registry = orca_cli::build_registry(&OrcaConfig::default());
    assert!(!registry.all().is_empty());
    for tool in registry.all() {
        let sig = format_signature(
            tool.name(),
            Some(tool.description()),
            &tool.parameter_schema(),
        );
        assert!(sig.starts_with(&format!("{}(", tool.name())), "{sig}");
        assert!(!sig.contains('\n'), "{sig}");
    }
}
