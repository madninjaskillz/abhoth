//! The shared profile list (`ProfileCatalog`, profiles.json on the inference server): server
//! layouts, profiles (a layout + client config overrides), the v3 engine / engine profile /
//! model sections and the live layout. Read with the web defaults of the profiles API
//! (camelCase, numbers also from strings).

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Name-keyed maps keep the file's order; lookups are case-insensitive (see the helpers).
pub type NameMap<V> = IndexMap<String, V>;

fn find_key<'a, V>(map: &'a NameMap<V>, name: &str) -> Option<&'a String> {
    map.keys().find(|k| k.eq_ignore_ascii_case(name))
}

/// A case-insensitive lookup in a [`NameMap`].
pub fn get_ci<'a, V>(map: &'a NameMap<V>, name: &str) -> Option<&'a V> {
    find_key(map, name).and_then(|k| map.get(k))
}

/// `JsonNumberHandling.AllowReadingFromString` for optional numbers.
fn num_or_str<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr + serde::de::DeserializeOwned,
{
    let v = Option::<Value>::deserialize(d)?;
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("not a number: {s}"))),
        Some(other) => serde_json::from_value(other)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn num_or_str_req<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr + serde::de::DeserializeOwned + Default,
{
    num_or_str(d).map(Option::unwrap_or_default)
}

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileCatalog {
    #[serde(deserialize_with = "num_or_str_req")]
    pub version: i32,
    pub etag: Option<String>,
    pub server: Option<ProfileServerInfo>,
    #[serde(deserialize_with = "null_as_default")]
    pub layouts: NameMap<LayoutDefinition>,
    #[serde(deserialize_with = "null_as_default")]
    pub profiles: NameMap<ProfileDefinition>,
    pub live: Option<LiveLayout>,
    pub layout_status: Option<NameMap<LayoutStatus>>,
    /// Schema v3: engines, their settings bundles, and the models (with their artifact per engine).
    pub engines: Option<NameMap<EngineInfo>>,
    pub engine_profiles: Option<NameMap<EngineProfileInfo>>,
    pub models: Option<NameMap<ModelInfo>>,
    /// Old profile name -> current name.
    pub aliases: Option<NameMap<String>>,
    /// Old layout name -> current name.
    pub layout_aliases: Option<NameMap<String>>,
    /// Set by the client: when this copy was fetched (Unix seconds), and whether it came from
    /// the offline cache.
    #[serde(skip)]
    pub fetched_at: Option<std::time::SystemTime>,
    #[serde(skip)]
    pub from_cache: bool,
}

impl ProfileCatalog {
    pub fn parse(json: &str) -> serde_json::Result<ProfileCatalog> {
        if json.trim() == "null" {
            return Ok(ProfileCatalog::default());
        }
        serde_json::from_str(json)
    }

    /// A profile by its name or an old one (through the aliases).
    pub fn find(&self, name: &str) -> Option<&ProfileDefinition> {
        self.resolve(Some(name)).and_then(|k| self.profiles.get(&k))
    }

    /// The catalog's own key for a profile name: exact (any case), else through the aliases.
    pub fn resolve(&self, name: Option<&str>) -> Option<String> {
        let mut name = Some(name?.trim().to_string()).filter(|n| !n.is_empty())?;
        for _ in 0..4 {
            if let Some(key) = find_key(&self.profiles, &name) {
                return Some(key.clone());
            }
            name = self.aliases.as_ref().and_then(|a| get_ci(a, &name))?.clone();
        }
        None
    }

    /// The current name of a layout (through the layout aliases), or the name as given.
    pub fn resolve_layout(&self, name: Option<&str>) -> Option<String> {
        let mut name = name?.to_string();
        if name.trim().is_empty() {
            return Some(name);
        }
        for _ in 0..4 {
            if let Some(key) = find_key(&self.layouts, &name) {
                return Some(key.clone());
            }
            match self.layout_aliases.as_ref().and_then(|a| get_ci(a, &name)) {
                Some(next) => name = next.clone(),
                None => break,
            }
        }
        Some(name)
    }

    /// Whether the profile runs on the live layout as it is (or needs no server layout at all).
    pub fn fits(&self, profile: &ProfileDefinition) -> bool {
        let Some(layout) = profile.layout.as_deref() else {
            return true;
        };
        let want = self.resolve_layout(Some(layout));
        let live = self.resolve_layout(self.live.as_ref().and_then(|l| l.layout.as_deref()));
        match (want, live) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
            (None, None) => true,
            _ => false,
        }
    }

    /// "needs restore (≈24 GB, ~6 min)" when the profile's layout must copy model files back first.
    pub fn restore_note(&self, profile: &ProfileDefinition) -> Option<String> {
        let layout = profile.layout.as_deref()?;
        let status = self.layout_status.as_ref()?;
        // layoutStatus keeps the default (case-sensitive) comparer in C#.
        let text = status.get(layout)?.restore.as_ref()?.text.clone()?;
        (!text.is_empty()).then_some(text)
    }

    /// Profile names sorted case-insensitively (the "known: ..." list).
    pub fn sorted_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileServerInfo {
    pub host: Option<String>,
    pub registry_url: Option<String>,
    pub api_url: Option<String>,
}

/// What runs on each card.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LayoutDefinition {
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub cards: IndexMap<String, LayoutCard>,
    /// Plain-English summary the profiles API generates from the cards.
    #[serde(rename = "summary")]
    pub generated_summary: Option<String>,
}

impl LayoutDefinition {
    /// "A strata×3, B strata×2" (cards in ordinal name order).
    pub fn summary(&self) -> String {
        let mut cards: Vec<_> = self.cards.iter().collect();
        cards.sort_by(|a, b| a.0.cmp(b.0));
        cards
            .iter()
            .map(|(k, c)| format!("{k} {}", c.summary()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LayoutCard {
    pub engine: String,
    #[serde(deserialize_with = "num_or_str")]
    pub slots: Option<i32>,
    pub model: Option<String>,
}

impl Default for LayoutCard {
    fn default() -> Self {
        Self {
            engine: "off".into(),
            slots: None,
            model: None,
        }
    }
}

impl LayoutCard {
    pub fn summary(&self) -> String {
        if self.engine == "off" {
            return "off".into();
        }
        let mut s = format!("{}×{}", self.engine, self.slots.unwrap_or(1));
        if let Some(m) = &self.model {
            s.push_str(&format!(" ({m})"));
        }
        s
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LayoutStatus {
    pub needs_restore: bool,
    pub restore: Option<RestoreInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RestoreInfo {
    #[serde(deserialize_with = "num_or_str_req")]
    pub restore_bytes: i64,
    #[serde(deserialize_with = "num_or_str_req")]
    pub restore_seconds: i32,
    pub restorable: bool,
    pub text: Option<String>,
}

impl Default for RestoreInfo {
    fn default() -> Self {
        Self {
            restore_bytes: 0,
            restore_seconds: 0,
            restorable: true,
            text: None,
        }
    }
}

/// A profile: a server layout (none = not needed, e.g. a bridge engine) and client overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileDefinition {
    pub title: Option<String>,
    pub description: Option<String>,
    pub layout: Option<String>,
    pub summary: Option<String>,
    /// Hidden from pickers (kept so old names still resolve).
    pub hidden: bool,
    /// The setup's default agent mode; `None` for a bridge.
    pub mode: Option<String>,
    /// Per card ("A", "B"): {engine, engineProfile, model}, or "off" / "same".
    pub cards: Option<IndexMap<String, Value>>,
    /// builtin, user or composed.
    pub source: Option<String>,
    /// Config overrides by dotted path, applied over the user's config.
    #[serde(deserialize_with = "null_as_default")]
    pub client: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EngineInfo {
    pub title: Option<String>,
    pub description: Option<String>,
}

/// An engine's settings bundle: slots/lanes, build, cache, images; a rough VRAM budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EngineProfileInfo {
    pub engine: String,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(deserialize_with = "num_or_str_req")]
    pub slots: i32,
    #[serde(deserialize_with = "num_or_str")]
    pub ctx: Option<i32>,
    #[serde(rename = "vramGiB", deserialize_with = "num_or_str")]
    pub vram_gib: Option<f64>,
    pub options: Option<Map<String, Value>>,
}

impl Default for EngineProfileInfo {
    fn default() -> Self {
        Self {
            engine: String::new(),
            title: None,
            description: None,
            slots: 1,
            ctx: None,
            vram_gib: None,
            options: None,
        }
    }
}

/// A model: what it is, and its artifact (file + served model id) per engine it runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelInfo {
    pub title: Option<String>,
    pub description: Option<String>,
    pub vision: bool,
    #[serde(rename = "vramGiB", deserialize_with = "num_or_str")]
    pub vram_gib: Option<f64>,
    #[serde(deserialize_with = "null_as_default")]
    pub artifacts: IndexMap<String, Value>,
}

impl Default for ModelInfo {
    fn default() -> Self {
        Self {
            title: None,
            description: None,
            vision: true,
            vram_gib: None,
            artifacts: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LiveLayout {
    pub layout: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub summary: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub cards: IndexMap<String, LiveCard>,
    pub since: Option<String>,
    pub healthy: bool,
    pub leases: Option<Vec<LeaseInfo>>,
    pub job: Option<SwitchJob>,
    pub quiet_window: bool,
    pub music_mode: bool,
    pub checked: Option<String>,
    pub last_switch: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LiveCard {
    pub engine: String,
    #[serde(deserialize_with = "num_or_str")]
    pub slots: Option<i32>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub healthy: bool,
    pub detail: Option<String>,
    pub gpu: Option<String>,
    /// The engine's options; "alias" is the model name the card serves.
    pub options: Option<Map<String, Value>>,
}

impl Default for LiveCard {
    fn default() -> Self {
        Self {
            engine: "off".into(),
            slots: None,
            model: None,
            endpoint: None,
            healthy: false,
            detail: None,
            gpu: None,
            options: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeaseInfo {
    pub slot: String,
    pub who: Option<String>,
}

/// GET /plan: does a profile fit the live layout; if not, what a switch would do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SwitchPlan {
    pub profile: Option<String>,
    pub layout: Option<String>,
    pub live: Option<String>,
    pub fits: bool,
    #[serde(deserialize_with = "num_or_str_req")]
    pub estimate_seconds: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub refusals: Vec<String>,
    pub effect: Option<String>,
    pub restore: Option<RestoreInfo>,
    pub note: Option<String>,
    pub changes: Option<Map<String, Value>>,
    #[serde(deserialize_with = "num_or_str")]
    pub back_estimate_seconds: Option<i32>,
}

/// A layout switch job on the server (POST /switch, GET /jobs/id).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SwitchJob {
    pub id: Option<String>,
    pub layout: Option<String>,
    pub profile: Option<String>,
    pub from: Option<String>,
    /// queued, running, done, rolled-back, failed, refused.
    pub state: Option<String>,
    pub already: bool,
    #[serde(deserialize_with = "num_or_str")]
    pub seconds: Option<f64>,
    #[serde(deserialize_with = "num_or_str")]
    pub estimate_seconds: Option<i32>,
    pub error: Option<String>,
    pub live: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub steps: Vec<SwitchStep>,
    pub last_step: Option<SwitchStep>,
}

impl SwitchJob {
    pub fn finished(&self) -> bool {
        matches!(
            self.state.as_deref(),
            Some("done" | "rolled-back" | "failed" | "refused")
        )
    }

    pub fn succeeded(&self) -> bool {
        self.state.as_deref() == Some("done")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SwitchStep {
    #[serde(deserialize_with = "num_or_str_req")]
    pub t: f64,
    pub msg: String,
}
