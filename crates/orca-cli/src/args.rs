//! Command-line arguments, parsed the way `CliArgumentParser` does (flags anywhere, a value
//! is the next argument), so scripts written for the C# build work unchanged.

use std::path::Path;

use orca_core::config::OrcaConfig;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliArgs {
    pub demo_mode: bool,
    pub show_version: bool,
    pub allow_tools: Option<Vec<String>>,
    pub json_output: bool,
    pub single_prompt: Option<String>,
    pub sandbox_mode: bool,
    pub allowed_directory: Option<String>,
    pub continue_session: bool,
    pub resume_session_id: Option<String>,
    pub engine: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
}

fn value_after(args: &[String], names: &[&str]) -> Option<String> {
    let i = args.iter().position(|a| names.contains(&a.as_str()))?;
    args.get(i + 1).cloned()
}

fn value_or_equals(args: &[String], name: &str) -> Option<String> {
    if let Some(v) = value_after(args, &[name]) {
        return Some(v);
    }
    let prefix = format!("{name}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
}

impl CliArgs {
    pub fn parse(args: &[String], cwd: &Path) -> Self {
        let has = |names: &[&str]| args.iter().any(|a| names.contains(&a.as_str()));
        let mut json_output =
            value_after(args, &["--output"]).is_some_and(|v| v.eq_ignore_ascii_case("json"));
        // --print / -p is --prompt plus --output json.
        let mut single_prompt = value_after(args, &["--print", "-p"]);
        if single_prompt.is_some() {
            json_output = true;
        }
        if single_prompt.is_none() {
            single_prompt = value_after(args, &["--prompt"]);
        }
        Self {
            demo_mode: has(&["--demo"]),
            show_version: has(&["--version", "-v"]),
            allow_tools: value_after(args, &["--allow"]).map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            }),
            json_output,
            single_prompt,
            sandbox_mode: has(&["--sandbox", "--simple"]),
            allowed_directory: value_after(args, &["--allow-dir"])
                .map(|d| orca_abstractions::full_path(cwd, &d).display().to_string()),
            continue_session: has(&["--continue", "-c"]),
            resume_session_id: value_after(args, &["--resume", "-r"]),
            engine: value_or_equals(args, "--engine"),
            profile: value_or_equals(args, "--profile"),
            mode: value_or_equals(args, "--mode"),
        }
    }

    /// `CliArgs.ApplyTo`.
    pub fn apply_to(&self, config: &mut OrcaConfig) {
        if self.demo_mode {
            config.demo_mode = true;
            config.lm_studio.native_tool_calling = false;
            config.lm_studio.models = vec!["demo-model".into()];
            config.permissions.auto_approve_all = true;
            config.session.auto_save = false;
            config.context.auto_compact_enabled = false;
        }
        if let Some(tools) = &self.allow_tools {
            config
                .permissions
                .always_approve
                .extend(tools.iter().cloned());
        }
        if self.sandbox_mode {
            config.sandbox_mode = true;
        }
        if let Some(dir) = &self.allowed_directory {
            config.allowed_directory = Some(dir.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> CliArgs {
        CliArgs::parse(
            &a.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Path::new("/work"),
        )
    }

    #[test]
    fn print_implies_json_and_wins_over_prompt() {
        let a = parse(&["--prompt", "b", "-p", "a"]);
        assert_eq!(
            (a.single_prompt.as_deref(), a.json_output),
            (Some("a"), true)
        );
        let a = parse(&["--prompt", "b"]);
        assert_eq!(
            (a.single_prompt.as_deref(), a.json_output),
            (Some("b"), false)
        );
        assert!(parse(&["--prompt", "x", "--output", "JSON"]).json_output);
    }

    #[test]
    fn values_and_flags() {
        let a = parse(&[
            "--allow",
            "bash, write_file,",
            "--engine=claude",
            "--profile",
            "fast",
            "-c",
            "--sandbox",
        ]);
        assert_eq!(
            a.allow_tools,
            Some(vec!["bash".to_string(), "write_file".to_string()])
        );
        assert_eq!(
            (a.engine.as_deref(), a.profile.as_deref()),
            (Some("claude"), Some("fast"))
        );
        assert!(a.continue_session && a.sandbox_mode && !a.demo_mode);
        assert!(
            parse(&["--print"]).single_prompt.is_none(),
            "a flag without its value is ignored"
        );
    }
    // Ported from CliArgumentParserTests (continue / resume).

    #[test]
    fn parse_continue_flag_sets_continue_session() {
        assert!(parse(&["--continue"]).continue_session);
    }

    #[test]
    fn parse_continue_short_flag_sets_continue_session() {
        assert!(parse(&["-c"]).continue_session);
    }

    #[test]
    fn parse_resume_flag_sets_resume_session_id() {
        assert_eq!(
            parse(&["--resume", "abc-123"]).resume_session_id.as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn parse_resume_short_flag_sets_resume_session_id() {
        assert_eq!(
            parse(&["-r", "abc-123"]).resume_session_id.as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn parse_no_args_has_no_session_flags() {
        let a = parse(&[]);
        assert!(!a.continue_session);
        assert!(a.resume_session_id.is_none());
    }
}
