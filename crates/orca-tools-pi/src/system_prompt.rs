//! Port of Pi's system-prompt.ts (`PiSystemPrompt`): an untagged preamble, then tagged
//! sections joined by blank lines: tools, rules, addendum, project_context, cwd. As in C#, the
//! `<docs>` and `<skills>` sections are omitted.

use std::collections::{HashMap, HashSet};

/// A project instruction file for `<project_context>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiContextFile {
    pub path: String,
    pub content: String,
}

pub const PREAMBLE: &str = "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";

/// OpenOrca addition (pi.batchCallsRule): several independent calls in one response.
pub const BATCH_RULE: &str = "When several reads, searches or commands do not depend on each other, make them all in ONE response as multiple tool calls: they run in parallel. Wait for a result only when the next call needs it";

/// `PiSystemPrompt.Build`.
pub fn build(
    selected_tools: &[String],
    tool_snippets: &HashMap<String, String>,
    tool_guidelines: &HashMap<String, Vec<String>>,
    context_files: &[PiContextFile],
    cwd: &str,
    append_system_prompt: Option<&str>,
    batch_rule: bool,
) -> String {
    let mut sections: Vec<(&str, String)> = Vec::new();
    let visible: Vec<String> = selected_tools
        .iter()
        .filter_map(|n| match tool_snippets.get(n) {
            Some(s) if !s.is_empty() => Some(format!("- {n}: {s}")),
            _ => None,
        })
        .collect();
    let tools = if visible.is_empty() {
        "(none)".to_string()
    } else {
        visible.join("\n")
    };
    sections.push((
        "tools",
        format!(
            "{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."
        ),
    ));
    sections.push((
        "rules",
        build_rules(selected_tools, tool_guidelines, batch_rule),
    ));
    if let Some(a) = append_system_prompt.filter(|a| !a.is_empty()) {
        sections.push(("addendum", a.to_string()));
    }
    if !context_files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
        parts.extend(context_files.iter().map(|f| {
            format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                f.path, f.content
            )
        }));
        sections.push(("project_context", parts.join("\n\n")));
    }
    sections.push((
        "cwd",
        if cwd.is_empty() { "." } else { cwd }.replace('\\', "/"),
    ));

    let mut sb = String::from(PREAMBLE);
    for (name, content) in sections {
        sb.push_str(&format!("\n\n<{name}>\n{content}\n</{name}>"));
    }
    sb
}

/// Pi's buildRules: shell file-op rule, tool guidelines (trimmed, deduped), two fixed rules.
pub fn build_rules(
    selected_tools: &[String],
    tool_guidelines: &HashMap<String, Vec<String>>,
    batch_rule: bool,
) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |rule: &str| {
        let normalized = crate::arguments::dotnet_trim(rule);
        if !normalized.is_empty() && seen.insert(normalized.to_string()) {
            rules.push(normalized.to_string());
        }
    };
    let has = |n: &str| selected_tools.iter().any(|t| t == n);
    if (has("bash") || has("powershell")) && !has("grep") && !has("find") && !has("ls") {
        if has("bash") && has("powershell") {
            add(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
            );
        } else if has("powershell") {
            add("Use PowerShell for file operations like listing, searching, and finding files");
        } else {
            add("Use bash for file operations like ls, rg, find");
        }
    }
    for name in selected_tools {
        if let Some(guidelines) = tool_guidelines.get(name) {
            for rule in guidelines {
                add(rule);
            }
        }
    }
    if batch_rule && selected_tools.len() > 1 {
        add(BATCH_RULE);
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|r| format!("- {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}
