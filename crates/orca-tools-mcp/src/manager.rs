//! `McpManager`: starts the configured MCP servers (stdio transport, as the C# SDK client),
//! lists their tools and shuts them down again.

use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;
use orca_core::config::McpServerConfig;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::serve_client;
use tokio::io::{AsyncBufReadExt, BufReader};

/// The C# SDK's default initialization timeout.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long shutdown waits for one server before giving up on it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected server's client handle.
pub type McpClient = Peer<RoleClient>;

/// A tool discovered on a connected MCP server: the config key of the server, the client to
/// call it through, and the protocol tool definition (name, description, schema, annotations).
#[derive(Clone)]
pub struct McpToolRegistration {
    pub server_name: String,
    pub client: McpClient,
    pub tool: Tool,
}

/// Owns the running MCP connections (`McpManager`).
#[derive(Default)]
pub struct McpManager {
    clients: Vec<RunningService<RoleClient, ClientConfig>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The connected servers.
    pub fn clients(&self) -> usize {
        self.clients.len()
    }

    /// Start every enabled server and return its tools. A server that fails to start or to
    /// list its tools is logged and skipped; it never aborts startup.
    pub async fn initialize(
        &mut self,
        servers: &IndexMap<String, McpServerConfig>,
        cwd: &Path,
    ) -> Vec<McpToolRegistration> {
        let mut all = Vec::new();
        for (name, config) in servers {
            if !config.enabled {
                tracing::info!("MCP server '{name}' is disabled, skipping");
                continue;
            }
            match self.connect(name, config, cwd).await {
                Ok(tools) => {
                    tracing::info!("MCP server '{name}' connected: {} tools", tools.len());
                    all.extend(tools);
                }
                Err(e) => tracing::warn!("Failed to connect to MCP server '{name}': {e}"),
            }
        }
        all
    }

    async fn connect(
        &mut self,
        name: &str,
        config: &McpServerConfig,
        cwd: &Path,
    ) -> Result<Vec<McpToolRegistration>, String> {
        let mut cmd = build_command(&config.command, &config.args);
        cmd.envs(&config.env).current_dir(cwd);
        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        // Drain stderr so a chatty server can never fill the pipe and deadlock.
        if let Some(stderr) = stderr {
            let server = name.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("MCP {server} stderr: {line}");
                }
            });
        }
        let info = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("openorca", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE);
        let service = tokio::time::timeout(INITIALIZE_TIMEOUT, serve_client(info, transport))
            .await
            .map_err(|_| "initialization timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let peer = service.peer().clone();
        // Keep the connection even if listing fails, so it is shut down with the rest.
        self.clients.push(service);
        let tools = peer.list_all_tools().await.map_err(|e| e.to_string())?;
        Ok(tools
            .into_iter()
            .map(|tool| McpToolRegistration {
                server_name: name.to_string(),
                client: peer.clone(),
                tool,
            })
            .collect())
    }

    /// Close every connection (`DisposeAsync`): the server's stdin is closed and the process
    /// waited for, then killed.
    pub async fn dispose(&mut self) {
        for mut client in self.clients.drain(..) {
            if let Err(e) = client.close_with_timeout(SHUTDOWN_TIMEOUT).await {
                tracing::warn!("Error disposing MCP client: {e}");
            }
        }
    }
}

/// The server's command line. On Windows, as the C# SDK does, anything but `cmd.exe` runs
/// through `cmd.exe /c`, so `npx` and other `.cmd` shims resolve.
fn build_command(command: &str, args: &[String]) -> tokio::process::Command {
    let is_cmd = Path::new(command)
        .file_name()
        .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case("cmd.exe"));
    let mut cmd = if cfg!(windows) && !is_cmd {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/c").arg(command);
        c
    } else {
        tokio::process::Command::new(command)
    };
    cmd.args(args);
    // The cross-check and the CLI wait for this process's pipes: never leave a server behind.
    cmd.kill_on_drop(true);
    cmd
}
