//! `/uncensored [on|off|status]` and `UncensoredSwitch` (what it does apart from printing).
//! The C# `UncensoredGate` (a server that refused the field) is not ported to the Rust client,
//! so no server counts as having refused it.

use async_trait::async_trait;
use orca_core::client::server_identity::{self, ServerIdentity, ServerInfo, ServerKind};
use orca_core::config::{AgentUncensoredConfig, ImageGenerationConfig, OrcaConfig};

use super::{Command, CommandContext, CommandResult};

/// What a /uncensored invocation asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    On,
    Off,
    Status,
    Invalid,
}

pub fn parse(args: &[String]) -> Action {
    match args {
        [] => Action::Toggle,
        [a] => match a.trim().to_lowercase().as_str() {
            "on" | "true" | "1" | "enable" => Action::On,
            "off" | "false" | "0" | "disable" => Action::Off,
            "status" | "show" | "?" => Action::Status,
            "toggle" => Action::Toggle,
            _ => Action::Invalid,
        },
        _ => Action::Invalid,
    }
}

/// One server the field goes to, and what is known about it.
pub struct UncensoredServer {
    pub label: String,
    pub base_url: String,
    pub info: Option<ServerInfo>,
}

fn kind_name(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Unknown => "Unknown",
        ServerKind::Strata => "Strata",
        ServerKind::Ninfer => "Ninfer",
        ServerKind::Other => "Other",
    }
}

fn is_read_by(u: &AgentUncensoredConfig, kind: ServerKind) -> bool {
    kind != ServerKind::Unknown
        && u.server_kinds
            .as_ref()
            .is_some_and(|k| k.iter().any(|s| s.eq_ignore_ascii_case(kind_name(kind))))
}

fn is_sent_to(u: &AgentUncensoredConfig, kind: ServerKind) -> bool {
    matches!(kind, ServerKind::Unknown | ServerKind::Other) || is_read_by(u, kind)
}

fn esc(s: &str) -> String {
    s.replace('[', "[[").replace(']', "]]")
}

/// `UncensoredSwitch.Apply`: carries out the action and returns the lines to print.
pub fn apply(
    uncensored: &mut AgentUncensoredConfig,
    action: Action,
    mid_conversation: bool,
    image_gen: Option<&ImageGenerationConfig>,
    servers: Option<&[UncensoredServer]>,
) -> Vec<String> {
    if action == Action::Invalid {
        return vec!["[yellow]Usage: /uncensored [[on|off|status]][/]".into()];
    }
    let field = Some(uncensored.body_field.trim().to_string()).filter(|f| !f.is_empty());
    let was = uncensored.is_on();
    if action != Action::Status {
        uncensored.session = Some(if action == Action::Toggle {
            !was
        } else {
            action == Action::On
        });
    }
    let now = uncensored.is_on();
    let no_server = now
        && field.is_some()
        && servers.is_some_and(|s| {
            !s.is_empty()
                && !s.iter().any(|s| {
                    is_sent_to(
                        uncensored,
                        s.info.as_ref().map_or(ServerKind::Unknown, |i| i.kind),
                    )
                })
        });
    let f = esc(field.as_deref().unwrap_or("(no field)"));
    let mut lines = Vec::new();
    if action == Action::Status || now == was {
        lines.push(if !now {
            "[grey]Uncensored is off[/] — requests are sent as normal.".to_string()
        } else if no_server {
            format!("[red]Uncensored is on[/] — but no chat server here takes [cyan]{f}[/], so chat requests are sent as normal.")
        } else {
            format!("[red]Uncensored is on[/] — requests to the local server carry [cyan]{f}[/].")
        });
    } else {
        lines.push(if !now {
            "[grey]Uncensored off[/] — requests are sent as normal again.".to_string()
        } else if no_server {
            format!("[red]Uncensored on[/] — but no chat server here takes [cyan]{f}[/], so chat requests stay normal (this session only).")
        } else {
            format!("[red]Uncensored on[/] — requests to the local server now carry [cyan]{f}[/] (this session only).")
        });
        if mid_conversation && !no_server {
            lines.push("[dim]Note: the server keeps its prompt cache per setting, so the next request re-reads the whole conversation once.[/]".into());
        }
    }
    if now && field.is_none() {
        lines.push("[yellow]agent.uncensored.bodyField is empty, so nothing is sent.[/]".into());
    } else if now && let Some(servers) = servers {
        lines.extend(server_lines(uncensored, servers));
    }
    if let Some(ig) = image_gen.filter(|i| i.enabled) {
        lines.push(image_line(
            uncensored,
            ig,
            now,
            action != Action::Status && now != was,
        ));
    }
    lines
}

fn server_lines(u: &AgentUncensoredConfig, servers: &[UncensoredServer]) -> Vec<String> {
    let mut out = Vec::new();
    for s in servers {
        let kind = s.info.as_ref().map_or(ServerKind::Unknown, |i| i.kind);
        let display = s.info.as_ref().map_or("", |i| i.display_name());
        let place = format!("{} ({})", esc(&s.label), esc(&s.base_url));
        if is_read_by(u, kind) {
            continue;
        } else if kind == ServerKind::Unknown {
            out.push(format!("[yellow]Chat: the field is sent, but {place} could not be identified, so it is not confirmed that it reads it.[/]"));
        } else if is_sent_to(u, kind) {
            out.push(format!("[yellow]Chat: the field is sent, but {place} is {display}, which is not known to read it: it may be ignored there.[/]"));
        } else {
            out.push(format!("[yellow]Chat: not available on {place}, which is {display}: the field is not sent there; requests there stay normal.[/]"));
        }
    }
    out
}

fn image_line(
    u: &AgentUncensoredConfig,
    ig: &ImageGenerationConfig,
    on: bool,
    changed: bool,
) -> String {
    let normal = match &ig.model {
        Some(m) => format!("[cyan]{}[/]", esc(m)),
        None => "the server's default model".into(),
    };
    let Some(im) = u.image_model.as_deref().filter(|m| !m.trim().is_empty()) else {
        return format!(
            "[dim]Images are not covered (agent.uncensored.imageModel is blank): generate_image stays on[/] {normal}[dim].[/]"
        );
    };
    let model = format!("[cyan]{}[/]", esc(im.trim()));
    if on {
        format!(
            "[red]Images:[/] generate_image {} {model} in place of {normal}.",
            if changed { "now uses" } else { "uses" }
        )
    } else if changed {
        format!("[grey]Images:[/] generate_image is back on {normal}.")
    } else {
        format!("[grey]Images:[/] generate_image uses {normal} ({model} while on).")
    }
}

/// `Endpoints`: lmStudio.baseUrl (unless Anthropic), each slot copy and the Lead endpoint.
pub fn endpoints(config: &OrcaConfig) -> Vec<(String, String, Option<String>)> {
    let mut list = Vec::new();
    let mut add = |label: String, url: &str, key: Option<String>| {
        if !url.trim().is_empty() {
            list.push((label, url.trim().to_string(), key));
        }
    };
    if !config.lm_studio.is_anthropic() {
        add(
            "the model server".into(),
            &config.lm_studio.base_url,
            Some(config.lm_studio.resolve_api_key()),
        );
    }
    if let Some(slots) = config.slots.as_ref().filter(|s| s.enabled) {
        let key = slots
            .api_key_env
            .as_deref()
            .filter(|e| !e.is_empty())
            .and_then(|e| std::env::var(e).ok())
            .or_else(|| slots.api_key.clone())
            .or_else(|| Some(config.lm_studio.api_key.clone()));
        for copy in &slots.copies {
            add(format!("copy {}", copy.name), &copy.base_url, key.clone());
        }
        if let Some(lead) = slots.lead.as_ref().filter(|l| !l.base_url.is_empty()) {
            let lead_key = lead
                .api_key_env
                .as_deref()
                .filter(|e| !e.is_empty())
                .and_then(|e| std::env::var(e).ok())
                .or_else(|| lead.api_key.clone())
                .or_else(|| key.clone());
            add(
                format!("the Lead endpoint {}", lead.name)
                    .trim_end()
                    .to_string(),
                &lead.base_url,
                lead_key,
            );
        }
    }
    list
}

/// `IdentifyAsync`: each chat server, identified (one entry per server root).
pub async fn identify(config: &OrcaConfig, identity: &ServerIdentity) -> Vec<UncensoredServer> {
    let eps = endpoints(config);
    let mut out: Vec<UncensoredServer> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (label, url, _) in &eps {
        let root = server_identity::root(url).to_lowercase();
        if seen.contains(&root) {
            continue;
        }
        seen.push(root.clone());
        let mut info = ServerInfo::UNKNOWN;
        for (_, u, key) in eps
            .iter()
            .filter(|(_, u, _)| server_identity::root(u).to_lowercase() == root)
        {
            info = identity.get(u, key.as_deref()).await;
            if !info.key_rejected {
                break;
            }
        }
        out.push(UncensoredServer {
            label: label.clone(),
            base_url: url.clone(),
            info: Some(info),
        });
    }
    out
}

pub struct UncensoredCommand;

#[async_trait]
impl Command for UncensoredCommand {
    fn name(&self) -> &'static str {
        "uncensored"
    }
    fn description(&self) -> &'static str {
        "Toggle uncensored chat requests and image model (on | off | status)"
    }
    async fn execute(&self, ctx: &mut CommandContext<'_>, args: &[String]) -> CommandResult {
        let mid = ctx
            .session
            .messages
            .iter()
            .any(|m| m.role != orca_core::client::Role::System);
        let action = parse(args);
        let is_on = ctx.session.config.agent.uncensored.is_on();
        let will_be_on = match action {
            Action::On => true,
            Action::Toggle => !is_on,
            Action::Status => is_on,
            _ => false,
        };
        let servers = if will_be_on {
            Some(identify(&ctx.session.config, &ServerIdentity::shared()).await)
        } else {
            None
        };
        let image_gen = ctx.session.config.image_gen.clone();
        let lines = apply(
            &mut ctx.session.config.agent.uncensored,
            action,
            mid,
            Some(&image_gen),
            servers.as_deref(),
        );
        for l in lines {
            ctx.console.markup_line(&l);
        }
        // Requests read the switch from the client's settings.
        ctx.session.client.settings_mut().session_field =
            ctx.session.config.agent.uncensored.field();
        CommandResult::Ok
    }
}

#[cfg(test)]
mod tests {
    //! Ported from UncensoredCommandTests.cs (the status-line test is in repl::panel).
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split(' ')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    }

    #[test]
    fn parse_reads_the_argument() {
        for (input, expected) in [
            ("", Action::Toggle),
            ("toggle", Action::Toggle),
            ("on", Action::On),
            ("ON", Action::On),
            ("off", Action::Off),
            ("status", Action::Status),
            ("maybe", Action::Invalid),
            ("on now", Action::Invalid),
        ] {
            assert_eq!(parse(&a(input)), expected, "{input}");
        }
    }

    #[test]
    fn toggle_flips_the_session_switch_and_leaves_the_config_value() {
        let mut u = AgentUncensoredConfig::default();
        let on = apply(&mut u, Action::Toggle, false, None, None);
        assert!(u.is_on());
        assert!(!u.enabled);
        assert_eq!(on.len(), 1);
        assert!(on[0].contains("Uncensored on"));
        assert!(on[0].contains("experimental_speed_projection"));
        let off = apply(&mut u, Action::Toggle, false, None, None);
        assert!(!u.is_on());
        assert_eq!(off.len(), 1);
        assert!(off[0].contains("Uncensored off"));
    }

    #[test]
    fn on_and_off_set_the_switch_and_status_only_reports() {
        let mut u = AgentUncensoredConfig {
            enabled: true,
            ..Default::default()
        };
        let s = apply(&mut u, Action::Status, true, None, None);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("is on"));
        assert!(u.session.is_none());
        apply(&mut u, Action::Off, false, None, None);
        assert!(!u.is_on());
        let s = apply(&mut u, Action::Status, true, None, None);
        assert!(s[0].contains("is off"));
        apply(&mut u, Action::On, false, None, None);
        assert!(u.is_on());
        let usage = apply(&mut u, Action::Invalid, false, None, None);
        assert_eq!(usage.len(), 1);
        assert!(usage[0].contains("Usage"));
        assert!(u.is_on());
    }

    #[test]
    fn a_change_mid_conversation_notes_the_prompt_re_read_but_no_change_does_not() {
        let mut u = AgentUncensoredConfig::default();
        let changed = apply(&mut u, Action::On, true, None, None);
        assert_eq!(changed.len(), 2);
        assert!(changed[1].contains("re-reads the whole conversation once"));
        let same = apply(&mut u, Action::On, true, None, None);
        assert_eq!(same.len(), 1);
        assert!(same[0].contains("is on"));
        let back = apply(&mut u, Action::Off, true, None, None);
        assert!(back[1].contains("re-reads"));
    }
}
