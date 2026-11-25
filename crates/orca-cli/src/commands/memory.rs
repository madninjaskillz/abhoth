//! OpenOrca.Commands.Memory: /memory.

use async_trait::async_trait;
use orca_core::config::memory_manager::MemoryManager;
use orca_core::config::project_instructions;

use super::{Command, CommandContext, CommandResult};
use crate::ui::markup::escape;
use crate::ui::widgets::write_panel;

pub struct MemoryCommand;

fn on(arg: &str) -> bool {
    matches!(arg.to_lowercase().as_str(), "on" | "true" | "1")
}

#[async_trait]
impl Command for MemoryCommand {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn description(&self) -> &'static str {
        "View/edit ORCA.md and auto memory"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let action = args
            .first()
            .map(|a| a.to_lowercase())
            .unwrap_or_else(|| "show".into());
        let cwd = ctx.cwd();
        let memory = MemoryManager::new(&ctx.session.config).with_cwd(cwd.clone());
        let mem = &mut ctx.session.config.memory;
        match action.as_str() {
            "show" => {
                match project_instructions::load(&cwd) {
                    Some(content) => {
                        write_panel(ctx.console, &escape(&content), "[cyan]ORCA.md[/]", "cyan1")
                    }
                    None => {
                        let path = project_instructions::get_instructions_path(&cwd)
                            .unwrap_or_else(|| cwd.clone());
                        ctx.console.markup_line(&format!(
                            "[yellow]No ORCA.md found. Create one at: {}[/]",
                            escape(&path.to_string_lossy())
                        ));
                    }
                }
                let auto = memory.load_all_memory();
                if !auto.trim().is_empty() {
                    write_panel(
                        ctx.console,
                        &escape(&auto),
                        if mem.include_in_prompt {
                            "[magenta]Auto Memory (in system prompt)[/]"
                        } else {
                            "[magenta]Auto Memory (not in prompt — /memory prompt on)[/]"
                        },
                        "magenta1",
                    );
                }
            }
            "edit" => {
                let path = match project_instructions::get_instructions_path(&cwd) {
                    Some(p) => p,
                    None => {
                        let root = project_instructions::find_project_root(&cwd)
                            .unwrap_or_else(|| cwd.clone());
                        let p = root.join("ORCA.md");
                        if !p.exists() {
                            let _ = std::fs::write(
                                &p,
                                "# Project Instructions\n\n<!-- Add project-specific instructions for Abhoth here -->\n",
                            );
                        }
                        p
                    }
                };
                let editor = std::env::var("EDITOR")
                    .unwrap_or_else(|_| if cfg!(windows) { "notepad" } else { "nano" }.to_string());
                match std::process::Command::new(&editor).arg(&path).status() {
                    Ok(_) => ctx.console.markup_line("[green]ORCA.md updated.[/]"),
                    Err(e) => ctx.console.markup_line(&format!(
                        "[red]Failed to open editor: {}[/]",
                        escape(&e.to_string())
                    )),
                }
            }
            "auto" => {
                if let Some(t) = args.get(1) {
                    mem.auto_memory_enabled = on(t);
                    ctx.console.markup_line(if mem.auto_memory_enabled {
                        "[green]Auto memory enabled.[/]"
                    } else {
                        "[grey]Auto memory disabled.[/]"
                    });
                } else {
                    let (c, w) = if mem.auto_memory_enabled {
                        ("green", "enabled")
                    } else {
                        ("grey", "disabled")
                    };
                    ctx.console
                        .markup_line(&format!("Auto memory is [{c}]{w}[/]."));
                }
            }
            "prompt" => {
                if let Some(t) = args.get(1) {
                    mem.include_in_prompt = on(t);
                    ctx.console.markup_line(if mem.include_in_prompt {
                        "[green]Memory files will be included in the system prompt (from the next session or /clear).[/]"
                    } else {
                        "[grey]Memory files will not be included in the system prompt.[/]"
                    });
                } else {
                    let (c, w) = if mem.include_in_prompt {
                        ("green", "on")
                    } else {
                        ("grey", "off")
                    };
                    ctx.console
                        .markup_line(&format!("Memory in system prompt is [{c}]{w}[/]."));
                }
            }
            "list" => {
                let files = memory.list();
                if files.is_empty() {
                    ctx.console.markup_line("[grey]No auto memory files.[/]");
                }
                for (path, preview) in files {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    ctx.console.markup_line(&format!(
                        "  [cyan]{}[/] — {}",
                        escape(&name),
                        escape(&preview)
                    ));
                }
            }
            "clear-auto" => {
                memory.clear();
                ctx.console
                    .markup_line("[green]Auto memory files cleared.[/]");
            }
            _ => ctx.console.markup_line(
                "[yellow]Usage: /memory [[show|edit|auto on/off|prompt on/off|list|clear-auto]][/]",
            ),
        }
        CommandResult::Ok
    }
}
