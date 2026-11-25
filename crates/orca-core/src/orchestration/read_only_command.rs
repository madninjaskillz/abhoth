//! What a read-only agent (the planner) may run (port of `ReadOnlyCommand.cs`): the
//! look-and-list tools, and bash commands made only of programs that read (dir, ls, cat, grep,
//! find, reg query, git log, Get-ChildItem ...), with no output redirection, command
//! substitution or in-place edits. Deliberately an allowlist: anything not recognised is
//! refused with a short reason the model can act on.

use std::sync::LazyLock;

use regex::Regex;

use super::types::{ci_key, eq_ci};
use crate::agent::prompt_constants::platform_lines;
use crate::chat::{head16, len16};

/// Tools a read-only agent may call at all (bash is checked command by command).
pub const TOOLS: [&str; 8] = [
    "read",
    "read_file",
    "grep",
    "find",
    "ls",
    "glob",
    "list_directory",
    "bash",
];

/// Programs that only read or list.
const PROGRAMS: [&str; 59] = [
    "dir",
    "ls",
    "cat",
    "type",
    "head",
    "tail",
    "more",
    "less",
    "find",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "findstr",
    "wc",
    "tree",
    "pwd",
    "echo",
    "where",
    "where.exe",
    "which",
    "file",
    "stat",
    "du",
    "df",
    "sort",
    "uniq",
    "cut",
    "tr",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "date",
    "whoami",
    "hostname",
    "uname",
    "env",
    "printenv",
    "test",
    "[",
    "true",
    "jq",
    "od",
    "xxd",
    "hexdump",
    "strings",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "column",
    "nl",
    "awk",
    "cd",
    "ver",
    "systeminfo",
    "set",
    "attrib",
    "certutil",
];

fn programs() -> impl Iterator<Item = &'static str> {
    PROGRAMS.iter().copied()
}

/// Two-word programs: the first word plus an allowed sub-command (in the C# set's order).
/// The bool is whether the sub-command match ignores case (`unzip`'s does not).
const SUB_COMMANDS: [(&str, bool, &[&str]); 12] = [
    (
        "git",
        true,
        &[
            "log",
            "status",
            "show",
            "diff",
            "ls-files",
            "branch",
            "rev-parse",
            "blame",
            "grep",
            "remote",
            "describe",
            "shortlog",
            "cat-file",
            "ls-tree",
        ],
    ),
    ("reg", true, &["query"]),
    (
        "dotnet",
        true,
        &["--info", "--version", "--list-sdks", "--list-runtimes"],
    ),
    ("node", true, &["--version", "-v"]),
    ("python", true, &["--version", "-V"]),
    ("python3", true, &["--version", "-V"]),
    ("py", true, &["--version", "-V", "--list", "-0"]),
    ("pip", true, &["--version", "list", "show"]),
    ("unzip", false, &["-l", "-v", "-Z"]),
    ("npm", true, &["--version", "-v", "ls", "list", "view"]),
    ("sc", true, &["query"]),
    ("tasklist", true, &[]),
];

/// PowerShell cmdlet verbs that only read.
const PS_VERBS: [&str; 14] = [
    "Get",
    "Test",
    "Select",
    "Measure",
    "Sort",
    "Format",
    "Resolve",
    "Compare",
    "Find",
    "Group",
    "ConvertFrom",
    "ConvertTo",
    "Join",
    "Split",
];

/// PowerShell commands that read but are not Verb-Noun (or are aliases).
const PS_READERS: [&str; 18] = [
    "Where-Object",
    "ForEach-Object",
    "Out-String",
    "Write-Output",
    "dir",
    "ls",
    "gci",
    "cat",
    "gc",
    "type",
    "select",
    "where",
    "sort",
    "measure",
    "fl",
    "ft",
    "%",
    "?",
];

fn contains_ci(set: &[&str], s: &str) -> bool {
    set.iter().any(|x| eq_ci(x, s))
}

/// The distinct values of a set with the given equality, first occurrence kept (a
/// `HashSet` built from the C# initializer).
fn distinct(values: &[&'static str], ignore_case: bool) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for v in values {
        if !out
            .iter()
            .any(|o| if ignore_case { eq_ci(o, v) } else { o == v })
        {
            out.push(v);
        }
    }
    out
}

/// What a read-only agent may run, for its prompt (built from the allowlist, so the prompt and
/// the check cannot drift apart). The text has platform line breaks, as the C# raw string
/// literal carries the checkout's.
pub fn allowed_summary() -> String {
    let mut progs: Vec<&str> = programs()
        .filter(|p| !matches!(*p, "[" | "true" | "where.exe"))
        .collect();
    progs.sort_by_key(|a| ci_key(a));
    let programs = progs.join(", ");

    let mut subs: Vec<&(&str, bool, &[&str])> = SUB_COMMANDS.iter().collect();
    subs.sort_by_key(|a| ci_key(a.0));
    let subs = subs
        .iter()
        .map(|(key, ignore_case, values)| {
            let values = distinct(values, *ignore_case);
            if values.is_empty() {
                key.to_string()
            } else {
                let mut ordered = values.clone();
                ordered.sort();
                format!("{key} {}", ordered.join("|"))
            }
        })
        .collect::<Vec<_>>()
        .join("; ");

    platform_lines(&format!(
        r#"Read-only commands (anything else is refused, which wastes a step):
- Tools: read, grep, find, ls (prefer these for files).
- bash programs: {programs}; also sed without -i, find without -delete/-exec, awk without system(), certutil -hashfile, cmd /c <one of these>.
- Only with these sub-commands: {subs}.
- PowerShell (powershell -Command "..."): Get-/Test-/Select-/Measure-/Sort-/Format-/Resolve-/Compare-/Find-/Group-/ConvertFrom-/ConvertTo-/Join-/Split- cmdlets, Where-Object, ForEach-Object; no Set-/New-/Remove-/Out-File/Invoke-/Start-Process.
- Chain with |, &&, || or ; and quote freely (quoted text is data), but never escape a quote with a backslash (\"): put single quotes inside double quotes instead. Not allowed: > or >> (except 2>/dev/null, 2>nul, 2>&1), $(...) or backticks, shell variables (X=...), for/while/if, and build, run or install commands (dotnet build/new/run, npm install, python script.py)."#
    ))
}

/// `None` when the call may run; otherwise why it was refused.
pub fn refusal(tool_name: &str, args_json: &str) -> Option<String> {
    if !contains_ci(&TOOLS, tool_name) {
        return Some(format!(
            "Error: you are read-only (planning): '{tool_name}' is not available. Use read, grep, find, ls, or bash with commands that only look (e.g. dir, ls, cat, git log, reg query)."
        ));
    }
    if !eq_ci(tool_name, "bash") {
        return None;
    }
    let json = if args_json.trim().is_empty() {
        "{}"
    } else {
        args_json
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(json) else {
        return Some("Error: could not read the bash arguments.".to_string());
    };
    let mut command = None;
    if let serde_json::Value::Object(map) = &doc {
        if let Some(serde_json::Value::String(c)) = map.get("command") {
            command = Some(c.clone());
        }
        if let Some(serde_json::Value::Bool(true)) = map.get("background") {
            return Some("Error: you are read-only (planning): no background jobs.".to_string());
        }
    }
    command_refusal(command.as_deref().unwrap_or(""))
}

static HARMLESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\d?>&\d|2>\s*/dev/null|2>\s*nul\b|>\s*/dev/null|>\s*nul\b|\|\s*Out-Null")
        .unwrap()
});
static PS_LAUNCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(powershell|pwsh)(\.exe)?\s").unwrap());
static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|\||&&|[|;&\n]").unwrap());
static CMD_C: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\S+\s+/[cCkK]\s+(.+)$").unwrap());
static CMDLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z]+)-[A-Za-z]+\b").unwrap());
static PS_BAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(Invoke-|Start-Process|iex\b|Remove-|Set-|New-|Out-File|Add-Content|Move-|Copy-|Rename-|Clear-|Stop-|Restart-|Expand-Archive|Compress-Archive)|\[System\.IO\.File\]|::(Write|Delete|Move|Copy|Create|Append)",
    )
    .unwrap()
});
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'|\S+"#).unwrap());

/// `None` when every part of `command` only reads; otherwise why not.
pub fn command_refusal(command: &str) -> Option<String> {
    let text = command.trim();
    if text.is_empty() {
        return Some("Error: empty command.".to_string());
    }
    // Harmless stderr/stdout plumbing first, then any redirection left is a write.
    let text = HARMLESS.replace_all(text, " ").into_owned();
    // \" means different things to bash (an escaped quote) and cmd (a path ending in \ then a
    // closing quote), so where a quoted span ends is ambiguous: refuse rather than guess.
    if text.contains("\\\"") {
        return Some(refused(
            command,
            "no escaped double quotes (\\\"): use single quotes inside a double-quoted string, e.g. powershell -Command \"Get-ChildItem 'C:\\Program Files'\"",
        ));
    }
    // Quoted text is data (grep -E 'a|b', '<Identity/>'): mask it before looking for shell
    // syntax. Bash still expands $( and backticks inside double quotes, so that check masks
    // single quotes only.
    let single_masked = mask(&text, false);
    let masked = mask(&text, true);
    if single_masked.contains('`') || single_masked.contains("$(") || masked.contains("<(") {
        return Some(refused(command, "no command substitution"));
    }
    if masked.contains('>') {
        return Some(refused(
            command,
            "no output redirection (> or >>): planning does not write files",
        ));
    }
    // A PowerShell command line is one script: its pipes belong to it.
    if PS_LAUNCH.is_match(&text) {
        return power_shell_refusal(&text).map(|why| refused(command, &why));
    }
    // `masked` has the same byte layout as `text`, so its separator offsets cut `text`.
    let mut parts = Vec::new();
    let mut start = 0;
    for sep in SEPARATORS.find_iter(&masked) {
        parts.push(&text[start..sep.start()]);
        start = sep.end();
    }
    parts.push(&text[start..]);
    for part in parts {
        let segment = part.trim();
        if segment.is_empty() {
            continue;
        }
        if let Some(why) = segment_refusal(segment) {
            return Some(refused(command, &why));
        }
    }
    None
}

/// `Path.GetFileName`: the text after the last directory separator (and, on Windows, after a
/// leading drive like `C:`).
fn file_name(path: &str) -> &str {
    let is_sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    let mut rest = match path.rfind(is_sep) {
        Some(i) => &path[i + 1..],
        None => path,
    };
    if cfg!(windows) && rest.len() == path.len() {
        let b = path.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            rest = &path[2..];
        }
    }
    rest
}

fn segment_refusal(segment: &str) -> Option<String> {
    let words: Vec<&str> = WORD.find_iter(segment).map(|m| m.as_str()).collect();
    // Leading environment assignments (FOO=1 cmd) are not allowed: keep it simple.
    if words.is_empty() {
        return None;
    }
    let mut program = file_name(words[0].trim_matches(['"', '\''])).to_string();
    if ci_key(&program).ends_with(".EXE") && !eq_ci(&program, "where.exe") {
        let cut = program.len() - 4;
        program.truncate(cut);
    }

    if program == "powershell" || program == "pwsh" {
        return power_shell_refusal(segment);
    }
    if eq_ci(&program, "cmd") {
        return match CMD_C.captures(segment) {
            Some(rest) => command_refusal(rest[1].trim().trim_matches('"')),
            None => Some("cmd only with /c and a read-only command".to_string()),
        };
    }
    if program == "sed" {
        return words
            .iter()
            .any(|w| w.starts_with("-i") || *w == "--in-place")
            .then(|| "no sed -i (in-place edit)".to_string());
    }
    if program == "find" {
        return words
            .iter()
            .any(|w| {
                matches!(
                    *w,
                    "-delete" | "-exec" | "-execdir" | "-ok" | "-fprint" | "-fprintf" | "-fls"
                )
            })
            .then(|| "find without -delete/-exec".to_string());
    }
    if program == "awk" {
        return segment
            .contains("system(")
            .then(|| "awk without system()".to_string());
    }
    if eq_ci(&program, "certutil") {
        return if words.iter().skip(1).any(|w| eq_ci(w, "-hashfile")) {
            None
        } else {
            Some("certutil only for -hashfile".to_string())
        };
    }
    if eq_ci(&program, "set") && words.len() > 1 {
        return words[1]
            .contains('=')
            .then(|| "set only to list variables".to_string());
    }
    if eq_ci(&program, "attrib") {
        return words
            .iter()
            .skip(1)
            .any(|w| w.starts_with('+') || w.starts_with('-'))
            .then(|| "attrib only to list attributes".to_string());
    }
    if programs().any(|p| eq_ci(p, &program)) {
        return None;
    }
    if let Some((_, ignore_case, values)) = SUB_COMMANDS.iter().find(|(k, _, _)| eq_ci(k, &program))
    {
        let subs = distinct(values, *ignore_case);
        if subs.is_empty() {
            return None;
        }
        let is_git = eq_ci(&program, "git");
        let mut rest: &[&str] = &words[1..];
        // git's global options (-C <dir>, -c <k=v>, --no-pager) come before the sub-command.
        while is_git && !rest.is_empty() && rest[0].starts_with('-') {
            let n = if matches!(rest[0], "-C" | "-c") && rest.len() > 1 {
                2
            } else {
                1
            };
            rest = &rest[n..];
        }
        let sub = rest.first().copied();
        if is_git
            && matches!(sub, Some("branch" | "remote"))
            && words.iter().skip(2).any(|w| {
                matches!(
                    *w,
                    "-d" | "-D"
                        | "-m"
                        | "-M"
                        | "add"
                        | "remove"
                        | "rm"
                        | "rename"
                        | "set-url"
                        | "--delete"
                )
            })
        {
            return Some("git branch/remote only to list".to_string());
        }
        let allowed = sub.is_some_and(|s| {
            subs.iter()
                .any(|v| if *ignore_case { eq_ci(v, s) } else { *v == s })
        });
        return if allowed {
            None
        } else {
            Some(format!("{program} only with: {}", subs.join(", ")))
        };
    }
    Some(format!("'{program}' is not a read-only command"))
}

fn power_shell_refusal(segment: &str) -> Option<String> {
    // Every cmdlet-like word must read; no writes, process starts or invocations.
    for m in CMDLET.captures_iter(segment) {
        let verb = &m[1];
        let name = &m[0];
        if contains_ci(&PS_READERS, name) || contains_ci(&PS_VERBS, verb) {
            continue;
        }
        return Some(format!("PowerShell '{name}' is not read-only"));
    }
    if PS_BAD.is_match(segment) {
        return Some("PowerShell: no writes, invocations or process starts".to_string());
    }
    None
}

/// `text` with the inside of quoted spans replaced by '_'. Each masked char becomes as many
/// '_' as its UTF-8 length, so byte offsets in the result are byte offsets in `text`.
fn mask(text: &str, mask_double: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    for c in text.chars() {
        match quote {
            None => {
                if c == '\'' || (mask_double && c == '"') {
                    quote = Some(c);
                }
                out.push(c);
            }
            Some(q) if c == q => {
                quote = None;
                out.push(c);
            }
            Some(_) => {
                for _ in 0..c.len_utf8() {
                    out.push('_');
                }
            }
        }
    }
    out
}

fn refused(command: &str, why: &str) -> String {
    let shown = if len16(command) > 160 {
        format!("{}...", head16(command, 160))
    } else {
        command.to_string()
    };
    format!(
        "Error: you are read-only (planning): {why}. Refused: {shown}. Use commands that only look (dir, ls, cat, type, grep, find, tree, git log, reg query, Get-ChildItem, Get-Content)."
    )
}
