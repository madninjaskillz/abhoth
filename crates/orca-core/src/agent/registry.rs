//! The tool registry (`ToolRegistry`): compile-time registration instead of reflection.

use std::sync::Arc;

use orca_abstractions::{Tool, ToolCapabilities};

use crate::client::ToolDefinition;

/// Decides which registered tools are advertised to the model (`ToolExposure`).
pub type AdvertiseFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    advertise: Option<AdvertiseFilter>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; a later tool with the same name (any case) replaces the earlier one.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools
            .retain(|t| !t.name().eq_ignore_ascii_case(tool.name()));
        self.tools.push(tool);
    }

    pub fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for t in tools {
            self.register(t);
        }
    }

    /// The tool with this name, case-insensitively.
    pub fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|t| t.name().eq_ignore_ascii_case(name))
            .cloned()
    }

    pub fn has_capability(&self, name: &str, cap: ToolCapabilities) -> bool {
        self.resolve(name)
            .is_some_and(|t| t.capabilities().contains(cap))
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Advertise only the tools `filter` accepts (by name); all stay executable. Without a
    /// filter every tool is advertised.
    pub fn set_advertise_filter(&mut self, filter: AdvertiseFilter) {
        self.advertise = Some(filter);
    }

    /// The tools offered to the model (native tool definitions and the system prompt list).
    pub fn advertised(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter(|t| self.advertise.as_ref().is_none_or(|f| f(t.name())))
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// `FindClosestMatch`: a tool whose alias is the unknown name, else one whose name contains
    /// it or is contained in it.
    pub fn find_closest_match(&self, unknown: &str) -> Option<String> {
        let lower = unknown.to_ascii_lowercase();
        for t in &self.tools {
            if let Some(h) = t.render_hints()
                && h.aliases.iter().any(|a| a.eq_ignore_ascii_case(unknown))
            {
                return Some(t.name().to_string());
            }
        }
        self.tools
            .iter()
            .find(|t| {
                let n = t.name().to_ascii_lowercase();
                n.contains(&lower) || lower.contains(&n)
            })
            .map(|t| t.name().to_string())
    }

    /// The schemas sent for native function calling (the advertised tools).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.advertised()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameter_schema(),
            })
            .collect()
    }
}
