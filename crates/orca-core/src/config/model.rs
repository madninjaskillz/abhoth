//! The `~/.openorca/config.json` schema (port of `OrcaConfig.cs`, `SlotsConfig.cs`,
//! `SamplingConfig.cs`).
//!
//! Compatibility rules (see `PORTING.md`, "Config compatibility"):
//! * Field order is the C# declaration order, so a file the C# version wrote is written back
//!   byte for byte (`tests/config_compat.rs` checks this against files the C# build produced).
//! * Names are camelCase, `null`s are not written (`JsonIgnoreCondition.WhenWritingNull`).
//! * Get-only C# properties that System.Text.Json serializes (`effectiveKeepFirstTokens`,
//!   `slots.leadContextWindow`) are written, recomputed on save, and ignored on load.
//! * Unlike the C# version, unknown top-level keys are kept (`extra`), so a config written by a
//!   newer C# build survives a round trip through the Rust port.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Case-sensitive, insertion-ordered map (System.Text.Json `Dictionary<,>` keeps insertion order).
pub type Map<V> = IndexMap<String, V>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OrcaConfig {
    pub lm_studio: LmStudioConfig,
    /// Named providers. Lookups are case-insensitive: use [`OrcaConfig::provider`].
    pub providers: Map<ProviderConfig>,
    pub permissions: PermissionsConfig,
    pub session: SessionConfig,
    pub context: ContextConfig,
    pub agent: AgentConfig,
    pub hooks: HooksConfig,
    pub memory: MemoryConfig,
    pub thinking: ThinkingConfig,
    pub shell: ShellConfig,
    pub image_gen: ImageGenerationConfig,
    pub lsp: LspConfig,
    pub formatter: FormatterConfig,
    pub tools: ToolExposureConfig,
    pub ui: UiConfig,
    pub mcp_servers: Map<McpServerConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<SlotsConfig>,
    pub tool_profile: String,
    pub pi: PiProfileConfig,
    pub bridges: BridgesConfig,
    pub profiles: ProfilesConfig,

    /// Rust-only: top-level keys this port doesn't know, kept as they were.
    #[serde(flatten)]
    pub extra: Map<Value>,

    /// Runtime-only (never serialized): the profile applied to this session.
    #[serde(skip)]
    pub active_profile: Option<String>,
    /// Runtime-only: --demo.
    #[serde(skip)]
    pub demo_mode: bool,
    /// Runtime-only: --sandbox / --simple.
    #[serde(skip)]
    pub sandbox_mode: bool,
    /// Runtime-only: --allow-dir.
    #[serde(skip)]
    pub allowed_directory: Option<String>,
}

impl Default for OrcaConfig {
    fn default() -> Self {
        Self {
            lm_studio: LmStudioConfig::default(),
            providers: Map::new(),
            permissions: PermissionsConfig::default(),
            session: SessionConfig::default(),
            context: ContextConfig::default(),
            agent: AgentConfig::default(),
            hooks: HooksConfig::default(),
            memory: MemoryConfig::default(),
            thinking: ThinkingConfig::default(),
            shell: ShellConfig::default(),
            image_gen: ImageGenerationConfig::default(),
            lsp: LspConfig::default(),
            formatter: FormatterConfig::default(),
            tools: ToolExposureConfig::default(),
            ui: UiConfig::default(),
            mcp_servers: Map::new(),
            slots: None,
            tool_profile: "openorca".into(),
            pi: PiProfileConfig::default(),
            bridges: BridgesConfig::default(),
            profiles: ProfilesConfig::default(),
            extra: Map::new(),
            active_profile: None,
            demo_mode: false,
            sandbox_mode: false,
            allowed_directory: None,
        }
    }
}

impl OrcaConfig {
    /// A named provider, case-insensitively (the C# dictionary uses OrdinalIgnoreCase).
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// Refresh the get-only values System.Text.Json writes out (call before serializing).
    pub(crate) fn refresh_computed(&mut self) {
        self.context.effective_keep_first_tokens =
            self.context.compute_effective_keep_first_tokens();
        if let Some(slots) = self.slots.as_mut() {
            slots.lead_context_window = slots.compute_lead_context_window();
        }
    }
}

/// A named LLM provider endpoint (`ProviderConfig`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderConfig {
    /// "openai-compat" (default), "anthropic" or "ollama".
    #[serde(rename = "type")]
    pub kind: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    pub models: Vec<String>,
    /// Write-only alias of `type` ("provider": "anthropic"); applied after loading.
    #[serde(skip_serializing)]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ProviderPricing>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai-compat".into(),
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
            models: Vec::new(),
            provider: None,
            model: None,
            context_window: None,
            max_tokens: None,
            pricing: None,
        }
    }
}

impl ProviderConfig {
    /// The model requests go to: the first non-blank of `models`, else `model`.
    pub fn primary_model(&self) -> Option<&str> {
        self.models
            .iter()
            .map(String::as_str)
            .find(|m| !m.trim().is_empty())
            .or(self.model.as_deref())
    }

    /// Apply the `provider` alias (C# does it in the property setter during deserialization).
    pub(crate) fn apply_alias(&mut self) {
        if let Some(p) = self.provider.take()
            && !p.trim().is_empty()
        {
            self.kind = p.trim().to_string();
        }
    }
}

/// USD per million tokens (`ProviderPricing`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderPricing {
    pub input_per_m_tok: f64,
    pub output_per_m_tok: f64,
    pub cache_read_per_m_tok: f64,
    pub cache_write_per_m_tok: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_context_threshold: Option<i32>,
    pub long_context_factor: f64,
}

impl Default for ProviderPricing {
    fn default() -> Self {
        Self {
            input_per_m_tok: 0.0,
            output_per_m_tok: 0.0,
            cache_read_per_m_tok: 0.0,
            cache_write_per_m_tok: 0.0,
            long_context_threshold: None,
            long_context_factor: 1.0,
        }
    }
}

impl ProviderPricing {
    /// USD for one request; `uncached` excludes cache reads and writes.
    pub fn cost(&self, uncached: i64, cache_read: i64, cache_write: i64, output: i64) -> f64 {
        let prompt = uncached + cache_read + cache_write;
        let factor = match self.long_context_threshold {
            Some(t) if t > 0 && prompt > i64::from(t) => self.long_context_factor,
            _ => 1.0,
        };
        factor
            * (uncached as f64 * self.input_per_m_tok
                + cache_read as f64 * self.cache_read_per_m_tok
                + cache_write as f64 * self.cache_write_per_m_tok
                + output as f64 * self.output_per_m_tok)
            / 1e6
    }
}

/// The main model server (`LmStudioConfig`; the name is historical).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LmStudioConfig {
    pub base_url: String,
    pub api_key: String,
    /// Wire protocol: "openai-compat" (default) or "anthropic".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Whether the servers take images; null = ask each server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    pub models: Vec<String>,
    /// Legacy; migrated into `models` on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Legacy; migrated into `models` on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_model: Option<String>,
    pub temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_sampling: Option<Map<SamplingConfig>>,
    pub timeout_seconds: i32,
    pub streaming_timeout_seconds: i32,
    pub native_tool_calling: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_profile: Option<String>,
    pub backup_min_response_length: i32,
    pub backup_on_refusal: bool,
    pub pair_model_programming: bool,
}

impl Default for LmStudioConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234/v1".into(),
            api_key: "lm-studio".into(),
            provider: None,
            api_key_env: None,
            vision: None,
            models: Vec::new(),
            model: None,
            backup_model: None,
            temperature: 0.7,
            max_tokens: None,
            sampling: None,
            model_sampling: None,
            timeout_seconds: 120,
            streaming_timeout_seconds: 120,
            native_tool_calling: false,
            prompt_profile: None,
            backup_min_response_length: 0,
            backup_on_refusal: false,
            pair_model_programming: false,
        }
    }
}

impl LmStudioConfig {
    /// True when the main model server speaks the Anthropic Messages API.
    pub fn is_anthropic(&self) -> bool {
        self.provider
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("anthropic"))
    }

    /// True when tool-result images go into the conversation.
    pub fn tool_images(&self) -> bool {
        self.is_anthropic() || self.vision != Some(false)
    }

    /// The primary (first) model, or `None`.
    pub fn primary_model(&self) -> Option<&str> {
        self.models.first().map(String::as_str)
    }

    /// The API key: `apiKeyEnv`'s value when set and non-empty, else `apiKey`.
    pub fn resolve_api_key(&self) -> String {
        resolve_key(self.api_key_env.as_deref(), Some(&self.api_key)).unwrap_or_default()
    }
}

/// `apiKeyEnv` wins when the variable is set and non-empty; else the literal key.
pub fn resolve_key(env_name: Option<&str>, literal: Option<&str>) -> Option<String> {
    if let Some(name) = env_name.filter(|n| !n.trim().is_empty())
        && let Ok(v) = std::env::var(name)
        && !v.is_empty()
    {
        return Some(v);
    }
    literal.map(str::to_string)
}

/// Optional sampler settings (`SamplingConfig`); unset fields are not sent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SamplingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// Sent as both `repeat_penalty` (llama.cpp) and `repetition_penalty` (vLLM).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f32>,
}

impl SamplingConfig {
    /// Field-wise merge: values set on `overlay` win.
    pub fn merged_with(&self, overlay: Option<&SamplingConfig>) -> SamplingConfig {
        let Some(o) = overlay else {
            return self.clone();
        };
        SamplingConfig {
            preset: o.preset.clone().or_else(|| self.preset.clone()),
            temperature: o.temperature.or(self.temperature),
            top_p: o.top_p.or(self.top_p),
            top_k: o.top_k.or(self.top_k),
            min_p: o.min_p.or(self.min_p),
            presence_penalty: o.presence_penalty.or(self.presence_penalty),
            repetition_penalty: o.repetition_penalty.or(self.repetition_penalty),
        }
    }
}

/// `toolProfile: "pi"` settings (`PiProfileConfig`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PiProfileConfig {
    pub extra_tools: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    pub read_before_edit: bool,
    pub batch_calls_rule: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    pub agent_tools: bool,
    pub bash_timeout_seconds: i32,
    pub jobs: bool,
}

impl Default for PiProfileConfig {
    fn default() -> Self {
        Self {
            extra_tools: false,
            shell_path: None,
            read_before_edit: false,
            batch_calls_rule: true,
            append_system_prompt: None,
            agent_tools: true,
            bash_timeout_seconds: 300,
            jobs: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolExposureConfig {
    pub extended_tools: bool,
    pub extra_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionsConfig {
    pub auto_approve_all: bool,
    pub auto_approve_read_only: bool,
    pub auto_approve_moderate: bool,
    pub always_approve: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub allow_patterns: Vec<String>,
    pub deny_patterns: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            auto_approve_all: false,
            auto_approve_read_only: true,
            auto_approve_moderate: false,
            always_approve: Vec::new(),
            disabled_tools: Vec::new(),
            allow_patterns: Vec::new(),
            deny_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionConfig {
    pub auto_save: bool,
    pub max_sessions: i32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            auto_save: true,
            max_sessions: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextConfig {
    pub context_window_size: i32,
    pub auto_detect_context_window: bool,
    pub auto_compact_threshold: f32,
    pub compact_preserve_last_n: i32,
    pub auto_compact_enabled: bool,
    pub reserve_tokens: i32,
    pub keep_recent_tokens: i32,
    pub compact_at_ratio: f32,
    pub keep_first_tokens: i32,
    /// Get-only in C# (`min(keepFirstTokens, window / 10)`) but serialized; ignored on load.
    #[serde(skip_deserializing)]
    pub effective_keep_first_tokens: i32,
    pub compact_cost_gate: bool,
    pub thinking_aware_compaction: bool,
    pub prefill_tokens_per_second: i32,
    pub chars_per_token: f32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        let mut c = Self {
            context_window_size: 8192,
            auto_detect_context_window: true,
            auto_compact_threshold: 0.8,
            compact_preserve_last_n: 4,
            auto_compact_enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 0,
            compact_at_ratio: 0.5,
            keep_first_tokens: 9000,
            effective_keep_first_tokens: 0,
            compact_cost_gate: true,
            thinking_aware_compaction: true,
            prefill_tokens_per_second: 1000,
            chars_per_token: 3.5,
        };
        c.effective_keep_first_tokens = c.compute_effective_keep_first_tokens();
        c
    }
}

impl ContextConfig {
    pub fn compute_effective_keep_first_tokens(&self) -> i32 {
        self.keep_first_tokens
            .min(self.context_window_size / 10)
            .max(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    /// "native", "claude" or "codex".
    pub engine: String,
    pub max_iterations: i32,
    pub main_loop_max_iterations: i32,
    pub max_iterations_hard_cap: i32,
    pub stuck_probe_threshold: i32,
    pub task_reminder_interval: i32,
    pub task_reminder_min_context_tokens: i32,
    pub verification_mode: String,
    pub max_concurrent: i32,
    pub preload_brief_files: bool,
    pub repeat_call_notes: bool,
    pub early_read_only_tools: bool,
    pub min_free_copies_to_delegate: i32,
    pub peer_mode: bool,
    pub peer_background: bool,
    pub peer_repetition_guard: bool,
    pub peer_max_cut_offs_in_a_row: i32,
    pub peer_max_tokens: i32,
    pub lead_mode: String,
    pub lead_bash: bool,
    pub reviewer_max_iterations: i32,
    pub iteration_extensions: i32,
    pub timeout_seconds: i32,
    pub reasoning: AgentReasoningConfig,
    pub uncensored: AgentUncensoredConfig,
    pub max_spawn_depth: i32,
    pub tool_timeout_seconds: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_schedule: Option<Vec<String>>,
    pub max_agents: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner: Option<PlannerConfig>,
}

impl AgentConfig {
    pub const DEFAULT_MAX_ITERATIONS: i32 = 40;
    pub const DEFAULT_TIMEOUT_SECONDS: i32 = 1200;
    pub const LOCAL_PROVIDER: &'static str = "local";

    /// The provider for the agent at 0-based creation `position`: `None` = local.
    pub fn provider_at(&self, position: usize) -> Option<String> {
        let schedule = self.provider_schedule.as_ref().filter(|s| !s.is_empty())?;
        let name = schedule[position.min(schedule.len() - 1)].trim();
        if name.is_empty() || name.eq_ignore_ascii_case(Self::LOCAL_PROVIDER) {
            None
        } else {
            Some(name.to_string())
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            engine: "native".into(),
            max_iterations: Self::DEFAULT_MAX_ITERATIONS,
            main_loop_max_iterations: 50,
            max_iterations_hard_cap: 150,
            stuck_probe_threshold: 8,
            task_reminder_interval: 8,
            task_reminder_min_context_tokens: 16000,
            verification_mode: "auto".into(),
            max_concurrent: 2,
            preload_brief_files: true,
            repeat_call_notes: true,
            early_read_only_tools: true,
            min_free_copies_to_delegate: 0,
            peer_mode: false,
            peer_background: true,
            peer_repetition_guard: true,
            peer_max_cut_offs_in_a_row: 3,
            peer_max_tokens: 0,
            lead_mode: "auto".into(),
            lead_bash: true,
            reviewer_max_iterations: 15,
            iteration_extensions: 2,
            timeout_seconds: Self::DEFAULT_TIMEOUT_SECONDS,
            reasoning: AgentReasoningConfig::default(),
            uncensored: AgentUncensoredConfig::default(),
            max_spawn_depth: 3,
            tool_timeout_seconds: 120,
            provider_schedule: None,
            max_agents: 0,
            planner: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlannerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub max_steps: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub timeout_seconds: i32,
    pub plan_file: String,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            provider: None,
            max_steps: 30,
            max_tokens: None,
            effort: Some("high".into()),
            timeout_seconds: 1200,
            plan_file: "PLAN.md".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HooksConfig {
    pub pre_tool_hooks: Map<String>,
    pub post_tool_hooks: Map<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryConfig {
    pub auto_memory_enabled: bool,
    pub include_in_prompt: bool,
    pub max_memory_files: i32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_memory_enabled: false,
            include_in_prompt: false,
            max_memory_files: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThinkingConfig {
    /// Max tokens for reasoning; 0 = unlimited.
    pub budget_tokens: i32,
    /// Sent as `reasoning_effort` when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// llama.cpp thinking level: off, minimal, low, medium, high, unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    pub runaway_cap_chunks: i32,
    pub default_visible: bool,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            budget_tokens: 0,
            reasoning_effort: None,
            level: None,
            runaway_cap_chunks: 12000,
            default_visible: false,
        }
    }
}

/// The `/uncensored` switch (`AgentUncensoredConfig`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentUncensoredConfig {
    pub enabled: bool,
    pub body_field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_kinds: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_model: Option<String>,
    /// Runtime-only: what /uncensored set for this session.
    #[serde(skip)]
    pub session: Option<bool>,
}

impl Default for AgentUncensoredConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            body_field: "experimental_speed_projection".into(),
            body_value: None,
            providers: None,
            server_kinds: Some(vec!["strata".into()]),
            image_model: Some("qwen-image-uncensored".into()),
            session: None,
        }
    }
}

impl AgentUncensoredConfig {
    /// Whether requests carry the field right now.
    pub fn is_on(&self) -> bool {
        self.session.unwrap_or(self.enabled)
    }

    /// The field to add to a request, or `None` while off (or with a blank field name).
    pub fn field(&self) -> Option<(String, Value)> {
        if !self.is_on() || self.body_field.trim().is_empty() {
            return None;
        }
        Some((
            self.body_field.trim().to_string(),
            self.body_value.clone().unwrap_or(Value::Bool(true)),
        ))
    }

    /// `IsReadBy`: a server of this kind is known to read the field (its kind is listed).
    pub fn is_read_by(&self, kind: crate::client::ServerKind) -> bool {
        kind != crate::client::ServerKind::Unknown
            && self.server_kinds.as_ref().is_some_and(|k| {
                k.iter()
                    .any(|s| s.eq_ignore_ascii_case(server_kind_name(kind)))
            })
    }

    /// `IsSentTo`: the field goes to a listed kind, to a server that could not be identified,
    /// and to one that is neither Strata nor ninfer (a gateway passes it on).
    pub fn is_sent_to(&self, kind: crate::client::ServerKind) -> bool {
        use crate::client::ServerKind;
        matches!(kind, ServerKind::Unknown | ServerKind::Other) || self.is_read_by(kind)
    }

    /// `FieldFor`: the field for a named provider's request: only while on, and only to a
    /// provider `providers` lists or one on a local server's host.
    pub fn field_for(
        &self,
        config: &OrcaConfig,
        provider_name: &str,
        base_url: &str,
    ) -> Option<(String, Value)> {
        let listed = self
            .providers
            .as_ref()
            .is_some_and(|p| p.iter().any(|n| n.eq_ignore_ascii_case(provider_name)));
        if self.is_on() && (listed || is_local_server(config, base_url)) {
            self.field()
        } else {
            None
        }
    }
}

/// The C# `ServerKind` enum name (as `serverKinds` lists it).
fn server_kind_name(kind: crate::client::ServerKind) -> &'static str {
    use crate::client::ServerKind;
    match kind {
        ServerKind::Unknown => "Unknown",
        ServerKind::Strata => "Strata",
        ServerKind::Ninfer => "Ninfer",
        ServerKind::Other => "Other",
    }
}

/// `IsLocalServer`: `base_url` is on the host of lmStudio.baseUrl (unless that is Anthropic),
/// a slot copy or slots.lead.
pub fn is_local_server(config: &OrcaConfig, base_url: &str) -> bool {
    fn host(url: &str) -> Option<String> {
        reqwest::Url::parse(url.trim())
            .ok()
            .and_then(|u| u.host_str().map(str::to_lowercase))
    }
    let Some(target) = host(base_url) else {
        return false;
    };
    let mut local: Vec<&str> = Vec::new();
    if !config.lm_studio.is_anthropic() {
        local.push(&config.lm_studio.base_url);
    }
    if let Some(slots) = &config.slots {
        if let Some(lead) = &slots.lead {
            local.push(&lead.base_url);
        }
        local.extend(slots.copies.iter().map(|c| c.base_url.as_str()));
    }
    local
        .iter()
        .any(|u| host(u).as_deref() == Some(target.as_str()))
}

/// Reasoning effort per role (`AgentReasoningConfig`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    pub max_step_tokens: i32,
    pub cap_retries: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive_skip_after: Option<Vec<String>>,
    pub adaptive_max_skips: i32,
    pub adaptive_brief_rule: bool,
}

impl Default for AgentReasoningConfig {
    fn default() -> Self {
        Self {
            lead: None,
            worker: None,
            max_step_tokens: 0,
            cap_retries: 1,
            adaptive: None,
            adaptive_skip_after: None,
            adaptive_max_skips: 3,
            adaptive_brief_rule: true,
        }
    }
}

impl AgentReasoningConfig {
    /// The effort for which thinking switches keep the Qwen prompt cache (no system-prompt line).
    pub const CACHE_STABLE_EFFORT: &'static str = "medium";

    /// Whether `adaptive` covers the Lead (`lead = true`) or the sub-agents.
    pub fn is_adaptive(&self, lead: bool) -> bool {
        match self
            .adaptive
            .as_deref()
            .map(|a| a.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("all" | "both" | "on" | "true") => true,
            Some("lead") => lead,
            Some("worker" | "workers") => !lead,
            _ => false,
        }
    }

    /// The effort an actor's thinking steps send.
    pub fn effort_for(&self, lead: bool) -> Option<String> {
        let effort = if lead {
            self.lead.clone()
        } else {
            self.worker.clone()
        };
        if !self.is_adaptive(lead) {
            return effort;
        }
        match effort {
            Some(e) if !e.trim().is_empty() && !e.trim().eq_ignore_ascii_case("off") => Some(e),
            _ => Some(Self::CACHE_STABLE_EFFORT.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiConfig {
    pub hyperlinks: String,
    pub inline_images: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            hyperlinks: "auto".into(),
            inline_images: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShellConfig {
    pub idle_timeout_seconds: i32,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageGenerationConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub timeout_seconds: i32,
    pub output_directory: String,
}

impl Default for ImageGenerationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://localhost:7860/v1".into(),
            api_key: "none".into(),
            model: None,
            timeout_seconds: 120,
            output_directory: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LspConfig {
    pub enabled: bool,
    pub servers: Map<LspLanguageServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LspLanguageServerConfig {
    pub command: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FormatterConfig {
    pub enabled: bool,
    pub commands: Map<String>,
    pub show_diagnostics_after_write: bool,
    pub diagnostics_limit: i32,
}

impl Default for FormatterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            commands: Map::new(),
            show_diagnostics_after_write: true,
            diagnostics_limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Map<String>,
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: Map::new(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BridgesConfig {
    pub claude: BridgeCliConfig,
    pub codex: BridgeCliConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BridgeCliConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub extra_args: Vec<String>,
    pub pass_api_key: bool,
    pub progress_bars: bool,
    pub isolate_settings: bool,
}

impl Default for BridgeCliConfig {
    fn default() -> Self {
        Self {
            path: None,
            model: None,
            extra_args: Vec::new(),
            pass_api_key: false,
            progress_bars: true,
            isolate_settings: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfilesConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    pub timeout_ms: i32,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            active: None,
            timeout_ms: 4000,
        }
    }
}

/// Sticky inference slots (`SlotsConfig`). Only the schema is ported in phase 1; the
/// allocator, registry and slotted client are phase 3 (see `PORTING.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotsConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
    pub copies: Vec<SlotCopyConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Map<Value>>,
    pub preserve_thinking: bool,
    pub whole_copy_per_agent: bool,
    /// Get-only in C# but serialized; ignored on load.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub lead_context_window: Option<i32>,
    pub pin_lead_prefix: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead_copy: Option<String>,
    pub agents_per_copy: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<SlotLeadConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_state_file: Option<String>,
    pub heartbeat_seconds: i32,
    pub idle_release_minutes: i32,
    pub retry_seconds: i32,
    pub health_timeout_ms: i32,
    pub registry_timeout_ms: i32,
}

impl Default for SlotsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            registry_url: None,
            copies: Vec::new(),
            model: None,
            api_key: None,
            api_key_env: None,
            extra_body: None,
            preserve_thinking: true,
            whole_copy_per_agent: true,
            lead_context_window: None,
            pin_lead_prefix: true,
            lead_copy: None,
            agents_per_copy: 1,
            lead: None,
            owner: None,
            local_state_file: None,
            heartbeat_seconds: 0,
            idle_release_minutes: 15,
            retry_seconds: 3,
            health_timeout_ms: 1500,
            registry_timeout_ms: 1500,
        }
    }
}

impl SlotsConfig {
    /// The Lead endpoint's context window when slots are on and `lead.contextWindow` is set.
    pub fn compute_lead_context_window(&self) -> Option<i32> {
        match &self.lead {
            Some(lead) if self.enabled && !lead.base_url.is_empty() => {
                lead.context_window.filter(|w| *w > 0)
            }
            _ => None,
        }
    }

    /// `extraBody` plus the `preserve_thinking` template switch (slot-wide).
    pub fn effective_extra_body(&self) -> Option<Map<Value>> {
        effective_extra_body(self.preserve_thinking, self.extra_body.as_ref())
    }

    /// The Lead endpoint's: its own `extraBody` (replacing the slot-wide one) and switch.
    pub fn lead_effective_extra_body(&self) -> Option<Map<Value>> {
        let lead = self.lead.as_ref();
        effective_extra_body(
            lead.and_then(|l| l.preserve_thinking)
                .unwrap_or(self.preserve_thinking),
            lead.and_then(|l| l.extra_body.as_ref())
                .or(self.extra_body.as_ref()),
        )
    }

    /// One copy's: its own `extraBody` (replacing the slot-wide one) and switch.
    pub fn copy_effective_extra_body(&self, copy: &SlotCopyConfig) -> Option<Map<Value>> {
        effective_extra_body(
            copy.preserve_thinking.unwrap_or(self.preserve_thinking),
            copy.extra_body.as_ref().or(self.extra_body.as_ref()),
        )
    }

    /// Whether requests send earlier reasoning back (`reasoning_content`).
    pub fn sends_reasoning_back(&self) -> bool {
        sends_reasoning_back(self.preserve_thinking, self.extra_body.as_ref())
    }

    pub fn lead_sends_reasoning_back(&self) -> bool {
        let lead = self.lead.as_ref();
        sends_reasoning_back(
            lead.and_then(|l| l.preserve_thinking)
                .unwrap_or(self.preserve_thinking),
            lead.and_then(|l| l.extra_body.as_ref())
                .or(self.extra_body.as_ref()),
        )
    }

    pub fn copy_sends_reasoning_back(&self, copy: &SlotCopyConfig) -> bool {
        sends_reasoning_back(
            copy.preserve_thinking.unwrap_or(self.preserve_thinking),
            copy.extra_body.as_ref().or(self.extra_body.as_ref()),
        )
    }
}

/// `SlotsConfig.EffectiveExtraBody(preserve, extraBody)`: adds
/// `chat_template_kwargs.preserve_thinking = true` unless the body already sets that key.
pub fn effective_extra_body(
    preserve_thinking: bool,
    extra_body: Option<&Map<Value>>,
) -> Option<Map<Value>> {
    if !preserve_thinking {
        return extra_body.cloned();
    }
    let mut body = extra_body.cloned().unwrap_or_default();
    let mut kwargs = match body.get("chat_template_kwargs") {
        Some(Value::Object(o)) => o.clone(),
        _ => serde_json::Map::new(),
    };
    if kwargs.contains_key("preserve_thinking") {
        return extra_body.cloned();
    }
    kwargs.insert("preserve_thinking".into(), Value::Bool(true));
    body.insert("chat_template_kwargs".into(), Value::Object(kwargs));
    Some(body)
}

/// `SlotsConfig.SendsReasoningBack(preserve, extraBody)`.
pub fn sends_reasoning_back(preserve_thinking: bool, extra_body: Option<&Map<Value>>) -> bool {
    if !preserve_thinking {
        return false;
    }
    let body = effective_extra_body(preserve_thinking, extra_body);
    let preserve = body
        .as_ref()
        .and_then(|b| b.get("chat_template_kwargs"))
        .and_then(Value::as_object)
        .and_then(|k| k.get("preserve_thinking"));
    !matches!(preserve, Some(Value::Bool(false)))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotCopyConfig {
    pub name: String,
    pub base_url: String,
    pub slots: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Map<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingConfig>,
}

impl Default for SlotCopyConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: String::new(),
            slots: 2,
            model: None,
            extra_body: None,
            preserve_thinking: None,
            sampling: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotLeadConfig {
    pub name: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Map<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingConfig>,
}

impl Default for SlotLeadConfig {
    fn default() -> Self {
        Self {
            name: "CPU".into(),
            base_url: String::new(),
            model: None,
            api_key_env: None,
            api_key: None,
            context_window: None,
            extra_body: None,
            preserve_thinking: None,
            sampling: None,
        }
    }
}
