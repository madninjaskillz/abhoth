//! Ported from `McpManagerTests`, plus end-to-end calls against the fake stdio server
//! (`orca-testkit`'s `fake-mcp-server`).

use std::time::Duration;

use indexmap::IndexMap;
use orca_abstractions::{Tool, ToolContext, ToolErrorCode, ToolResultPart, ToolRiskLevel};
use orca_core::config::McpServerConfig;
use orca_tools_mcp::{McpManager, McpProxyTool, proxy_tools};
use serde_json::json;

fn fake_server(args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        command: orca_testkit::fake_bin("fake-mcp-server")
            .display()
            .to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        ..McpServerConfig::default()
    }
}

fn servers(list: Vec<(&str, McpServerConfig)>) -> IndexMap<String, McpServerConfig> {
    list.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn ctx() -> ToolContext {
    ToolContext::new(std::env::temp_dir())
}

#[tokio::test]
async fn initialize_async_empty_config_returns_no_tools() {
    let mut manager = McpManager::new();
    let tools = manager
        .initialize(&IndexMap::new(), &std::env::temp_dir())
        .await;
    assert!(tools.is_empty());
    assert_eq!(manager.clients(), 0);
}

#[tokio::test]
async fn initialize_async_disabled_server_skipped() {
    let mut manager = McpManager::new();
    let config = McpServerConfig {
        command: "nonexistent_command_12345".into(),
        enabled: false,
        ..McpServerConfig::default()
    };
    let tools = manager
        .initialize(&servers(vec![("test", config)]), &std::env::temp_dir())
        .await;
    assert!(tools.is_empty());
    assert_eq!(manager.clients(), 0);
}

#[tokio::test]
async fn initialize_async_invalid_command_handles_gracefully() {
    let mut manager = McpManager::new();
    let config = McpServerConfig {
        command: "this_command_does_not_exist_xyz_123".into(),
        ..McpServerConfig::default()
    };
    // Should not panic: failures are logged and skipped.
    let tools = manager
        .initialize(&servers(vec![("test", config)]), &std::env::temp_dir())
        .await;
    assert!(tools.is_empty());
    manager.dispose().await;
}

#[tokio::test]
async fn dispose_async_empty_manager_no_error() {
    McpManager::new().dispose().await;
}

#[tokio::test]
async fn dispose_async_after_init_clears_clients() {
    let mut manager = McpManager::new();
    manager
        .initialize(
            &servers(vec![("fake", fake_server(&[]))]),
            &std::env::temp_dir(),
        )
        .await;
    assert_eq!(manager.clients(), 1);
    manager.dispose().await;
    assert_eq!(manager.clients(), 0);
}

#[tokio::test]
async fn fake_server_tools_are_listed_and_called() {
    let mut manager = McpManager::new();
    let mut env_server = fake_server(&[]);
    env_server
        .env
        .insert("FAKE_MCP_VAR".into(), "hello env".into());
    let regs = manager
        .initialize(
            &servers(vec![
                ("Fake", env_server),
                ("broken", fake_server(&["--exit"])),
            ]),
            &std::env::temp_dir(),
        )
        .await;
    // The broken server is skipped; the other one's tools come in its order.
    let tools = proxy_tools(regs);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "mcp_fake_echo",
            "mcp_fake_add",
            "mcp_fake_fail",
            "mcp_fake_image",
            "mcp_fake_mixed",
            "mcp_fake_env",
            "mcp_fake_slow"
        ]
    );
    let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().clone();

    let echo = by_name("mcp_fake_echo");
    assert_eq!(echo.risk_level(), ToolRiskLevel::ReadOnly);
    assert_eq!(echo.description(), "[MCP:Fake] Echoes the text back");
    let r = echo.execute(&json!({"text": "hi"}), &ctx()).await;
    assert!(!r.is_error);
    assert_eq!(r.content, "echo: hi");

    let r = by_name("mcp_fake_add")
        .execute(&json!({"a": 2, "b": 3.5}), &ctx())
        .await;
    assert_eq!(r.content, "5.5");

    let r = by_name("mcp_fake_fail").execute(&json!({}), &ctx()).await;
    assert!(r.is_error);
    assert_eq!(r.error_code, ToolErrorCode::ExecutionFailed);
    assert_eq!(r.content, "the fake tool failed");

    let r = by_name("mcp_fake_image").execute(&json!({}), &ctx()).await;
    assert_eq!(r.content, "an image:");
    let parts = r.parts.unwrap();
    assert_eq!(
        parts[1],
        ToolResultPart::Image {
            data: vec![0x89, b'P', b'N', b'G'],
            mime_type: "image/png".into()
        }
    );

    let r = by_name("mcp_fake_env").execute(&json!({}), &ctx()).await;
    assert_eq!(r.content, "FAKE_MCP_VAR=hello env");

    manager.dispose().await;
    // After shutdown a call fails with an error result instead of hanging.
    let r = echo.execute(&json!({"text": "late"}), &ctx()).await;
    assert!(r.is_error);
    assert!(r.content.starts_with("MCP tool error: "), "{}", r.content);
}

#[tokio::test]
async fn slow_call_times_out() {
    let mut manager = McpManager::new();
    let regs = manager
        .initialize(
            &servers(vec![("fake", fake_server(&[]))]),
            &std::env::temp_dir(),
        )
        .await;
    let reg = regs
        .into_iter()
        .find(|r| r.tool.name == "slow")
        .expect("slow tool");
    let tool = McpProxyTool::with_timeout(reg, Duration::from_millis(200));
    let r = tool.execute(&json!({"ms": 3000}), &ctx()).await;
    assert!(r.is_error);
    assert_eq!(r.error_code, ToolErrorCode::Timeout);
    assert_eq!(r.content, "MCP tool 'mcp_fake_slow' timed out after 0s.");
    manager.dispose().await;
}
