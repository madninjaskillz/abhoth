//! OpenOrca.Commands.Custom: `/customcommand <name> [args]`. In C# this adds the rendered
//! template to the conversation but never runs a turn (and `/name` runs a turn without it);
//! here both forms run the rendered template as the prompt (decision D60).

use async_trait::async_trait;

use super::{Command, CommandContext, CommandResult};
use crate::custom_commands::render_template;
use crate::ui::markup::escape;

pub struct CustomCommand;

#[async_trait]
impl Command for CustomCommand {
    fn name(&self) -> &'static str {
        "customcommand"
    }
    fn description(&self) -> &'static str {
        "Run a custom command from .orca/commands/"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let Some(name) = args.first() else {
            ctx.console
                .markup_line("[red]Custom command name missing.[/]");
            return CommandResult::Ok;
        };
        match ctx.custom.load_template(name) {
            Some(template) => {
                let rendered = render_template(&template, &args[1..]);
                ctx.console.markup_line(&format!(
                    "[cyan]Running custom command: /{}[/]",
                    escape(name)
                ));
                CommandResult::RunAgent(rendered)
            }
            None => {
                ctx.console.markup_line(&format!(
                    "[red]Custom command template not found: {}[/]",
                    escape(name)
                ));
                CommandResult::Ok
            }
        }
    }
}
