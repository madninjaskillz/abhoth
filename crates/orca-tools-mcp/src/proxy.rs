//! `McpProxyTool`: one MCP server tool as a [`Tool`], executed through the server's client.

use std::time::Duration;

use orca_abstractions::{
    Tool, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use crate::manager::{McpClient, McpToolRegistration};
use crate::mapper;

/// Default per-request timeout for MCP tool calls.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Registered as `mcp_{server}_{tool}`, so same-named tools on different servers never
/// collide and permission patterns can target one server.
pub struct McpProxyTool {
    name: String,
    description: String,
    server_name: String,
    tool: rmcp::model::Tool,
    client: Option<McpClient>,
    call_timeout: Duration,
}

impl McpProxyTool {
    pub fn new(registration: McpToolRegistration) -> Self {
        Self::build(
            registration.server_name,
            Some(registration.client),
            registration.tool,
            DEFAULT_CALL_TIMEOUT,
        )
    }

    pub fn with_timeout(registration: McpToolRegistration, call_timeout: Duration) -> Self {
        Self::build(
            registration.server_name,
            Some(registration.client),
            registration.tool,
            call_timeout,
        )
    }

    /// A proxy without a connection (tests of the name, description, risk and schema);
    /// executing it fails with an error result.
    pub fn detached(server_name: &str, tool: rmcp::model::Tool) -> Self {
        Self::build(server_name.to_string(), None, tool, DEFAULT_CALL_TIMEOUT)
    }

    fn build(
        server_name: String,
        client: Option<McpClient>,
        tool: rmcp::model::Tool,
        call_timeout: Duration,
    ) -> Self {
        Self {
            name: mapper::build_tool_name(&server_name, &tool.name),
            description: format!(
                "[MCP:{server_name}] {}",
                tool.description.as_deref().unwrap_or_default()
            ),
            server_name,
            tool,
            client,
            call_timeout,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[async_trait::async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn risk_level(&self) -> ToolRiskLevel {
        mapper::get_risk_level(self.tool.annotations.as_ref())
    }

    fn parameter_schema(&self) -> Value {
        Value::Object((*self.tool.input_schema).clone())
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f50c}",
            short_description: "Call MCP tool",
            summary_arg_keys: &[],
            aliases: &[],
        })
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let Some(client) = &self.client else {
            return ToolResult::error("MCP tool error: the server is not connected.");
        };
        let mut params = CallToolRequestParams::new(self.tool.name.clone());
        if let Some(arguments) = mapper::to_argument_map(args) {
            params = params.with_arguments(arguments);
        }
        match tokio::time::timeout(self.call_timeout, client.call_tool(params)).await {
            Err(_) => ToolResult::error_with(
                format!(
                    "MCP tool '{}' timed out after {:.0}s.",
                    self.name,
                    self.call_timeout.as_secs_f64()
                ),
                ToolErrorCode::Timeout,
            ),
            Ok(Err(e)) => ToolResult::error(format!("MCP tool error: {e}")),
            Ok(Ok(result)) => mapper::to_tool_result(&result),
        }
    }
}
