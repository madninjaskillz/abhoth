//! Custom agent definitions (port of `CustomAgentLoader.cs`): `.md` files with YAML
//! frontmatter in `<config>/agents/` (global) and `<project>/.orca/agents/` (project).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use super::types::{AgentTypeDefinition, CiMap, ci_key};
use crate::config::project_instructions::find_project_root;

/// YAML frontmatter delimited by `---` lines at the start of the file.
static FRONTMATTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A---\s*\n(.*?)\n---\s*\n").unwrap());
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^name:\s*(.+)$").unwrap());
static DESCRIPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^description:\s*(.+)$").unwrap());
static TOOLS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^tools:\s*\[([^\]]*)\]").unwrap());
/// Standard YAML block list: `tools:\n  - read_file\n  - grep`.
static TOOLS_BLOCK_LIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^tools:\s*\r?\n((?:[ \t]+-[ \t]*.+(?:\r?\n|$))+)").unwrap());
static MODEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^model:\s*(.+)$").unwrap());
static MAX_ITERATIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^max_iterations:\s*(\d+)\s*$").unwrap());
static TIMEOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^timeout:\s*(\d+)\s*$").unwrap());

/// Discovers and parses custom agent definitions from `.orca/agents/` and
/// `~/.openorca/agents/`. Each `.md` file with YAML frontmatter becomes a custom agent type.
#[derive(Debug, Clone, Copy, Default)]
pub struct CustomAgentLoader;

impl CustomAgentLoader {
    /// Discover custom agent definitions from the global directory (`config_dir/agents`) and
    /// the project directory (`<project root of cwd>/.orca/agents`). Project agents override
    /// global agents with the same name (case-insensitive; the first spelling of a name is
    /// kept as the key). Malformed files are reported via `on_warning` instead of being
    /// silently skipped. Returns name → definition, in discovery order.
    pub fn discover_agents(
        config_dir: &Path,
        cwd: &Path,
        registered_tool_names: Option<&[String]>,
        on_warning: &mut dyn FnMut(String),
    ) -> IndexMap<String, AgentTypeDefinition> {
        let mut agents = CiMap::default();

        // Global agents first (can be overridden by project agents).
        scan_directory(
            &config_dir.join("agents"),
            &mut agents,
            registered_tool_names,
            on_warning,
        );

        // Project agents (override global).
        if let Some(project_root) = find_project_root(cwd) {
            scan_directory(
                &project_root.join(".orca").join("agents"),
                &mut agents,
                registered_tool_names,
                on_warning,
            );
        }

        agents
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// Parse a single agent definition file. The second value is why parsing failed
    /// ("file not found", …), or a non-fatal warning when a definition is returned.
    pub fn parse_agent_file(
        file_path: &Path,
        registered_tool_names: Option<&[String]>,
    ) -> (Option<AgentTypeDefinition>, Option<String>) {
        if !file_path.is_file() {
            return (None, Some("file not found".to_string()));
        }
        let content = match std::fs::read(file_path) {
            Ok(bytes) => {
                let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
                String::from_utf8_lossy(bytes).into_owned()
            }
            Err(e) => return (None, Some(e.to_string())),
        };
        Self::parse_agent_content(
            &content,
            &file_path.to_string_lossy(),
            registered_tool_names,
        )
    }

    /// Parse agent definition content (`file_path` becomes the prompt resource name, so the
    /// prompt loader reads the body from that file). The second value is why parsing failed,
    /// or a non-fatal warning (unknown tools ignored) when a definition is returned.
    pub fn parse_agent_content(
        content: &str,
        file_path: &str,
        registered_tool_names: Option<&[String]>,
    ) -> (Option<AgentTypeDefinition>, Option<String>) {
        let Some(fm) = FRONTMATTER.captures(content) else {
            return (
                None,
                Some(
                    "missing YAML frontmatter (--- delimited block at the top of the file)"
                        .to_string(),
                ),
            );
        };
        let frontmatter = fm.get(1).map_or("", |m| m.as_str());

        let Some(name_match) = NAME.captures(frontmatter) else {
            return (
                None,
                Some("frontmatter is missing required field 'name'".to_string()),
            );
        };
        let Some(desc_match) = DESCRIPTION.captures(frontmatter) else {
            return (
                None,
                Some("frontmatter is missing required field 'description'".to_string()),
            );
        };
        let Some(mut tools) = parse_tools_list(frontmatter) else {
            return (
                None,
                Some(
                    "frontmatter is missing 'tools' (inline `tools: [a, b]` or a YAML block list)"
                        .to_string(),
                ),
            );
        };

        let name = name_match[1].trim().to_string();
        let description = desc_match[1].trim().to_string();
        let mut error = None;

        // Validate tool names if a registry is available.
        if let Some(registered) = registered_tool_names {
            let valid: std::collections::HashSet<String> =
                registered.iter().map(|t| ci_key(t)).collect();
            let unknown: Vec<String> = tools
                .iter()
                .filter(|t| !valid.contains(&ci_key(t)))
                .cloned()
                .collect();
            tools.retain(|t| valid.contains(&ci_key(t)));
            if tools.is_empty() {
                return (
                    None,
                    Some(format!(
                        "no valid tools remain (unknown: {})",
                        unknown.join(", ")
                    )),
                );
            }
            if !unknown.is_empty() {
                error = Some(format!("ignoring unknown tools: {}", unknown.join(", ")));
            }
        }

        if tools.is_empty() {
            return (None, Some("'tools' list is empty".to_string()));
        }

        let mut def = AgentTypeDefinition::new(name, description, Some(tools), file_path);
        def.model = parse_model(frontmatter);
        def.max_iterations = parse_positive_int(&MAX_ITERATIONS, frontmatter);
        def.timeout_seconds = parse_positive_int(&TIMEOUT, frontmatter);
        (Some(def), error)
    }

    /// Substitute the template variables (`{{TASK}}`, `{{CWD}}`, `{{PLATFORM}}`) in an agent
    /// prompt (ordinal, case-sensitive, in that order).
    pub fn substitute_template_vars(
        template: &str,
        task: &str,
        cwd: &str,
        platform: &str,
    ) -> String {
        template
            .replace("{{TASK}}", task)
            .replace("{{CWD}}", cwd)
            .replace("{{PLATFORM}}", platform)
    }
}

fn is_trim_quote(c: char) -> bool {
    c == '"' || c == '\'' || c == ' '
}

fn parse_model(frontmatter: &str) -> Option<String> {
    let caps = MODEL.captures(frontmatter)?;
    let model = caps[1].trim().trim_matches(|c| c == '"' || c == '\'');
    if model.trim().is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

fn parse_positive_int(pattern: &Regex, frontmatter: &str) -> Option<i32> {
    let caps = pattern.captures(frontmatter)?;
    match caps[1].parse::<i32>() {
        Ok(v) if v > 0 => Some(v),
        _ => None,
    }
}

fn parse_tools_list(frontmatter: &str) -> Option<Vec<String>> {
    if let Some(inline) = TOOLS.captures(frontmatter) {
        return Some(
            inline[1]
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|t| t.trim_matches(is_trim_quote))
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    if let Some(block) = TOOLS_BLOCK_LIST.captures(frontmatter) {
        return Some(
            block[1]
                .split('\n')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|line| {
                    line.trim_start_matches('-')
                        .trim()
                        .trim_matches(is_trim_quote)
                })
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    None
}

/// `*.md` files of a directory, in file-name order (`Directory.EnumerateFiles(dir, "*.md")`
/// lists NTFS directories alphabetically). The extension match is case-insensitive on Windows.
pub(crate) fn files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            let suffix = format!(".{ext}");
            name.is_some_and(|n| {
                if cfg!(windows) {
                    n.to_ascii_lowercase().ends_with(&suffix)
                } else {
                    n.ends_with(&suffix)
                }
            })
        })
        .collect();
    files.sort();
    files
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn scan_directory(
    dir: &Path,
    agents: &mut CiMap<AgentTypeDefinition>,
    registered_tool_names: Option<&[String]>,
    on_warning: &mut dyn FnMut(String),
) {
    if !dir.is_dir() {
        return;
    }
    for file in files_with_extension(dir, "md") {
        match CustomAgentLoader::parse_agent_file(&file, registered_tool_names) {
            (Some(def), error) => {
                // A definition can carry a non-fatal warning (e.g. unknown tools ignored).
                if let Some(error) = error {
                    on_warning(format!("Custom agent '{}': {}", file_name(&file), error));
                }
                let name = def.name.clone();
                agents.insert(&name, def);
            }
            (None, error) => on_warning(format!(
                "Skipping custom agent file '{}': {}",
                file_name(&file),
                error.as_deref().unwrap_or("unparseable")
            )),
        }
    }
}
