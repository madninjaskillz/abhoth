//! `/agent` and `/agents` (`AgentCommands`): start a background agent, list agents, cancel
//! one, message one, show a result or print a transcript. They need the orchestrator
//! ([`super::AgentHost::manager`]); without one they say sub-agents are not available.

use async_trait::async_trait;
use orca_core::orchestration::palette;
use orca_core::orchestration::transcript::AgentTranscriptKind;
use orca_core::orchestration::types::AgentStatus;
use orca_core::orchestration::{AgentMessageResult, manager::preview};

use super::{Command, CommandContext, CommandResult};
use crate::ui::markup::escape;
use crate::ui::widgets::write_panel;

const NO_AGENTS: &str = "[yellow]Sub-agents are not available in this build.[/]";

/// An xterm-256 colour as `#rrggbb`.
pub fn xterm_hex(xterm: i32) -> String {
    let (r, g, b) = match xterm {
        16..=231 => {
            let i = xterm - 16;
            let level = |v: i32| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        232..=255 => {
            let v = 8 + (xterm - 232) * 10;
            (v, v, v)
        }
        _ => (128, 128, 128),
    };
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// `AgentBadge.Markup`: " A " in black on the bar's colour.
pub fn badge_markup(slot: i32) -> String {
    format!(
        "[bold #000000 on {}] {} [/]",
        xterm_hex(palette::xterm_of(slot)),
        palette::letter_of(slot)
    )
}

pub struct AgentCommand;

#[async_trait]
impl Command for AgentCommand {
    fn name(&self) -> &'static str {
        "agent"
    }
    fn description(&self) -> &'static str {
        "Start a background agent: /agent <type> <task>"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let Some(manager) = ctx.agents.and_then(|h| h.manager()) else {
            ctx.console.markup_line(NO_AGENTS);
            return CommandResult::Ok;
        };
        let o = manager.orchestrator().clone();
        let (default, resolves_first) = {
            let registry = o.type_registry();
            (
                registry.get_default().name,
                args.first().is_some_and(|a| registry.resolve(a).is_some()),
            )
        };
        if args.is_empty() {
            let mut names = o.type_registry().get_all_names();
            names.sort();
            ctx.console
                .markup_line("[yellow]Usage: /agent [[type]] <task>[/]");
            ctx.console.markup_line(&format!(
                "[dim]Types: {} (default: {})[/]",
                escape(&names.join(", ")),
                default
            ));
            return CommandResult::Ok;
        }
        // First word is the type when it names one; otherwise the whole line is the task.
        let type_name = if resolves_first && args.len() > 1 {
            args[0].clone()
        } else {
            default
        };
        let task = if type_name == args[0] {
            args[1..].join(" ")
        } else {
            args.join(" ")
        };
        let agent = manager.start_user_agent(&task, &type_name);
        if agent.status() == AgentStatus::Failed {
            ctx.console.markup_line(&format!(
                "[red]Could not start agent: {}[/]",
                escape(&agent.error().unwrap_or_else(|| "unknown error".into()))
            ));
            return CommandResult::Ok;
        }
        ctx.console.markup_line(&format!(
            "[green]\u{25cf} Started[/] {} [bold]{}[/] [dim]({} \u{b7} #{} {}) in the background \u{2014} \u{2193} or Ctrl+G to select its bar, or /agents {}; its result is handed to the main agent on your next message.[/]",
            badge_markup(agent.color_slot),
            escape(&agent.tag()),
            escape(&agent.label()),
            agent.number,
            escape(&agent.agent_type),
            agent.number
        ));
        CommandResult::Ok
    }
}

pub struct AgentsCommand;

/// A transcript entry as a plain line (`AgentView.Format`, without the colours).
fn entry_line(kind: AgentTranscriptKind, text: &str, tool: Option<&str>) -> String {
    match kind {
        AgentTranscriptKind::Task => format!("task: {text}"),
        AgentTranscriptKind::ToolCall => format!("\u{25b8} {} {text}", tool.unwrap_or("")),
        AgentTranscriptKind::ToolResult => format!("  \u{21b3} {}", preview(text, 200)),
        AgentTranscriptKind::UserMessage => format!("you: {text}"),
        AgentTranscriptKind::AssistantText => text.to_string(),
        _ => format!("\u{2022} {text}"),
    }
}

#[async_trait]
impl Command for AgentsCommand {
    fn name(&self) -> &'static str {
        "agents"
    }
    fn description(&self) -> &'static str {
        "List and watch agents (Ctrl+G): /agents [n|cancel n|msg n text|result n]"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let Some(manager) = ctx.agents.and_then(|h| h.manager()) else {
            ctx.console.markup_line(NO_AGENTS);
            return CommandResult::Ok;
        };
        let o = manager.orchestrator().clone();
        let mut agents = o.get_all_agents();
        agents.sort_by_key(|a| a.number);
        let watch = |ctx: &CommandContext<'_>, a: &orca_core::orchestration::AgentContext| {
            // No interactive agent view: print what we have.
            for e in a.transcript.snapshot() {
                ctx.console
                    .write_line(&entry_line(e.kind, &e.text, e.tool.as_deref()));
            }
        };
        if args.is_empty() {
            if agents.is_empty() {
                ctx.console.markup_line("[dim]No agents yet. Start one with[/] [cyan]/agent <type> <task>[/][dim], or let the main agent delegate.[/]");
                return CommandResult::Ok;
            }
            let running: Vec<_> = agents.iter().filter(|a| a.is_active()).collect();
            if running.len() == 1 {
                watch(ctx, running[0]);
            } else {
                // No arrow-key picker here: list them, newest running first.
                let mut list = agents.clone();
                list.sort_by_key(|a| {
                    (
                        std::cmp::Reverse(a.is_active()),
                        std::cmp::Reverse(a.number),
                    )
                });
                for a in list {
                    let state = if a.is_active() {
                        a.current_activity().unwrap_or_else(|| "running".into())
                    } else {
                        a.status().as_str().to_lowercase()
                    };
                    ctx.console.write_line(&format!(
                        "{} \u{b7} #{} {} \u{b7} {state} \u{2014} {}",
                        a.tag(),
                        a.number,
                        a.agent_type,
                        preview(&a.task, 60)
                    ));
                }
            }
            return CommandResult::Ok;
        }
        let verb = args[0].to_lowercase();
        match verb.as_str() {
            "cancel" | "kill" | "stop" if args.len() >= 2 => {
                ctx.console.markup_line(&if o.cancel_agent(&args[1]) {
                    format!("[yellow]Cancelling agent {}\u{2026}[/]", escape(&args[1]))
                } else {
                    format!("[red]No running agent {}.[/]", escape(&args[1]))
                });
                return CommandResult::Ok;
            }
            "msg" | "message" | "send" if args.len() >= 3 => {
                let text = args[2..].join(" ");
                let id = escape(&args[1]);
                ctx.console.markup_line(&match o.send_message(&args[1], &text) {
                    AgentMessageResult::Queued => {
                        format!("[green]Queued for agent {id} \u{2014} delivered at its next turn.[/]")
                    }
                    AgentMessageResult::Resumed => {
                        format!("[green]Agent {id} resumed with your message.[/]")
                    }
                    AgentMessageResult::NotAccepting => format!(
                        "[yellow]Agent {id} finished and released its context; only /agent agents can be resumed.[/]"
                    ),
                    AgentMessageResult::NotFound => format!("[red]No agent {id}.[/]"),
                });
                return CommandResult::Ok;
            }
            "result" if args.len() >= 2 => {
                let Some(done) = o.find_agent(&args[1]) else {
                    ctx.console
                        .markup_line(&format!("[red]No agent {}.[/]", escape(&args[1])));
                    return CommandResult::Ok;
                };
                let body = done.result().or_else(|| done.error()).unwrap_or_else(|| {
                    if done.is_active() {
                        "(still running)".into()
                    } else {
                        "(no result)".into()
                    }
                });
                write_panel(
                    ctx.console,
                    &escape(body.trim_end()),
                    &format!(
                        "[bold]#{} {}[/] [dim]{}[/]",
                        done.number,
                        escape(&done.agent_type),
                        done.status().as_str()
                    ),
                    "default",
                );
                return CommandResult::Ok;
            }
            _ => {}
        }
        if let Some(agent) = o.find_agent(&args[0]) {
            watch(ctx, &agent);
            return CommandResult::Ok;
        }
        ctx.console
            .markup_line("[yellow]Usage: /agents [[n | cancel n | msg n <text> | result n]][/]");
        CommandResult::Ok
    }
}
