//! The tool registry (`ToolRegistry`): compile-time registration instead of reflection.

use std::sync::Arc;

use orca_abstractions::{Tool, ToolCapabilities};

use crate::client::ToolDefinition;

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    /// The names sent to the model, in this order (C# `ToolExposure`, e.g. the Pi profile's
    /// `FilterPiProfile`). `None` = every registered tool, in registration order. Tools left
    /// out stay registered and callable.
    advertised: Option<Vec<String>>,
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

    /// Limit (and order) the tools sent to the model; `None` advertises every tool.
    pub fn set_advertised(&mut self, names: Option<Vec<String>>) {
        self.advertised = names;
    }

    /// The tools sent to the model, in advertised order (names that resolve to no tool are
    /// skipped).
    pub fn advertised(&self) -> Vec<Arc<dyn Tool>> {
        match &self.advertised {
            None => self.tools.clone(),
            Some(names) => {
                let mut out: Vec<Arc<dyn Tool>> = Vec::new();
                for n in names {
                    if let Some(t) = self.resolve(n)
                        && !out.iter().any(|o| o.name().eq_ignore_ascii_case(t.name()))
                    {
                        out.push(t);
                    }
                }
                out
            }
        }
    }

    /// The schemas sent for native function calling.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.advertised()
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameter_schema(),
            })
            .collect()
    }
}
