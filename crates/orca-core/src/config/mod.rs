//! Configuration: the `config.json` schema, loading/saving, sampler presets and thinking levels.

pub mod dotnet_json;
pub mod manager;
pub mod memory_manager;
pub mod model;
pub mod project_instructions;
pub mod prompt_manager;

pub use manager::{
    ConfigError, ConfigManager, default_config_dir, serialize_config, serialize_config_with_newline,
};
pub use model::*;

/// Built-in sampler presets (`SamplingPresets`), following the model vendors' recommendations.
pub mod sampling_presets {
    use super::model::{LmStudioConfig, SamplingConfig};

    pub const NAMES: [&str; 3] = [
        "qwen-thinking-coding",
        "qwen-thinking-general",
        "qwen-instruct",
    ];

    /// The preset with this name (any case), or `None`.
    pub fn get(name: Option<&str>) -> Option<SamplingConfig> {
        let name = name?.to_ascii_lowercase();
        let preset = match name.as_str() {
            // Qwen's recommendation for thinking mode on precise coding tasks.
            "qwen-thinking-coding" => SamplingConfig {
                temperature: Some(0.6),
                top_p: Some(0.95),
                top_k: Some(20),
                min_p: Some(0.0),
                presence_penalty: Some(0.0),
                ..SamplingConfig::default()
            },
            // Qwen's general thinking-mode recommendation.
            "qwen-thinking-general" => SamplingConfig {
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(20),
                min_p: Some(0.0),
                presence_penalty: Some(1.5),
                ..SamplingConfig::default()
            },
            // Qwen's recommendation for non-thinking (instruct) mode.
            "qwen-instruct" => SamplingConfig {
                temperature: Some(0.7),
                top_p: Some(0.8),
                top_k: Some(20),
                min_p: Some(0.0),
                ..SamplingConfig::default()
            },
            _ => return None,
        };
        Some(preset)
    }

    /// Effective sampling for a model: `lmStudio.sampling`, overlaid by the model's
    /// `lmStudio.modelSampling` entry (model name matched in any case), with the selected
    /// preset filling whatever is still unset.
    pub fn resolve(lm_studio: &LmStudioConfig, model: Option<&str>) -> SamplingConfig {
        let base = lm_studio.sampling.clone().unwrap_or_default();
        let per_model = model.and_then(|m| {
            lm_studio
                .model_sampling
                .as_ref()?
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(m))
                .map(|(_, v)| v)
        });
        let effective = base.merged_with(per_model);
        match get(effective.preset.as_deref()) {
            Some(preset) => preset.merged_with(Some(&effective)),
            None => effective,
        }
    }
}

/// llama.cpp thinking levels (`ThinkingLevels`): `chat_template_kwargs.enable_thinking` plus
/// llama-server's per-request `thinking_budget_tokens`. Budgets match PiG's defaults.
pub mod thinking_levels {
    use super::model::ThinkingConfig;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Level {
        pub enabled: bool,
        pub budget_tokens: Option<i32>,
    }

    pub const NAMES: [&str; 6] = ["off", "minimal", "low", "medium", "high", "unlimited"];

    fn budget(name: &str) -> Option<Option<i32>> {
        Some(match name.to_ascii_lowercase().as_str() {
            "off" | "unlimited" => None,
            "minimal" => Some(1024),
            "low" => Some(2048),
            "medium" => Some(8192),
            "high" => Some(16384),
            _ => return None,
        })
    }

    pub fn is_valid(level: Option<&str>) -> bool {
        level.is_none_or(|l| budget(l).is_some())
    }

    /// The level to send, or `None` when `thinking.level` is unset or unknown (send nothing).
    /// A positive `thinking.budgetTokens` overrides the level's default budget.
    pub fn resolve(thinking: &ThinkingConfig) -> Option<Level> {
        let name = thinking.level.as_deref().filter(|l| !l.trim().is_empty())?;
        let default_budget = budget(name)?;
        if name.eq_ignore_ascii_case("off") {
            return Some(Level {
                enabled: false,
                budget_tokens: None,
            });
        }
        Some(Level {
            enabled: true,
            budget_tokens: if thinking.budget_tokens > 0 {
                Some(thinking.budget_tokens)
            } else {
                default_budget
            },
        })
    }
}
