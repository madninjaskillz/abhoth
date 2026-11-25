//! The `openorca` program. Phase 1: one-shot prompts (`--print` / `--prompt`); the
//! interactive REPL/TUI and the web host follow in later phases (see PORTING.md).

pub mod args;
pub mod sessions;
pub mod tool_exposure;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use orca_abstractions::{ToolContext, ToolResult};
use orca_core::agent::{AgentObserver, AgentSession, ToolCallRecord, ToolRegistry};
use orca_core::client::{ServedModels, ToolCall, client_from_config};
use orca_core::config::{ConfigManager, OrcaConfig};
use serde::Serialize;

pub use args::CliArgs;

/// `SinglePromptResult`: what `--print` writes to stdout.
#[derive(Debug, Serialize)]
pub struct SinglePromptResult {
    pub response: String,
    pub tokens: i64,
    pub duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_modified: Option<Vec<String>>,
    pub success: bool,
}

/// The compiled-in tool set (C# loads these from plugin assemblies): file and shell tools,
/// plus the optional groups behind Cargo features (`image`, `lsp`). MCP tools need the servers
/// started, see [`ToolServices::start_mcp`]. Language servers start in the current directory.
pub fn build_registry(config: &OrcaConfig) -> ToolRegistry {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    build_registry_in(config, &cwd).0
}

/// [`build_registry`] for a working folder, with the services its tools run on.
pub fn build_registry_in(config: &OrcaConfig, cwd: &Path) -> (ToolRegistry, ToolServices) {
    let mut r = ToolRegistry::new();
    #[allow(unused_mut)]
    let mut services = ToolServices::default();
    r.register_all(orca_tools_fs::all_tools());
    r.register_all(orca_tools_shell::all_tools(
        config.shell.idle_timeout_seconds,
    ));
    #[cfg(feature = "image")]
    r.register_all(orca_tools_image::all_tools(
        &config.image_gen,
        Some(Arc::new(std::sync::Mutex::new(
            config.agent.uncensored.clone(),
        ))),
    ));
    #[cfg(feature = "lsp")]
    {
        services.lsp = lsp_manager(config, cwd);
        r.register_all(orca_tools_lsp::all_tools(services.lsp.clone()));
    }
    #[cfg(not(feature = "lsp"))]
    let _ = cwd;
    // Disabled tools stay registered; the permission check refuses them (as in C#).
    let exposure = config.clone();
    r.set_advertise_filter(Arc::new(move |name: &str| {
        tool_exposure::is_advertised(&exposure, name)
    }));
    (r, services)
}

/// The LSP client manager when `lsp.enabled` and servers are configured.
#[cfg(feature = "lsp")]
fn lsp_manager(config: &OrcaConfig, cwd: &Path) -> Option<Arc<orca_tools_lsp::LspClientManager>> {
    if !config.lsp.enabled || config.lsp.servers.is_empty() {
        return None;
    }
    let servers = config
        .lsp
        .servers
        .iter()
        .map(|(lang, s)| {
            (
                lang.clone(),
                orca_tools_lsp::LspServerConfig {
                    command: s.command.clone(),
                    args: s.args.clone(),
                    root_uri: s.root_uri.clone(),
                },
            )
        })
        .collect();
    tracing::info!(
        "LSP enabled with {} server(s): {}",
        config.lsp.servers.len(),
        config
            .lsp
            .servers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some(Arc::new(orca_tools_lsp::LspClientManager::new(
        servers, cwd,
    )))
}

/// What the tools run on: MCP server connections and language servers. Shut down with
/// [`ToolServices::shutdown`] when the session ends.
#[derive(Default)]
pub struct ToolServices {
    #[cfg(feature = "mcp")]
    pub mcp: orca_tools_mcp::McpManager,
    #[cfg(feature = "lsp")]
    pub lsp: Option<Arc<orca_tools_lsp::LspClientManager>>,
}

impl ToolServices {
    /// Start the configured MCP servers and register their tools (`mcp_{server}_{tool}`).
    /// Servers that fail are logged and skipped.
    pub async fn start_mcp(
        &mut self,
        config: &OrcaConfig,
        cwd: &Path,
        registry: &mut ToolRegistry,
    ) {
        #[cfg(feature = "mcp")]
        if !config.mcp_servers.is_empty() {
            let tools = self.mcp.initialize(&config.mcp_servers, cwd).await;
            if !tools.is_empty() {
                tracing::info!("Registered {} MCP tools", tools.len());
            }
            registry.register_all(orca_tools_mcp::proxy_tools(tools));
        }
        #[cfg(not(feature = "mcp"))]
        let _ = (config, cwd, registry);
    }

    /// Close the MCP connections and stop the language servers.
    pub async fn shutdown(&mut self) {
        #[cfg(feature = "mcp")]
        self.mcp.dispose().await;
        #[cfg(feature = "lsp")]
        if let Some(lsp) = self.lsp.take() {
            lsp.dispose().await;
        }
    }
}

/// Prints streamed text to stderr when a human is watching (not in JSON mode).
struct ConsoleObserver {
    quiet: bool,
}

impl AgentObserver for ConsoleObserver {
    fn on_text(&mut self, text: &str) {
        if !self.quiet {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }
    fn on_tool_start(&mut self, call: &ToolCall) {
        if !self.quiet {
            eprintln!("\n> {} {}", call.name, call.arguments);
        }
    }
    fn on_tool_end(&mut self, _call: &ToolCall, result: &ToolResult) {
        if !self.quiet && result.is_error {
            eprintln!("  (error)");
        }
    }
}

/// Load the config from `config_dir` (default `OPENORCA_HOME` or `~/.openorca`).
pub fn load_config(config_dir: Option<&Path>) -> Result<OrcaConfig> {
    let mut m = ConfigManager::new(config_dir);
    m.load()
        .with_context(|| format!("loading {}", m.file_path().display()))?;
    Ok(m.config)
}

/// Run one prompt to the end and return the result (`RunSinglePromptJsonAsync`).
pub async fn run_single_prompt(
    config: OrcaConfig,
    cwd: PathBuf,
    prompt: &str,
    quiet: bool,
) -> Result<SinglePromptResult> {
    run_single_prompt_in_session(config, cwd, prompt, quiet, None, None).await
}

/// [`run_single_prompt`] with the CLI's session wiring: checkpoints and hooks from
/// `config_dir` (none when `None`), and a restored session (`-c` / `-r`) to continue.
pub async fn run_single_prompt_in_session(
    config: OrcaConfig,
    cwd: PathBuf,
    prompt: &str,
    quiet: bool,
    config_dir: Option<&Path>,
    restored: Option<sessions::RestoredSession>,
) -> Result<SinglePromptResult> {
    let served = Arc::new(ServedModels::new());
    let client = client_from_config(&config, served)?;
    let (mut registry, mut services) = build_registry_in(&config, &cwd);
    services.start_mcp(&config, &cwd, &mut registry).await;
    let tool_hooks = config_dir.map(|d| orca_core::agent::ToolHooks::from_config(&config, d));
    let mut session = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    if let Some(hooks) = tool_hooks {
        session.tool_hooks = hooks;
    }
    if let Some(r) = restored {
        // The restored conversation; its system prompt is replaced with the current one below.
        session.messages = r.messages;
        session.tool_hooks.session_id = Some(r.id);
    }
    let result = run_prepared(&mut session, prompt, quiet).await;
    // Post-hooks run in the background; give them a moment before the process exits.
    session
        .tool_hooks
        .flush(std::time::Duration::from_secs(10))
        .await;
    services.shutdown().await;
    result
}

async fn run_prepared(
    session: &mut AgentSession,
    prompt: &str,
    quiet: bool,
) -> Result<SinglePromptResult> {
    session.native_tools = session.resolve_native_tools();
    let system = session.default_system_prompt();
    session.set_system_prompt(system);

    let started = Instant::now();
    let mut observer = ConsoleObserver { quiet };
    session.run_turn(prompt, &mut observer).await?;
    let duration_ms = started.elapsed().as_millis() as i64;

    let success = !session.tool_call_history.last().is_some_and(|r| r.is_error);
    Ok(SinglePromptResult {
        // `LastAssistantResponse`: the last reply's text, even when the wrap-up shown to the
        // user is an earlier message (a bare "Verified." after an unchanged verification).
        response: session.last_assistant_response.clone().unwrap_or_default(),
        tokens: session.total_output_tokens,
        duration_ms,
        tool_calls: (!session.tool_call_history.is_empty())
            .then(|| session.tool_call_history.clone()),
        files_modified: (!session.files_modified.is_empty())
            .then(|| session.files_modified.clone()),
        success,
    })
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The whole program; returns the process exit code.
pub async fn main_with_args(raw: Vec<String>) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let args = CliArgs::parse(&raw, &cwd);
    if args.show_version {
        println!("OpenOrca v{} (Rust port)", version());
        return Ok(0);
    }
    let Some(prompt) = args.single_prompt.clone() else {
        eprintln!(
            "The interactive mode is not ported to Rust yet. Use --print \"<prompt>\" (JSON output) or --prompt \"<prompt>\"."
        );
        return Ok(2);
    };
    let mut config = load_config(None)?;
    args.apply_to(&mut config);
    // Ensure the default prompt template exists and is current (Program.cs does it at startup).
    if let Err(e) = orca_core::config::prompt_manager::PromptManager::new().ensure_default_prompt()
    {
        tracing::warn!("could not write the default prompt template: {e}");
    }

    let config_dir = orca_core::config::default_config_dir();
    // Sweep checkpoint snapshots older than a week (best effort, in the background).
    let sweeper = orca_core::session::CheckpointManager::new(config_dir.join("checkpoints"));
    tokio::task::spawn_blocking(move || {
        sweeper.cleanup_older_than(std::time::Duration::from_secs(7 * 24 * 3600))
    });
    let restored = match sessions::restore_from_args(
        &args,
        &orca_core::session::SessionManager::new(&config_dir),
        &cwd,
    ) {
        sessions::Restore::Fresh => None,
        sessions::Restore::Resumed(r) => Some(r),
        sessions::Restore::NotFound => return Ok(0),
        sessions::Restore::Unreadable => return Ok(1),
    };
    // --print does not auto-save (only the interactive loop does, after each turn).
    let result = run_single_prompt_in_session(
        config,
        cwd,
        &prompt,
        args.json_output,
        Some(&config_dir),
        restored,
    )
    .await?;
    if args.json_output {
        println!(
            "{}",
            orca_core::config::dotnet_json::to_string_dotnet(&result)?
        );
    } else {
        println!();
    }
    Ok(0)
}
