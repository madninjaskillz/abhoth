//! Agent status/origin enums, agent type definitions and the agent type registry (port of
//! `AgentStatus` / `AgentOrigin` from `AgentContext.cs`, `AgentTypeDefinition.cs` and
//! `AgentTypeRegistry.cs`).

use indexmap::IndexMap;

/// `AgentStatus`: where a sub-agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Pending,
    Running,
    Completed,
    /// The agent hit its iteration limit before finishing; `result` holds a salvaged partial
    /// summary.
    CompletedPartial,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// The C# enum member name (`ToString()`), e.g. `"CompletedPartial"`.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Pending => "Pending",
            AgentStatus::Running => "Running",
            AgentStatus::Completed => "Completed",
            AgentStatus::CompletedPartial => "CompletedPartial",
            AgentStatus::Failed => "Failed",
            AgentStatus::Cancelled => "Cancelled",
        }
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `AgentOrigin`: who started an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AgentOrigin {
    /// Spawned by the main agent through a spawn tool; its result returns to that tool call.
    #[default]
    Model,
    /// Started by the user (/agent); runs in the background, independent of the main turn.
    User,
    /// Started by a workflow or a command such as /review.
    Command,
}

impl AgentOrigin {
    /// The C# enum member name.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentOrigin::Model => "Model",
            AgentOrigin::User => "User",
            AgentOrigin::Command => "Command",
        }
    }
}

/// Defines a typed sub-agent: its name, LLM-facing description, allowed tools, and prompt
/// resource.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTypeDefinition {
    pub name: String,
    pub description: String,
    /// The tools the agent may use; `None` = every tool.
    pub allowed_tools: Option<Vec<String>>,
    /// An embedded prompt's file name ("review.md") or a prompt file path (custom agents).
    pub prompt_resource_name: String,
    /// Optional model reference ("provider/model" or bare "model") this agent type runs on.
    /// `None` uses the orchestrator's default chat client.
    pub model: Option<String>,
    /// Optional iteration budget override; `None` uses `config.agent.max_iterations`.
    pub max_iterations: Option<i32>,
    /// Optional wall-clock timeout override (seconds); `None` uses `config.agent.timeout_seconds`.
    pub timeout_seconds: Option<i32>,
}

impl AgentTypeDefinition {
    /// A definition without model / iteration / timeout overrides.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        allowed_tools: Option<Vec<String>>,
        prompt_resource_name: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            allowed_tools,
            prompt_resource_name: prompt_resource_name.into(),
            model: None,
            max_iterations: None,
            timeout_seconds: None,
        }
    }
}

/// The key `StringComparer.OrdinalIgnoreCase` compares by: each char upper-cased
/// (invariant, one-to-one mappings only, as .NET's `ToUpperInvariant` on a UTF-16 unit).
pub(crate) fn ci_key(s: &str) -> String {
    s.chars().map(upper_invariant).collect()
}

/// `char.ToUpperInvariant`: the simple upper-case mapping (a char whose upper case is several
/// chars, like `ß`, stays as it is).
pub(crate) fn upper_invariant(c: char) -> char {
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)`.
pub(crate) fn eq_ci(a: &str, b: &str) -> bool {
    a.chars()
        .map(upper_invariant)
        .eq(b.chars().map(upper_invariant))
}

/// A `Dictionary<string, T>(StringComparer.OrdinalIgnoreCase)`: insertion-ordered, the first
/// spelling of a key is kept when a later same-key entry overwrites the value.
#[derive(Debug, Clone)]
pub(crate) struct CiMap<T> {
    map: IndexMap<String, (String, T)>,
}

impl<T> Default for CiMap<T> {
    fn default() -> Self {
        Self {
            map: IndexMap::new(),
        }
    }
}

impl<T> CiMap<T> {
    pub(crate) fn insert(&mut self, key: &str, value: T) {
        match self.map.get_mut(&ci_key(key)) {
            Some(slot) => slot.1 = value,
            None => {
                self.map.insert(ci_key(key), (key.to_string(), value));
            }
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&T> {
        self.map.get(&ci_key(key)).map(|(_, v)| v)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.map.values().map(|(k, v)| (k.as_str(), v))
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

/// Registry of agent types. Built-in types are always available; custom types can be
/// registered at runtime.
#[derive(Debug, Clone)]
pub struct AgentTypeRegistry {
    built_in_types: CiMap<AgentTypeDefinition>,
    custom_types: CiMap<AgentTypeDefinition>,
    /// Pi profile built-ins (scout/planner/reviewer/worker, after Pi's subagent example
    /// agents). Their tools are Pi's read/bash/edit/write (+ grep/find/ls when enabled) — the
    /// OpenOrca built-ins name tools the Pi profile doesn't advertise, so they would run with
    /// no tools at all.
    pi_types: Option<CiMap<AgentTypeDefinition>>,
}

/// Old OpenOrca type names → Pi profile types, so either vocabulary works.
const PI_ALIASES: [(&str, &str); 5] = [
    ("explore", "scout"),
    ("plan", "planner"),
    ("review", "reviewer"),
    ("general", "worker"),
    ("bash", "worker"),
];

fn tools(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|s| s.to_string()).collect())
}

impl Default for AgentTypeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTypeRegistry {
    /// A registry holding the five OpenOrca built-in types.
    pub fn new() -> Self {
        let mut built_in_types = CiMap::default();
        built_in_types.insert(
            "explore",
            AgentTypeDefinition::new(
                "explore",
                "Fast read-only codebase exploration — find files, search code, understand structure",
                tools(&["read_file", "list_directory", "glob", "grep", "think"]),
                "explore.md",
            ),
        );
        built_in_types.insert(
            "plan",
            AgentTypeDefinition::new(
                "plan",
                "Architecture and implementation planning — research and design before coding",
                tools(&[
                    "read_file",
                    "list_directory",
                    "glob",
                    "grep",
                    "think",
                    "web_search",
                    "web_fetch",
                ]),
                "plan.md",
            ),
        );
        built_in_types.insert(
            "bash",
            AgentTypeDefinition::new(
                "bash",
                "Command execution specialist — run shell commands, manage processes",
                tools(&[
                    "bash",
                    "read_file",
                    "think",
                    "get_process_output",
                    "start_background_process",
                    "stop_process",
                    "env",
                ]),
                "bash.md",
            ),
        );
        built_in_types.insert(
            "review",
            AgentTypeDefinition::new(
                "review",
                "Code review and diff analysis — examine changes, check quality, find issues",
                tools(&[
                    "read_file",
                    "list_directory",
                    "glob",
                    "grep",
                    "git_diff",
                    "git_log",
                    "git_status",
                    "think",
                ]),
                "review.md",
            ),
        );
        built_in_types.insert(
            "general",
            AgentTypeDefinition::new(
                "general",
                "General-purpose agent with full tool access — use when no specialized type fits",
                None,
                "general.md",
            ),
        );
        Self {
            built_in_types,
            custom_types: CiMap::default(),
            pi_types: None,
        }
    }

    /// Whether the Pi profile's agent types are active.
    pub fn pi_profile(&self) -> bool {
        self.pi_types.is_some()
    }

    /// Switch the built-in types to the Pi profile's agents. `extra_tools` mirrors
    /// `pi.extraTools` (grep/find/ls advertised).
    pub fn use_pi_profile(&mut self, extra_tools: bool) {
        let read_only = if extra_tools {
            tools(&["read", "bash", "grep", "find", "ls", "job"])
        } else {
            tools(&["read", "bash", "job"])
        };
        let mut pi = CiMap::default();
        pi.insert(
            "scout",
            AgentTypeDefinition::new(
                "scout",
                "Fast codebase recon; returns files with line ranges, key findings and where to start, for handoff",
                read_only.clone(),
                "pi-scout.md",
            ),
        );
        pi.insert(
            "planner",
            AgentTypeDefinition::new(
                "planner",
                "Turns a goal plus context into a concrete implementation plan; does not modify files",
                read_only.clone(),
                "pi-planner.md",
            ),
        );
        pi.insert(
            "reviewer",
            AgentTypeDefinition::new(
                "reviewer",
                "Reviews code or changes for bugs, risks and quality; does not modify files",
                read_only,
                "pi-reviewer.md",
            ),
        );
        pi.insert(
            "worker",
            AgentTypeDefinition::new(
                "worker",
                "Builds and changes code with full capabilities (read, bash, edit, write); give it one self-contained piece of the work",
                None,
                "pi-worker.md",
            ),
        );
        self.pi_types = Some(pi);
    }

    /// Register a single custom agent type. Overwrites any existing custom type with the same
    /// name.
    pub fn register(&mut self, def: AgentTypeDefinition) {
        let name = def.name.clone();
        self.custom_types.insert(&name, def);
    }

    /// Register multiple custom agent types (keyed by name). Project types override global
    /// types. Custom types override built-in types with a same-name collision.
    pub fn register_custom<'a>(
        &mut self,
        customs: impl IntoIterator<Item = (&'a String, &'a AgentTypeDefinition)>,
    ) {
        for (name, def) in customs {
            self.custom_types.insert(name, def.clone());
        }
    }

    /// Clear all custom agent registrations.
    pub fn clear_custom(&mut self) {
        self.custom_types.clear();
    }

    /// Resolve an agent type by name (case-insensitive). Custom types take precedence over
    /// built-in types. Returns `None` if not found.
    pub fn resolve(&self, name: &str) -> Option<AgentTypeDefinition> {
        if let Some(custom) = self.custom_types.get(name) {
            return Some(custom.clone());
        }
        if let Some(pi) = &self.pi_types {
            if let Some(def) = pi.get(name) {
                return Some(def.clone());
            }
            return PI_ALIASES
                .iter()
                .find(|(old, _)| eq_ci(old, name))
                .and_then(|(_, alias)| pi.get(alias).cloned());
        }
        self.built_in_types.get(name).cloned()
    }

    /// The default agent type ("general", or "worker" in the Pi profile).
    pub fn get_default(&self) -> AgentTypeDefinition {
        match &self.pi_types {
            Some(pi) => pi.get("worker").cloned().expect("worker is a Pi built-in"),
            None => self
                .built_in_types
                .get("general")
                .cloned()
                .expect("general is a built-in"),
        }
    }

    fn built_ins(&self) -> &CiMap<AgentTypeDefinition> {
        self.pi_types.as_ref().unwrap_or(&self.built_in_types)
    }

    /// All registered agent types (built-in + custom; a custom type overrides a same-named
    /// built-in in the built-in's position, other customs follow in registration order).
    pub fn get_all(&self) -> Vec<AgentTypeDefinition> {
        let mut merged = self.built_ins().clone();
        for (name, def) in self.custom_types.iter() {
            merged.insert(name, def.clone());
        }
        merged.iter().map(|(_, d)| d.clone()).collect()
    }

    /// All registered agent type names (built-in keys, then custom keys not already present).
    pub fn get_all_names(&self) -> Vec<String> {
        let mut names: CiMap<()> = CiMap::default();
        for (name, _) in self.built_ins().iter() {
            names.insert(name, ());
        }
        for (name, _) in self.custom_types.iter() {
            names.insert(name, ());
        }
        names.iter().map(|(k, _)| k.to_string()).collect()
    }
}
