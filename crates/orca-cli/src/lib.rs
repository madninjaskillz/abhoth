//! The `openorca` program. Phase 1: one-shot prompts (`--print` / `--prompt`); the
//! interactive REPL/TUI and the web host follow in later phases (see PORTING.md).

pub mod args;
pub mod pi_profile;
pub mod prompter;
pub mod sessions;

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

/// The compiled-in tools (C# loads them from plugin DLLs). Registration order is the C# one:
/// plugin assemblies in file-name order (Agent, Archive, FileSystem, Git, GitHub,
/// ImageGeneration, Interactive, Lsp, Network, Shell, Utility, Web), and within each the tool
/// classes sorted by name. The advertised list keeps this order. Optional groups sit behind
/// Cargo features (`archive`, `network`, `image`, `lsp`, `mcp`); MCP tools need the servers
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
    #[cfg(feature = "archive")]
    r.register_all(orca_tools_archive::all_tools());
    r.register_all(orca_tools_fs::all_tools());
    r.register_all(orca_tools_git::all_tools());
    #[cfg(feature = "image")]
    r.register_all(orca_tools_image::all_tools(
        &config.image_gen,
        Some(Arc::new(std::sync::Mutex::new(
            config.agent.uncensored.clone(),
        ))),
    ));
    r.register_all(orca_tools_utility::interactive_tools(Some(Arc::new(
        prompter::ConsolePrompter,
    ))));
    #[cfg(feature = "lsp")]
    {
        services.lsp = lsp_manager(config, cwd);
        r.register_all(orca_tools_lsp::all_tools(services.lsp.clone()));
    }
    #[cfg(not(feature = "lsp"))]
    let _ = cwd;
    #[cfg(feature = "network")]
    r.register_all(orca_tools_network::all_tools());
    r.register_all(orca_tools_shell::all_tools(
        config.shell.idle_timeout_seconds,
    ));
    r.register_all(orca_tools_utility::utility_tools());
    r.register_all(orca_tools_web::all_tools());
    // Disabled tools stay registered; the permission check refuses them (as in C#).
    if orca_tools_pi::is_pi_profile(&config.tool_profile) {
        pi_profile::register(&mut r, config);
    }
    let exposure = config.clone();
    r.set_advertise_filter(Arc::new(move |name: &str| {
        orca_core::agent::tool_exposure::is_advertised(name, &exposure)
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
    if orca_core::engines::selected_bridge(&config).is_some() {
        return run_bridge_prompt(config, cwd, prompt, quiet, restored).await;
    }
    let served = Arc::new(ServedModels::new());
    let mut client = client_from_config(&config, served)?;
    // Sticky slots (config "slots"): the app-wide client is the Lead's pinned client, with
    // the normal provider as its fallback.
    let slots = if config.demo_mode {
        None
    } else {
        orca_core::slots::SlotRuntime::create(&config, Arc::new(client.clone()))
    };
    if let Some(rt) = &slots {
        client = client.with_router(rt.lead_client());
    }
    let (mut registry, mut services) = build_registry_in(&config, &cwd);
    services.start_mcp(&config, &cwd, &mut registry).await;
    let tool_hooks = config_dir.map(|d| orca_core::agent::ToolHooks::from_config(&config, d));
    let mut session = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    // C# runs the startup probes (max_tokens negotiation, context-window detection) before
    // the first turn.
    session.startup_probes = true;
    if let Some(hooks) = tool_hooks {
        session.tool_hooks = hooks;
    }
    if let Some(r) = restored {
        // The restored conversation; its system prompt is replaced with the current one below.
        session.messages = r.messages;
        session.tool_hooks.session_id = Some(r.id);
    }
    let result = run_prepared(&mut session, prompt, quiet).await;
    // Every lease goes back on exit.
    if let Some(rt) = &slots {
        rt.shutdown().await;
    }
    // Post-hooks run in the background; give them a moment before the process exits.
    session
        .tool_hooks
        .flush(std::time::Duration::from_secs(10))
        .await;
    services.shutdown().await;
    pi_profile::kill_jobs();
    result
}

/// `--print` with a bridge engine (agent.engine claude / codex): the CLI runs the turn.
async fn run_bridge_prompt(
    config: OrcaConfig,
    cwd: PathBuf,
    prompt: &str,
    quiet: bool,
    restored: Option<sessions::RestoredSession>,
) -> Result<SinglePromptResult> {
    // The tools (ask_user is offered to the CLI as a host tool) and the system prompt the
    // native loop would use (C# adds it before the prompt).
    let (registry, mut services) = build_registry_in(&config, &cwd);
    let mut native = AgentSession::new(
        config.clone(),
        client_from_config(&config, Arc::new(ServedModels::new()))?,
        registry,
        ToolContext::new(cwd.clone()),
    );
    native.native_tools = native.resolve_native_tools();
    let system = native.default_system_prompt();
    let registry = std::mem::take(&mut native.registry);
    let session_id = restored.as_ref().map(|r| r.id.clone());
    let mut conversation = orca_core::chat::Conversation::from_api_messages(
        restored.map(|r| r.messages).unwrap_or_default(),
        Default::default(),
    );
    let outcome = orca_core::engines::run_print_turn(
        orca_core::engines::PrintTurnRequest {
            config: &config,
            cwd: &cwd,
            conversation: &mut conversation,
            system_prompt: Some(system),
            prompt,
            registry: Some(&registry),
            session_id: session_id.as_deref(),
            quiet,
        },
        &orca_core::engines::CancelToken::new(),
    )
    .await;
    services.shutdown().await;
    let outcome = outcome.map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(message) = &outcome.unavailable {
        eprintln!("✗ {message}");
    }
    Ok(SinglePromptResult {
        response: outcome.response,
        tokens: outcome.tokens,
        duration_ms: outcome.duration_ms,
        tool_calls: outcome.tool_calls,
        files_modified: outcome.files_modified,
        success: outcome.success,
    })
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
    let mut manager = ConfigManager::new(None);
    manager
        .load()
        .with_context(|| format!("loading {}", manager.file_path().display()))?;
    let config_dir = orca_core::config::default_config_dir();
    // An inference profile (--profile, else profiles.active) is applied over config.json first,
    // so explicit flags such as --engine still win.
    let mut profile_notices = Vec::new();
    let profile_name = args
        .profile
        .clone()
        .or_else(|| manager.config.profiles.active.clone());
    if let Some(name) = profile_name.filter(|_| !args.demo_mode)
        && let Some(error) = orca_core::profiles::apply_at_startup(
            &mut manager,
            &config_dir,
            &name,
            &mut profile_notices,
        )
        .await
    {
        eprintln!("{error}");
        if args.profile.is_some() {
            return Ok(2);
        }
    }
    // --mode (the web UI's per-project mode): over the profile's own agent.leadMode / peerMode.
    if let Some(mode) = args
        .mode
        .as_deref()
        .filter(|m| !m.is_empty() && !args.demo_mode)
    {
        if orca_core::profiles::agent_modes::normalize(Some(mode)).is_none() {
            eprintln!(
                "{}",
                orca_core::profiles::agent_modes::unknown_mode_message(mode)
            );
            return Ok(2);
        }
        if let Err(e) = manager.apply_mode(mode) {
            eprintln!("{e}");
            return Ok(2);
        }
    }
    let mut config = manager.config.clone();
    if let Err(e) = args.apply_to(&mut config) {
        eprintln!("{e}");
        return Ok(2);
    }
    if orca_core::engines::AgentEngines::normalize(Some(&config.agent.engine)).is_none() {
        eprintln!(
            "Unknown agent.engine '{}' in config (native, claude or codex).",
            config.agent.engine
        );
        return Ok(2);
    }
    // Ensure the default prompt template exists and is current (Program.cs does it at startup).
    if let Err(e) = orca_core::config::prompt_manager::PromptManager::new().ensure_default_prompt()
    {
        tracing::warn!("could not write the default prompt template: {e}");
    }

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
    for notice in &profile_notices {
        eprintln!("{notice}");
    }
    // A bridge engine must be ready (installed, logged in) before the turn.
    match orca_core::engines::prepare_engine_non_interactive(
        &config,
        &orca_core::engines::CancelToken::new(),
    )
    .await
    {
        Ok(Some(message)) => {
            eprintln!("{message}");
            return Ok(2);
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            return Ok(2);
        }
    }
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
