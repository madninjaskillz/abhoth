//! Loading and saving `config.json` (port of `ConfigManager.cs`).

use std::path::{Path, PathBuf};

use super::dotnet_json;
use super::model::{AgentConfig, OrcaConfig, ProviderConfig};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config.json is not valid: {0}")]
    Json(#[from] serde_json::Error),
}

/// The config directory: `OPENORCA_HOME`, else `~/.openorca`.
pub fn default_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("OPENORCA_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    user_profile().join(".openorca")
}

/// `Environment.SpecialFolder.UserProfile`.
pub fn user_profile() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub struct ConfigManager {
    dir: PathBuf,
    path: PathBuf,
    pub config: OrcaConfig,
}

impl ConfigManager {
    /// The config in `dir` (default: `OPENORCA_HOME` or `~/.openorca`).
    pub fn new(dir: Option<&Path>) -> Self {
        let dir = dir
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_dir);
        let path = dir.join("config.json");
        Self {
            dir,
            path,
            config: OrcaConfig::default(),
        }
    }

    /// The config.json this manager reads and writes.
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// `LoadAsync`: a missing file is created with the defaults (as C# does, before validation);
    /// a file with legacy `model` / `backupModel` fields is migrated and saved back.
    pub fn load(&mut self) -> Result<(), ConfigError> {
        if !self.path.exists() {
            self.config = OrcaConfig::default();
            return self.save();
        }
        let bytes = std::fs::read(&self.path).map_err(|source| ConfigError::Io {
            path: self.path.clone(),
            source,
        })?;
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let text = String::from_utf8_lossy(bytes);
        if self.load_from_text(&text)? {
            self.save()?;
        }
        Ok(())
    }

    /// `LoadFromText`: parse, migrate and validate. Returns whether a legacy migration ran.
    pub fn load_from_text(&mut self, json: &str) -> Result<bool, ConfigError> {
        let mut config: OrcaConfig = serde_json::from_str(json)?;
        for provider in config.providers.values_mut() {
            provider.apply_alias();
        }
        let migrated = migrate_legacy_models(&mut config);
        validate(&mut config);
        self.config = config;
        Ok(migrated)
    }

    /// `SaveAsync`: write the config as the C# version would.
    pub fn save(&self) -> Result<(), ConfigError> {
        std::fs::create_dir_all(&self.dir).map_err(|source| ConfigError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let json = self.serialize_for_save()?;
        std::fs::write(&self.path, json).map_err(|source| ConfigError::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// The text that goes to disk. (Profile overlays are phase 2; until then this is the config.)
    pub fn serialize_for_save(&self) -> Result<String, ConfigError> {
        Ok(serialize_config(&self.config)?)
    }
}

/// `JsonSerializer.Serialize(config, JsonDefaults.Options)`.
pub fn serialize_config(config: &OrcaConfig) -> serde_json::Result<String> {
    let mut c = config.clone();
    c.refresh_computed();
    dotnet_json::to_string_dotnet(&c)
}

/// [`serialize_config`] with an explicit line break.
pub fn serialize_config_with_newline(
    config: &OrcaConfig,
    newline: &str,
) -> serde_json::Result<String> {
    let mut c = config.clone();
    c.refresh_computed();
    dotnet_json::to_string_dotnet_with_newline(&c, newline)
}

/// Move legacy `model` / `backupModel` into `models`. Returns true if it did.
pub fn migrate_legacy_models(config: &mut OrcaConfig) -> bool {
    let lm = &mut config.lm_studio;
    let needs = lm.models.is_empty()
        && (lm.model.as_deref().is_some_and(|m| !m.is_empty())
            || lm.backup_model.as_deref().is_some_and(|m| !m.is_empty()));
    if needs {
        if let Some(m) = lm.model.as_ref().filter(|m| !m.is_empty()) {
            lm.models.push(m.clone());
        }
        if let Some(m) = lm.backup_model.as_ref().filter(|m| !m.is_empty()) {
            lm.models.push(m.clone());
        }
    }
    lm.model = None;
    lm.backup_model = None;
    needs
}

/// Invalid sub-agent limits fall back to the defaults, and the old defaults (15 iterations,
/// 300 s) move to the current ones.
pub fn normalize_agent_limits(agent: &mut AgentConfig) {
    if agent.max_iterations <= 0 || agent.max_iterations == 15 {
        agent.max_iterations = AgentConfig::DEFAULT_MAX_ITERATIONS;
    }
    if agent.timeout_seconds <= 0 || agent.timeout_seconds == 300 {
        agent.timeout_seconds = AgentConfig::DEFAULT_TIMEOUT_SECONDS;
    }
}

/// `ConfigManager.ValidateConfig`.
pub fn validate(config: &mut OrcaConfig) {
    if config.lm_studio.base_url.trim().is_empty() {
        config.lm_studio.base_url = "http://localhost:1234/v1".into();
    }
    if !(0.0..=2.0).contains(&config.lm_studio.temperature) {
        config.lm_studio.temperature = 0.7;
    }
    if config.context.context_window_size <= 0 {
        config.context.context_window_size = 8192;
    }
    // The Lead runs on its own endpoint (slots.lead): its window is the one that matters.
    if let Some(window) = config
        .slots
        .as_ref()
        .and_then(|s| s.compute_lead_context_window())
    {
        config.context.context_window_size = window;
    }
    let ctx = &mut config.context;
    if ctx.auto_compact_threshold <= 0.0 || ctx.auto_compact_threshold > 1.0 {
        ctx.auto_compact_threshold = 0.8;
    }
    if ctx.compact_at_ratio < 0.0 || ctx.compact_at_ratio.is_nan() {
        ctx.compact_at_ratio = 0.0;
    }
    if ctx.prefill_tokens_per_second <= 0 {
        ctx.prefill_tokens_per_second = 1000;
    }
    if ctx.reserve_tokens < 0 {
        ctx.reserve_tokens = 0;
    }
    if ctx.keep_recent_tokens < 0 {
        ctx.keep_recent_tokens = 0;
    }
    if config.lm_studio.streaming_timeout_seconds <= 0 {
        config.lm_studio.streaming_timeout_seconds = 120;
    }
    normalize_agent_limits(&mut config.agent);
    if config.shell.idle_timeout_seconds < 0 {
        config.shell.idle_timeout_seconds = 120;
    }
    // The old 15 s default was too short for package managers and builds.
    if config.shell.idle_timeout_seconds == 15 {
        config.shell.idle_timeout_seconds = 120;
    }
    synthesize_default_provider(config);
}

/// With no named providers, add an "lmstudio" provider built from the `lmStudio` section.
pub fn synthesize_default_provider(config: &mut OrcaConfig) {
    if !config.providers.is_empty() {
        return;
    }
    config.providers.insert(
        "lmstudio".into(),
        ProviderConfig {
            kind: "openai-compat".into(),
            base_url: config.lm_studio.base_url.clone(),
            api_key: Some(config.lm_studio.api_key.clone()),
            models: config.lm_studio.models.clone(),
            ..ProviderConfig::default()
        },
    );
}
