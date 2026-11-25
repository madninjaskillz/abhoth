//! The settings `/config` edits (`ConfigSchema`, `SettingDef`, `ProfileOverlay`): where each
//! lives in [`OrcaConfig`], how it's labelled, described, parsed, validated and shown. The
//! terminal's `/config` and the web UI's settings page are both drawn from [`all`], so they
//! can't drift apart.

use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use super::manager::ConfigManager;
use super::model::OrcaConfig;

/// The kinds of value a setting holds (how it's edited and parsed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    Bool,
    Int,
    Float,
    Text,
    Url,
    /// An API key or the like: shown masked, replaced or removed, never sent out.
    Secret,
    /// A list of strings (tool names, models in priority order).
    List,
}

impl SettingType {
    /// The name the web UI gets (`Type.ToString().ToLowerInvariant()`).
    pub fn name(self) -> &'static str {
        match self {
            SettingType::Bool => "bool",
            SettingType::Int => "int",
            SettingType::Float => "float",
            SettingType::Text => "text",
            SettingType::Url => "url",
            SettingType::Secret => "secret",
            SettingType::List => "list",
        }
    }
}

/// A setting's value (C#'s boxed `object?`).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Null,
    Bool(bool),
    Int(i32),
    Float(f32),
    Text(String),
    List(Vec<String>),
}

impl SettingValue {
    fn text(v: &Option<String>) -> SettingValue {
        v.as_ref()
            .map_or(SettingValue::Null, |s| SettingValue::Text(s.clone()))
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            SettingValue::Text(s) => Some(s),
            _ => None,
        }
    }

    fn into_string(self) -> Option<String> {
        match self {
            SettingValue::Text(s) => Some(s),
            _ => None,
        }
    }

    fn as_bool(&self) -> bool {
        matches!(self, SettingValue::Bool(true))
    }

    fn as_int(&self) -> Option<i32> {
        match self {
            SettingValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    fn as_float(&self) -> f32 {
        match self {
            SettingValue::Float(f) => *f,
            SettingValue::Int(i) => *i as f32,
            _ => 0.0,
        }
    }

    fn into_list(self) -> Vec<String> {
        match self {
            SettingValue::List(l) => l,
            _ => Vec::new(),
        }
    }
}

/// One setting of `/config` (`SettingDef`).
pub struct SettingDef {
    /// Stable key: the JSON path in config.json (e.g. "lmStudio.temperature").
    pub key: &'static str,
    pub section: &'static str,
    pub label: &'static str,
    pub kind: SettingType,
    pub description: &'static str,
    pub get: fn(&OrcaConfig) -> SettingValue,
    pub set: fn(&mut OrcaConfig, SettingValue),
    /// Lower/upper bounds for numbers (inclusive unless `min_exclusive`).
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub min_exclusive: bool,
    /// The message when a number is out of range (as /config has always said it).
    pub range_message: Option<&'static str>,
    /// A blank value is allowed and means this (e.g. "unlimited"); the setting is then
    /// null/empty/0.
    pub blank_means: Option<&'static str>,
    /// For a number: this value is shown as `blank_means` ("0 = auto").
    pub blank_value: Option<f64>,
    /// The running session picks a change up at once. False: it applies to sessions started
    /// afterwards (the client, its timeouts and the tool list are built at start).
    pub live: bool,
    /// Ask before turning it on (it changes what runs without asking).
    pub dangerous: bool,
}

impl std::fmt::Debug for SettingDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingDef")
            .field("key", &self.key)
            .finish()
    }
}

impl SettingDef {
    fn new(
        section: &'static str,
        key: &'static str,
        label: &'static str,
        kind: SettingType,
        description: &'static str,
        get: fn(&OrcaConfig) -> SettingValue,
        set: fn(&mut OrcaConfig, SettingValue),
    ) -> Self {
        Self {
            key,
            section,
            label,
            kind,
            description,
            get,
            set,
            min: None,
            max: None,
            min_exclusive: false,
            range_message: None,
            blank_means: None,
            blank_value: None,
            live: true,
            dangerous: false,
        }
    }

    fn not_live(mut self) -> Self {
        self.live = false;
        self
    }

    fn dangerous(mut self) -> Self {
        self.dangerous = true;
        self
    }

    fn range(
        mut self,
        min: Option<f64>,
        max: Option<f64>,
        min_exclusive: bool,
        message: &'static str,
    ) -> Self {
        self.min = min;
        self.max = max;
        self.min_exclusive = min_exclusive;
        self.range_message = Some(message);
        self
    }

    fn blank(mut self, means: &'static str, value: Option<f64>) -> Self {
        self.blank_means = Some(means);
        self.blank_value = value;
        self
    }

    /// The value in `config`.
    pub fn get(&self, config: &OrcaConfig) -> SettingValue {
        (self.get)(config)
    }

    /// Set the value (as parsed by [`Self::try_parse`] / [`Self::try_read`]).
    pub fn set(&self, config: &mut OrcaConfig, value: SettingValue) {
        (self.set)(config, value)
    }

    /// The value in a default config.
    pub fn default_value(&self) -> SettingValue {
        self.get(&OrcaConfig::default())
    }

    pub fn is_modified(&self, config: &OrcaConfig) -> bool {
        !value_equals(&self.get(config), &self.default_value())
    }

    /// The value as /config shows it (secrets masked, blanks as their meaning).
    pub fn display(&self, config: &OrcaConfig) -> String {
        self.display_value(&self.get(config))
    }

    pub fn display_value(&self, value: &SettingValue) -> String {
        let blank = || self.blank_means.map(|m| format!("({m})"));
        if self.kind == SettingType::Secret {
            return mask(value.as_str());
        }
        match value {
            SettingValue::Bool(b) => if *b { "On" } else { "Off" }.to_string(),
            SettingValue::Null => blank().unwrap_or_default(),
            SettingValue::Text(s) if s.is_empty() && self.blank_means.is_some() => {
                blank().unwrap_or_default()
            }
            SettingValue::List(list) => {
                if list.is_empty() {
                    blank().unwrap_or_else(|| "(none)".to_string())
                } else {
                    list.join(", ")
                }
            }
            SettingValue::Float(f)
                if self.blank_means.is_some()
                    && self
                        .blank_value
                        .is_some_and(|bv| (*f as f64 - bv).abs() < 1e-9) =>
            {
                blank().unwrap_or_default()
            }
            SettingValue::Int(i)
                if self.blank_means.is_some()
                    && self.blank_value.is_some_and(|bv| *i == bv as i32) =>
            {
                blank().unwrap_or_default()
            }
            SettingValue::Float(f) => format_float(*f),
            SettingValue::Int(i) => i.to_string(),
            SettingValue::Text(s) => s.clone(),
        }
    }

    /// Parse what someone typed: the value, or the error message.
    pub fn try_parse(&self, text: Option<&str>) -> Result<SettingValue, String> {
        let text = text.unwrap_or("").trim();
        match self.kind {
            SettingType::Bool => match text.to_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => Ok(SettingValue::Bool(true)),
                "off" | "false" | "no" | "0" => Ok(SettingValue::Bool(false)),
                _ => Err("Must be on or off".into()),
            },
            SettingType::Int => {
                if text.is_empty() && self.blank_means.is_some() {
                    let value = self
                        .blank_value
                        .map_or(SettingValue::Null, |b| SettingValue::Int(b as i32));
                    return self.validated(value);
                }
                match text.parse::<i32>() {
                    Ok(i) => self.validated(SettingValue::Int(i)),
                    Err(_) => Err("Must be a whole number".into()),
                }
            }
            SettingType::Float => match parse_float(text) {
                Some(f) => self.validated(SettingValue::Float(f)),
                None => Err("Must be a number".into()),
            },
            SettingType::List => {
                let list = text
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                self.validated(SettingValue::List(list))
            }
            SettingType::Text | SettingType::Url | SettingType::Secret => {
                let value = if text.is_empty()
                    && self.blank_means.is_some()
                    && self.default_value() == SettingValue::Null
                {
                    SettingValue::Null
                } else {
                    SettingValue::Text(text.to_string())
                };
                self.validated(value)
            }
        }
    }

    fn validated(&self, value: SettingValue) -> Result<SettingValue, String> {
        match self.validate(&value) {
            Some(e) => Err(e),
            None => Ok(value),
        }
    }

    /// Read a JSON value (the web UI's; `None` = JSON null): the value, or the error message.
    pub fn try_read(&self, node: Option<&Value>) -> Result<SettingValue, String> {
        let node = match node {
            None | Some(Value::Null) => return self.try_parse(Some("")),
            Some(n) => n,
        };
        match self.kind {
            SettingType::Bool => match node {
                Value::Bool(b) => Ok(SettingValue::Bool(*b)),
                _ => Err("Must be on or off".into()),
            },
            SettingType::List => {
                let Value::Array(arr) = node else {
                    return Err("Must be a list".into());
                };
                let mut list = Vec::new();
                for item in arr {
                    match item.as_str().map(str::trim) {
                        Some(s) if !s.is_empty() => list.push(s.to_string()),
                        _ => return Err("Every item must be non-empty text".into()),
                    }
                }
                self.validated(SettingValue::List(list))
            }
            SettingType::Int | SettingType::Float => match node {
                Value::String(s) => self.try_parse(Some(s)),
                other => self.try_parse(Some(&other.to_string())),
            },
            SettingType::Text | SettingType::Url | SettingType::Secret => match node {
                Value::String(s) => self.try_parse(Some(s)),
                _ => Err("Must be text".into()),
            },
        }
    }

    /// The rules /config enforces: the error message, or `None`.
    pub fn validate(&self, value: &SettingValue) -> Option<String> {
        match self.kind {
            SettingType::Int | SettingType::Float => {
                let d = match value {
                    SettingValue::Null => {
                        return if self.blank_means.is_some() {
                            None
                        } else {
                            Some("A value is required".into())
                        };
                    }
                    SettingValue::Int(i) => *i as f64,
                    SettingValue::Float(f) => *f as f64,
                    _ => 0.0,
                };
                if let Some(min) = self.min
                    && (if self.min_exclusive {
                        d <= min
                    } else {
                        d < min
                    })
                {
                    return Some(self.range_message.map(str::to_string).unwrap_or_else(|| {
                        format!(
                            "Must be {} {min}",
                            if self.min_exclusive {
                                "more than"
                            } else {
                                "at least"
                            }
                        )
                    }));
                }
                if let Some(max) = self.max
                    && d > max
                {
                    return Some(
                        self.range_message
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("Must be at most {max}")),
                    );
                }
                None
            }
            SettingType::Url => match value.as_str() {
                Some(u) if is_http_url(u) => None,
                _ => Some("Invalid URL format. Must be http:// or https://".into()),
            },
            SettingType::Text | SettingType::Secret => match value.as_str() {
                Some(s) if s.contains(['\r', '\n', '\0']) => Some("Must be a single line".into()),
                _ => None,
            },
            _ => None,
        }
    }
}

/// `Uri.TryCreate(u, UriKind.Absolute)` with an http or https scheme.
pub fn is_http_url(u: &str) -> bool {
    reqwest::Url::parse(u.trim())
        .is_ok_and(|url| (url.scheme() == "http" || url.scheme() == "https") && url.has_host())
}

/// `float.TryParse(text, NumberStyles.Float, Invariant)` and finite.
fn parse_float(text: &str) -> Option<f32> {
    // .NET accepts digits, sign, one point and an exponent; Rust's parser also takes "inf" /
    // "nan" words, which are refused here as they are not finite anyway.
    if text.is_empty()
        || !text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
    {
        return None;
    }
    text.parse::<f32>().ok().filter(|f| f.is_finite())
}

/// `f.ToString("0.0##", Invariant)`.
pub fn format_float(f: f32) -> String {
    let mut s = format!("{:.3}", f as f64);
    while s.ends_with('0') && !s.ends_with(".0") {
        s.pop();
    }
    if s == "-0.0" {
        s = "0.0".into();
    }
    s
}

/// `SettingDef.ValueEquals`.
pub fn value_equals(a: &SettingValue, b: &SettingValue) -> bool {
    match (a, b) {
        (SettingValue::Null, SettingValue::Null) => true,
        (SettingValue::List(x), SettingValue::List(y)) => x == y,
        (SettingValue::Float(x), SettingValue::Float(y)) => (x - y).abs() < 1e-6,
        (SettingValue::Text(x), SettingValue::Null) => x.is_empty(),
        (SettingValue::Null, SettingValue::Text(y)) => y.is_empty(),
        _ => a == b,
    }
}

/// A change made through the schema: what it was and is now (masked for secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChange {
    pub ok: bool,
    pub key: String,
    pub error: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub live: bool,
}

impl SettingChange {
    fn failed(key: &str, error: String, live: bool) -> Self {
        Self {
            ok: false,
            key: key.to_string(),
            error: Some(error),
            old: None,
            new: None,
            live,
        }
    }
}

pub const LLM_SERVER: &str = "LLM Server";
pub const IMAGE_GENERATION: &str = "Image Generation";
pub const PERMISSIONS: &str = "Permissions";
pub const SESSION: &str = "Session";
pub const CONTEXT: &str = "Context";
/// Rust-only (D113): the desktop app's window.
pub const DESKTOP: &str = "Desktop";

/// The sections, in order.
pub const SECTIONS: [&str; 6] = [
    LLM_SERVER,
    IMAGE_GENERATION,
    PERMISSIONS,
    SESSION,
    CONTEXT,
    DESKTOP,
];

/// The default-profile setting (the web shows it as a profile picker).
pub const PROFILES_ACTIVE_KEY: &str = "profiles.active";

fn blank_to_none(v: SettingValue) -> Option<String> {
    v.into_string()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

static ALL: LazyLock<Vec<SettingDef>> = LazyLock::new(|| {
    use SettingType as T;
    use SettingValue as V;
    vec![
        // ---- LLM Server ----
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.baseUrl",
            "Base URL",
            T::Url,
            "The OpenAI-compatible server the models run on (LM Studio, llama.cpp, vLLM…).",
            |c| V::Text(c.lm_studio.base_url.clone()),
            |c, v| c.lm_studio.base_url = v.into_string().unwrap_or_default(),
        )
        .not_live(),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.apiKey",
            "API Key",
            T::Secret,
            "Sent to the server as a bearer token.",
            |c| V::Text(c.lm_studio.api_key.clone()),
            |c, v| c.lm_studio.api_key = v.into_string().unwrap_or_default(),
        )
        .not_live(),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.models",
            "Models",
            T::List,
            "Models in priority order: the first is used, the next ones are backups.",
            |c| V::List(c.lm_studio.models.clone()),
            |c, v| c.lm_studio.models = v.into_list(),
        )
        .not_live()
        .blank("none", None),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.backupMinResponseLength",
            "Backup Min Length",
            T::Int,
            "Minimum response length in chars; a shorter answer moves to the next model (0 = disabled).",
            |c| V::Int(c.lm_studio.backup_min_response_length),
            |c, v| c.lm_studio.backup_min_response_length = v.as_int().unwrap_or_default(),
        )
        .range(Some(0.0), None, false, "Must be 0 or positive")
        .blank("disabled", Some(0.0)),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.backupOnRefusal",
            "Backup On Refusal",
            T::Bool,
            "Switch to next model on refusal/apology patterns.",
            |c| V::Bool(c.lm_studio.backup_on_refusal),
            |c, v| c.lm_studio.backup_on_refusal = v.as_bool(),
        ),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.pairModelProgramming",
            "Pair Mode",
            T::Bool,
            "Pair model programming (all models run in parallel and vote on the best response).",
            |c| V::Bool(c.lm_studio.pair_model_programming),
            |c, v| c.lm_studio.pair_model_programming = v.as_bool(),
        ),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.temperature",
            "Temperature",
            T::Float,
            "Sampling temperature (0.0 - 2.0).",
            |c| V::Float(c.lm_studio.temperature),
            |c, v| c.lm_studio.temperature = v.as_float(),
        )
        .range(Some(0.0), Some(2.0), false, "Must be between 0.0 and 2.0"),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.maxTokens",
            "Max Tokens",
            T::Int,
            "Most tokens per response (blank for unlimited).",
            |c| c.lm_studio.max_tokens.map_or(V::Null, V::Int),
            |c, v| c.lm_studio.max_tokens = v.as_int(),
        )
        .not_live()
        .range(
            Some(0.0),
            None,
            true,
            "Must be positive (blank for unlimited)",
        )
        .blank("unlimited", None),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.timeoutSeconds",
            "Timeout (seconds)",
            T::Int,
            "How long to wait for the server.",
            |c| V::Int(c.lm_studio.timeout_seconds),
            |c, v| c.lm_studio.timeout_seconds = v.as_int().unwrap_or_default(),
        )
        .not_live()
        .range(Some(0.0), None, true, "Must be positive"),
        SettingDef::new(
            LLM_SERVER,
            "lmStudio.nativeToolCalling",
            "Native Tool Calling",
            T::Bool,
            "Native tool calling (only enable if your model supports OpenAI function calling).",
            |c| V::Bool(c.lm_studio.native_tool_calling),
            |c, v| c.lm_studio.native_tool_calling = v.as_bool(),
        ),
        // ---- Image Generation ----
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.enabled",
            "Enabled",
            T::Bool,
            "Enable image generation (adds the image tool).",
            |c| V::Bool(c.image_gen.enabled),
            |c, v| c.image_gen.enabled = v.as_bool(),
        )
        .not_live(),
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.baseUrl",
            "Base URL",
            T::Url,
            "The image server (OpenAI-compatible images API).",
            |c| V::Text(c.image_gen.base_url.clone()),
            |c, v| c.image_gen.base_url = v.into_string().unwrap_or_default(),
        )
        .not_live(),
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.apiKey",
            "API Key",
            T::Secret,
            "Sent to the image server as a bearer token.",
            |c| V::Text(c.image_gen.api_key.clone()),
            |c, v| c.image_gen.api_key = v.into_string().unwrap_or_default(),
        )
        .not_live(),
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.model",
            "Model",
            T::Text,
            "Image model (blank for the server's default).",
            |c| V::text(&c.image_gen.model),
            |c, v| {
                c.image_gen.model = v.into_string().filter(|s| !s.trim().is_empty());
            },
        )
        .not_live()
        .blank("server default", None),
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.timeoutSeconds",
            "Timeout (seconds)",
            T::Int,
            "How long to wait for an image.",
            |c| V::Int(c.image_gen.timeout_seconds),
            |c, v| c.image_gen.timeout_seconds = v.as_int().unwrap_or_default(),
        )
        .not_live()
        .range(Some(0.0), None, true, "Must be positive"),
        SettingDef::new(
            IMAGE_GENERATION,
            "imageGen.outputDirectory",
            "Output Directory",
            T::Text,
            "Where generated images are saved (blank for ~/.openorca/images/).",
            |c| V::Text(c.image_gen.output_directory.clone()),
            |c, v| {
                c.image_gen.output_directory =
                    v.into_string().unwrap_or_default().trim().to_string();
            },
        )
        .not_live()
        .blank("~/.openorca/images/", None),
        SettingDef::new(
            IMAGE_GENERATION,
            "agent.uncensored.imageModel",
            "Uncensored Model",
            T::Text,
            "Image model generate_image uses while /uncensored is on (blank: images don't follow the switch).",
            |c| V::text(&c.agent.uncensored.image_model),
            |c, v| {
                c.agent.uncensored.image_model =
                    Some(v.into_string().unwrap_or_default().trim().to_string());
            },
        )
        .blank("images ignore /uncensored", None),
        // ---- Permissions ----
        SettingDef::new(
            PERMISSIONS,
            "permissions.autoApproveReadOnly",
            "Auto-approve read-only",
            T::Bool,
            "Auto-approve read-only tools.",
            |c| V::Bool(c.permissions.auto_approve_read_only),
            |c, v| c.permissions.auto_approve_read_only = v.as_bool(),
        ),
        SettingDef::new(
            PERMISSIONS,
            "permissions.autoApproveModerate",
            "Auto-approve moderate",
            T::Bool,
            "Auto-approve moderate tools.",
            |c| V::Bool(c.permissions.auto_approve_moderate),
            |c, v| c.permissions.auto_approve_moderate = v.as_bool(),
        ),
        SettingDef::new(
            PERMISSIONS,
            "permissions.autoApproveAll",
            "Auto-approve ALL",
            T::Bool,
            "Auto-approve ALL tools (dangerous).",
            |c| V::Bool(c.permissions.auto_approve_all),
            |c, v| c.permissions.auto_approve_all = v.as_bool(),
        )
        .dangerous(),
        SettingDef::new(
            PERMISSIONS,
            "permissions.alwaysApprove",
            "Always approve",
            T::List,
            "Tools that never ask.",
            |c| V::List(c.permissions.always_approve.clone()),
            |c, v| c.permissions.always_approve = v.into_list(),
        ),
        SettingDef::new(
            PERMISSIONS,
            "permissions.disabledTools",
            "Disabled tools",
            T::List,
            "Tools that are refused.",
            |c| V::List(c.permissions.disabled_tools.clone()),
            |c, v| c.permissions.disabled_tools = v.into_list(),
        ),
        // ---- Session ----
        SettingDef::new(
            SESSION,
            "session.autoSave",
            "Auto-save",
            T::Bool,
            "Auto-save sessions.",
            |c| V::Bool(c.session.auto_save),
            |c, v| c.session.auto_save = v.as_bool(),
        ),
        SettingDef::new(
            SESSION,
            "session.maxSessions",
            "Max sessions",
            T::Int,
            "How many saved sessions to keep.",
            |c| V::Int(c.session.max_sessions),
            |c, v| c.session.max_sessions = v.as_int().unwrap_or_default(),
        )
        .range(Some(0.0), None, true, "Must be positive"),
        SettingDef::new(
            SESSION,
            PROFILES_ACTIVE_KEY,
            "Default profile",
            T::Text,
            "Inference profile new sessions start with when none is chosen (blank for the config as it is). /profile lists them.",
            |c| V::text(&c.profiles.active),
            |c, v| c.profiles.active = blank_to_none(v),
        )
        .not_live()
        .blank("none", None),
        // ---- Context ----
        SettingDef::new(
            CONTEXT,
            "context.contextWindowSize",
            "Context window size",
            T::Int,
            "Context window size (tokens).",
            |c| V::Int(c.context.context_window_size),
            |c, v| c.context.context_window_size = v.as_int().unwrap_or_default(),
        )
        .range(Some(100.0), None, true, "Must be > 100"),
        SettingDef::new(
            CONTEXT,
            "context.charsPerToken",
            "Chars per token",
            T::Float,
            "Characters per token, for estimates (1.0-10.0).",
            |c| V::Float(c.context.chars_per_token),
            |c, v| c.context.chars_per_token = v.as_float(),
        )
        .range(Some(1.0), Some(10.0), false, "Must be between 1.0 and 10.0"),
        SettingDef::new(
            CONTEXT,
            "context.autoCompactEnabled",
            "Auto-compact enabled",
            T::Bool,
            "Compact the conversation automatically as it nears the window.",
            |c| V::Bool(c.context.auto_compact_enabled),
            |c, v| c.context.auto_compact_enabled = v.as_bool(),
        ),
        SettingDef::new(
            CONTEXT,
            "context.autoCompactThreshold",
            "Auto-compact threshold",
            T::Float,
            "Auto-compact threshold (0.0-1.0).",
            |c| V::Float(c.context.auto_compact_threshold),
            |c, v| c.context.auto_compact_threshold = v.as_float(),
        )
        .range(Some(0.0), Some(1.0), true, "Must be between 0.0 and 1.0"),
        SettingDef::new(
            CONTEXT,
            "context.reserveTokens",
            "Reserve tokens",
            T::Int,
            "Compact when context exceeds window minus this many tokens (0 = auto).",
            |c| V::Int(c.context.reserve_tokens),
            |c, v| c.context.reserve_tokens = v.as_int().unwrap_or_default(),
        )
        .range(Some(0.0), None, false, "Must be >= 0")
        .blank("auto", Some(0.0)),
        SettingDef::new(
            CONTEXT,
            "context.keepRecentTokens",
            "Keep recent tokens",
            T::Int,
            "Tokens of recent messages kept verbatim by compaction (0 = auto).",
            |c| V::Int(c.context.keep_recent_tokens),
            |c, v| c.context.keep_recent_tokens = v.as_int().unwrap_or_default(),
        )
        .range(Some(0.0), None, false, "Must be >= 0")
        .blank("auto", Some(0.0)),
        // ---- Desktop ----
        SettingDef::new(
            DESKTOP,
            "desktop.closeToTray",
            "Close to tray",
            T::Bool,
            "Desktop app: closing the window hides it to the tray and agents keep running (tray Quit exits). Off: closing quits.",
            |c| V::Bool(c.desktop.close_to_tray),
            |c, v| c.desktop.close_to_tray = v.as_bool(),
        ),
    ]
});

/// Every setting, in its sections and order (`ConfigSchema.All`).
pub fn all() -> &'static [SettingDef] {
    &ALL
}

/// `ConfigSchema.Find`.
pub fn find(key: &str) -> Option<&'static SettingDef> {
    all().iter().find(|s| s.key == key)
}

/// `ConfigSchema.InSection`.
pub fn in_section(section: &str) -> impl Iterator<Item = &'static SettingDef> + '_ {
    all().iter().filter(move |s| s.section == section)
}

/// A secret as it may be shown: never more than its first and last three characters.
pub fn mask(secret: Option<&str>) -> String {
    match secret {
        None | Some("") => "(not set)".into(),
        Some(s) => {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() <= 10 {
                "••••".into()
            } else {
                let head: String = chars[..3].iter().collect();
                let tail: String = chars[chars.len() - 3..].iter().collect();
                format!("{head}…{tail}")
            }
        }
    }
}

/// `ConfigSchema.Apply`: apply a JSON value (`reset`: the default) to `config`, after the same
/// parsing and validation as /config. Old and new values come back as shown (masked).
/// `overlay`: the session's active profile; a key it sets is refused, since the profile would
/// put its own value back (and config.json keeps the file's value on save).
pub fn apply(
    config: &mut OrcaConfig,
    key: &str,
    value: Option<&Value>,
    reset: bool,
    overlay: Option<&ProfileOverlay>,
) -> SettingChange {
    let Some(def) = find(key) else {
        return SettingChange::failed(key, format!("Unknown setting: {key}"), true);
    };
    if let Some(o) = overlay.filter(|o| o.covers(key)) {
        return SettingChange::failed(
            key,
            format!(
                "Set by profile {}: choose another profile (or none) to change it.",
                o.profile
            ),
            def.live,
        );
    }
    let parsed = if reset {
        def.default_value()
    } else {
        match def.try_read(value) {
            Ok(v) => v,
            Err(e) => return SettingChange::failed(key, e, def.live),
        }
    };
    let old = def.display(config);
    def.set(config, parsed);
    SettingChange {
        ok: true,
        key: key.to_string(),
        error: None,
        old: Some(old),
        new: Some(def.display(config)),
        live: def.live,
    }
}

/// A JSON number as System.Text.Json writes a `double` (whole numbers without ".0").
fn number(d: f64) -> Value {
    if d.fract() == 0.0 && d.abs() < 1e15 {
        json!(d as i64)
    } else {
        json!(d)
    }
}

fn to_json(v: &SettingValue) -> Value {
    match v {
        SettingValue::Null => Value::Null,
        SettingValue::Bool(b) => json!(b),
        SettingValue::Int(i) => json!(i),
        SettingValue::Float(f) => number((*f as f64 * 10_000.0).round_ties_even() / 10_000.0),
        SettingValue::Text(s) => json!(s),
        SettingValue::List(l) => json!(l),
    }
}

/// `ConfigSchema.Describe`: every setting with its value, default and metadata, for the web
/// UI. Secrets carry only whether they're set and their mask, never the value.
pub fn describe(config: &OrcaConfig, overlay: Option<&ProfileOverlay>) -> Value {
    let opt = |v: Option<f64>| v.map_or(Value::Null, number);
    let sections: Vec<Value> = SECTIONS
        .iter()
        .map(|section| {
            let settings: Vec<Value> = in_section(section)
                .map(|s| {
                    let secret = s.kind == SettingType::Secret;
                    let value = s.get(config);
                    let default = s.default_value();
                    let mut o = Map::new();
                    o.insert("key".into(), json!(s.key));
                    o.insert("label".into(), json!(s.label));
                    o.insert("type".into(), json!(s.kind.name()));
                    o.insert("description".into(), json!(s.description));
                    o.insert(
                        "value".into(),
                        if secret { Value::Null } else { to_json(&value) },
                    );
                    o.insert("display".into(), json!(s.display_value(&value)));
                    o.insert(
                        "default".into(),
                        if secret {
                            Value::Null
                        } else {
                            to_json(&default)
                        },
                    );
                    o.insert("defaultDisplay".into(), json!(s.display_value(&default)));
                    o.insert(
                        "set".into(),
                        if secret {
                            json!(value.as_str().is_some_and(|v| !v.is_empty()))
                        } else {
                            Value::Null
                        },
                    );
                    o.insert("modified".into(), json!(s.is_modified(config)));
                    o.insert("live".into(), json!(s.live));
                    o.insert("dangerous".into(), json!(s.dangerous));
                    o.insert("min".into(), opt(s.min));
                    o.insert("max".into(), opt(s.max));
                    o.insert("minExclusive".into(), json!(s.min_exclusive));
                    o.insert("blankMeans".into(), json!(s.blank_means));
                    o.insert(
                        "profile".into(),
                        match overlay.filter(|o| o.covers(s.key)) {
                            Some(o) => json!(o.profile),
                            None => Value::Null,
                        },
                    );
                    Value::Object(o)
                })
                .collect();
            json!({"name": section, "settings": settings})
        })
        .collect();
    json!({"sections": sections, "profile": overlay.map(|o| o.profile.clone())})
}

/// A profile's client overrides (dotted config paths): the settings under them come from the
/// profile, not from config.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileOverlay {
    pub profile: String,
    pub paths: Vec<String>,
}

impl ProfileOverlay {
    pub fn new(profile: impl Into<String>, paths: Vec<String>) -> Self {
        Self {
            profile: profile.into(),
            paths,
        }
    }

    /// Whether the profile sets `key`, a section above it, or a part of it.
    pub fn covers(&self, key: &str) -> bool {
        let key = key.to_lowercase();
        self.paths.iter().any(|p| {
            let p = p.to_lowercase();
            p == key || key.starts_with(&format!("{p}.")) || p.starts_with(&format!("{key}."))
        })
    }

    /// The overlay of a profile applied by [`ConfigManager::apply_profile`], or `None`.
    pub fn of(manager: &ConfigManager) -> Option<Self> {
        let name = manager
            .config
            .active_profile
            .as_deref()
            .filter(|n| !n.is_empty())?;
        if manager.overlay_paths().is_empty() {
            return None;
        }
        Some(Self::new(name, manager.overlay_paths().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::serialize_config;
    use crate::profiles::ProfileDefinition;

    fn sections_settings(described: &Value) -> Vec<Value> {
        described["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["settings"].as_array().unwrap().clone())
            .collect()
    }

    #[test]
    fn every_key_is_the_settings_path_in_config_json_and_holds_its_value() {
        let mut config = OrcaConfig::default();
        config.lm_studio.temperature = 1.25;
        config.lm_studio.models = vec!["a".into(), "b".into()];
        config.context.reserve_tokens = 512;
        // `desktop` is written only when it differs from the default.
        config.desktop.close_to_tray = false;
        let json: Value = serde_json::from_str(&serialize_config(&config).unwrap()).unwrap();
        for s in all() {
            let node = s
                .key
                .split('.')
                .try_fold(&json, |n, part| n.get(part))
                .filter(|n| !n.is_null());
            let value = s.get(&config);
            if value == SettingValue::Null {
                assert!(node.is_none(), "{}", s.key);
            } else {
                assert!(node.is_some(), "{} isn't in config.json", s.key);
            }
            match value {
                SettingValue::Bool(b) => assert_eq!(node.unwrap().as_bool(), Some(b)),
                SettingValue::Int(i) => assert_eq!(node.unwrap().as_i64(), Some(i as i64)),
                SettingValue::Text(t) => assert_eq!(node.unwrap().as_str(), Some(t.as_str())),
                _ => {}
            }
        }
        let mut keys: Vec<&str> = all().iter().map(|s| s.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), all().len());
        assert!(all().iter().all(|s| SECTIONS.contains(&s.section)));
    }

    #[test]
    fn desktop_close_to_tray_reaches_config_json_only_when_turned_off() {
        let mut config = OrcaConfig::default();
        let text = crate::config::manager::serialize_config(&config).unwrap();
        assert!(!text.contains("\"desktop\""), "{text}");
        assert!(config.desktop.close_to_tray);
        assert!(
            apply(
                &mut config,
                "desktop.closeToTray",
                Some(&json!(false)),
                false,
                None
            )
            .ok
        );
        let text = crate::config::manager::serialize_config(&config).unwrap();
        let back: OrcaConfig = serde_json::from_str(&text).unwrap();
        assert!(!back.desktop.close_to_tray);
        assert!(back.extra.is_empty());
    }

    #[test]
    fn describe_then_apply_each_value_back_round_trips_through_config_json() {
        let mut config = OrcaConfig::default();
        assert!(
            apply(
                &mut config,
                "lmStudio.temperature",
                Some(&json!(1.5)),
                false,
                None
            )
            .ok
        );
        assert!(
            apply(
                &mut config,
                "permissions.alwaysApprove",
                Some(&json!(["read_file", "glob"])),
                false,
                None
            )
            .ok
        );
        assert!(
            apply(
                &mut config,
                "lmStudio.maxTokens",
                Some(&json!("4096")),
                false,
                None
            )
            .ok
        );
        assert!(
            apply(
                &mut config,
                "imageGen.model",
                Some(&json!("flux")),
                false,
                None
            )
            .ok
        );
        assert!(
            apply(
                &mut config,
                "session.autoSave",
                Some(&json!(false)),
                false,
                None
            )
            .ok
        );

        // Every described (non-secret) value applied to a fresh config gives the same values.
        let described = describe(&config, None);
        let mut copy = OrcaConfig::default();
        for setting in sections_settings(&described) {
            if setting["type"] == "secret" {
                continue;
            }
            let key = setting["key"].as_str().unwrap();
            let r = apply(&mut copy, key, Some(&setting["value"]), false, None);
            assert!(r.ok, "{key}: {:?}", r.error);
        }
        let reloaded: OrcaConfig = serde_json::from_str(&serialize_config(&copy).unwrap()).unwrap();
        for s in all().iter().filter(|s| s.kind != SettingType::Secret) {
            assert_eq!(s.display(&config), s.display(&reloaded), "{}", s.key);
        }
        assert_eq!(reloaded.lm_studio.temperature, 1.5);
        assert_eq!(
            reloaded.permissions.always_approve,
            vec!["read_file", "glob"]
        );
        assert_eq!(reloaded.lm_studio.max_tokens, Some(4096));

        // Modified markers and reset.
        let temp = find("lmStudio.temperature").unwrap();
        assert!(temp.is_modified(&config));
        let reset = apply(&mut config, "lmStudio.temperature", None, true, None);
        assert!(reset.ok);
        assert_eq!(reset.old.as_deref(), Some("1.5"));
        assert_eq!(reset.new.as_deref(), Some("0.7"));
        assert!(!temp.is_modified(&config));
        // Blank means unlimited / server default.
        assert!(
            apply(
                &mut config,
                "lmStudio.maxTokens",
                Some(&json!("")),
                false,
                None
            )
            .ok
        );
        assert_eq!(config.lm_studio.max_tokens, None);
        assert_eq!(
            find("lmStudio.maxTokens").unwrap().display(&config),
            "(unlimited)"
        );
    }

    #[test]
    fn secrets_are_masked_everywhere_and_can_be_replaced_or_removed() {
        const KEY: &str = "sk-test-0123456789abcdefXYZ";
        let mut config = OrcaConfig::default();
        let r = apply(
            &mut config,
            "lmStudio.apiKey",
            Some(&json!(KEY)),
            false,
            None,
        );
        assert!(r.ok);
        assert_eq!(config.lm_studio.api_key, KEY);
        assert!(!r.new.as_deref().unwrap().contains(KEY));
        assert_eq!(r.new.as_deref(), Some("sk-…XYZ"));

        let text = describe(&config, None).to_string();
        assert!(!text.contains(KEY));
        assert!(!text.contains("0123456789"));
        let api_key = sections_settings(&describe(&config, None))
            .into_iter()
            .find(|s| s["key"] == "lmStudio.apiKey")
            .unwrap();
        assert!(api_key["value"].is_null());
        assert!(api_key["default"].is_null());
        assert_eq!(api_key["set"], json!(true));

        // Remove: empty; short keys never show any of their characters.
        let removed = apply(&mut config, "lmStudio.apiKey", None, false, None);
        assert!(removed.ok);
        assert_eq!(config.lm_studio.api_key, "");
        assert_eq!(removed.new.as_deref(), Some("(not set)"));
        assert_eq!(mask(Some("short-key")), "••••");
    }

    #[test]
    fn validation_refuses_what_config_refuses_and_leaves_the_value() {
        let cases = [
            ("lmStudio.temperature", "3", "Must be between 0.0 and 2.0"),
            ("lmStudio.temperature", "warm", "Must be a number"),
            ("context.contextWindowSize", "50", "Must be > 100"),
            (
                "context.autoCompactThreshold",
                "0",
                "Must be between 0.0 and 1.0",
            ),
            (
                "context.charsPerToken",
                "11",
                "Must be between 1.0 and 10.0",
            ),
            ("lmStudio.timeoutSeconds", "0", "Must be positive"),
            (
                "lmStudio.backupMinResponseLength",
                "-1",
                "Must be 0 or positive",
            ),
            (
                "lmStudio.baseUrl",
                "ftp://host/v1",
                "Invalid URL format. Must be http:// or https://",
            ),
            (
                "lmStudio.baseUrl",
                "not a url",
                "Invalid URL format. Must be http:// or https://",
            ),
            ("session.maxSessions", "1.5", "Must be a whole number"),
        ];
        for (key, text, message) in cases {
            let mut config = OrcaConfig::default();
            let def = find(key).unwrap();
            let before = def.display(&config);
            let r = apply(&mut config, key, Some(&json!(text)), false, None);
            assert!(!r.ok, "{key}={text}");
            assert_eq!(r.error.as_deref(), Some(message), "{key}={text}");
            assert_eq!(before, def.display(&config));
            // The terminal's parser says the same.
            assert_eq!(def.try_parse(Some(text)).unwrap_err(), message);
        }
    }

    #[test]
    fn profile_keys_are_marked_and_refused_while_the_profile_is_active() {
        let mut manager = ConfigManager::new(Some(std::path::Path::new("unused")));
        manager
            .load_from_text(r#"{"lmStudio":{"temperature":0.3}}"#)
            .unwrap();
        assert_eq!(ProfileOverlay::of(&manager), None);
        let mut client = indexmap::IndexMap::new();
        client.insert("lmStudio.temperature".to_string(), json!(0.9));
        client.insert("permissions".to_string(), json!({}));
        manager
            .apply_profile(
                "fast",
                &ProfileDefinition {
                    client,
                    ..ProfileDefinition::default()
                },
            )
            .unwrap();
        let overlay = ProfileOverlay::of(&manager).unwrap();
        assert_eq!(overlay.profile, "fast");

        // The key itself, every key under a section it sets, and a key it sets part of.
        assert!(overlay.covers("lmStudio.temperature"));
        assert!(overlay.covers("permissions.disabledTools"));
        assert!(
            ProfileOverlay::new("p", vec!["slots.copies.0.slots".into()]).covers("slots.copies")
        );
        assert!(!overlay.covers("lmStudio.temperatureX"));
        assert!(!overlay.covers("session.maxSessions"));

        let described = describe(&manager.config, Some(&overlay));
        assert_eq!(described["profile"], json!("fast"));
        let settings = sections_settings(&described);
        let by_key = |k: &str| settings.iter().find(|s| s["key"] == k).unwrap().clone();
        assert_eq!(by_key("lmStudio.temperature")["profile"], json!("fast"));
        assert!((by_key("lmStudio.temperature")["value"].as_f64().unwrap() - 0.9).abs() < 1e-3);
        assert!(by_key("session.maxSessions")["profile"].is_null());
        assert!(describe(&manager.config, None)["profile"].is_null());

        let refused = apply(
            &mut manager.config,
            "lmStudio.temperature",
            Some(&json!(0.5)),
            false,
            Some(&overlay),
        );
        assert!(!refused.ok);
        assert!(refused.error.unwrap().starts_with("Set by profile fast"));
        assert_eq!(manager.config.lm_studio.temperature, 0.9);
        assert!(
            !apply(
                &mut manager.config,
                "permissions.disabledTools",
                Some(&json!(["bash"])),
                false,
                Some(&overlay)
            )
            .ok
        );
        assert!(
            apply(
                &mut manager.config,
                "session.maxSessions",
                Some(&json!(9)),
                false,
                Some(&overlay)
            )
            .ok
        );
    }

    #[test]
    fn default_profile_is_a_setting_blank_for_none() {
        let mut config = OrcaConfig::default();
        assert!(
            apply(
                &mut config,
                PROFILES_ACTIVE_KEY,
                Some(&json!(" dense2 ")),
                false,
                None
            )
            .ok
        );
        assert_eq!(config.profiles.active.as_deref(), Some("dense2"));
        assert!(
            apply(
                &mut config,
                PROFILES_ACTIVE_KEY,
                Some(&json!("")),
                false,
                None
            )
            .ok
        );
        assert_eq!(config.profiles.active, None);
        assert_eq!(
            find(PROFILES_ACTIVE_KEY).unwrap().display(&config),
            "(none)"
        );
    }

    #[test]
    fn wrong_shapes_and_unknown_keys_are_refused() {
        let mut config = OrcaConfig::default();
        let err = |c: &mut OrcaConfig, k: &str, v: Value| apply(c, k, Some(&v), false, None).error;
        assert_eq!(
            err(&mut config, "lmStudio.nope", json!(1)).as_deref(),
            Some("Unknown setting: lmStudio.nope")
        );
        assert_eq!(
            err(&mut config, "session.autoSave", json!("maybe")).as_deref(),
            Some("Must be on or off")
        );
        assert_eq!(
            err(&mut config, "permissions.disabledTools", json!("bash")).as_deref(),
            Some("Must be a list")
        );
        assert_eq!(
            err(
                &mut config,
                "permissions.disabledTools",
                json!(["bash", ""])
            )
            .as_deref(),
            Some("Every item must be non-empty text")
        );
        assert_eq!(
            err(&mut config, "imageGen.model", json!("a\nb")).as_deref(),
            Some("Must be a single line")
        );
    }

    #[test]
    fn uncensored_image_model_is_a_setting_that_can_be_blanked_and_reset() {
        // `AgentUncensoredConfig.ActiveImageModel` (not in the Rust model yet).
        fn active_image_model(c: &OrcaConfig) -> Option<String> {
            let u = &c.agent.uncensored;
            u.image_model
                .as_deref()
                .filter(|m| u.is_on() && !m.trim().is_empty())
                .map(|m| m.trim().to_string())
        }
        let mut config = OrcaConfig::default();
        let setting = find("agent.uncensored.imageModel").unwrap();
        assert_eq!(setting.section, IMAGE_GENERATION);
        assert_eq!(
            setting.get(&config),
            SettingValue::Text("qwen-image-uncensored".into())
        );

        let key = "agent.uncensored.imageModel";
        assert!(apply(&mut config, key, Some(&json!(" my-model ")), false, None).ok);
        assert_eq!(
            config.agent.uncensored.image_model.as_deref(),
            Some("my-model")
        );

        // Blank: images don't follow /uncensored.
        assert!(apply(&mut config, key, Some(&json!("")), false, None).ok);
        config.agent.uncensored.session = Some(true);
        assert_eq!(active_image_model(&config), None);

        assert!(apply(&mut config, key, None, true, None).ok);
        assert_eq!(
            active_image_model(&config).as_deref(),
            Some("qwen-image-uncensored")
        );
    }

    #[test]
    fn floats_show_as_dotnet_formats_them() {
        assert_eq!(format_float(0.7), "0.7");
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(1.25), "1.25");
        assert_eq!(format_float(0.12345), "0.123");
        assert_eq!(format_float(4.0), "4.0");
    }
}
