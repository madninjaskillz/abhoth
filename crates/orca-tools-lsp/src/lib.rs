//! LSP tools (port of `OpenOrca.Tools.Lsp`): `lsp_diagnostics`, `lsp_hover`,
//! `lsp_go_to_definition`, `lsp_find_references`, `lsp_document_symbols`,
//! `lsp_workspace_symbols` and `lsp_call_hierarchy`, over [`LspClientManager`].

pub mod client;
pub mod protocol;
pub mod tools;

use std::sync::Arc;

use orca_abstractions::Tool;

pub use client::{LspClientManager, LspServerConfig};
pub use tools::*;

/// The LSP tools, in the C# registration order. `manager` is `None` when LSP is off (the tools
/// then answer "LSP is not configured."). `lsp_workspace_symbols` never gets the manager: the
/// C# tool does not declare `IConfigurable<LspClientManager>`, so the host never configures
/// it, and it always answers "LSP is not configured." (kept for parity).
pub fn all_tools(manager: Option<Arc<LspClientManager>>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CallHierarchyTool::new(manager.clone())),
        Arc::new(DiagnosticsTool::new(manager.clone())),
        Arc::new(DocumentSymbolsTool::new(manager.clone())),
        Arc::new(FindReferencesTool::new(manager.clone())),
        Arc::new(GoToDefinitionTool::new(manager.clone())),
        Arc::new(HoverTool::new(manager)),
        Arc::new(WorkspaceSymbolsTool::new(None)),
    ]
}
