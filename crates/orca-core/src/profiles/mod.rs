//! Inference profiles and agent modes (port of `OpenOrca.Core.Profiles`): the profiles API
//! client and its catalog (Profiles v3: engine / engine profile / model per card), dotted-path
//! config overrides, the agent modes and the resolved-settings snapshot. Applying a profile or a
//! mode to the loaded config is [`crate::config::ConfigManager::apply_profile`] /
//! [`crate::config::ConfigManager::apply_mode`].

pub mod agent_modes;
pub mod catalog;
pub mod config_overrides;
pub mod service;

pub use catalog::{
    LayoutCard, LayoutDefinition, LiveCard, LiveLayout, ProfileCatalog, ProfileDefinition,
    SwitchJob, SwitchPlan, SwitchStep,
};
pub use service::{ProfileError, ProfileService, resolve_server_url};

use serde_json::{Map, Value, json};

use crate::config::{ConfigManager, OrcaConfig};

/// `ProfileSnapshot.Resolve`: the resolved inference settings of a config (after any profile),
/// keys and secrets left out.
pub fn snapshot(config: &OrcaConfig) -> Value {
    const SECTIONS: [&str; 4] = ["agent", "slots", "thinking", "toolProfile"];
    let all: Value = crate::config::serialize_config(config)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut snap = Map::new();
    snap.insert("profile".into(), json!(config.active_profile));
    for key in SECTIONS {
        if let Some(node) = all.get(key).filter(|v| !v.is_null()) {
            snap.insert(key.into(), node.clone());
        }
    }
    snap.insert(
        "lmStudio".into(),
        json!({"baseUrl": config.lm_studio.base_url, "models": config.lm_studio.models}),
    );
    snap.insert(
        "context".into(),
        json!({"contextWindowSize": config.context.context_window_size}),
    );
    let mut snap = Value::Object(snap);
    strip_secrets(&mut snap);
    snap
}

/// Remove every apiKey / token / password / secret value (key names compared case-insensitively;
/// not apiKeyEnv or maxTokens-like names that merely contain "token").
pub fn strip_secrets(node: &mut Value) {
    match node {
        Value::Object(o) => {
            let keys: Vec<String> = o.keys().cloned().collect();
            for k in keys {
                let lower = k.to_lowercase();
                let secret = lower.ends_with("apikey")
                    || lower.ends_with("token")
                    || lower.contains("password")
                    || lower.contains("secret");
                let scalar = !matches!(o.get(&k), Some(Value::Object(_) | Value::Array(_)));
                if secret && scalar {
                    o.shift_remove(&k);
                } else if let Some(v) = o.get_mut(&k) {
                    strip_secrets(v);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

/// `ProfileCommand.ApplyAtStartupAsync`: apply `name` (from --profile or profiles.active) to the
/// loaded config. Returns an error for an unknown profile or no list at all; notices (offline
/// copy, layout not live) go to `notices`.
pub async fn apply_at_startup(
    manager: &mut ConfigManager,
    config_dir: &std::path::Path,
    name: &str,
    notices: &mut Vec<String>,
) -> Option<String> {
    let service = ProfileService::from_config(&manager.config, config_dir);
    let (catalog, error) = service.get_catalog().await;
    let Some(catalog) = catalog else {
        return Some(format!("Profile '{name}': {}", error.unwrap_or_default()));
    };
    let Some(profile) = catalog.find(name).cloned() else {
        return Some(format!(
            "Unknown profile '{name}' (known: {}).",
            catalog.sorted_names().join(", ")
        ));
    };
    let key = catalog
        .resolve(Some(name))
        .unwrap_or_else(|| name.to_string());
    if let Err(e) = manager.apply_profile(&key, &profile) {
        return Some(format!("Profile '{name}': {e}"));
    }
    if let Some(error) = error {
        notices.push(format!("Profile '{name}' from the offline copy ({error})."));
    } else if !catalog.fits(&profile) {
        notices.push(format!(
            "Profile '{name}' needs server layout '{}', but '{}' is live: its model endpoints may not answer. /profile switch {name} switches the server.",
            profile.layout.as_deref().unwrap_or(""),
            catalog
                .live
                .as_ref()
                .and_then(|l| l.layout.as_deref())
                .unwrap_or("unknown")
        ));
    }
    None
}

#[cfg(test)]
mod tests;
