//! MCP support (port of `OpenOrca.Core/Mcp` and `OpenOrca.Tools.Mcp`): [`McpManager`] starts
//! the servers configured under `mcpServers` (stdio, through the official `rmcp` client) and
//! [`McpProxyTool`] exposes each server tool as `mcp_{server}_{tool}`.

pub mod manager;
pub mod mapper;
pub mod proxy;

use std::sync::Arc;

use orca_abstractions::Tool;

pub use manager::{McpClient, McpManager, McpToolRegistration};
pub use proxy::McpProxyTool;

/// One proxy tool per discovered server tool, in discovery order.
pub fn proxy_tools(registrations: Vec<McpToolRegistration>) -> Vec<Arc<dyn Tool>> {
    registrations
        .into_iter()
        .map(|r| Arc::new(McpProxyTool::new(r)) as Arc<dyn Tool>)
        .collect()
}
