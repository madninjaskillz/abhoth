//! Workflow definitions and their discovery (port of `WorkflowDefinition.cs`,
//! `WorkflowLoader.cs` and `WorkflowRunner.SubstituteTemplate`).
//!
//! A workflow is a deterministic, user-invoked agent pipeline: sequential stages, each fanning
//! out a fixed set of agents in parallel. This is how "agent teams" ship without betting on a
//! small local model driving orchestration — the user scripts the shape, agents do the work.
//! Stored as `.orca/workflows/*.json` (project) or `~/.openorca/workflows/*.json` (global).
//! Task templates support `{{input}}` and `{{stage.N.output}}` (1-based).

use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::custom_agents::files_with_extension;
use super::types::{CiMap, eq_ci, upper_invariant};
use crate::config::project_instructions::find_project_root;

/// A workflow: a name, an optional description and its stages.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDefinition {
    pub name: String,
    pub description: Option<String>,
    pub stages: Vec<WorkflowStage>,
}

/// One stage: agents that run in parallel.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStage {
    pub name: Option<String>,
    pub agents: Vec<WorkflowAgentSpec>,
}

/// One agent of a stage: its task template and agent type (default "general").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentSpec {
    pub task: String,
    pub agent_type: String,
}

impl Default for WorkflowAgentSpec {
    fn default() -> Self {
        Self {
            task: String::new(),
            agent_type: "general".to_string(),
        }
    }
}

impl WorkflowDefinition {
    /// Parse a workflow the way the C# options do: property names case-insensitive, `//` and
    /// `/* */` comments skipped, trailing commas allowed, unknown properties ignored. Errors
    /// carry the C# messages for invalid workflows; JSON syntax and type errors carry
    /// serde_json's message. (Differences: a JSON `null` for a list or `agentType` counts as
    /// absent, where C# would store null and later fail.)
    pub fn parse(json: &str) -> Result<WorkflowDefinition, String> {
        let cleaned = strip_comments_and_trailing_commas(json);
        let value: Value = serde_json::from_str(&cleaned).map_err(|e| e.to_string())?;
        let wf = match value {
            Value::Null => None,
            Value::Object(_) => Some(read_definition(&value)?),
            _ => return Err(type_error("$", "WorkflowDefinition")),
        };
        let Some(wf) = wf.filter(|w| !w.name.trim().is_empty()) else {
            return Err("workflow must have a non-empty 'name'".to_string());
        };
        if wf.stages.is_empty() || wf.stages.iter().any(|s| s.agents.is_empty()) {
            return Err(
                "workflow needs at least one stage, and every stage at least one agent".to_string(),
            );
        }
        if wf
            .stages
            .iter()
            .flat_map(|s| &s.agents)
            .any(|a| a.task.trim().is_empty())
        {
            return Err("every agent needs a non-empty 'task'".to_string());
        }
        Ok(wf)
    }
}

fn type_error(path: &str, ty: &str) -> String {
    format!("The JSON value could not be converted to {ty}. Path: {path}")
}

/// The value of a property matched case-insensitively (the last one wins, as System.Text.Json
/// sets a property again for each occurrence).
fn prop<'a>(obj: &'a Value, name: &str) -> Option<&'a Value> {
    obj.as_object()?
        .iter()
        .filter(|(k, _)| eq_ci(k, name))
        .map(|(_, v)| v)
        .last()
}

fn opt_string(obj: &Value, name: &str, path: &str) -> Result<Option<String>, String> {
    match prop(obj, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(type_error(&format!("{path}.{name}"), "System.String")),
    }
}

fn list<'a>(obj: &'a Value, name: &str, path: &str) -> Result<&'a [Value], String> {
    match prop(obj, name) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(type_error(
            &format!("{path}.{name}"),
            "System.Collections.Generic.List",
        )),
    }
}

fn read_definition(v: &Value) -> Result<WorkflowDefinition, String> {
    let mut stages = Vec::new();
    for (i, s) in list(v, "stages", "$")?.iter().enumerate() {
        let path = format!("$.stages[{i}]");
        if !s.is_object() {
            return Err(type_error(
                &path,
                "OpenOrca.Core.Orchestration.WorkflowStage",
            ));
        }
        let mut agents = Vec::new();
        for (j, a) in list(s, "agents", &path)?.iter().enumerate() {
            let apath = format!("{path}.agents[{j}]");
            if !a.is_object() {
                return Err(type_error(
                    &apath,
                    "OpenOrca.Core.Orchestration.WorkflowAgentSpec",
                ));
            }
            let mut spec = WorkflowAgentSpec::default();
            if let Some(task) = opt_string(a, "task", &apath)? {
                spec.task = task;
            }
            if let Some(ty) = opt_string(a, "agentType", &apath)? {
                spec.agent_type = ty;
            }
            agents.push(spec);
        }
        stages.push(WorkflowStage {
            name: opt_string(s, "name", &path)?,
            agents,
        });
    }
    Ok(WorkflowDefinition {
        name: opt_string(v, "name", "$")?.unwrap_or_default(),
        description: opt_string(v, "description", "$")?,
        stages,
    })
}

/// JSON with `//` and `/* */` comments (outside strings) replaced by a space and commas
/// directly before a closing `]` / `}` removed.
fn strip_comments_and_trailing_commas(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                out.push(' ');
            }
            ']' | '}' => {
                let kept = out.trim_end().len();
                if out[..kept].ends_with(',') {
                    out.truncate(kept - 1);
                }
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Discovers workflow definitions: built-ins, then `<config>/workflows/*.json`, then
/// `<project>/.orca/workflows/*.json` — later sources override earlier ones by name
/// (case-insensitive; the first spelling of a name stays the key).
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowLoader;

impl WorkflowLoader {
    /// All workflows, by name. `config_dir` is the OpenOrca config directory; the project is
    /// found from `cwd`. Files that fail to parse are reported through `on_warning`.
    pub fn discover(
        config_dir: &Path,
        cwd: &Path,
        on_warning: &mut dyn FnMut(String),
    ) -> IndexMap<String, WorkflowDefinition> {
        let mut workflows = CiMap::default();
        for built_in in Self::built_ins() {
            let name = built_in.name.clone();
            workflows.insert(&name, built_in);
        }
        scan_directory(&config_dir.join("workflows"), &mut workflows, on_warning);
        if let Some(project_root) = find_project_root(cwd) {
            scan_directory(
                &project_root.join(".orca").join("workflows"),
                &mut workflows,
                on_warning,
            );
        }
        workflows
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// Built-in workflows — overridable by dropping a same-named definition on disk.
    pub fn built_ins() -> Vec<WorkflowDefinition> {
        let agent = |agent_type: &str, task: &str| WorkflowAgentSpec {
            task: task.to_string(),
            agent_type: agent_type.to_string(),
        };
        let stage = |name: &str, agents: Vec<WorkflowAgentSpec>| WorkflowStage {
            name: Some(name.to_string()),
            agents,
        };
        vec![
            WorkflowDefinition {
                name: "verify".to_string(),
                description: Some(
                    "Review the current uncommitted changes with a review agent.".to_string(),
                ),
                stages: vec![stage(
                    "Review",
                    vec![agent(
                        "review",
                        "Review the current uncommitted git changes (use git_status and git_diff). \
                         Look for bugs, missing error handling, and inconsistencies with the surrounding code. \
                         Report findings with file paths and line references, most severe first. \
                         User context: {{input}}",
                    )],
                )],
            },
            WorkflowDefinition {
                name: "explore-wide".to_string(),
                description: Some(
                    "Three explore agents map the codebase from different angles, then one synthesizes."
                        .to_string(),
                ),
                stages: vec![
                    stage(
                        "Explore",
                        vec![
                            agent(
                                "explore",
                                "Map the STRUCTURE of this codebase: top-level directories, projects/modules, entry points, \
                                 and how they depend on each other. Focus area: {{input}}",
                            ),
                            agent(
                                "explore",
                                "Find the CORE LOGIC of this codebase relevant to: {{input}}. \
                                 Identify the key classes/functions, what they do, and where they live.",
                            ),
                            agent(
                                "explore",
                                "Survey the TESTS and configuration of this codebase: test projects, coverage patterns, \
                                 build/config files. Note anything relevant to: {{input}}",
                            ),
                        ],
                    ),
                    stage(
                        "Synthesize",
                        vec![agent(
                            "general",
                            "Synthesize these three exploration reports into one coherent overview \
                             answering: {{input}}\n\n{{stage.1.output}}",
                        )],
                    ),
                ],
            },
            WorkflowDefinition {
                name: "test-fix".to_string(),
                description: Some("Run the tests, fix what fails, verify the fix.".to_string()),
                stages: vec![
                    stage(
                        "Run tests",
                        vec![agent(
                            "bash",
                            "Run this project's test suite (detect the toolchain from the repo files). \
                             Report which tests failed with their error output, or state that all passed. \
                             Context: {{input}}",
                        )],
                    ),
                    stage(
                        "Fix",
                        vec![agent(
                            "general",
                            "Fix the test failures described below. Make the smallest correct change; \
                             re-run the failing tests to confirm. If everything already passed, do nothing and say so.\n\n{{stage.1.output}}",
                        )],
                    ),
                    stage(
                        "Verify",
                        vec![agent(
                            "review",
                            "Review the uncommitted changes just made to fix tests (git_diff). \
                             Confirm they are minimal and correct; flag anything suspicious.\n\nFix report:\n{{stage.2.output}}",
                        )],
                    ),
                ],
            },
        ]
    }
}

fn scan_directory(
    dir: &Path,
    workflows: &mut CiMap<WorkflowDefinition>,
    on_warning: &mut dyn FnMut(String),
) {
    if !dir.is_dir() {
        return;
    }
    for file in files_with_extension(dir, "json") {
        let file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = match std::fs::read(&file) {
            Ok(bytes) => {
                let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
                String::from_utf8_lossy(bytes).into_owned()
            }
            Err(e) => {
                on_warning(format!("Skipping workflow '{file_name}': {e}"));
                continue;
            }
        };
        match WorkflowDefinition::parse(&text) {
            Ok(wf) => {
                let name = wf.name.clone();
                workflows.insert(&name, wf);
            }
            Err(error) => on_warning(format!("Skipping workflow '{file_name}': {error}")),
        }
    }
}

/// Replace `{{input}}` and `{{stage.N.output}}` (1-based) in a task template, ignoring case.
pub fn substitute_template(template: &str, input: &str, stage_outputs: &[String]) -> String {
    let mut result = replace_ci(template, "{{input}}", input);
    for (n, output) in stage_outputs.iter().enumerate() {
        result = replace_ci(&result, &format!("{{{{stage.{}.output}}}}", n + 1), output);
    }
    result
}

/// `string.Replace(old, new, StringComparison.OrdinalIgnoreCase)`: every non-overlapping
/// occurrence, left to right.
fn replace_ci(haystack: &str, old: &str, new: &str) -> String {
    let pattern: Vec<char> = old.chars().map(upper_invariant).collect();
    if pattern.is_empty() {
        return haystack.to_string();
    }
    let chars: Vec<(usize, char)> = haystack.char_indices().collect();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < chars.len() {
        let matches = i + pattern.len() <= chars.len()
            && pattern
                .iter()
                .enumerate()
                .all(|(k, p)| upper_invariant(chars[i + k].1) == *p);
        if matches {
            out.push_str(new);
            i += pattern.len();
        } else {
            out.push(chars[i].1);
            i += 1;
        }
    }
    out
}
