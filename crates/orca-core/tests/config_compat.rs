//! config.json compatibility with the C# build, plus ports of `ConfigManagerTests`.
//!
//! The fixtures were written by the C# `ConfigManager` itself (see `tests/fixtures`): the Rust
//! port must read them and write them back byte for byte.

use std::path::Path;

use orca_core::config::{
    AgentConfig, ConfigManager, OrcaConfig, manager, serialize_config_with_newline,
};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

/// The fixtures were saved on Windows (CRLF); compare line content so the test runs anywhere.
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

#[test]
fn default_config_serializes_exactly_like_csharp() {
    // C# writes `new OrcaConfig()` on first run, before validation.
    let ours = serialize_config_with_newline(&OrcaConfig::default(), "\n").unwrap();
    assert_eq!(ours, lf(&fixture("csharp-default.config.json")));
}

#[test]
fn csharp_default_file_round_trips() {
    let text = fixture("csharp-default.config.json");
    let config: OrcaConfig = serde_json::from_str(&text).unwrap();
    assert_eq!(
        serialize_config_with_newline(&config, "\n").unwrap(),
        lf(&text)
    );
}

#[test]
fn rich_config_load_migrate_validate_save_matches_csharp() {
    let mut m = ConfigManager::new(Some(Path::new("unused")));
    let migrated = m.load_from_text(&fixture("rich.input.json")).unwrap();
    assert!(
        migrated,
        "legacy model/backupModel must trigger a migration"
    );
    let ours = serialize_config_with_newline(&m.config, "\n").unwrap();

    // C# drops unknown top-level keys; the port keeps them so a newer file survives an older
    // binary. Apart from that key the output is identical.
    let mut ours_v: serde_json::Value = serde_json::from_str(&ours).unwrap();
    assert_eq!(ours_v["unknownTopLevel"], serde_json::json!({"keep": true}));
    ours_v
        .as_object_mut()
        .unwrap()
        .shift_remove("unknownTopLevel");
    let without =
        orca_core::config::dotnet_json::to_string_dotnet_with_newline(&ours_v, "\n").unwrap();
    assert_eq!(without, lf(&fixture("rich.csharp-saved.config.json")));
}

#[test]
fn csharp_saved_rich_file_round_trips() {
    let text = fixture("rich.csharp-saved.config.json");
    let mut m = ConfigManager::new(Some(Path::new("unused")));
    assert!(!m.load_from_text(&text).unwrap());
    assert_eq!(
        serialize_config_with_newline(&m.config, "\n").unwrap(),
        lf(&text)
    );
}

#[test]
fn load_creates_default_file_on_first_run_and_reloads_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = ConfigManager::new(Some(dir.path()));
    m.load().unwrap();
    let written = std::fs::read_to_string(m.file_path()).unwrap();
    assert_eq!(lf(&written), lf(&fixture("csharp-default.config.json")));
    if cfg!(windows) {
        assert!(
            written.contains("\r\n"),
            "Windows saves use CRLF like Environment.NewLine"
        );
    }
    let mut again = ConfigManager::new(Some(dir.path()));
    again.load().unwrap();
    assert!(
        again.config.providers.contains_key("lmstudio"),
        "validation synthesizes the provider"
    );
}

#[test]
fn load_strips_a_utf8_bom() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(br#"{"lmStudio":{"models":["m1"]}}"#);
    std::fs::write(dir.path().join("config.json"), bytes).unwrap();
    let mut m = ConfigManager::new(Some(dir.path()));
    m.load().unwrap();
    assert_eq!(m.config.lm_studio.primary_model(), Some("m1"));
}

// ── Ported from ConfigManagerTests.cs ──

#[test]
fn default_config_has_sensible_defaults() {
    let c = OrcaConfig::default();
    assert_eq!(c.lm_studio.base_url, "http://localhost:1234/v1");
    assert_eq!(c.lm_studio.api_key, "lm-studio");
    assert!(c.lm_studio.models.is_empty());
    assert_eq!(c.lm_studio.primary_model(), None);
    assert_eq!(c.lm_studio.temperature, 0.7);
    assert_eq!(c.lm_studio.timeout_seconds, 120);
    assert!(c.permissions.auto_approve_read_only);
    assert!(!c.permissions.auto_approve_all);
    assert!(c.session.auto_save);
    assert_eq!(c.session.max_sessions, 100);
}

#[test]
fn normalize_agent_limits_migrates_old_defaults() {
    for (it, to, want_it, want_to) in [(15, 300, 40, 1200), (0, -1, 40, 1200), (25, 600, 25, 600)] {
        let mut a = AgentConfig {
            max_iterations: it,
            timeout_seconds: to,
            ..AgentConfig::default()
        };
        manager::normalize_agent_limits(&mut a);
        assert_eq!((a.max_iterations, a.timeout_seconds), (want_it, want_to));
    }
    let d = AgentConfig::default();
    assert_eq!(
        (d.max_iterations, d.timeout_seconds, d.max_concurrent),
        (40, 1200, 2)
    );
}

fn load(json: &str) -> (OrcaConfig, bool) {
    let mut m = ConfigManager::new(Some(Path::new("unused")));
    let migrated = m.load_from_text(json).unwrap();
    (m.config, migrated)
}

#[test]
fn legacy_model_and_backup_migrate_to_models_list() {
    let (c, migrated) = load(r#"{"lmStudio":{"model":"mistral-7b","backupModel":"deepseek-r1"}}"#);
    assert!(migrated);
    assert_eq!(c.lm_studio.models, ["mistral-7b", "deepseek-r1"]);
    assert_eq!(c.lm_studio.primary_model(), Some("mistral-7b"));
    assert!(c.lm_studio.model.is_none() && c.lm_studio.backup_model.is_none());
    let json = serialize_config_with_newline(&c, "\n").unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["lmStudio"].get("model").is_none() && v["lmStudio"].get("backupModel").is_none());
    assert!(v["lmStudio"].get("models").is_some());
}

#[test]
fn legacy_model_only_migrates_to_single_model_list() {
    let (c, _) = load(r#"{"lmStudio":{"model":"mistral-7b"}}"#);
    assert_eq!(c.lm_studio.models, ["mistral-7b"]);
}

#[test]
fn models_list_is_not_overwritten_by_migration() {
    let (c, migrated) = load(r#"{"lmStudio":{"models":["model-a","model-b","model-c"]}}"#);
    assert!(!migrated);
    assert_eq!(c.lm_studio.models.len(), 3);
    assert_eq!(c.lm_studio.primary_model(), Some("model-a"));
}

#[test]
fn empty_providers_synthesize_lmstudio_provider() {
    let (c, _) = load(
        r#"{"lmStudio":{"baseUrl":"http://myhost:9999/v1","apiKey":"secret","models":["model-a","model-b"]}}"#,
    );
    assert_eq!(c.providers.len(), 1);
    let p = &c.providers["lmstudio"];
    assert_eq!(p.kind, "openai-compat");
    assert_eq!(p.base_url, "http://myhost:9999/v1");
    assert_eq!(p.api_key.as_deref(), Some("secret"));
    assert_eq!(p.models, ["model-a", "model-b"]);
}

#[test]
fn existing_providers_are_not_overwritten_and_lookup_ignores_case() {
    let (c, _) = load(r#"{"providers":{"claude":{"type":"anthropic","models":["x"]}}}"#);
    assert_eq!(c.providers.len(), 1);
    assert_eq!(c.providers["claude"].kind, "anthropic");
    assert!(c.provider("CLAUDE").is_some());
}

#[test]
fn provider_alias_and_round_trip() {
    let (c, _) = load(
        r#"{"providers":{"anthropic":{"provider":"anthropic","baseUrl":"https://api.anthropic.com","apiKeyEnv":"ANTHROPIC_API_KEY","models":["claude-x"]}}}"#,
    );
    let p = &c.providers["anthropic"];
    assert_eq!(p.kind, "anthropic");
    assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    assert!(p.api_key.is_none());
    let json = serialize_config_with_newline(&c, "\n").unwrap();
    assert!(json.contains("\"type\": \"anthropic\""));
    assert!(!json.contains("\"provider\":"), "the alias is read-only");
}

#[test]
fn context_and_hooks_defaults() {
    let c = OrcaConfig::default();
    assert_eq!(c.context.context_window_size, 8192);
    assert_eq!(c.context.auto_compact_threshold, 0.8);
    assert_eq!(c.context.compact_preserve_last_n, 4);
    assert!(c.context.auto_compact_enabled);
    assert!(c.hooks.pre_tool_hooks.is_empty() && c.hooks.post_tool_hooks.is_empty());
    let (h, _) = load(
        r#"{"hooks":{"preToolHooks":{"bash":"echo pre-bash"},"postToolHooks":{"*":"echo post-any"}}}"#,
    );
    assert_eq!(h.hooks.pre_tool_hooks["bash"], "echo pre-bash");
    assert_eq!(h.hooks.post_tool_hooks["*"], "echo post-any");
}

#[test]
fn validation_fixes_out_of_range_values() {
    let (c, _) = load(
        r#"{"lmStudio":{"temperature":5},"context":{"contextWindowSize":0},"shell":{"idleTimeoutSeconds":15},"slots":{"enabled":true,"lead":{"baseUrl":"http://127.0.0.1:9/v1","contextWindow":32768}}}"#,
    );
    assert_eq!(c.lm_studio.temperature, 0.7);
    assert_eq!(
        c.context.context_window_size, 32768,
        "slots.lead.contextWindow wins"
    );
    assert_eq!(c.shell.idle_timeout_seconds, 120);
}
