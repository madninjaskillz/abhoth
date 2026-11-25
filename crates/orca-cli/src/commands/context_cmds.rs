//! OpenOrca.Commands.Context: /context, /copy, /export, /stats.

use async_trait::async_trait;
use orca_core::chat::CompactionSettings;
use orca_core::client::Role;

use super::{Command, CommandContext, CommandResult, n0};
use crate::ui::markup::escape;
use crate::ui::widgets::Table;

pub struct ContextCommand;

#[async_trait]
impl Command for ContextCommand {
    fn name(&self) -> &'static str {
        "context"
    }
    fn description(&self) -> &'static str {
        "Show context window usage"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["ctx"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let config = &ctx.session.config;
        let conversation = ctx.session.conversation();
        let estimated = conversation.estimate_context_tokens(config.context.chars_per_token);
        let compaction = CompactionSettings::from_config(&config.context);
        let window = config.context.context_window_size;
        let usage = if window > 0 {
            estimated as f32 / window as f32 * 100.0
        } else {
            0.0
        };
        let color = if usage >= 90.0 {
            "red"
        } else if usage >= 70.0 {
            "yellow"
        } else {
            "green"
        };
        let mut table = Table::new()
            .expand()
            .title("[bold]Context Window[/]")
            .column("[bold]Metric[/]")
            .column("[bold]Value[/]");
        table.row(&[
            "Estimated tokens".into(),
            format!("[{color}]{}[/]", n0(estimated as i64)),
        ]);
        table.row(&["Context window".into(), n0(window as i64)]);
        table.row(&["Usage".into(), format!("[{color}]{usage:.1}%[/]")]);
        table.row(&[
            "Token source",
            if conversation.has_usage_base() {
                "server usage + estimate of newer messages"
            } else {
                "chars-per-token estimate"
            },
        ]);
        if compaction.planned_trigger_tokens < compaction.trigger_tokens() {
            table.row(&[
                "Planned compaction".into(),
                format!(
                    "{} ({:.0}% of the window; thinking-aware{})",
                    n0(compaction.planned_trigger_tokens as i64),
                    config.context.compact_at_ratio * 100.0,
                    if config.context.compact_cost_gate {
                        ", when it pays for the cache break"
                    } else {
                        ""
                    }
                ),
            ]);
        }
        table.row(&[
            "Auto-compact at".into(),
            format!(
                "{} (window − reserve {}; keeps ~{}) ({})",
                n0(compaction.trigger_tokens() as i64),
                n0(compaction.reserve_tokens as i64),
                n0(compaction.keep_recent_tokens as i64),
                if config.context.auto_compact_enabled {
                    "[green]enabled[/]"
                } else {
                    "[grey]disabled[/]"
                }
            ),
        ]);
        let system_tokens = conversation
            .system_prompt()
            .map_or(0, |s| s.encode_utf16().count() / 4);
        table.row(&[
            "System prompt tokens".into(),
            format!("~{}", n0(system_tokens as i64)),
        ]);
        table.row(&["", ""]);
        table.row(&["[yellow]Messages[/]", ""]);
        for (role, count) in conversation.get_message_count_by_role() {
            table.row(&[format!("  {role}"), count.to_string()]);
        }
        table.row(&["  total".into(), conversation.messages().len().to_string()]);
        table.write(ctx.console);
        let width = 40usize;
        let filled = ((usage / 100.0 * width as f32) as i64).clamp(0, width as i64) as usize;
        let bar = format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled));
        ctx.console
            .markup_line(&format!("  [{color}]{}[/]", escape(&bar)));
        CommandResult::Ok
    }
}

pub struct CopyCommand;

/// The response with `<think>…</think>` blocks removed (case-insensitive), trimmed.
pub fn strip_think_blocks(text: &str) -> String {
    let re = regex::Regex::new(r"(?is)<think>.*?</think>").expect("static pattern");
    re.replace_all(text, "").trim().to_string()
}

#[async_trait]
impl Command for CopyCommand {
    fn name(&self) -> &'static str {
        "copy"
    }
    fn description(&self) -> &'static str {
        "Copy last response to clipboard"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["cp"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let Some(last) = ctx
            .session
            .last_assistant_response
            .clone()
            .filter(|r| !r.is_empty())
        else {
            ctx.console
                .markup_line("[yellow]No assistant response to copy.[/]");
            return CommandResult::Ok;
        };
        let cleaned = strip_think_blocks(&last);
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd.exe", &["/c", "clip"])
        } else if cfg!(target_os = "macos") {
            ("pbcopy", &[])
        } else {
            ("xclip", &["-selection", "clipboard"])
        };
        let result = async {
            use tokio::io::AsyncWriteExt;
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(cleaned.as_bytes()).await?;
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            std::io::Result::Ok(())
        }
        .await;
        match result {
            Ok(()) => ctx.console.markup_line(&format!(
                "[green]Copied {} chars to clipboard.[/]",
                cleaned.encode_utf16().count()
            )),
            Err(e) => ctx.console.markup_line(&format!(
                "[red]Failed to copy to clipboard: {}[/]",
                escape(&e.to_string())
            )),
        }
        CommandResult::Ok
    }
}

pub struct ExportCommand;

/// The markdown `/export` writes (`now` is the local time it is written at).
pub fn export_markdown(
    system_prompt: Option<&str>,
    messages: &[orca_core::client::ChatMessage],
    now: &str,
) -> String {
    let mut lines: Vec<String> = vec![
        "# Abhoth Conversation Export".into(),
        format!("Exported: {now}"),
        String::new(),
    ];
    if let Some(sp) = system_prompt {
        lines.push("## System Prompt".into());
        let units: Vec<u16> = sp.encode_utf16().collect();
        let sp = if units.len() > 2000 {
            format!(
                "{}\n... (truncated)",
                String::from_utf16_lossy(&units[..2000])
            )
        } else {
            sp.to_string()
        };
        lines.push("```".into());
        lines.push(sp);
        lines.push("```".into());
        lines.push(String::new());
    }
    for m in messages {
        let role = m.role.as_str();
        let mut cs = role.chars();
        let title = cs
            .next()
            .map(|c| c.to_uppercase().collect::<String>() + cs.as_str())
            .unwrap_or_default();
        lines.push(format!("## {title}"));
        // A tool message's text is its result (shown below), as FunctionResultContent.
        if m.role != Role::Tool
            && let Some(text) = m.content.as_deref().filter(|t| !t.trim().is_empty())
        {
            lines.push(text.to_string());
        }
        for call in &m.tool_calls {
            lines.push(format!("**Tool call:** `{}`", call.name));
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                && let Ok(json) = orca_core::config::dotnet_json::to_string_dotnet(&v)
            {
                lines.push("```json".into());
                lines.push(json);
                lines.push("```".into());
            }
        }
        if m.role == Role::Tool {
            lines.push("**Tool result:**".into());
            lines.push("```".into());
            let text = m.content.clone().unwrap_or_else(|| "(no result)".into());
            let units: Vec<u16> = text.encode_utf16().collect();
            lines.push(if units.len() > 8000 {
                format!(
                    "{}\n... (truncated)",
                    String::from_utf16_lossy(&units[..8000])
                )
            } else {
                text
            });
            lines.push("```".into());
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

#[async_trait]
impl Command for ExportCommand {
    fn name(&self) -> &'static str {
        "export"
    }
    fn description(&self) -> &'static str {
        "Export conversation to markdown"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let now = chrono::Local::now();
        let path = args
            .first()
            .cloned()
            .unwrap_or_else(|| format!("abhoth-export-{}.md", now.format("%Y%m%d-%H%M%S")));
        let conversation = ctx.session.conversation();
        let text = export_markdown(
            conversation.system_prompt(),
            conversation.messages(),
            &now.format("%Y-%m-%d %H:%M:%S").to_string(),
        );
        let full = orca_abstractions::full_path(&ctx.cwd(), &path);
        let result = (|| {
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&full, text)
        })();
        match result {
            Ok(()) => ctx.console.markup_line(&format!(
                "[green]Exported conversation to {} ({} messages)[/]",
                escape(&path),
                conversation.messages().len()
            )),
            Err(e) => ctx.console.markup_line(&format!(
                "[red]Failed to export: {}[/]",
                escape(&e.to_string())
            )),
        }
        CommandResult::Ok
    }
}

pub struct StatsCommand;

/// `TimeSpan.ToString(@"hh\:mm\:ss")`.
pub fn hh_mm_ss(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

#[async_trait]
impl Command for StatsCommand {
    fn name(&self) -> &'static str {
        "stats"
    }
    fn description(&self) -> &'static str {
        "Show session statistics"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["cost"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let conversation = ctx.session.conversation();
        let tokens = ctx.session.total_output_tokens;
        let turns = ctx.state.total_turns as i64;
        let mut table = Table::new()
            .expand()
            .title("[bold]Session Stats[/]")
            .column("[bold]Metric[/]")
            .column("[bold]Value[/]");
        table.row(&[
            "Session duration".into(),
            hh_mm_ss(ctx.state.session_duration()),
        ]);
        table.row(&["Total turns".into(), turns.to_string()]);
        table.row(&["Output tokens".into(), n0(tokens)]);
        table.row(&[
            "Messages in context".into(),
            conversation.messages().len().to_string(),
        ]);
        table.row(&[
            "Context tokens".into(),
            format!(
                "~{}",
                n0(
                    conversation.estimate_token_count(ctx.session.config.context.chars_per_token)
                        as i64
                )
            ),
        ]);
        if turns > 0 {
            table.row(&["Avg tokens/turn".into(), n0(tokens / turns)]);
        }
        // Usage per provider (local vs a named provider such as Haiku in a mixed swarm).
        for usage in orca_core::client::ProviderUsageLedger::shared().snapshot() {
            table.row(&[
                format!("Usage: {}", escape(&usage.provider)),
                escape(&usage.to_string()),
            ]);
        }
        table.write(ctx.console);
        CommandResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::client::{ChatMessage, ToolCall};

    #[test]
    fn strip_think_blocks_removes_reasoning() {
        assert_eq!(strip_think_blocks("<THINK>x\ny</think> Answer "), "Answer");
    }

    #[test]
    fn export_markdown_lists_messages_calls_and_results() {
        let mut call = ChatMessage::assistant("");
        call.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        });
        let text = export_markdown(
            Some("sys"),
            &[
                ChatMessage::user("hi"),
                call,
                ChatMessage::tool_result("c1", "1: a"),
            ],
            "2026-11-25 10:00:00",
        );
        assert!(text.starts_with("# Abhoth Conversation Export\nExported: 2026-11-25 10:00:00\n\n## System Prompt\n```\nsys\n```\n"));
        assert!(text.contains("## User\nhi\n"));
        assert!(text.contains("**Tool call:** `read_file`\n```json\n{"));
        assert!(text.contains("## Tool\n**Tool result:**\n```\n1: a\n```"));
    }

    #[test]
    fn hh_mm_ss_formats_durations() {
        assert_eq!(hh_mm_ss(std::time::Duration::from_secs(3725)), "01:02:05");
    }
}
