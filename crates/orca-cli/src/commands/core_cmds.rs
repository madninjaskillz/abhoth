//! OpenOrca.Commands.Core: /ask, /clear, /doctor, /exit, /help.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use orca_core::config::{sampling_presets, thinking_levels};

use super::{
    ASK_TOGGLE_SENTINEL, CLEAR_SENTINEL, Command, CommandContext, CommandRegistry, CommandResult,
};
use crate::ui::markup::escape;
use crate::ui::widgets::{Table, write_grid};

pub struct AskToggleCommand;

#[async_trait]
impl Command for AskToggleCommand {
    fn name(&self) -> &'static str {
        "ask"
    }
    fn description(&self) -> &'static str {
        "Toggle ask mode (no tools)"
    }
    async fn execute(&self, _ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        CommandResult::RunAgent(ASK_TOGGLE_SENTINEL.into())
    }
}

pub struct ClearCommand;

#[async_trait]
impl Command for ClearCommand {
    fn name(&self) -> &'static str {
        "clear"
    }
    fn description(&self) -> &'static str {
        "Clear conversation and screen"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["c"]
    }
    async fn execute(&self, _ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        CommandResult::RunAgent(CLEAR_SENTINEL.into())
    }
}

pub struct ExitCommand;

#[async_trait]
impl Command for ExitCommand {
    fn name(&self) -> &'static str {
        "exit"
    }
    fn description(&self) -> &'static str {
        "Exit OpenOrca"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["quit", "q"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        ctx.console.markup_line("[grey]Goodbye![/]");
        CommandResult::Exit
    }
}

pub struct DoctorCommand;

#[async_trait]
impl Command for DoctorCommand {
    fn name(&self) -> &'static str {
        "doctor"
    }
    fn description(&self) -> &'static str {
        "Run diagnostic checks"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["diag"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let config = ctx.session.config.clone();
        let mut table = Table::new()
            .expand()
            .title("[bold]OpenOrca Doctor[/]")
            .column("[bold]Check[/]")
            .column("[bold]Status[/]");
        let models = crate::model_discovery::available_models(&config).await;
        table.row(&[
            "LLM Server connection",
            &if models.is_empty() {
                "[yellow]Connected but no models loaded[/]".to_string()
            } else {
                format!("[green]Connected ({} model(s))[/]", models.len())
            },
        ]);
        table.row(&[
            "Model configured",
            &if config.lm_studio.primary_model().is_some() {
                format!("[green]{}[/]", escape(&config.lm_studio.models.join(" > ")))
            } else {
                "[yellow]Auto-detect[/]".to_string()
            },
        ]);
        table.row(&[
            "Config file",
            if ctx.config_dir.join("config.json").exists() {
                "[green]Found[/]"
            } else {
                "[yellow]Using defaults[/]"
            },
        ]);
        table.row(&[
            "Log directory",
            if std::fs::create_dir_all(ctx.config_dir.join("logs")).is_ok() {
                "[green]Writable[/]"
            } else {
                "[red]Not writable[/]"
            },
        ]);
        table.row(&[
            "Session storage",
            &format!(
                "[green]Accessible ({} sessions)[/]",
                ctx.sessions.list().len()
            ),
        ]);
        table.row(&[
            "Default prompt template",
            if ctx.config_dir.join("prompts").join("default.md").exists() {
                "[green]Found[/]"
            } else {
                "[yellow]Missing[/]"
            },
        ]);
        let files = orca_core::config::project_instructions::load_all(&ctx.cwd());
        table.row(&[
            "Project instructions",
            &if files.is_empty() {
                "[grey]Not found (ORCA.md / AGENTS.md / CLAUDE.md)[/]".to_string()
            } else {
                let names: Vec<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();
                format!("[green]{}[/]", escape(&names.join(", ")))
            },
        ]);
        table.row(&[
            "Native tool calling",
            if config.lm_studio.native_tool_calling {
                "[green]Enabled[/]"
            } else {
                "[grey]Disabled[/]"
            },
        ]);
        table.row(&[
            "Sandbox mode",
            if config.sandbox_mode {
                "[yellow]Enabled (read-only tools only)[/]"
            } else {
                "[grey]Disabled[/]"
            },
        ]);
        table.row(&[
            "Directory restriction",
            &match &config.allowed_directory {
                Some(d) => format!("[yellow]{}[/]", escape(d)),
                None => "[grey]None[/]".to_string(),
            },
        ]);
        table.row(&[
            "Thinking budget",
            &if config.thinking.budget_tokens > 0 {
                format!("[cyan]{} tokens[/]", config.thinking.budget_tokens)
            } else {
                "[grey]Unlimited[/]".to_string()
            },
        ]);
        let level = config.thinking.level.as_deref().unwrap_or("");
        table.row(&[
            "Thinking level",
            &if level.trim().is_empty() {
                "[grey]Not sent (server default)[/]".to_string()
            } else if let Some(l) = thinking_levels::resolve(&config.thinking) {
                format!(
                    "[cyan]{}[/] [grey](enable_thinking={}{})[/]",
                    escape(level),
                    l.enabled,
                    l.budget_tokens
                        .map(|b| format!(", budget {b} tokens"))
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "[red]Unknown level '{}' — not sent. Use: {}[/]",
                    escape(level),
                    thinking_levels::NAMES.join(", ")
                )
            },
        ]);
        let sampling =
            sampling_presets::resolve(&config.lm_studio, config.lm_studio.primary_model());
        let mut parts = vec![format!(
            "temp {}",
            sampling.temperature.unwrap_or(config.lm_studio.temperature)
        )];
        if let Some(v) = sampling.top_p {
            parts.push(format!("top_p {v}"));
        }
        if let Some(v) = sampling.top_k {
            parts.push(format!("top_k {v}"));
        }
        if let Some(v) = sampling.min_p {
            parts.push(format!("min_p {v}"));
        }
        if let Some(v) = sampling.presence_penalty {
            parts.push(format!("presence {v}"));
        }
        if let Some(v) = sampling.repetition_penalty {
            parts.push(format!("repeat {v}"));
        }
        let preset_note = match &sampling.preset {
            None => String::new(),
            Some(p) if sampling_presets::get(Some(p)).is_none() => {
                format!(" [red](unknown preset '{}')[/]", escape(p))
            }
            Some(p) => format!(" [grey](preset {})[/]", escape(p)),
        };
        table.row(&[
            "Sampling",
            &format!("[cyan]{}[/]{preset_note}", escape(&parts.join(", "))),
        ]);
        let enabled = config.mcp_servers.values().filter(|s| s.enabled).count();
        table.row(&[
            "MCP servers",
            &if config.mcp_servers.is_empty() {
                "[grey]None configured[/]".to_string()
            } else {
                format!("[cyan]{enabled}/{} enabled[/]", config.mcp_servers.len())
            },
        ]);
        table.write(ctx.console);
        CommandResult::Ok
    }
}

pub struct HelpCommand;

/// Commands the REPL handles itself (not in the registry).
const BUILT_INS: [(&str, &[&str], &str); 5] = [
    ("config", &[], "View or edit configuration"),
    (
        "retry",
        &["r!"],
        "Retry the last turn with the backup model",
    ),
    (
        "engine",
        &[],
        "Show or switch the engine: native, claude (Claude Code), codex",
    ),
    (
        "profile",
        &["profiles"],
        "Inference profiles: list, show <name>, switch <name> (server layout + client settings)",
    ),
    ("benchmark", &["bench"], "Run the benchmark suite"),
];

const GROUPS: [(&str, &[&str]); 5] = [
    (
        "Conversation",
        &[
            "clear", "compact", "context", "stats", "rewind", "retry", "copy", "export",
        ],
    ),
    ("Sessions", &["session", "fork", "rename"]),
    (
        "Model & modes",
        &[
            "model",
            "engine",
            "profile",
            "config",
            "plan",
            "ask",
            "uncensored",
        ],
    ),
    (
        "Files & project",
        &["add", "diff", "undo", "checkpoint", "init", "memory"],
    ),
    (
        "Agents & tools",
        &["agent", "agents", "review", "workflow", "docs", "benchmark"],
    ),
];

/// name (lowercase) -> (name, aliases, description), `customcommand` left out.
pub fn collect_commands(registry: &CommandRegistry) -> CommandInfo {
    let mut commands = BTreeMap::new();
    for c in registry.all() {
        if c.name() == "customcommand" {
            continue;
        }
        commands.insert(
            c.name().to_lowercase(),
            (
                c.name().to_string(),
                c.aliases().iter().map(|a| a.to_string()).collect(),
                c.description().to_string(),
            ),
        );
    }
    for (name, aliases, description) in BUILT_INS {
        commands.entry(name.to_string()).or_insert_with(|| {
            (
                name.to_string(),
                aliases.iter().map(|a| a.to_string()).collect(),
                description.to_string(),
            )
        });
    }
    commands
}

type CommandInfo = BTreeMap<String, (String, Vec<String>, String)>;

/// One titled group of the /help grid (only the commands that exist).
fn add_group(
    commands: &CommandInfo,
    placed: &mut BTreeSet<String>,
    title: &str,
    names: &[String],
    rows: &mut Vec<(String, String)>,
) {
    let present: Vec<&String> = names
        .iter()
        .filter(|n| commands.contains_key(&n.to_lowercase()))
        .collect();
    if present.is_empty() {
        return;
    }
    rows.push((format!("[bold]{}[/]", escape(title)), String::new()));
    for n in present {
        let (name, aliases, description) = &commands[&n.to_lowercase()];
        let alias = if aliases.is_empty() {
            String::new()
        } else {
            let a: Vec<String> = aliases.iter().map(|a| escape(a)).collect();
            format!(" [dim]/{}[/]", a.join(" /"))
        };
        rows.push((
            format!("  [cyan]/{}[/]{alias}", escape(name)),
            escape(description),
        ));
        placed.insert(n.to_lowercase());
    }
}

#[async_trait]
impl Command for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }
    fn description(&self) -> &'static str {
        "Show available commands"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["h", "?"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let commands = collect_commands(ctx.registry);
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut placed: BTreeSet<String> = BTreeSet::new();
        for (title, names) in GROUPS {
            let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            add_group(&commands, &mut placed, title, &names, &mut rows);
        }
        let others: Vec<String> = commands
            .iter()
            .filter(|(k, _)| !placed.contains(*k))
            .map(|(_, v)| v.0.clone())
            .collect();
        add_group(&commands, &mut placed, "Other", &others, &mut rows);
        write_grid(ctx.console, &rows);
        ctx.console.blank();
        let key = |k: &str, d: &str| (format!("  [cyan]{k}[/]"), d.to_string());
        let keys = vec![
            ("[bold]Input[/]".to_string(), String::new()),
            key(
                "Enter",
                "Send · end a line with [cyan]\\[/] to continue on the next line",
            ),
            key(
                "Tab",
                "Complete /commands and @file paths · accept a suggested next prompt",
            ),
            key("Up/Down", "Prompt history"),
            key(
                "Shift+Tab",
                "Cycle mode: Normal → Plan (read-only) → Ask (no tools)",
            ),
            key(
                "Ctrl+O",
                "Output detail: quiet → normal → full tool output → thinking → full thinking",
            ),
            key(
                "Ctrl+G",
                "Watch running agents (live transcript, message, cancel) · Esc returns",
            ),
            key(
                "Ctrl+C",
                "Cancel the running turn · press twice quickly to exit",
            ),
            key(
                "!cmd",
                "Run a shell command directly (not sent to the model)",
            ),
            key("@path", "Attach a file's contents to your message"),
            key(
                "Alt+V",
                "Paste from the clipboard: an image or copied files attach as a chip, text is typed · drop an image file on the terminal to attach it · Backspace removes a chip",
            ),
        ];
        write_grid(ctx.console, &keys);
        ctx.console.blank();
        ctx.console
            .markup_line("[bold]Settings[/] [dim](~/.openorca/config.json, or /config)[/]");
        let settings = vec![
            key(
                "toolProfile",
                "\"pi\" = Pi's read/bash/edit/write tools and prompt; \"openorca\" = full tool set",
            ),
            key(
                "lmStudio.sampling",
                "preset (e.g. \"qwen-thinking-coding\") or top_p/top_k/min_p/penalties",
            ),
            key(
                "thinking.level",
                "off · minimal · low · medium · high · unlimited (llama.cpp-style servers)",
            ),
            key(
                "agent.verificationMode",
                "auto · always · off — re-check changed files before finishing",
            ),
            key(
                "context.*",
                "autoCompactEnabled, reserveTokens, keepRecentTokens — compaction",
            ),
            key(
                "memory.includeInPrompt",
                "opt in to saved memories in the prompt ([cyan]/memory prompt on[/])",
            ),
        ];
        write_grid(ctx.console, &settings);
        CommandResult::Ok
    }
}
