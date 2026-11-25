//! OpenOrca.Commands.Agent: /docs, /review, /workflow. /review and /workflow run sub-agents
//! through [`super::AgentHost`] (the orchestration port installs one); without a host they say
//! sub-agents are not available.

use async_trait::async_trait;
use serde_json::json;

use super::{Command, CommandContext, CommandResult, n0};
use crate::ui::markup::escape;
use crate::ui::widgets::write_panel;

pub struct DocsCommand;

/// `Context7Helper.FindTools`: the MCP tools ending in `resolve-library-id` and
/// `get-library-docs` / `query-docs`.
fn context7_tools(
    registry: &orca_core::agent::ToolRegistry,
) -> Option<(
    std::sync::Arc<dyn orca_abstractions::Tool>,
    std::sync::Arc<dyn orca_abstractions::Tool>,
)> {
    let mut resolve = None;
    let mut query = None;
    for tool in registry.all() {
        let name = tool.name().to_lowercase();
        if name.ends_with("resolve-library-id") {
            resolve = Some(tool.clone());
        } else if name.ends_with("get-library-docs") || name.ends_with("query-docs") {
            query = Some(tool.clone());
        }
    }
    Some((resolve?, query?))
}

#[async_trait]
impl Command for DocsCommand {
    fn name(&self) -> &'static str {
        "docs"
    }
    fn description(&self) -> &'static str {
        "Fetch library docs via Context7"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["doc"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let Some((resolve, query_tool)) = context7_tools(&ctx.session.registry) else {
            ctx.console
                .markup_line("[yellow]Context7 is not configured.[/]");
            ctx.console.markup_line(
                "[grey]To enable, add a Context7 MCP server to your config.json mcpServers section.[/]",
            );
            return CommandResult::Ok;
        };
        if args.is_empty() {
            ctx.console
                .markup_line("[yellow]Usage: /docs <library> [[query]][/]");
            ctx.console.markup_line(
                "[grey]Examples: /docs react hooks | /docs express middleware routing[/]",
            );
            return CommandResult::Ok;
        }
        let library = &args[0];
        let query = (args.len() > 1).then(|| args[1..].join(" "));
        let tctx = ctx.session.ctx.clone();
        let resolved = resolve
            .execute(&json!({ "libraryName": library }), &tctx)
            .await;
        let docs = if resolved.is_error || resolved.content.trim().is_empty() {
            None
        } else {
            let id = resolved.content.trim().to_string();
            let a = match &query {
                Some(q) => json!({ "libraryId": id, "query": q }),
                None => json!({ "libraryId": id }),
            };
            let r = query_tool.execute(&a, &tctx).await;
            (!r.is_error && !r.content.trim().is_empty()).then_some(r.content)
        };
        let Some(docs) = docs else {
            ctx.console.markup_line(&format!(
                "[red]Failed to fetch documentation for '{}'.[/]",
                escape(library)
            ));
            ctx.console.markup_line(
                "[grey]The library may not be available in Context7, or the server may be unreachable.[/]",
            );
            return CommandResult::Ok;
        };
        let units: Vec<u16> = docs.encode_utf16().collect();
        let preview = if units.len() > 500 {
            format!(
                "{}\n... (truncated in preview)",
                String::from_utf16_lossy(&units[..500])
            )
        } else {
            docs.clone()
        };
        write_panel(
            ctx.console,
            &escape(preview.trim_end()),
            &format!("[cyan]Documentation: {}[/]", escape(library)),
            "cyan1",
        );
        ctx.console.markup_line(&format!(
            "[green]Injected {} chars of documentation into context.[/]",
            n0(units.len() as i64)
        ));
        let note = query.map(|q| format!(" (query: {q})")).unwrap_or_default();
        ctx.session
            .messages
            .push(orca_core::client::ChatMessage::user(format!(
                "[Documentation for {library}{note} via Context7]\n{docs}\n[/Documentation]"
            )));
        CommandResult::Ok
    }
}

pub struct ReviewCommand;

/// The review agent's task for `/review [staged|<file>|<commit>]`.
pub fn review_task(args: &[String]) -> String {
    match args.first() {
        None => "Review the uncommitted changes in this git repository. Use git_status and git_diff to understand what changed, then examine the modified files for bugs, quality issues, and suggestions.".into(),
        Some(a) if a.eq_ignore_ascii_case("staged") => "Review the staged changes in this git repository. Use git_diff with the staged option to see only staged changes, then examine the modified files for bugs, quality issues, and suggestions.".into(),
        Some(a) if a.contains('.') || a.contains('/') || a.contains('\\') => {
            format!("Review the code in {} for quality, bugs, and suggestions.", args.join(" "))
        }
        Some(a) => format!("Review the changes introduced by commit {a}. Use git_diff and git_log to understand the changes, then examine the modified files for bugs, quality issues, and suggestions."),
    }
}

const NO_AGENTS: &str = "[yellow]Sub-agents are not available in this build.[/]";

#[async_trait]
impl Command for ReviewCommand {
    fn name(&self) -> &'static str {
        "review"
    }
    fn description(&self) -> &'static str {
        "Run code review via sub-agent"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let task = review_task(args);
        let Some(host) = ctx.agents else {
            ctx.console.markup_line(NO_AGENTS);
            return CommandResult::Ok;
        };
        ctx.console
            .markup_line("[yellow]Spawning review agent...[/]");
        match host.spawn_agent(&task, "review").await {
            Ok(outcome) if outcome.completed => {
                ctx.console.markup_line(&format!(
                    "[green]Review completed ({} iterations)[/]",
                    outcome.iterations
                ));
                if let Some(r) = outcome.result.filter(|r| !r.trim().is_empty()) {
                    write_panel(
                        ctx.console,
                        &escape(r.trim_end()),
                        "[green]Code Review[/]",
                        "green",
                    );
                }
            }
            Ok(outcome) => ctx.console.markup_line(&format!(
                "[red]Review agent {}: {}[/]",
                outcome.status,
                escape(outcome.error.as_deref().unwrap_or(""))
            )),
            Err(e) => ctx
                .console
                .markup_line(&format!("[red]Review failed: {}[/]", escape(&e))),
        }
        CommandResult::Ok
    }
}

pub struct WorkflowCommand;

#[async_trait]
impl Command for WorkflowCommand {
    fn name(&self) -> &'static str {
        "workflow"
    }
    fn description(&self) -> &'static str {
        "Run a scripted agent pipeline (list with no args)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["wf"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        use orca_core::orchestration::workflow::WorkflowLoader;
        let Some(manager) = ctx.agents.and_then(|h| h.manager()) else {
            ctx.console.markup_line(NO_AGENTS);
            return CommandResult::Ok;
        };
        let cwd = ctx.cwd();
        let console = ctx.console;
        let workflows = WorkflowLoader::discover(ctx.config_dir, &cwd, &mut |w| {
            console.markup_line(&format!("[yellow]\u{26a0} {}[/]", escape(&w)));
        });

        let Some(name) = args.first() else {
            let mut table = crate::ui::widgets::Table::new()
                .column("Workflow")
                .column("Stages")
                .column("Description");
            let mut all: Vec<_> = workflows.values().collect();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            for wf in all {
                table.row(&[
                    wf.name.clone(),
                    wf.stages.len().to_string(),
                    escape(wf.description.as_deref().unwrap_or("")),
                ]);
            }
            table.write(ctx.console);
            ctx.console.markup_line(
                "[grey]Run one with /workflow <name> [[input...]]. Define your own in .orca/workflows/*.json.[/]",
            );
            return CommandResult::Ok;
        };
        let Some(workflow) = workflows
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, w)| w.clone())
        else {
            ctx.console.markup_line(&format!(
                "[red]Unknown workflow: {}[/] [grey](/workflow lists available)[/]",
                escape(name)
            ));
            return CommandResult::Ok;
        };
        let input = args[1..].join(" ");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let on_status = move |s: &str| {
            let _ = tx.send(s.to_string());
        };
        let o = manager.orchestrator().clone();
        let ct = orca_core::orchestration::CancelToken::new();
        let run = orca_core::orchestration::workflow_runner::run_workflow(
            &o,
            &workflow,
            &input,
            Some(&on_status),
            &ct,
        );
        tokio::pin!(run);
        // Stage lines print as they come.
        let result = loop {
            tokio::select! {
                r = &mut run => break r,
                Some(line) = rx.recv() => {
                    ctx.console.markup_line(&format!("[blue]{}[/]", escape(&line)));
                }
            }
        };
        while let Ok(line) = rx.try_recv() {
            ctx.console
                .markup_line(&format!("[blue]{}[/]", escape(&line)));
        }
        match result {
            Ok(result) => {
                ctx.console.blank();
                ctx.console.markup_line(&format!(
                    "[green]Workflow '{}' complete:[/]",
                    escape(&workflow.name)
                ));
                ctx.console.write_line(&result);
            }
            Err(_) if ct.is_cancelled() => {
                ctx.console.markup_line("[yellow]Workflow cancelled.[/]");
            }
            Err(e) => ctx
                .console
                .markup_line(&format!("[red]Workflow failed: {}[/]", escape(&e))),
        }
        CommandResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_task_picks_the_target() {
        assert!(review_task(&[]).starts_with("Review the uncommitted changes"));
        assert!(review_task(&["STAGED".into()]).starts_with("Review the staged changes"));
        assert_eq!(
            review_task(&["src/a.rs".into()]),
            "Review the code in src/a.rs for quality, bugs, and suggestions."
        );
        assert!(
            review_task(&["abc123".into()])
                .starts_with("Review the changes introduced by commit abc123.")
        );
    }
}
