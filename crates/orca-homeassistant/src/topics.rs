//! Topic and id naming (SUPERVISOR.md 11.1). One place, so discovery, state and commands agree.

/// Lowercase, `[a-z0-9_]` only: safe in topics, unique ids and entity ids.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_us = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_us = false;
        } else if !last_us && !out.is_empty() {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topics {
    /// `abhoth/<server>`
    pub base: String,
    /// `homeassistant`
    pub discovery_prefix: String,
    /// `deepthought`
    pub server: String,
}

impl Topics {
    pub fn new(base_topic: &str, discovery_prefix: &str, server_id: &str) -> Self {
        let server = slug(server_id);
        Self { base: format!("{base_topic}/{server}"), discovery_prefix: discovery_prefix.to_string(), server }
    }

    pub fn availability(&self) -> String {
        format!("{}/availability", self.base)
    }
    pub fn card_availability(&self, card: &str) -> String {
        format!("{}/card/{}/availability", self.base, slug(card))
    }
    pub fn state(&self) -> String {
        format!("{}/state", self.base)
    }
    pub fn card_state(&self, card: &str) -> String {
        format!("{}/card/{}/state", self.base, slug(card))
    }
    pub fn fan_state(&self, fan: &str) -> String {
        format!("{}/fan/{}/state", self.base, slug(fan))
    }
    pub fn engine_state(&self, engine: &str) -> String {
        format!("{}/engine/{}/state", self.base, slug(engine))
    }
    pub fn layout_target(&self) -> String {
        format!("{}/layout_target", self.base)
    }
    pub fn set(&self, obj: &str) -> String {
        format!("{}/set/{obj}", self.base)
    }
    pub fn set_filter(&self) -> String {
        format!("{}/set/#", self.base)
    }
    /// Command results (not retained).
    pub fn result(&self) -> String {
        format!("{}/result", self.base)
    }
    /// The MQTT alert sink: one retained JSON per rule (10.3).
    pub fn alert(&self, rule: &str) -> String {
        format!("{}/alert/{}", self.base, slug(rule))
    }
    /// The HA `event` entity's state topic (not retained).
    pub fn alert_event(&self) -> String {
        format!("{}/alert_event", self.base)
    }

    /// `abhoth_<server>_<key>`.
    pub fn unique_id(&self, key: &str) -> String {
        format!("abhoth_{}_{}", self.server, key)
    }

    pub fn discovery(&self, component: &str, unique_id: &str) -> String {
        format!("{}/{component}/{unique_id}/config", self.discovery_prefix)
    }

    /// Any discovery topic this publisher owns (new ids only; legacy ids are listed explicitly).
    pub fn owns_discovery_topic(&self, topic: &str) -> bool {
        let Some(rest) = topic.strip_prefix(&format!("{}/", self.discovery_prefix)) else {
            return false;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        parts.len() == 3 && parts[2] == "config" && parts[1].starts_with(&format!("abhoth_{}_", self.server))
    }
}

/// ha-bridge's topics (SUPERVISOR.md 3.4, as recorded 2026-11-26). Cleared at cutover.
pub const LEGACY_TOPICS: &[&str] = &[
    "deepthought/state",
    "deepthought/bridge/availability",
    "deepthought/llm/state",
    "deepthought/llm/target",
    "deepthought/settings/maxclock",
    "deepthought/set/maxclock",
    "deepthought/set/llm_target",
    "deepthought/cmd/llm_apply",
];

/// ha-bridge discovery configs that have no replacement with the same id. "GPU Max Clock" (one
/// select for every card through clockctl) becomes a per-card clock cap with a TTL (Q6).
pub const LEGACY_RETIRED_DISCOVERY: &[&str] = &["select/deepthought_maxclock"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("DeepThought"), "deepthought");
        assert_eq!(slug("strata-a"), "strata_a");
        assert_eq!(slug("  ComfyUI (card C) "), "comfyui_card_c");
        assert_eq!(slug("B"), "b");
    }

    #[test]
    fn topic_shapes() {
        let t = Topics::new("abhoth", "homeassistant", "DeepThought");
        assert_eq!(t.availability(), "abhoth/deepthought/availability");
        assert_eq!(t.card_availability("B"), "abhoth/deepthought/card/b/availability");
        assert_eq!(t.set("layout_apply"), "abhoth/deepthought/set/layout_apply");
        assert_eq!(t.unique_id("card_b_core_temp"), "abhoth_deepthought_card_b_core_temp");
        assert_eq!(t.discovery("sensor", "abhoth_deepthought_x"), "homeassistant/sensor/abhoth_deepthought_x/config");
        assert!(t.owns_discovery_topic("homeassistant/sensor/abhoth_deepthought_x/config"));
        assert!(!t.owns_discovery_topic("homeassistant/sensor/abhoth_other_x/config"));
        assert!(!t.owns_discovery_topic("homeassistant/sensor/deepthought_gpu_temp/config"));
    }
}
