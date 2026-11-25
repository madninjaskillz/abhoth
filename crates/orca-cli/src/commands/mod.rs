//! Slash commands (`src/plugins/commands` in C#), compiled in (decision D3). Each command gets
//! a [`CommandContext`] with the REPL's session, state and services, and returns a
//! [`CommandResult`]; a few return sentinel prompts (`__clear__`, `__ask_toggle__`,
//! `__plan_toggle__`) that the REPL handles, as in C#.

mod agent;
pub mod agent_cmds;
pub mod benchmark_cmd;
mod config_cmds;
pub mod config_editor;
mod context_cmds;
mod core_cmds;
mod custom;
mod fileops;
mod memory;
pub mod profile_cmd;
mod session_cmds;
pub mod uncensored;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use orca_core::agent::AgentSession;
use orca_core::session::{CheckpointManager, SessionManager};

use crate::custom_commands::CustomCommandLoader;
use crate::repl::state::ReplState;
use crate::ui::Console;

/// `CommandResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    Ok,
    Exit,
    /// Run the agent loop with this prompt (or a sentinel the REPL handles).
    RunAgent(String),
}

pub const CLEAR_SENTINEL: &str = "__clear__";
pub const ASK_TOGGLE_SENTINEL: &str = "__ask_toggle__";
pub const PLAN_TOGGLE_SENTINEL: &str = "__plan_toggle__";

/// What a sub-agent run reports (`AgentContext`: status, iterations, result, error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunOutcome {
    pub completed: bool,
    /// The status name ("Completed", "Failed", "Cancelled", ...).
    pub status: String,
    pub iterations: u32,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Sub-agents and workflows for `/review` and `/workflow` (the orchestration port). Until a
/// host is installed those commands say sub-agents are not available.
#[async_trait]
pub trait AgentHost: Send + Sync {
    /// `AgentOrchestrator.SpawnAgentAsync(task, agentType)`.
    async fn spawn_agent(&self, task: &str, agent_type: &str) -> Result<AgentRunOutcome, String>;

    /// The session's agent manager (`/workflow`, `/agent`, `/agents`); None = not available.
    fn manager(&self) -> Option<Arc<orca_core::orchestration::AgentManager>> {
        None
    }
}

/// The REPL services a command can use (`ICommandContext`).
pub struct CommandContext<'a> {
    pub console: &'a Console,
    pub session: &'a mut AgentSession,
    pub state: &'a mut ReplState,
    pub sessions: &'a SessionManager,
    pub checkpoints: &'a CheckpointManager,
    pub config_dir: &'a Path,
    pub registry: &'a CommandRegistry,
    pub custom: &'a CustomCommandLoader,
    pub agents: Option<&'a dyn AgentHost>,
}

impl CommandContext<'_> {
    /// The working folder (`Directory.GetCurrentDirectory` in C#; the session's, which `cd`
    /// changes).
    pub fn cwd(&self) -> PathBuf {
        self.session.ctx.cwd.clone()
    }
}

/// `IOrcaCommand`.
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult;
}

/// `CommandRegistry`: names and aliases, case-insensitive.
#[derive(Default)]
pub struct CommandRegistry {
    commands: Vec<Arc<dyn Command>>,
    by_name: BTreeMap<String, usize>,
    aliases: BTreeMap<String, String>,
}

impl CommandRegistry {
    pub fn register(&mut self, command: Arc<dyn Command>) {
        let name = command.name().to_lowercase();
        for a in command.aliases() {
            self.aliases
                .insert(a.to_lowercase(), command.name().to_string());
        }
        match self.by_name.get(&name) {
            Some(&i) => self.commands[i] = command,
            None => {
                self.by_name.insert(name, self.commands.len());
                self.commands.push(command);
            }
        }
    }

    pub fn resolve(&self, name_or_alias: &str) -> Option<Arc<dyn Command>> {
        let key = name_or_alias.to_lowercase();
        if let Some(&i) = self.by_name.get(&key) {
            return Some(self.commands[i].clone());
        }
        let real = self.aliases.get(&key)?;
        self.by_name
            .get(&real.to_lowercase())
            .map(|&i| self.commands[i].clone())
    }

    pub fn all(&self) -> &[Arc<dyn Command>] {
        &self.commands
    }

    pub fn command_names(&self) -> Vec<&'static str> {
        self.commands.iter().map(|c| c.name()).collect()
    }

    /// Every name and alias -> the command's name.
    pub fn alias_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for c in &self.commands {
            map.insert(c.name().to_string(), c.name().to_string());
            for a in c.aliases() {
                map.insert(a.to_string(), c.name().to_string());
            }
        }
        map
    }
}

/// The compiled-in commands (the C# plugin assemblies Agent, Config, Context, Core, Custom,
/// FileOps, Memory and Session).
pub fn registry() -> CommandRegistry {
    let mut r = CommandRegistry::default();
    let all: Vec<Arc<dyn Command>> = vec![
        Arc::new(agent::DocsCommand),
        Arc::new(agent::ReviewCommand),
        Arc::new(agent::WorkflowCommand),
        Arc::new(agent_cmds::AgentCommand),
        Arc::new(agent_cmds::AgentsCommand),
        Arc::new(config_cmds::InitCommand),
        Arc::new(config_cmds::ModelCommand),
        Arc::new(uncensored::UncensoredCommand),
        Arc::new(context_cmds::ContextCommand),
        Arc::new(context_cmds::CopyCommand),
        Arc::new(context_cmds::ExportCommand),
        Arc::new(context_cmds::StatsCommand),
        Arc::new(core_cmds::AskToggleCommand),
        Arc::new(core_cmds::ClearCommand),
        Arc::new(core_cmds::DoctorCommand),
        Arc::new(core_cmds::ExitCommand),
        Arc::new(core_cmds::HelpCommand),
        Arc::new(custom::CustomCommand),
        Arc::new(fileops::AddCommand),
        Arc::new(fileops::CheckpointCommand),
        Arc::new(fileops::DiffCommand),
        Arc::new(fileops::UndoCommand),
        Arc::new(memory::MemoryCommand),
        Arc::new(session_cmds::CompactCommand),
        Arc::new(session_cmds::ForkCommand),
        Arc::new(session_cmds::PlanCommand),
        Arc::new(session_cmds::RenameCommand),
        Arc::new(session_cmds::RewindCommand),
        Arc::new(session_cmds::SessionCommand),
    ];
    for c in all {
        r.register(c);
    }
    r
}

/// Run a shell command line for `/diff` and `/undo` (`cmd.exe /c` or `bash -c`) in `cwd`;
/// stdout only, as C# reads it.
pub(crate) async fn run_shell_stdout(command: &str, cwd: &Path) -> std::io::Result<String> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/c").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("/bin/bash");
        c.arg("-c").arg(command);
        c
    };
    let out = cmd
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `RenderUnifiedDiff` of /diff and /checkpoint: coloured lines in a rounded panel.
pub(crate) fn diff_markup(diff: &str) -> String {
    diff.split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            let esc = crate::ui::markup::escape(line);
            if line.starts_with('+') && !line.starts_with("+++") {
                format!("[green]{esc}[/]")
            } else if line.starts_with('-') && !line.starts_with("---") {
                format!("[red]{esc}[/]")
            } else if line.starts_with("@@") {
                format!("[cyan]{esc}[/]")
            } else {
                esc
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `N0` formatting: thousands separated with commas.
pub(crate) fn n0(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    //! Ported from CommandRegistryTests.cs.
    use super::*;

    #[test]
    fn discover_commands_finds_core_commands() {
        assert!(registry().all().len() >= 4);
    }

    #[test]
    fn resolve_by_name_and_alias() {
        let r = registry();
        assert_eq!(r.resolve("exit").unwrap().name(), "exit");
        assert_eq!(r.resolve("q").unwrap().name(), "exit");
    }

    #[test]
    fn resolve_case_insensitive() {
        let r = registry();
        assert!(r.resolve("EXIT").is_some());
        assert!(r.resolve("Help").is_some());
        assert!(r.resolve("CLEAR").is_some());
    }

    #[test]
    fn resolve_unknown_returns_null() {
        assert!(registry().resolve("nonexistent").is_none());
    }

    #[test]
    fn discover_commands_multiple_assemblies() {
        let r = registry();
        for n in [
            "exit", "help", "clear", "ask", "diff", "undo", "init", "memory",
        ] {
            assert!(r.resolve(n).is_some(), "{n}");
        }
    }

    #[test]
    fn get_alias_map_contains_names_and_aliases() {
        let map = registry().alias_map();
        for k in ["exit", "quit", "q", "help", "h", "?"] {
            assert!(map.contains_key(k), "{k}");
        }
        assert_eq!(map["quit"], "exit");
        assert_eq!(map["q"], "exit");
    }

    #[test]
    fn get_command_names_returns_only_primary_names() {
        let r = registry();
        let names = r.command_names();
        for n in ["exit", "help", "clear", "ask"] {
            assert!(names.contains(&n));
        }
        assert!(!names.contains(&"q") && !names.contains(&"quit"));
    }

    #[test]
    fn n0_groups_thousands() {
        assert_eq!(n0(0), "0");
        assert_eq!(n0(1234), "1,234");
        assert_eq!(n0(1234567), "1,234,567");
        assert_eq!(n0(-1000), "-1,000");
    }
}
