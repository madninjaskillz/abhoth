//! `CommandParser`: slash-command input to a command (registry first, then the built-ins the
//! REPL handles itself, then custom markdown commands), plus "did you mean" suggestions.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::commands::CommandRegistry;

/// `SlashCommand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Config,
    Benchmark,
    Retry,
    CustomCommand,
    Engine,
    Profile,
    /// A command from the [`CommandRegistry`]; its name is in [`ParsedCommand::plugin_name`].
    PluginCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub command: SlashCommand,
    pub args: Vec<String>,
    pub plugin_name: Option<String>,
}

const BUILT_IN_NAMES: [&str; 8] = [
    "config",
    "benchmark",
    "bench",
    "retry",
    "r!",
    "engine",
    "profile",
    "profiles",
];

#[derive(Default, Clone)]
pub struct CommandParser {
    registry: Option<Arc<CommandRegistry>>,
    /// Lowercased custom command names.
    custom_names: Option<BTreeSet<String>>,
}

impl CommandParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_command_registry(&mut self, registry: Arc<CommandRegistry>) {
        self.registry = Some(registry);
    }

    pub fn set_custom_command_names<I: IntoIterator<Item = S>, S: AsRef<str>>(&mut self, names: I) {
        self.custom_names = Some(
            names
                .into_iter()
                .map(|n| n.as_ref().to_lowercase())
                .collect(),
        );
    }

    /// `TryParse`: None when the input is not a known command.
    pub fn try_parse(&self, input: &str) -> Option<ParsedCommand> {
        if input.trim().is_empty() || !input.starts_with('/') {
            return None;
        }
        // `Split(' ', RemoveEmptyEntries)`: spaces only.
        let parts: Vec<&str> = input.split(' ').filter(|p| !p.is_empty()).collect();
        let cmd = parts[0].to_lowercase();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        if let Some(registry) = &self.registry
            && let Some(resolved) = registry.resolve(&cmd[1..])
        {
            return Some(ParsedCommand {
                command: SlashCommand::PluginCommand,
                args,
                plugin_name: Some(resolved.name().to_string()),
            });
        }
        let built_in = |command| {
            Some(ParsedCommand {
                command,
                args: args.clone(),
                plugin_name: None,
            })
        };
        match cmd.as_str() {
            "/config" => built_in(SlashCommand::Config),
            "/benchmark" | "/bench" => built_in(SlashCommand::Benchmark),
            "/retry" | "/r!" => built_in(SlashCommand::Retry),
            "/engine" => built_in(SlashCommand::Engine),
            "/profile" | "/profiles" => built_in(SlashCommand::Profile),
            _ => {
                let names = self.custom_names.as_ref()?;
                let name = &cmd[1..];
                if !names.contains(name) {
                    return None;
                }
                // The command name goes first, the user's arguments follow.
                let mut custom = vec![name.to_string()];
                custom.extend(args.iter().cloned());
                Some(ParsedCommand {
                    command: SlashCommand::CustomCommand,
                    args: custom,
                    plugin_name: None,
                })
            }
        }
    }

    /// Every command name (and alias) without the slash, sorted case-insensitively: Tab
    /// completion and suggestions.
    pub fn all_command_names(&self, include_aliases: bool) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        if let Some(registry) = &self.registry {
            for n in registry.command_names() {
                if n != "customcommand" {
                    names.insert(n.to_string());
                }
            }
            if include_aliases {
                for a in registry.alias_map().keys() {
                    names.insert(a.clone());
                }
            }
        }
        for n in BUILT_IN_NAMES {
            if include_aliases || matches!(n, "config" | "benchmark" | "retry") {
                names.insert(n.to_string());
            }
        }
        if let Some(custom) = &self.custom_names {
            names.extend(custom.iter().cloned());
        }
        let mut list: Vec<String> = names.into_iter().collect();
        list.sort_by_key(|a| a.to_lowercase());
        list.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        list
    }

    /// `LooksLikeCommand`: "/word" (letters, digits, `-_!?`, up to 24 chars) rather than a
    /// message that starts with a path such as "/src/app.py is broken".
    pub fn looks_like_command(input: &str) -> bool {
        if input.trim().is_empty() || !input.starts_with('/') {
            return false;
        }
        let first = input.split(' ').next().unwrap_or("");
        let len = first.encode_utf16().count();
        len > 1
            && len <= 24
            && first
                .chars()
                .skip(1)
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '!' | '?'))
    }

    /// `SuggestCommand`: the closest known command to a mistyped one.
    pub fn suggest_command(&self, typed: &str) -> Option<String> {
        let name = typed
            .trim_start_matches('/')
            .split(' ')
            .next()
            .unwrap_or("")
            .to_lowercase();
        if name.is_empty() {
            return None;
        }
        let candidates = self.all_command_names(false);
        if let Some(prefix) = candidates
            .iter()
            .find(|c| c.to_lowercase().starts_with(&name))
        {
            return Some(prefix.clone());
        }
        let mut best: Option<(&String, usize)> = None;
        for c in &candidates {
            let d = levenshtein(&name, &c.to_lowercase());
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((c, d));
            }
        }
        let max = std::cmp::max(1, name.chars().count() / 3);
        best.filter(|(_, d)| *d <= max).map(|(c, _)| c.clone())
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            cur[j] = (cur[j - 1] + 1)
                .min(prev[j] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    //! Ported from CommandParserTests.cs and CommandParserCustomTests.cs.
    use super::*;

    fn parser() -> CommandParser {
        let mut p = CommandParser::new();
        p.set_command_registry(Arc::new(crate::commands::registry()));
        p
    }

    fn plugin(p: &CommandParser, input: &str) -> ParsedCommand {
        let r = p.try_parse(input).unwrap_or_else(|| panic!("{input}"));
        assert_eq!(r.command, SlashCommand::PluginCommand, "{input}");
        r
    }

    #[test]
    fn parse_builtin_command_returns_correct_slash_command() {
        assert_eq!(
            parser().try_parse("/config").unwrap().command,
            SlashCommand::Config
        );
    }

    #[test]
    fn parse_plugin_command_returns_plugin_command_with_name() {
        let p = parser();
        for (input, name) in [
            ("/help", "help"),
            ("/h", "help"),
            ("/?", "help"),
            ("/clear", "clear"),
            ("/c", "clear"),
            ("/exit", "exit"),
            ("/quit", "exit"),
            ("/q", "exit"),
            ("/ask", "ask"),
            ("/memory", "memory"),
            ("/init", "init"),
            ("/diff", "diff"),
            ("/undo", "undo"),
            ("/model", "model"),
            ("/m", "model"),
            ("/session", "session"),
            ("/s", "session"),
            ("/plan", "plan"),
            ("/uncensored", "uncensored"),
            ("/p", "plan"),
            ("/compact", "compact"),
            ("/rewind", "rewind"),
            ("/context", "context"),
            ("/ctx", "context"),
            ("/stats", "stats"),
            ("/cost", "stats"),
            ("/doctor", "doctor"),
            ("/diag", "doctor"),
            ("/copy", "copy"),
            ("/cp", "copy"),
            ("/export", "export"),
            ("/rename", "rename"),
            ("/add", "add"),
            ("/fork", "fork"),
            ("/f!", "fork"),
            ("/review", "review"),
            ("/checkpoint", "checkpoint"),
            ("/cp!", "checkpoint"),
        ] {
            assert_eq!(plugin(&p, input).plugin_name.as_deref(), Some(name));
        }
    }

    #[test]
    fn parse_case_insensitive_returns_command() {
        let p = parser();
        for input in ["/HELP", "/Help", "/hElP"] {
            assert_eq!(plugin(&p, input).plugin_name.as_deref(), Some("help"));
        }
    }

    #[test]
    fn parse_command_with_args_extracts_arguments() {
        let r = plugin(&parser(), "/session save my session name");
        assert_eq!(r.plugin_name.as_deref(), Some("session"));
        assert_eq!(r.args, ["save", "my", "session", "name"]);
    }

    #[test]
    fn parse_command_with_single_arg_extracts_argument() {
        let r = plugin(&parser(), "/model gpt-4");
        assert_eq!(r.plugin_name.as_deref(), Some("model"));
        assert_eq!(r.args, ["gpt-4"]);
    }

    #[test]
    fn parse_command_with_no_args_returns_empty_args() {
        assert!(plugin(&parser(), "/help").args.is_empty());
    }

    #[test]
    fn parse_empty_whitespace_no_slash_unknown_and_bang_return_null() {
        let p = parser();
        for input in ["", "   ", "hello world", "/unknown", "!ls -la"] {
            assert!(p.try_parse(input).is_none(), "{input}");
        }
    }

    #[test]
    fn parse_multiple_spaces_handled_gracefully() {
        let r = plugin(&parser(), "/session   save   name");
        assert_eq!(r.args, ["save", "name"]);
    }

    #[test]
    fn parse_args_of_various_commands() {
        let p = parser();
        assert_eq!(plugin(&p, "/rewind 3").args, ["3"]);
        assert_eq!(
            plugin(&p, "/export /tmp/conversation.md").args,
            ["/tmp/conversation.md"]
        );
        assert_eq!(
            plugin(&p, "/compact focus on code changes").args,
            ["focus", "on", "code", "changes"]
        );
        assert_eq!(
            plugin(&p, "/rename My Cool Session").args,
            ["My", "Cool", "Session"]
        );
        assert_eq!(
            plugin(&p, "/add src/Program.cs tests/*.cs").args,
            ["src/Program.cs", "tests/*.cs"]
        );
        assert_eq!(
            plugin(&p, "/ask what is a monad").args,
            ["what", "is", "a", "monad"]
        );
        assert_eq!(plugin(&p, "/review staged").args, ["staged"]);
        assert_eq!(plugin(&p, "/review abc123").args, ["abc123"]);
        assert_eq!(
            plugin(&p, "/review src/Program.cs").args,
            ["src/Program.cs"]
        );
    }

    #[test]
    fn custom_command_resolved_from_known_names() {
        let mut p = CommandParser::new();
        p.set_custom_command_names(["review-pr", "deploy"]);
        let r = p.try_parse("/review-pr 123").unwrap();
        assert_eq!(r.command, SlashCommand::CustomCommand);
        assert_eq!(r.args, ["review-pr", "123"]);
    }

    #[test]
    fn custom_command_no_args() {
        let mut p = CommandParser::new();
        p.set_custom_command_names(["lint"]);
        let r = p.try_parse("/lint").unwrap();
        assert_eq!(r.command, SlashCommand::CustomCommand);
        assert_eq!(r.args, ["lint"]);
    }

    #[test]
    fn unknown_command_returns_null_when_not_custom() {
        let mut p = CommandParser::new();
        p.set_custom_command_names(["deploy"]);
        assert!(p.try_parse("/unknown-cmd").is_none());
    }

    #[test]
    fn plugin_command_takes_priority_over_custom() {
        let mut p = parser();
        p.set_custom_command_names(["help"]);
        let r = p.try_parse("/help").unwrap();
        assert_eq!(r.command, SlashCommand::PluginCommand);
        assert_eq!(r.plugin_name.as_deref(), Some("help"));
    }

    #[test]
    fn checkpoint_requires_registry() {
        assert!(CommandParser::new().try_parse("/checkpoint list").is_none());
    }

    #[test]
    fn no_custom_names_returns_null() {
        assert!(CommandParser::new().try_parse("/custom-thing").is_none());
    }

    #[test]
    fn looks_like_command_and_suggestions() {
        assert!(CommandParser::looks_like_command("/hlep"));
        assert!(CommandParser::looks_like_command("/r!"));
        assert!(!CommandParser::looks_like_command("/src/app.py is broken"));
        assert!(!CommandParser::looks_like_command("/"));
        let p = parser();
        // Two edits away from "help": too far for a 4-letter name.
        assert_eq!(p.suggest_command("/hlep"), None);
        assert_eq!(p.suggest_command("/hel").as_deref(), Some("help"));
        assert_eq!(p.suggest_command("/exot").as_deref(), Some("exit"));
        assert_eq!(p.suggest_command("/sess").as_deref(), Some("session"));
        assert_eq!(p.suggest_command("/zzzzzz"), None);
    }
}
