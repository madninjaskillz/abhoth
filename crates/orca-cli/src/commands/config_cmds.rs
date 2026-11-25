//! OpenOrca.Commands.Config: /init, /model (/uncensored is in `uncensored`).

use async_trait::async_trait;
use orca_core::config::project_instructions;

use super::{Command, CommandContext, CommandResult};
use crate::ui::markup::{escape, remove};
use crate::ui::prompt;

pub struct InitCommand;

pub const ORCA_MD_TEMPLATE: &str = "# Project Instructions\n\n## Overview\n<!-- Brief description of the project -->\n\n## Architecture\n<!-- Key directories, patterns, frameworks -->\n\n## Code Style\n<!-- Naming conventions, formatting, preferred patterns -->\n\n## Testing\n<!-- How to run tests, testing conventions -->\n\n## Common Commands\n<!-- Build, test, deploy commands -->";

/// The template as `/init` writes it: the C# raw string literal keeps its source file's line
/// endings, which are CRLF in the Windows build (cross-check 226).
pub fn orca_md_template() -> String {
    if cfg!(windows) {
        ORCA_MD_TEMPLATE.replace('\n', "\r\n")
    } else {
        ORCA_MD_TEMPLATE.to_string()
    }
}

#[async_trait]
impl Command for InitCommand {
    fn name(&self) -> &'static str {
        "init"
    }
    fn description(&self) -> &'static str {
        "Create a project ORCA.md file"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, _args: &[String]) -> CommandResult {
        let cwd = ctx.cwd();
        let root = project_instructions::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());
        let orca_dir = root.join(".orca");
        let orca_path = orca_dir.join("ORCA.md");
        if let Some(existing) = project_instructions::get_instructions_path(&cwd)
            && existing.exists()
        {
            ctx.console.markup_line(&format!(
                "[yellow]Project instructions already exist at: {}[/]",
                escape(&existing.to_string_lossy())
            ));
            ctx.console
                .markup_line("[grey]Use /memory edit to modify them.[/]");
            return CommandResult::Ok;
        }
        let written = std::fs::create_dir_all(&orca_dir)
            .and_then(|_| std::fs::write(&orca_path, orca_md_template()));
        if let Err(e) = written {
            ctx.console
                .markup_line(&format!("[red]{}[/]", escape(&e.to_string())));
            return CommandResult::Ok;
        }
        ctx.console.markup_line(&format!(
            "[green]Created project instructions at: {}[/]",
            escape(&orca_path.to_string_lossy())
        ));
        ctx.console
            .markup_line("[grey]Edit with /memory edit or open the file directly.[/]");
        CommandResult::Ok
    }
}

pub struct ModelCommand;

#[async_trait]
impl Command for ModelCommand {
    fn name(&self) -> &'static str {
        "model"
    }
    fn description(&self) -> &'static str {
        "List or set model(s)"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["m"]
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        if let Some(model) = args.first() {
            ctx.session.config.lm_studio.models = vec![model.clone()];
            ctx.session.primary_model_override = Some(model.clone());
            ctx.console
                .markup_line(&format!("[green]Model set to: {}[/]", escape(model)));
            return CommandResult::Ok;
        }
        let available = crate::model_discovery::available_models(&ctx.session.config).await;
        if available.is_empty() {
            ctx.console
                .markup_line("[yellow]No models found. Is the LLM server running?[/]");
            return CommandResult::Ok;
        }
        let selected = model_picker(ctx, &available);
        if selected.is_empty() {
            ctx.console
                .markup_line("[green]Models cleared (auto mode)[/]");
        } else {
            ctx.console.markup_line(&format!(
                "[green]Models set to: {}[/]",
                escape(&selected.join(" > "))
            ));
        }
        ctx.session.primary_model_override = selected.first().cloned();
        ctx.session.config.lm_studio.models = selected;
        CommandResult::Ok
    }
}

/// `PromptModelPicker`. Spectre's multi-selection for "+ Add model" is a single selection
/// here (one model per pick; pick again to add more).
fn model_picker(ctx: &CommandContext<'_>, available: &[String]) -> Vec<String> {
    let mut selected = ctx.session.config.lm_studio.models.clone();
    loop {
        if selected.is_empty() {
            ctx.console
                .markup_line("\n[bold]Models:[/] [grey](none)[/]");
        } else {
            let display: Vec<String> = selected
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    format!(
                        "  [cyan]{}.[/] {}{}",
                        i + 1,
                        escape(m),
                        if i == 0 { " [green](primary)[/]" } else { "" }
                    )
                })
                .collect();
            ctx.console
                .markup_line(&format!("\n[bold]Models:[/]\n{}", display.join("\n")));
        }
        let unselected: Vec<String> = available
            .iter()
            .filter(|m| !selected.iter().any(|s| s.eq_ignore_ascii_case(m)))
            .cloned()
            .collect();
        let mut choices = vec!["Done".to_string()];
        if !unselected.is_empty() {
            choices.push("+ Add model".into());
        }
        if !selected.is_empty() {
            choices.push("- Remove model".into());
        }
        for m in selected.iter().skip(1) {
            choices.push(format!("\u{2191} {}", escape(m)));
        }
        for m in selected.iter().take(selected.len().saturating_sub(1)) {
            choices.push(format!("\u{2193} {}", escape(m)));
        }
        let Some(i) = prompt::select(ctx.console, "[bold]Edit models:[/]", &choices) else {
            break;
        };
        let action = choices[i].clone();
        if action == "Done" {
            break;
        }
        if action == "+ Add model" {
            let escaped: Vec<String> = unselected.iter().map(|m| escape(m)).collect();
            if let Some(j) = prompt::select(ctx.console, "[bold]Add models[/]", &escaped) {
                selected.push(unselected[j].clone());
            }
        } else if action == "- Remove model" {
            let escaped: Vec<String> = selected.iter().map(|m| escape(m)).collect();
            if let Some(j) = prompt::select(ctx.console, "Remove which model?", &escaped) {
                selected.remove(j);
            }
        } else if let Some(rest) = action.strip_prefix("\u{2191} ") {
            let model = remove(rest);
            if let Some(idx) = selected.iter().position(|m| *m == model)
                && idx > 0
            {
                selected.swap(idx, idx - 1);
            }
        } else if let Some(rest) = action.strip_prefix("\u{2193} ") {
            let model = remove(rest);
            if let Some(idx) = selected.iter().position(|m| *m == model)
                && idx + 1 < selected.len()
            {
                selected.swap(idx, idx + 1);
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_template_uses_the_csharp_build_line_endings() {
        let t = orca_md_template();
        assert!(t.starts_with("# Project Instructions"));
        if cfg!(windows) {
            assert!(t.contains("# Project Instructions\r\n\r\n## Overview\r\n"));
            assert!(!t.replace("\r\n", "").contains('\n'));
        } else {
            assert_eq!(t, ORCA_MD_TEMPLATE);
        }
    }
}
