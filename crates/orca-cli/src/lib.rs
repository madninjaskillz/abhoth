//! The `openorca` program. Phase 1: one-shot prompts (`--print` / `--prompt`); the
//! interactive REPL/TUI and the web host follow in later phases (see PORTING.md).

pub mod args;
pub mod sessions;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use orca_abstractions::{ToolContext, ToolResult};
use orca_core::agent::{AgentObserver, AgentSession, ToolCallRecord, ToolRegistry};
use orca_core::client::{ServedModels, ToolCall, client_from_config};
use orca_core::agent::sub_agent_tools::{ClientFactory, SessionToolExecutor, SharedPipeline};
use orca_core::config::{ConfigManager, OrcaConfig};
use orca_core::orchestration::custom_agents::CustomAgentLoader;
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{
    AgentChatClient, AgentManager, AgentOrchestrator, DefaultClientProvider,
};
pub use orca_tools_agent::AgentToolsHandle;
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

/// The tool set, registered at compile time (D3): file, shell, git/GitHub, web and agent tools.
/// The agent tools stay unwired (see [`build_registry_with_agents`]).
pub fn build_registry(config: &OrcaConfig) -> ToolRegistry {
    build_registry_with_agents(config).0
}

/// [`build_registry`] and the handle that wires its agent tools to a session's sub-agents.
pub fn build_registry_with_agents(config: &OrcaConfig) -> (ToolRegistry, AgentToolsHandle) {
    let handle = AgentToolsHandle::new();
    let mut r = ToolRegistry::new();
    r.register_all(orca_tools_fs::all_tools());
    r.register_all(orca_tools_shell::all_tools(
        config.shell.idle_timeout_seconds,
    ));
    r.register_all(orca_tools_git::all_tools());
    r.register_all(orca_tools_web::all_tools());
    r.register_all(orca_tools_agent::all_tools(&handle));
    // Disabled tools stay registered; the permission check refuses them (as in C#).
    (r, handle)
}

/// Sub-agents for `session` (`ToolWiring`, agent part): the orchestrator with the session's
/// tools and custom agent types, sub-agent tool calls through a copy of the session's
/// pipeline, and the agent tools wired to the manager. Start/finish lines go to stderr unless
/// `quiet`.
pub fn wire_agents(
    session: &mut AgentSession,
    handle: &AgentToolsHandle,
    served: Arc<ServedModels>,
    config_dir: Option<&Path>,
    quiet: bool,
) -> Result<Arc<AgentManager>> {
    let config = session.config.clone();
    let cwd = session.ctx.cwd.clone();
    let client: Arc<dyn AgentChatClient> = Arc::new(client_from_config(&config, served.clone())?);
    let mut types = AgentTypeRegistry::new();
    if let Some(dir) = config_dir {
        let names: Vec<String> = session
            .registry
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        let custom = CustomAgentLoader::discover_agents(dir, &cwd, Some(&names), &mut |w| {
            tracing::warn!("{w}");
            if !quiet {
                eprintln!("\u{26a0} {w}");
            }
        });
        types.register_custom(custom.iter());
    }
    let orchestrator = AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(client)),
        config.clone(),
        types,
        cwd,
    );
    // Sub-agents never choose a Lead mode.
    orchestrator.set_tools(
        session
            .registry
            .definitions()
            .into_iter()
            .filter(|d| d.name != orca_core::agent::policies::LEAD_MODE_TOOL_NAME)
            .collect(),
    );
    orchestrator.set_use_streaming(true);
    let shared = SharedPipeline::new();
    let factory_config = config.clone();
    let factory_served = served;
    let factory: ClientFactory = Arc::new(move || {
        client_from_config(&factory_config, factory_served.clone())
            .expect("the main client was built from the same config")
    });
    orchestrator.set_executor(Some(Arc::new(SessionToolExecutor::new(
        config,
        session.registry.clone(),
        session.ctx.clone(),
        session.tool_hooks.clone(),
        factory,
        shared.clone(),
        &orchestrator,
    ))));
    let manager = AgentManager::new(orchestrator.clone());
    if !quiet {
        manager.set_line_sink(Some(Arc::new(|line: &str| eprintln!("{line}"))));
    }
    handle.set(Some(manager.clone()));
    session.orchestrator = Some(orchestrator);
    session.sub_agent_pipeline = Some(shared);
    Ok(manager)
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
    let client = client_from_config(&config, served.clone())?;
    let (registry, agent_tools) = build_registry_with_agents(&config);
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
    wire_agents(&mut session, &agent_tools, served, config_dir, quiet)?;
    let result = run_prepared(&mut session, prompt, quiet).await;
    // Post-hooks run in the background; give them a moment before the process exits.
    session
        .tool_hooks
        .flush(std::time::Duration::from_secs(10))
        .await;
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
