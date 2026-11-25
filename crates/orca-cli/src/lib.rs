//! The `openorca` program. Phase 1: one-shot prompts (`--print` / `--prompt`); the
//! interactive REPL/TUI and the web host follow in later phases (see PORTING.md).

pub mod args;

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

/// The phase-1 tool set: the core file and shell tools, registered at compile time.
pub fn build_registry(config: &OrcaConfig) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register_all(orca_tools_fs::all_tools());
    r.register_all(orca_tools_shell::all_tools(
        config.shell.idle_timeout_seconds,
    ));
    // Disabled tools stay registered; the permission check refuses them (as in C#).
    r
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
    let served = Arc::new(ServedModels::new());
    let client = client_from_config(&config, served)?;
    let registry = build_registry(&config);
    let mut session = AgentSession::new(config, client, registry, ToolContext::new(cwd));
    let system = session.default_system_prompt();
    session.set_system_prompt(system);

    let started = Instant::now();
    let mut observer = ConsoleObserver { quiet };
    session.run_turn(prompt, &mut observer).await?;
    let duration_ms = started.elapsed().as_millis() as i64;

    let success = !session.tool_call_history.last().is_some_and(|r| r.is_error);
    Ok(SinglePromptResult {
        response: session
            .final_prose
            .clone()
            .or_else(|| session.last_assistant_response.clone())
            .unwrap_or_default(),
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
    let result = run_single_prompt(config, cwd, &prompt, args.json_output).await?;
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
