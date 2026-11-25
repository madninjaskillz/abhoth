//! Tool permissions (`PermissionManager`, `PermissionPattern`, `IPermissionPrompter`). A call
//! that needs approval asks the session's prompter; without one (or in a non-interactive run)
//! it is refused (fail closed), as the C# manager does.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::config::PermissionsConfig;
use orca_abstractions::ToolRiskLevel;

/// `Tool(argGlob)`, e.g. `Bash(git *)`, `write_file(src/**)`.
#[derive(Debug, Clone)]
pub struct PermissionPattern {
    pub tool_name: String,
    pub argument_glob: String,
    glob: Regex,
}

static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\w+)\((.+)\)$").unwrap());

impl PermissionPattern {
    pub fn parse(pattern: &str) -> Option<Self> {
        let p = pattern.trim();
        if p.is_empty() {
            return None;
        }
        let caps = PATTERN.captures(p)?;
        let argument_glob = caps[2].to_string();
        Some(Self {
            tool_name: caps[1].to_string(),
            glob: build_glob_regex(&argument_glob),
            argument_glob,
        })
    }

    pub fn matches(&self, tool_name: &str, relevant_arg: Option<&str>) -> bool {
        if !self.tool_name.eq_ignore_ascii_case(tool_name) {
            return false;
        }
        match relevant_arg {
            Some(arg) if !arg.is_empty() => self.glob.is_match(&normalize_arg(arg)),
            _ => false,
        }
    }
}

/// The value of `arg_name` in the call's arguments, when it is a string.
pub fn extract_relevant_arg(arg_name: &str, args: &Value) -> Option<String> {
    args.get(arg_name)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Backslashes become slashes and `.` / `..` segments are resolved (a command with spaces is
/// left alone), so `src/../etc/passwd` cannot slip past `src/**`.
pub fn normalize_arg(arg: &str) -> String {
    let s = arg.replace('\\', "/");
    if s.contains(' ') || !s.contains('/') {
        return s;
    }
    let mut stack: Vec<&str> = Vec::new();
    for part in s.split('/') {
        if part == "." {
            continue;
        }
        if part == ".."
            && let Some(last) = stack.last()
            && !last.is_empty()
            && *last != ".."
            && !last.ends_with(':')
        {
            stack.pop();
            continue;
        }
        stack.push(part);
    }
    stack.join("/")
}

fn build_glob_regex(glob: &str) -> Regex {
    let normalized = glob.replace('\\', "/");
    let single_star = if normalized.contains('/') {
        "[^/]*"
    } else {
        ".*"
    };
    let mut body = String::new();
    let chars: Vec<char> = normalized.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                body.push_str(".*");
                i += 1;
            }
            '*' => body.push_str(single_star),
            '?' => body.push('.'),
            c => body.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    Regex::new(&format!("(?i)^{body}$")).expect("escaped glob is a valid regex")
}

/// `PermissionManager.BuildScopeKey`: what "approve all" covers (a command family or a folder).
pub fn build_scope_key(tool_name: &str, arg_name: &str, relevant_arg: Option<&str>) -> String {
    let Some(arg) = relevant_arg.filter(|a| !a.trim().is_empty()) else {
        return tool_name.to_string();
    };
    let scope = if arg_name == "command" {
        match arg.find(' ') {
            Some(idx) if idx > 0 => arg[..idx].to_string(),
            _ => arg.to_string(),
        }
    } else {
        let normalized = normalize_arg(arg);
        match normalized.rfind('/') {
            Some(slash) if slash > 0 => normalized[..slash].to_string(),
            _ => normalized,
        }
    };
    format!("{tool_name}::{scope}")
}

/// `PermissionDecision`: the answer to an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Approved,
    Denied,
    /// Approve this tool for this argument scope for the rest of the session.
    ApproveAll,
}

/// `IPermissionPrompter`: asks the user whether a tool call may run.
pub trait PermissionPrompter: Send + Sync {
    /// `risk` is the C# risk level name ("ReadOnly", "Moderate", "Dangerous").
    fn prompt(&mut self, tool_name: &str, risk: &str) -> PermissionDecision;
}

/// The outcome of the configured rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny,
    /// Needs interactive approval; an "approve all" answer is remembered under this key.
    Ask {
        scope_key: String,
    },
}

pub fn risk_name(risk: ToolRiskLevel) -> &'static str {
    match risk {
        ToolRiskLevel::ReadOnly => "ReadOnly",
        ToolRiskLevel::Moderate => "Moderate",
        ToolRiskLevel::Dangerous => "Dangerous",
    }
}

/// `PermissionManager.CheckPermissionAsync` up to the prompt. `session_approved` holds the
/// scope keys approved with "yes to all" this session (lowercase: the C# set ignores case).
pub fn evaluate_permission(
    config: &PermissionsConfig,
    tool_name: &str,
    risk: ToolRiskLevel,
    permission_arg_name: &str,
    args: &Value,
    session_approved: &std::collections::HashSet<String>,
) -> PermissionVerdict {
    if config
        .disabled_tools
        .iter()
        .any(|t| t.eq_ignore_ascii_case(tool_name))
    {
        return PermissionVerdict::Deny;
    }
    let relevant = extract_relevant_arg(permission_arg_name, args);
    for p in &config.deny_patterns {
        match PermissionPattern::parse(p) {
            // A deny rule we can't parse must fail CLOSED.
            None => {
                tracing::warn!(pattern = %p, "malformed deny permission pattern: denying");
                return PermissionVerdict::Deny;
            }
            Some(pattern) if pattern.matches(tool_name, relevant.as_deref()) => {
                return PermissionVerdict::Deny;
            }
            Some(_) => {}
        }
    }
    for p in &config.allow_patterns {
        match PermissionPattern::parse(p) {
            None => tracing::warn!(pattern = %p, "malformed allow permission pattern ignored"),
            Some(pattern) if pattern.matches(tool_name, relevant.as_deref()) => {
                return PermissionVerdict::Allow;
            }
            Some(_) => {}
        }
    }
    if config.auto_approve_all
        || config
            .always_approve
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tool_name))
    {
        return PermissionVerdict::Allow;
    }
    let scope_key = build_scope_key(tool_name, permission_arg_name, relevant.as_deref());
    if session_approved.contains(&scope_key.to_lowercase()) {
        return PermissionVerdict::Allow;
    }
    match risk {
        ToolRiskLevel::ReadOnly if config.auto_approve_read_only => PermissionVerdict::Allow,
        ToolRiskLevel::Moderate if config.auto_approve_moderate => PermissionVerdict::Allow,
        _ => PermissionVerdict::Ask { scope_key },
    }
}

/// `PermissionManager.CheckPermissionAsync` without a prompter: approval needed means denied.
pub fn check_permission(
    config: &PermissionsConfig,
    tool_name: &str,
    risk: ToolRiskLevel,
    permission_arg_name: &str,
    args: &Value,
) -> bool {
    evaluate_permission(
        config,
        tool_name,
        risk,
        permission_arg_name,
        args,
        &Default::default(),
    ) == PermissionVerdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── PermissionPatternTests ──

    #[test]
    fn parse_valid_patterns() {
        for (input, tool, glob) in [
            ("Bash(git *)", "Bash", "git *"),
            ("write_file(src/**)", "write_file", "src/**"),
            ("Bash(rm -rf *)", "Bash", "rm -rf *"),
            ("Bash(sudo *)", "Bash", "sudo *"),
        ] {
            let p = PermissionPattern::parse(input).unwrap();
            assert_eq!(
                (p.tool_name.as_str(), p.argument_glob.as_str()),
                (tool, glob)
            );
        }
    }

    #[test]
    fn parse_invalid_patterns() {
        for input in ["", "  ", "invalid", "no_parens", "(just parens)"] {
            assert!(PermissionPattern::parse(input).is_none(), "{input}");
        }
    }

    #[test]
    fn command_glob_matches_any_case_tool() {
        let p = PermissionPattern::parse("Bash(git *)").unwrap();
        assert!(p.matches("bash", Some("git status")));
        assert!(p.matches("Bash", Some("git commit -m test")));
        assert!(p.matches("BASH", Some("git push origin main")));
        assert!(!p.matches("bash", Some("rm -rf /")));
        assert!(!p.matches("write_file", Some("git status")));
        assert!(!p.matches("bash", None));
        assert!(!p.matches("bash", Some("")));
    }

    #[test]
    fn path_glob_double_star_and_single_star() {
        let p = PermissionPattern::parse("write_file(src/**)").unwrap();
        assert!(p.matches("write_file", Some("src/main.cs")));
        assert!(p.matches("write_file", Some("src/deep/nested/file.txt")));
        assert!(!p.matches("write_file", Some("tests/foo.cs")));
        let single = PermissionPattern::parse("write(src/*)").unwrap();
        assert!(single.matches("write", Some("src/main.cs")));
        assert!(!single.matches("write", Some("src/nested/deep.cs")));
        // `..` cannot escape the allowed folder.
        assert!(!p.matches("write_file", Some("src/../etc/passwd")));
    }

    #[test]
    fn scope_keys() {
        assert_eq!(
            build_scope_key("bash", "command", Some("git status")),
            "bash::git"
        );
        assert_eq!(
            build_scope_key("bash", "command", Some("rm -rf /")),
            "bash::rm"
        );
        assert_eq!(
            build_scope_key("write_file", "path", Some("src/models/user.cs")),
            "write_file::src/models"
        );
        assert_eq!(
            build_scope_key("write_file", "path", Some("src\\models\\other.cs")),
            "write_file::src/models"
        );
    }

    #[test]
    fn deny_wins_and_unapproved_fails_closed() {
        let config = PermissionsConfig {
            allow_patterns: vec!["bash(git *)".into()],
            deny_patterns: vec!["bash(git push*)".into()],
            ..PermissionsConfig::default()
        };
        let allowed = check_permission(
            &config,
            "bash",
            ToolRiskLevel::Dangerous,
            "command",
            &json!({"command": "git status"}),
        );
        let denied = check_permission(
            &config,
            "bash",
            ToolRiskLevel::Dangerous,
            "command",
            &json!({"command": "git push"}),
        );
        let unknown = check_permission(
            &config,
            "bash",
            ToolRiskLevel::Dangerous,
            "command",
            &json!({"command": "ls"}),
        );
        let read = check_permission(
            &config,
            "read_file",
            ToolRiskLevel::ReadOnly,
            "path",
            &json!({"path": "a"}),
        );
        assert!(allowed && !denied && !unknown && read);
        let broken = PermissionsConfig {
            deny_patterns: vec!["oops".into()],
            auto_approve_all: true,
            ..PermissionsConfig::default()
        };
        assert!(!check_permission(
            &broken,
            "read_file",
            ToolRiskLevel::ReadOnly,
            "path",
            &json!({})
        ));
    }
}
