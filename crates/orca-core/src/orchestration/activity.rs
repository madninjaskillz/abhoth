//! Tool calls as a few words for the agent panel, and agents' progress markers (port of
//! `AgentActivity.cs`).
//!
//! Lengths count UTF-16 units, as C# `string.Length` does.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::types::{ci_key, eq_ci, upper_invariant};
use crate::chat::{head16, len16};

/// Longest activity line, in UTF-16 units.
pub const MAX_CHARS: usize = 72;

/// Tool names that spawn agents — hidden from agents at the depth limit.
pub const AGENT_TOOL_NAMES: [&str; 3] = ["spawn_agent", "spawn_agents", "subagent"];

/// Whether `name` is one of [`AGENT_TOOL_NAMES`] (case-insensitive).
pub fn is_agent_tool(name: &str) -> bool {
    AGENT_TOOL_NAMES.iter().any(|t| eq_ci(t, name))
}

/// A tool call as a short activity line ("read src/Foo.cs", "bash dotnet test"), at most
/// [`MAX_CHARS`]. Paths are shown relative to `cwd` (`None` = the process's current
/// directory). Malformed or non-object arguments describe the call by name only.
pub fn describe(tool_name: &str, args_json: Option<&str>, cwd: Option<&Path>) -> String {
    let args = args_json
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        });

    let arg = |names: &[&str]| -> Option<String> {
        let args = args.as_ref()?;
        names.iter().find_map(|n| match args.get(*n) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
    };
    let cwd_text = || -> String {
        match cwd {
            Some(p) => p.to_string_lossy().into_owned(),
            None => std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    };
    let short = |path: Option<String>| short_path(path.as_deref(), &cwd_text());

    let name = tool_name.to_lowercase();
    let text = match name.as_str() {
        "read" | "read_file" => join("read", short(arg(&["path", "file_path"])).as_deref()),
        "write" | "write_file" => join("write", short(arg(&["path", "file_path"])).as_deref()),
        "edit" | "edit_file" | "multi_edit" => {
            join("edit", short(arg(&["path", "file_path"])).as_deref())
        }
        "bash" | "powershell" => join(&name, first_line(arg(&["command"]).as_deref()).as_deref()),
        "grep" => join("grep", arg(&["pattern"]).as_deref()),
        "find" | "glob" => join("find", arg(&["pattern"]).as_deref()),
        "ls" | "list_directory" => join(
            "ls",
            Some(
                short(arg(&["path"]))
                    .unwrap_or_else(|| ".".to_string())
                    .as_str(),
            ),
        ),
        "spawn_agent" => join(
            "spawn",
            Some(
                arg(&["agent_type"])
                    .unwrap_or_else(|| "general".to_string())
                    .as_str(),
            ),
        ),
        "spawn_agents" => "spawn agents".to_string(),
        "subagent" => join("subagent", arg(&["agent"]).as_deref()),
        "web_fetch" => join("fetch", arg(&["url"]).as_deref()),
        "web_search" => join("search", arg(&["query"]).as_deref()),
        "think" => "thinking".to_string(),
        "job" => {
            let parts: Vec<String> = [arg(&["action"]), arg(&["id"]), arg(&["until"])]
                .into_iter()
                .flatten()
                .collect();
            join("job", Some(parts.join(" ").as_str()))
        }
        _ => join(
            tool_name,
            first_line(arg(&["path", "command", "query", "pattern"]).as_deref()).as_deref(),
        ),
    };
    truncate(&text, MAX_CHARS)
}

fn join(verb: &str, detail: Option<&str>) -> String {
    match detail {
        Some(d) if !d.trim().is_empty() => format!("{verb} {d}"),
        _ => verb.to_string(),
    }
}

fn first_line(s: Option<&str>) -> Option<String> {
    let s = s?;
    let no_cr = s.replace('\r', "");
    let line = no_cr.split('\n').find(|l| !l.is_empty()).unwrap_or("");
    Some(line.trim().to_string())
}

fn short_path(path: Option<&str>, cwd: &str) -> Option<String> {
    let path = path.filter(|p| !p.trim().is_empty())?;
    let normalized = path.replace('\\', "/");
    let root = format!("{}/", cwd.replace('\\', "/").trim_end_matches('/'));
    match strip_prefix_ci(&normalized, &root) {
        Some(rest) => Some(rest.to_string()),
        None => Some(normalized),
    }
}

/// `s.StartsWith(prefix, OrdinalIgnoreCase) ? s[prefix.Length..] : null`.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let mut rest = s.char_indices();
    for p in prefix.chars() {
        let (_, c) = rest.next()?;
        if upper_invariant(c) != upper_invariant(p) {
            return None;
        }
    }
    Some(rest.as_str())
}

/// `s` cut to `max` UTF-16 units, ending in "…" when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if len16(s) <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        format!("{}…", head16(s, max - 1))
    }
}

// [progress 40%] note  |  [progress 3/5] note  |  Progress: 40% — note
static BRACKET_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\[\s*progress\s*[:=]?\s*(?P<num>\d{1,3}(?:\.\d+)?)\s*(?:(?P<pct>%)|/\s*(?P<den>\d{1,3}))\s*\]\s*(?P<note>[^\r\n]*)",
    )
    .unwrap()
});

static LINE_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*progress\s*:\s*(?P<num>\d{1,3}(?:\.\d+)?)\s*(?:(?P<pct>%)|/\s*(?P<den>\d{1,3}))\s*(?:[-—–:]\s*)?(?P<note>[^\r\n]*)",
    )
    .unwrap()
});

static BRACKET_MARKER_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\[\s*progress\s*[:=]?\s*\d{1,3}(?:\.\d+)?\s*(?:%|/\s*\d{1,3})\s*\][ \t]*")
        .unwrap()
});

fn mentions_progress(text: &str) -> bool {
    ci_key(text).contains("PROGRESS")
}

/// Remove "[progress N%]" markers (keeping their notes) and trim.
pub fn strip_progress_markers(text: Option<&str>) -> String {
    match text {
        Some(t) if !t.is_empty() && mentions_progress(t) => {
            BRACKET_MARKER_ONLY.replace_all(t, "").trim().to_string()
        }
        _ => text.unwrap_or("").trim().to_string(),
    }
}

/// The last progress marker in an agent's text: `(fraction 0..=1, note)`. `None` when there is
/// none or it doesn't parse to a sane fraction. Bracket markers win over line markers; the
/// note is cut to 60 UTF-16 units.
pub fn try_parse_progress(text: Option<&str>) -> Option<(f64, Option<String>)> {
    let text = text.filter(|t| !t.is_empty() && mentions_progress(t))?;
    let last = BRACKET_MARKER
        .captures_iter(text)
        .last()
        .or_else(|| LINE_MARKER.captures_iter(text).last())?;

    // C#'s double.Parse would throw on a non-ASCII digit `\d` matched; here it is no marker.
    let num: f64 = last.name("num")?.as_str().parse().ok()?;
    let fraction = if last.name("pct").is_some() {
        if num > 100.0 {
            return None;
        }
        num / 100.0
    } else {
        let den: f64 = last.name("den")?.as_str().parse().ok()?;
        if den <= 0.0 || num > den {
            return None;
        }
        num / den
    };

    let n = last.name("note").map_or("", |m| m.as_str()).trim();
    let note = if n.is_empty() {
        None
    } else {
        Some(truncate(n, 60))
    };
    Some((fraction, note))
}
