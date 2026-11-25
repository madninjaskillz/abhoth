//! OpenOrca.Commands.Session: /compact, /fork, /plan, /rename, /rewind, /session.

use async_trait::async_trait;

use super::{Command, CommandContext, CommandResult, PLAN_TOGGLE_SENTINEL};
use crate::ui::markup::escape;
use crate::ui::widgets::{Table, write_tree};

pub struct CompactCommand;

/// The colour of a compaction notice (the C# compactor prints them in these colours).
pub fn compaction_notice_markup(notice: &str) -> String {
    let color = if notice.starts_with("Compacted") {
        "green"
    } else if notice.starts_with("Failed") {
        "red"
    } else {
        "yellow"
    };
    format!("[{color}]{}[/]", escape(notice))
}

#[async_trait]
impl Command for CompactCommand {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn description(&self) -> &'static str {
        "Compact conversation context with summary"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let extra = (!args.is_empty()).then(|| args.join(" "));
        let report = ctx.session.compact_now(extra.as_deref()).await;
        for n in &report.notices {
            ctx.console.markup_line(&compaction_notice_markup(n));
        }
        CommandResult::Ok
    }
}

pub struct ForkCommand;

#[async_trait]
impl Command for ForkCommand {
    fn name(&self) -> &'static str {
        "fork"
    }
    fn description(&self) -> &'static str {
        "Fork current session (creates a branch)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["f!"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let parent_id = match ctx.state.current_session_id.clone() {
            Some(id) => id,
            None => match ctx.sessions.save(&ctx.session.messages, None, None) {
                Ok(id) => {
                    ctx.state.current_session_id = Some(id.clone());
                    id
                }
                Err(e) => {
                    ctx.console.markup_line(&format!(
                        "[red]Failed to save current session before forking: {}[/]",
                        escape(&e.to_string())
                    ));
                    return CommandResult::Ok;
                }
            },
        };
        let title = (!args.is_empty()).then(|| args.join(" "));
        // `conversation.Messages.Count`: without the system prompt.
        let index = ctx.session.conversation().messages().len() as i32;
        match ctx
            .sessions
            .fork(&ctx.session.messages, title.as_deref(), &parent_id, index)
        {
            Ok(new_id) => {
                let shown = title.unwrap_or_else(|| format!("Fork of {parent_id}"));
                ctx.console.markup_line(&format!(
                    "[green]Forked as session {new_id}: {}[/]",
                    escape(&shown)
                ));
                ctx.console.markup_line(
                    "[grey]Current session continues unaffected. Use /session load to switch.[/]",
                );
            }
            Err(e) => ctx.console.markup_line(&format!(
                "[red]Failed to fork session: {}[/]",
                escape(&e.to_string())
            )),
        }
        CommandResult::Ok
    }
}

pub struct PlanCommand;

#[async_trait]
impl Command for PlanCommand {
    fn name(&self) -> &'static str {
        "plan"
    }
    fn description(&self) -> &'static str {
        "Toggle plan mode (plan before executing)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["p"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        match args.first().map(|a| a.to_lowercase()).as_deref() {
            Some("on") => ctx.state.set_plan_mode(true),
            Some("off") => ctx.state.set_plan_mode(false),
            _ => {
                let on = !ctx.state.plan_mode();
                ctx.state.set_plan_mode(on);
            }
        }
        if ctx.state.plan_mode() {
            ctx.console
                .markup_line("[cyan]Plan mode enabled[/] — I'll plan before executing.");
        } else {
            ctx.console
                .markup_line("[grey]Plan mode disabled[/] — back to normal mode.");
        }
        CommandResult::RunAgent(PLAN_TOGGLE_SENTINEL.into())
    }
}

pub struct RenameCommand;

#[async_trait]
impl Command for RenameCommand {
    fn name(&self) -> &'static str {
        "rename"
    }
    fn description(&self) -> &'static str {
        "Rename current session"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        if args.is_empty() {
            ctx.console.markup_line("[yellow]Usage: /rename <new name>[/]");
            return CommandResult::Ok;
        }
        let Some(id) = ctx.state.current_session_id.clone() else {
            ctx.console
                .markup_line("[yellow]No active session. Save first with /session save.[/]");
            return CommandResult::Ok;
        };
        let title = args.join(" ");
        match ctx
            .sessions
            .save(&ctx.session.messages, Some(&title), Some(&id))
        {
            Ok(id) => {
                ctx.state.current_session_id = Some(id);
                ctx.console.markup_line(&format!(
                    "[green]Session renamed to: {}[/]",
                    escape(&title)
                ));
            }
            // C# lets the exception reach the REPL's error line.
            Err(e) => ctx
                .console
                .markup_line(&format!("[red]{}[/]", escape(&e.to_string()))),
        }
        CommandResult::Ok
    }
}

pub struct RewindCommand;

#[async_trait]
impl Command for RewindCommand {
    fn name(&self) -> &'static str {
        "rewind"
    }
    fn description(&self) -> &'static str {
        "Remove last N turn(s) from conversation"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let turns = args
            .first()
            .and_then(|a| a.trim().parse::<i32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1);
        let mut conversation = ctx.session.conversation();
        let removed = conversation.remove_last_turns(turns as usize);
        ctx.session.set_conversation(conversation);
        if removed > 0 {
            ctx.console.markup_line(&format!(
                "[green]Rewound {turns} turn(s) (removed {removed} messages).[/]"
            ));
        } else {
            ctx.console.markup_line("[yellow]No messages to rewind.[/]");
        }
        CommandResult::Ok
    }
}

pub struct SessionCommand;

#[async_trait]
impl Command for SessionCommand {
    fn name(&self) -> &'static str {
        "session"
    }
    fn description(&self) -> &'static str {
        "Manage sessions (list/save/load/delete/tree)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["s"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let action = args
            .first()
            .map(|a| a.to_lowercase())
            .unwrap_or_else(|| "list".into());
        match action.as_str() {
            "list" => {
                let sessions = ctx.sessions.list();
                if sessions.is_empty() {
                    ctx.console.markup_line("[grey]No saved sessions.[/]");
                    return CommandResult::Ok;
                }
                let mut table = Table::new()
                    .expand()
                    .column("ID")
                    .column("Title")
                    .column("Updated")
                    .column("Messages");
                for s in sessions.iter().take(20) {
                    let units: Vec<u16> = s.title.encode_utf16().collect();
                    let title = if units.len() > 40 {
                        format!("{}...", String::from_utf16_lossy(&units[..37]))
                    } else {
                        s.title.clone()
                    };
                    table.row(&[
                        escape(&s.id),
                        escape(&title),
                        local_minutes(s.updated_at.to_system_time()),
                        s.messages.len().to_string(),
                    ]);
                }
                table.write(ctx.console);
            }
            "save" => {
                let title = (args.len() > 1).then(|| args[1..].join(" "));
                match ctx.sessions.save(
                    &ctx.session.messages,
                    title.as_deref(),
                    ctx.state.current_session_id.as_deref(),
                ) {
                    Ok(id) => {
                        ctx.console
                            .markup_line(&format!("[green]Session saved: {id}[/]"));
                        ctx.state.current_session_id = Some(id);
                    }
                    Err(e) => ctx
                        .console
                        .markup_line(&format!("[red]{}[/]", escape(&e.to_string()))),
                }
            }
            "load" => {
                let Some(id) = args.get(1) else {
                    ctx.console
                        .markup_line("[yellow]Usage: /session load <id>[/]");
                    return CommandResult::Ok;
                };
                let data = match ctx.sessions.load(id) {
                    Ok(Some(d)) => d,
                    Ok(None) | Err(_) => {
                        ctx.console.markup_line(&format!(
                            "[red]Session not found: {}[/]",
                            escape(id)
                        ));
                        return CommandResult::Ok;
                    }
                };
                // The live system prompt is kept: it was built for the current tools and mode.
                let restored = orca_core::session::session_to_messages(&data);
                let live_prompt = ctx
                    .session
                    .messages
                    .first()
                    .filter(|m| m.role == orca_core::client::Role::System)
                    .cloned();
                let mut messages: Vec<_> = restored
                    .iter()
                    .filter(|m| m.role != orca_core::client::Role::System)
                    .cloned()
                    .collect();
                let system = live_prompt.or_else(|| {
                    restored
                        .first()
                        .filter(|m| m.role == orca_core::client::Role::System)
                        .cloned()
                });
                if let Some(s) = system {
                    messages.insert(0, s);
                }
                let conversation = orca_core::chat::Conversation::from_api_messages(
                    messages,
                    Default::default(),
                );
                ctx.session.set_conversation(conversation);
                ctx.state.current_session_id = Some(data.id.clone());
                ctx.session.tool_hooks.session_id = Some(data.id.clone());
                ctx.console.markup_line(&format!(
                    "[green]Session loaded: {} ({} messages)[/]",
                    escape(&data.title),
                    data.messages.len()
                ));
            }
            "delete" => {
                let Some(id) = args.get(1) else {
                    ctx.console
                        .markup_line("[yellow]Usage: /session delete <id>[/]");
                    return CommandResult::Ok;
                };
                if ctx.sessions.delete(id).unwrap_or(false) {
                    ctx.console
                        .markup_line(&format!("[green]Session deleted: {id}[/]"));
                } else {
                    ctx.console.markup_line(&format!(
                        "[red]Session not found: {}[/]",
                        escape(id)
                    ));
                }
            }
            "tree" => {
                let tree = ctx.sessions.session_tree();
                if tree.is_empty() {
                    ctx.console.markup_line("[grey]No saved sessions.[/]");
                    return CommandResult::Ok;
                }
                let current = ctx.state.current_session_id.clone();
                let nodes: Vec<(String, usize)> = tree
                    .iter()
                    .map(|(s, depth)| {
                        let mut label = if current.as_deref() == Some(s.id.as_str()) {
                            format!(
                                "[green bold]{}[/] [green]{}[/] [grey]({} msgs)[/]",
                                escape(&s.id),
                                escape(&s.title),
                                s.messages.len()
                            )
                        } else {
                            format!(
                                "[cyan]{}[/] {} [grey]({} msgs)[/]",
                                escape(&s.id),
                                escape(&s.title),
                                s.messages.len()
                            )
                        };
                        if let Some(p) = &s.parent_session_id {
                            label.push_str(&format!(" [dim]<- fork of {}[/]", escape(p)));
                        }
                        (label, *depth)
                    })
                    .collect();
                write_tree(ctx.console, "[bold]Sessions[/]", &nodes);
            }
            _ => ctx.console.markup_line(
                "[yellow]Usage: /session list|save [[name]]|load <id>|delete <id>|tree[/]",
            ),
        }
        CommandResult::Ok
    }
}

/// `yyyy-MM-dd HH:mm` in local time.
pub(crate) fn local_minutes(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}
