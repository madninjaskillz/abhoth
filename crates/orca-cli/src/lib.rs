//! The `openorca` program. Phase 1: one-shot prompts (`--print` / `--prompt`); the
//! interactive REPL and the `--hostui` web host.

pub mod agent_host;
pub mod args;
pub mod commands;
pub mod custom_commands;
#[cfg(feature = "desktop")]
pub mod desktop;
pub mod file_log;
pub mod hostui;
pub mod hostui_password;
pub mod input_preprocessor;
pub mod model_discovery;
pub mod pi_agents;
pub mod pi_profile;
pub mod prompter;
pub mod render;
pub mod repl;
pub mod sessions;
pub mod ui;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use orca_abstractions::{ToolContext, ToolResult};
use orca_core::agent::sub_agent_tools::{ClientFactory, SessionToolExecutor, SharedPipeline};
use orca_core::agent::{AgentObserver, AgentSession, ToolCallRecord, ToolRegistry};
use orca_core::client::{ServedModels, ToolCall, client_from_config};
use orca_core::config::{ConfigManager, OrcaConfig};
use orca_core::orchestration::CancelToken;
use orca_core::orchestration::custom_agents::CustomAgentLoader;
use orca_core::orchestration::planner_phase::{PlannerPhase, planner_enabled};
use orca_core::orchestration::types::AgentTypeRegistry;
use orca_core::orchestration::{
    AgentChatClient, AgentClientProvider, AgentManager, AgentOrchestrator, DefaultClientProvider,
    SlotClientProvider,
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
    build_registry_with(config, cwd, Arc::new(prompter::ConsolePrompter))
}

/// [`build_registry_in`] with `ask_user` answered by `asker` (the web UI's question picker).
pub fn build_registry_with(
    config: &OrcaConfig,
    cwd: &Path,
    asker: Arc<dyn orca_tools_utility::UserPrompter>,
) -> (ToolRegistry, ToolServices) {
    let mut r = ToolRegistry::new();
    #[allow(unused_mut)]
    let mut services = ToolServices::default();
    // The Agent plugin comes first (ReportProgressTool, SpawnAgentTool, SpawnAgentsTool).
    r.register_all(orca_tools_agent::all_tools(&services.agent_tools));
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
    r.register_all(orca_tools_utility::interactive_tools(Some(asker)));
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
    /// Wires the agent tools to a session's sub-agents ([`wire_agents`]).
    pub agent_tools: AgentToolsHandle,
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

/// Sub-agents for `session` (`ToolWiring`, agent part): the orchestrator with the session's
/// tools and custom agent types, sub-agent tool calls through a copy of the session's
/// pipeline, and the agent tools wired to the manager. Start/finish lines go to stderr unless
/// `quiet`. With `slots` (the runtime [`wire_client`] returned), every agent on the default
/// model leases its own slot (`orchestrator.Slots = runtime`).
pub fn wire_agents(
    session: &mut AgentSession,
    handle: &AgentToolsHandle,
    config_dir: Option<&Path>,
    quiet: bool,
    slots: Option<&Arc<orca_core::slots::SlotRuntime>>,
) -> Result<Arc<AgentManager>> {
    let config = session.config.clone();
    let cwd = session.ctx.cwd.clone();
    // Sub-agents send through the session's client (its usage logging and routing).
    let client: Arc<dyn AgentChatClient> = Arc::new(session.client.clone());
    let mut types = AgentTypeRegistry::new();
    // Pi profile with pi.agentTools: Pi's agent types (scout, planner, reviewer, worker).
    if orca_tools_pi::is_pi_profile(&config.tool_profile) && config.pi.agent_tools {
        types.use_pi_profile(config.pi.extra_tools);
    }
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
    let mut provider: Arc<dyn AgentClientProvider> = Arc::new(DefaultClientProvider::new(client));
    // Named providers (agent.providerSchedule, agent.planner): logged with provider= and model=
    // and added up in the session's usage ledger.
    provider = Arc::new(orca_core::orchestration::ProviderRoutes::new(
        provider,
        config.clone(),
        orca_core::client::usage::ProviderUsageLedger::shared(),
        orca_core::logging::process_sink(),
    ));
    if let Some(rt) = slots {
        provider = Arc::new(SlotClientProvider::new(provider, rt.clone()));
    }
    let orchestrator = AgentOrchestrator::new(provider, config.clone(), types, cwd);
    let manager = AgentManager::new(orchestrator.clone());
    // The Pi agent tools (subagent, progress, lead_mode) go through the manager.
    pi_agents::wire_pi_agent_tools(&mut session.registry, &manager, &config, slots);
    pi_agents::register_auto_lead_mode(&mut session.registry, &manager, &config, slots);
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
    // Sub-agents' context compaction uses the session's LLM summariser.
    orchestrator.set_compactor(Some(Arc::new(orca_core::agent::SessionCompactor::new(
        session.client.clone(),
        config.clone(),
    ))));
    orchestrator.set_system_prompt_factory(Some(pi_agents::pi_prompt_factory(
        &session.config,
        session.ctx.cwd.clone(),
    )));
    let shared = SharedPipeline::new();
    let factory_client = session.client.clone();
    let factory: ClientFactory = Arc::new(move || factory_client.clone());
    orchestrator.set_executor(Some(Arc::new(SessionToolExecutor::new(
        config,
        session.registry.clone(),
        session.ctx.clone(),
        session.tool_hooks.clone(),
        factory,
        shared.clone(),
        &orchestrator,
    ))));
    if !quiet {
        manager.set_line_sink(Some(Arc::new(|line: &str| eprintln!("{line}"))));
    }
    handle.set(Some(manager.clone()));
    // Chat lines naming a known agent get its badge (`AgentBadge.IsKnownLetter`).
    let weak = Arc::downgrade(&orchestrator);
    crate::render::agent_badge::set_known_letter(Some(Arc::new(move |letter| {
        weak.upgrade()
            .is_some_and(|o| o.get_all_agents().iter().any(|a| a.letter() == letter))
    })));
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

/// `ServiceRegistration`'s chat client: the provider client behind the usage logger (route
/// "provider", so each request's "Slot usage" line lands in `log`), the sticky-slot runtime
/// over it when `slots` is on and configured (the Lead's pinned client, the logged provider as
/// its fallback), and canned answers in demo mode (`DemoChatClient`, not logged).
pub fn wire_client(
    config: &OrcaConfig,
    log: Option<Arc<dyn orca_core::logging::LogSink>>,
    slots: bool,
) -> Result<(
    orca_core::client::OpenAiClient,
    Option<Arc<orca_core::slots::SlotRuntime>>,
)> {
    let client = client_from_config(config, Arc::new(ServedModels::new()))?;
    if config.demo_mode {
        // `--demo`: canned answers, no server (DemoChatClient).
        return Ok((
            client.with_router(Arc::new(orca_core::client::demo::DemoChatClient)),
            None,
        ));
    }
    let fallback: Arc<dyn orca_core::client::ChatBackend> =
        Arc::new(orca_core::client::UsageLoggingBackend::new(
            Arc::new(client.clone()),
            log.clone(),
            orca_core::client::usage::USAGE_CATEGORY,
            "provider",
        ));
    let runtime = if slots {
        orca_core::slots::SlotRuntime::create_with_log(config, fallback.clone(), log)
    } else {
        None
    };
    let router = match &runtime {
        Some(rt) => rt.lead_client(),
        None => fallback,
    };
    Ok((client.with_router(router), runtime))
}

/// "Provider usage ..." per provider (`Program`'s exit lines), to the process log.
pub fn log_provider_usage() {
    let Some(log) = orca_core::logging::process_log() else {
        return;
    };
    for usage in orca_core::client::ProviderUsageLedger::shared().snapshot() {
        orca_core::logging::LogSink::log(
            &*log,
            orca_core::logging::Level::Info,
            "OpenOrca.Program",
            &format!("Provider usage {usage}"),
        );
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
    let (client, slots) = wire_client(&config, orca_core::logging::process_sink(), true)?;
    let (mut registry, mut services) = build_registry_in(&config, &cwd);
    services.start_mcp(&config, &cwd, &mut registry).await;
    let tool_hooks = config_dir.map(|d| orca_core::agent::ToolHooks::from_config(&config, d));
    let mut session = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    // C# runs the startup probes (max_tokens negotiation, context-window detection) before
    // the first turn.
    session.startup_probes = !session.config.demo_mode;
    if let Some(hooks) = tool_hooks {
        session.tool_hooks = hooks;
    }
    if let Some(r) = restored {
        // The restored conversation; its system prompt is replaced with the current one below.
        session.messages = r.messages;
        session.tool_hooks.session_id = Some(r.id);
    }
    wire_agents(
        &mut session,
        &services.agent_tools,
        config_dir,
        quiet,
        slots.as_ref(),
    )?;
    // agent.planner: a read-only planner writes the plan before the Lead's first turn.
    let prompt = match session.orchestrator.clone() {
        Some(o) if planner_enabled(&session.config) => {
            let mut planner = PlannerPhase::new(session.config.clone(), o, session.ctx.cwd.clone());
            session.seed_sub_agents();
            let prompt = planner
                .prepare_first_message(prompt, &CancelToken::new())
                .await;
            session.take_sub_agent_work();
            prompt
        }
        _ => prompt.to_string(),
    };
    let result = run_prepared(&mut session, &prompt, quiet).await;
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
    let engine = restored
        .as_ref()
        .map(|r| r.engine.clone())
        .unwrap_or_default();
    let mut conversation = orca_core::chat::Conversation::from_api_messages(
        restored.map(|r| r.messages).unwrap_or_default(),
        Default::default(),
    );
    // A session the engine ran before resumes that CLI's own session (SessionToConversation).
    conversation.engine_id = engine.engine;
    conversation.engine_session_id = engine.engine_session_id;
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

/// `abhoth --version`.
pub fn version_line() -> String {
    format!(
        "{} v{} (the Rust successor of OpenOrca)",
        orca_core::brand::PRODUCT_NAME,
        version()
    )
}

/// `abhoth --help`: the common options (the C# build has no help page; D88).
pub fn help_text() -> String {
    let bin = orca_core::brand::BINARY_NAME;
    format!(
        "{name} v{ver}: an autonomous AI coding agent for local models.

Usage:
  {bin}                          interactive session in the current folder
  {bin} -p \"<prompt>\"            one prompt, then exit (--print; --output json)
  {bin} --hostui[=host:port]     the web UI (--hostui-open, --hostui-login, --hostui-token <t>)
  {bin} --desktop                the web UI in its own window (desktop builds)

Common options:
  -c, --continue                 continue the last session
  -r, --resume <id>              resume a session
  --profile <name>               use a profile from the profile server
  --mode <name>                  agent mode for this run
  --engine <native|claude|codex> the agent engine
  --sandbox                      read-only tools, no shell
  --allow <tools>                pre-approve tools (comma-separated)
  --allow-dir <dir>              keep file tools inside a folder
  --demo                         demo mode (no model server)
  -v, --version                  print the version

Settings and sessions live in ~/.abhoth (ABHOTH_HOME; OPENORCA_HOME is read too).
",
        name = orca_core::brand::PRODUCT_NAME,
        ver = version(),
    )
}

/// The whole program; returns the process exit code.
pub async fn main_with_args(raw: Vec<String>) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    // --hostui-set-password / --hostui-reset-2fa: manage the web UI's sign-in account, then exit.
    if let Some(code) = hostui_password::try_run(&raw) {
        return Ok(code);
    }
    if raw.iter().any(|a| a == "--desktop") {
        // The desktop app starts in main() (it needs the main thread); here only without it.
        eprintln!(
            "abhoth: this build has no desktop app (build with --features desktop); use --hostui."
        );
        return Ok(2);
    }
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", help_text());
        return Ok(0);
    }
    let args = CliArgs::parse(&raw, &cwd);
    if args.show_version {
        println!("{}", version_line());
        return Ok(0);
    }
    // --hostui: the web UI, headless (src/OpenOrca.Cli/HostUi). Ignored with --print.
    let hostui_args = if args.single_prompt.is_none() {
        orca_webui::hostui::HostUiArgs::parse(&raw)
    } else {
        None
    };
    if let Some(e) = hostui_args.as_ref().and_then(|h| h.error.clone()) {
        eprintln!("abhoth: {e}");
        return Ok(2);
    }
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

    if let Some(h) = hostui_args {
        for notice in &profile_notices {
            eprintln!("{notice}");
        }
        let overlay = orca_core::config::schema::ProfileOverlay::of(&manager);
        return hostui::run(h, config, cwd, overlay).await;
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
    // A bridged session resumes on its engine (and the CLI's own session) unless --engine says
    // otherwise.
    if args.engine.is_none()
        && let Some(r) = &restored
        && orca_core::engines::AgentEngines::is_bridge(r.engine.engine.as_deref())
        && let Some(id) = orca_core::engines::AgentEngines::normalize(r.engine.engine.as_deref())
    {
        config.agent.engine = id.to_string();
    }
    if args.single_prompt.is_some() {
        for notice in &profile_notices {
            eprintln!("{notice}");
        }
    }
    let Some(prompt) = args.single_prompt.clone() else {
        return run_interactive(config, cwd, &config_dir, restored, profile_notices).await;
    };
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

/// The interactive REPL (`ReplLoop.RunAsync`), with the same session wiring as `--print`.
async fn run_interactive(
    config: OrcaConfig,
    cwd: PathBuf,
    config_dir: &Path,
    restored: Option<sessions::RestoredSession>,
    startup_notices: Vec<String>,
) -> Result<i32> {
    let (client, slots) = wire_client(&config, orca_core::logging::process_sink(), true)?;
    let (mut registry, mut services) = build_registry_in(&config, &cwd);
    services.start_mcp(&config, &cwd, &mut registry).await;
    let tool_hooks = orca_core::agent::ToolHooks::from_config(&config, config_dir);
    let active_profile = config.active_profile.clone();
    let mut session = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    session.startup_probes = !session.config.demo_mode;
    session.tool_hooks = tool_hooks;
    let mut session_id = None;
    let mut engine = orca_core::session::EngineRef::default();
    if let Some(r) = restored {
        session.messages = r.messages;
        session.tool_hooks.session_id = Some(r.id.clone());
        session_id = Some(r.id);
        engine = r.engine;
    }
    // Sub-agents (ToolWiring): the agent tools, /review, /workflow, /agent and the planner.
    let manager = wire_agents(
        &mut session,
        &services.agent_tools,
        Some(config_dir),
        false,
        slots.as_ref(),
    )?;
    let planner = session
        .orchestrator
        .clone()
        .filter(|_| planner_enabled(&session.config))
        .map(|o| PlannerPhase::new(session.config.clone(), o, session.ctx.cwd.clone()));
    let console = ui::Console::stdout();
    let mut repl = repl::Repl::new(console, session, config_dir.to_path_buf(), session_id);
    repl.agents = Some(Box::new(agent_host::ReplAgentHost::new(manager)));
    repl.planner = planner;
    repl.state.engine = engine;
    repl.startup_notices = startup_notices;
    repl.active_profile = active_profile;
    // A bridge engine is checked (installed, logged in) before the first turn.
    repl.prepare_engine().await;
    let exit = repl.run().await;
    if let Some(rt) = &slots {
        rt.shutdown().await;
    }
    repl.session
        .tool_hooks
        .flush(std::time::Duration::from_secs(10))
        .await;
    services.shutdown().await;
    pi_profile::kill_jobs();
    match exit {
        repl::ReplExit::Quit => Ok(0),
        // The C# build restarts in-process; here the caller re-runs with the new profile.
        repl::ReplExit::Restart {
            profile,
            session_id,
        } => {
            let mut next = vec!["--profile".to_string(), profile];
            if let Some(id) = session_id {
                next.push("--resume".into());
                next.push(id);
            }
            Box::pin(main_with_args(next)).await
        }
    }
}
