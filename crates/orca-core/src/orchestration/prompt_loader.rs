//! Agent prompt templates (port of `AgentPromptLoader.cs`): embedded prompts or prompt files,
//! with template variables substituted.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::custom_agents::CustomAgentLoader;
use super::types::AgentTypeDefinition;

/// The embedded prompts (`OpenOrca.Core.Orchestration.Prompts.*` resources), by file name.
/// The files are byte copies of the C# ones (CRLF line breaks included).
const EMBEDDED: [(&str, &str); 9] = [
    ("bash.md", include_str!("prompts/bash.md")),
    ("explore.md", include_str!("prompts/explore.md")),
    ("general.md", include_str!("prompts/general.md")),
    ("pi-planner.md", include_str!("prompts/pi-planner.md")),
    ("pi-reviewer.md", include_str!("prompts/pi-reviewer.md")),
    ("pi-scout.md", include_str!("prompts/pi-scout.md")),
    ("pi-worker.md", include_str!("prompts/pi-worker.md")),
    ("plan.md", include_str!("prompts/plan.md")),
    ("review.md", include_str!("prompts/review.md")),
];

static FRONTMATTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A---[ \t]*\r?\n.*?\r?\n---[ \t]*(\r?\n|\z)").unwrap());

/// Loads agent prompt templates from embedded resources or file paths, and substitutes
/// template variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentPromptLoader;

impl AgentPromptLoader {
    /// Load the prompt for the given agent type, substituting template variables. If
    /// `prompt_resource_name` is an absolute or relative file path, loads from the file system
    /// (frontmatter stripped, leading whitespace trimmed); otherwise from the embedded prompts.
    /// Returns `None` if the resource/file is not found.
    pub fn load_prompt(
        agent_type: &AgentTypeDefinition,
        task: &str,
        cwd: &str,
        platform: &str,
    ) -> Option<String> {
        let template = if is_file_path(&agent_type.prompt_resource_name) {
            load_from_file(&agent_type.prompt_resource_name)?
        } else {
            load_from_embedded_resource(&agent_type.prompt_resource_name)?.to_string()
        };
        Some(CustomAgentLoader::substitute_template_vars(
            &template, task, cwd, platform,
        ))
    }

    /// Strip YAML frontmatter if present (the body is the prompt template). Tolerates CRLF
    /// line endings — Windows-authored agent files used to leak their frontmatter verbatim
    /// into the sub-agent system prompt.
    pub fn strip_frontmatter(content: &str) -> String {
        if !content.starts_with("---") {
            return content.to_string();
        }
        match FRONTMATTER.find(content) {
            Some(m) => content[m.end()..].to_string(),
            None => content.to_string(),
        }
    }
}

/// Whether a prompt resource name is a file path (vs an embedded resource): file paths contain
/// directory separators or are rooted; embedded resources are bare file names like
/// "review.md".
fn is_file_path(resource_name: &str) -> bool {
    resource_name.contains(std::path::MAIN_SEPARATOR)
        || resource_name.contains('/')
        || Path::new(resource_name).has_root()
}

fn load_from_file(file_path: &str) -> Option<String> {
    let path = Path::new(file_path);
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(bytes);
    Some(
        AgentPromptLoader::strip_frontmatter(&text)
            .trim_start()
            .to_string(),
    )
}

/// An embedded prompt by file name (ordinal, as manifest resource names are matched).
fn load_from_embedded_resource(resource_name: &str) -> Option<&'static str> {
    EMBEDDED
        .iter()
        .find(|(name, _)| *name == resource_name)
        .map(|(_, text)| *text)
}
