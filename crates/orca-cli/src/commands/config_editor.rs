//! `/config` (`ConfigEditor` and `Repl/ConfigSections`): the interactive settings editor, drawn
//! from [`orca_core::config::schema`] (the settings the web UI's page shows too), and the
//! "View All" table.
//!
//! As in C#, the editor changes the session's live config as you go; "Save & Exit" writes it
//! to config.json (the profile's overridden paths keep the file's values) and "Exit (discard
//! changes)" only skips the save.

use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use orca_core::config::schema::{self, ProfileOverlay, SettingDef, SettingType, SettingValue};
use orca_core::config::{ConfigManager, OrcaConfig};

use super::CommandContext;
use crate::ui::markup::{escape, remove};
use crate::ui::widgets::Table;
use crate::ui::{Console, prompt};

const VIEW_ALL: &str = "View All";
const SAVE_EXIT: &str = "Save & Exit";
const EXIT_DISCARD: &str = "Exit (discard changes)";
const TEST_CONNECTION: &str = "Test Connection";

/// `ShowConfigAsync`: the section menu, until "Save & Exit" or "Exit (discard changes)".
/// `config_manager_dir` holds config.json. With a profile active in the session, its overlay
/// is looked up from the profile list (see [`session_overlay`]); a caller that kept
/// `ProfileOverlay::of(&manager)` from startup uses [`show_config_with_overlay`].
pub async fn show_config(ctx: &mut CommandContext<'_>, config_manager_dir: &Path) {
    let overlay = session_overlay(&ctx.session.config, config_manager_dir).await;
    show_config_with_overlay(ctx, config_manager_dir, overlay.as_ref()).await;
}

/// The overlay of the session's active profile (`config.active_profile`): the profile's
/// client paths from the profile list (the API, else its cached copy), or `None` without a
/// profile or a list. An agent mode's paths (`--mode`) aren't known here.
pub async fn session_overlay(config: &OrcaConfig, config_dir: &Path) -> Option<ProfileOverlay> {
    let name = config.active_profile.as_deref().filter(|n| !n.is_empty())?;
    let service = orca_core::profiles::ProfileService::from_config(config, config_dir);
    let (catalog, _) = service.get_catalog().await;
    let profile = catalog?.find(name)?.clone();
    if profile.client.is_empty() {
        return None;
    }
    Some(ProfileOverlay::new(
        name,
        profile.client.keys().cloned().collect(),
    ))
}

/// [`show_config`] with the session's overlay (`ProfileOverlay.Of(configManager)`): the
/// settings it sets are shown as the profile's, aren't edited here and keep the file's values
/// on save.
pub async fn show_config_with_overlay(
    ctx: &mut CommandContext<'_>,
    config_manager_dir: &Path,
    overlay: Option<&ProfileOverlay>,
) {
    loop {
        ctx.console.blank();
        let mut choices: Vec<String> = schema::SECTIONS.iter().map(|s| s.to_string()).collect();
        choices.extend([VIEW_ALL, SAVE_EXIT, EXIT_DISCARD].map(String::from));
        let Some(i) = prompt::select(ctx.console, "[bold]Configuration[/]", &choices) else {
            return;
        };
        match choices[i].as_str() {
            VIEW_ALL => render_config_table_with_overlay(
                ctx.console,
                &ctx.session.config,
                config_manager_dir,
                overlay,
            ),
            SAVE_EXIT => {
                match save(&ctx.session.config, config_manager_dir, overlay) {
                    Ok(()) => ctx.console.markup_line("[green]Configuration saved.[/]"),
                    Err(e) => ctx.console.markup_line(&format!("[red]{}[/]", escape(&e))),
                }
                return;
            }
            EXIT_DISCARD => return,
            section => {
                let section = section.to_string();
                edit_section(ctx.console, &mut ctx.session.config, &section, overlay).await;
            }
        }
    }
}

/// `ConfigManager.SaveAsync` of the session's config.
fn save(config: &OrcaConfig, dir: &Path, overlay: Option<&ProfileOverlay>) -> Result<(), String> {
    let mut manager = ConfigManager::new(Some(dir));
    // The file as it is (what the overridden paths are restored from); a broken file is
    // simply written over, as C# does.
    let _ = manager.load();
    manager.adopt(
        config.clone(),
        overlay.map(|o| o.paths.clone()).unwrap_or_default(),
    );
    manager.save().map_err(|e| e.to_string())
}

/// `RenderConfigTable`: every /config setting with its value, the hooks and the file.
pub fn render_config_table(console: &Console, config: &OrcaConfig, config_dir: &Path) {
    render_config_table_with_overlay(console, config, config_dir, None);
}

/// [`render_config_table`] with "(profile X)" after the settings the profile sets.
pub fn render_config_table_with_overlay(
    console: &Console,
    config: &OrcaConfig,
    config_dir: &Path,
    overlay: Option<&ProfileOverlay>,
) {
    let mut table = Table::new()
        .expand()
        .title("[bold]Current Configuration[/]")
        .column("[bold]Setting[/]")
        .column("[bold]Value[/]");
    for section in schema::SECTIONS {
        table.row(&[format!("[yellow]{}[/]", escape(section)), String::new()]);
        for setting in schema::in_section(section) {
            table.row(&[
                format!("  {}", escape(setting.label)),
                format!(
                    "{}{}",
                    escape(&setting.display(config)),
                    profile_note(overlay, setting.key)
                ),
            ]);
        }
        table.row(&["", ""]);
    }
    let hooks = &config.hooks;
    let names = |m: &orca_core::config::Map<String>| {
        if m.is_empty() {
            "(none)".to_string()
        } else {
            m.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    table.row(&["[yellow]Hooks[/]", ""]);
    table.row(&["  Pre-tool hooks".to_string(), names(&hooks.pre_tool_hooks)]);
    table.row(&[
        "  Post-tool hooks".to_string(),
        names(&hooks.post_tool_hooks),
    ]);
    table.row(&["", ""]);
    table.row(&[
        "[grey]Config file[/]".to_string(),
        escape(&format!("{}/config.json", config_dir.display())),
    ]);
    table.write(console);
}

fn profile_note(overlay: Option<&ProfileOverlay>, key: &str) -> String {
    match overlay.filter(|o| o.covers(key)) {
        Some(o) => format!(" [grey](profile {})[/]", escape(&o.profile)),
        None => String::new(),
    }
}

/// `SchemaConfigSection.EditAsync` (with the LLM Server and Image Generation sections' model
/// pickers and Test Connection).
async fn edit_section(
    console: &Console,
    config: &mut OrcaConfig,
    section: &str,
    overlay: Option<&ProfileOverlay>,
) {
    let settings: Vec<&SettingDef> = schema::in_section(section).collect();
    let width = settings
        .iter()
        .map(|s| s.label.chars().count())
        .max()
        .unwrap_or(0);
    let actions: &[&str] = if section == schema::LLM_SERVER {
        &[TEST_CONNECTION]
    } else {
        &[]
    };
    loop {
        let mut choices: Vec<String> = settings
            .iter()
            .map(|s| {
                format!(
                    "{:<width$} : {}{}",
                    s.label,
                    escape(&s.display(config)),
                    profile_note(overlay, s.key)
                )
            })
            .collect();
        choices.extend(actions.iter().map(|a| a.to_string()));
        choices.push("Back".into());
        let title = format!("[bold]{} Settings[/]", escape(section));
        let Some(i) = prompt::select(console, &title, &choices) else {
            return;
        };
        if i == choices.len() - 1 {
            return;
        }
        if i >= settings.len() {
            if actions[i - settings.len()] == TEST_CONNECTION {
                test_connection(console, config).await;
            }
            continue;
        }
        let def = settings[i];
        if let Some(o) = overlay.filter(|o| o.covers(def.key)) {
            console.markup_line(&format!(
                "[yellow]Set by profile {}:[/] /profile switches to another profile (or none) to change it.",
                escape(&o.profile)
            ));
            continue;
        }
        match def.key {
            "lmStudio.models" => {
                let available = crate::model_discovery::available_models(config).await;
                config.lm_studio.models =
                    prompt_model_priority_list(console, &available, &config.lm_studio.models);
            }
            "imageGen.model" => {
                let models = fetch_image_gen_models(config).await;
                let current = config.image_gen.model.clone();
                config.image_gen.model = prompt_model_selection(
                    console,
                    models,
                    "Select image generation model:",
                    "(server default)",
                    "Model name (blank for server default):",
                    current.as_deref(),
                );
            }
            _ => edit(console, config, def),
        }
    }
}

/// `SchemaConfigSection.Edit`: the plain editor for a setting.
fn edit(console: &Console, config: &mut OrcaConfig, def: &SettingDef) {
    match def.kind {
        SettingType::Bool => {
            let title = format!("{}:", def.description.trim_end_matches('.'));
            let choices = vec!["On".to_string(), "Off".to_string()];
            if let Some(i) = prompt::select(console, &title, &choices) {
                def.set(config, SettingValue::Bool(i == 0));
            }
            return;
        }
        SettingType::List => {
            let SettingValue::List(current) = def.get(config) else {
                return;
            };
            let list = edit_string_list(console, def.label, current);
            def.set(config, SettingValue::List(list));
            return;
        }
        SettingType::Secret => {
            let current = match def.get(config) {
                SettingValue::Text(s) => s,
                _ => String::new(),
            };
            let Some(secret) =
                text_with_default(console, &format!("{}:", escape(def.label)), &current, true)
            else {
                return;
            };
            if let Ok(key) = def.try_parse(Some(&secret)) {
                def.set(config, key);
            }
            return;
        }
        _ => {}
    }
    let current = match def.get(config) {
        SettingValue::Null => String::new(),
        SettingValue::Float(f) => schema::format_float(f),
        SettingValue::Int(i) => i.to_string(),
        SettingValue::Text(s) => s,
        SettingValue::Bool(b) => if b { "True" } else { "False" }.to_string(),
        SettingValue::List(l) => l.join(", "),
    };
    let label = format!("{}:", escape(def.description.trim_end_matches('.')));
    // TextPrompt.Validate: an invalid answer is reported and asked again.
    let text = loop {
        let Some(text) = text_with_default(console, &label, &current, false) else {
            return;
        };
        match def.try_parse(Some(&text)) {
            Ok(_) => break text,
            Err(e) => console.markup_line(&escape(&e)),
        }
    };
    match def.try_parse(Some(&text)) {
        Ok(value) => def.set(config, value),
        Err(e) => console.markup_line(&format!("[red]{}[/]", escape(&e))),
    }
}

/// `TextPrompt<string>` with a default value (shown as `[green](default)[/]`, returned for a
/// blank answer); `secret` echoes `*` and masks the default. `None` when cancelled.
fn text_with_default(
    console: &Console,
    label: &str,
    default: &str,
    secret: bool,
) -> Option<String> {
    let shown = if secret {
        "*".repeat(default.chars().count())
    } else {
        escape(default)
    };
    let markup = format!("{} [green]({shown})[/]:", label.trim_end());
    let answer = if secret && prompt::interactive() && console.is_terminal() {
        read_masked(console, &markup)?
    } else {
        prompt::text(console, &markup)?
    };
    Some(if answer.trim().is_empty() {
        default.to_string()
    } else {
        answer
    })
}

/// A secret line: each character echoes as `*`.
fn read_masked(console: &Console, markup: &str) -> Option<String> {
    console.markup(&format!("{markup} "));
    let _raw = prompt::RawGuard::enable();
    let mut buf = String::new();
    loop {
        let Ok(Event::Key(key)) = crossterm::event::read() else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                console.raw("\r\n");
                return Some(buf);
            }
            KeyCode::Esc => {
                console.raw("\r\n");
                return None;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                console.raw("\r\n");
                return None;
            }
            KeyCode::Backspace => {
                if buf.pop().is_some() {
                    console.raw("\x08 \x08");
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                console.raw("*");
            }
            _ => {}
        }
    }
}

/// `ConfigEditorHelpers.EditStringList`.
fn edit_string_list(console: &Console, label: &str, mut current: Vec<String>) -> Vec<String> {
    let title = format!("[bold]{label}[/]: [[{}]]", escape(&current.join(", ")));
    let choices: Vec<String> = ["Add item", "Remove item", "Clear all", "Back"]
        .map(String::from)
        .to_vec();
    let Some(i) = prompt::select(console, &title, &choices) else {
        return current;
    };
    match choices[i].as_str() {
        "Add item" => {
            if let Some(item) = prompt::text(console, "Tool name to add:")
                && !item.trim().is_empty()
                && !current.iter().any(|c| c.eq_ignore_ascii_case(&item))
            {
                current.push(item);
            }
        }
        "Remove item" if !current.is_empty() => {
            let escaped: Vec<String> = current.iter().map(|c| escape(c)).collect();
            if let Some(j) = prompt::select(console, "Select item to remove:", &escaped) {
                current.remove(j);
            }
        }
        "Clear all" => current.clear(),
        _ => {}
    }
    current
}

/// `ConfigEditorHelpers.PromptModelSelectionAsync`: pick from the server's models, or type one
/// when it has none. `None` = the server default.
fn prompt_model_selection(
    console: &Console,
    mut models: Vec<String>,
    title: &str,
    null_label: &str,
    manual_prompt: &str,
    current: Option<&str>,
) -> Option<String> {
    if !models.is_empty() {
        models.insert(0, null_label.to_string());
        let escaped: Vec<String> = models.iter().map(|m| escape(m)).collect();
        let i = prompt::select(console, title, &escaped)?;
        let selected = &models[i];
        return if selected == null_label || selected.starts_with('(') {
            None
        } else {
            Some(selected.clone())
        };
    }
    let value = text_with_default(console, manual_prompt, current.unwrap_or(""), false)?;
    (!value.trim().is_empty()).then_some(value)
}

/// `ConfigEditorHelpers.PromptModelPriorityList`: add, remove and reorder models in one loop.
/// Spectre's multi-selection for "+ Add model" is a single selection here (one model per
/// pick), as in `/model`.
fn prompt_model_priority_list(
    console: &Console,
    available: &[String],
    current: &[String],
) -> Vec<String> {
    let mut selected = current.to_vec();
    let has_server = !available.is_empty();
    if !has_server {
        console.markup_line("[yellow]Cannot connect to server. Enter model names manually.[/]");
    }
    loop {
        if selected.is_empty() {
            console.markup_line("\n[bold]Models:[/] [grey](none)[/]");
        } else {
            let display: Vec<String> = selected
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    format!(
                        "  [cyan]{}.[/] {}{}",
                        i + 1,
                        escape(m),
                        if i == 0 { " [green](primary)[/]" } else { "" }
                    )
                })
                .collect();
            console.markup_line(&format!("\n[bold]Models:[/]\n{}", display.join("\n")));
        }
        let unselected: Vec<String> = available
            .iter()
            .filter(|m| !selected.iter().any(|s| s.eq_ignore_ascii_case(m)))
            .cloned()
            .collect();
        let mut choices = vec!["Done".to_string()];
        if !has_server || !unselected.is_empty() {
            choices.push("+ Add model".into());
        }
        if !selected.is_empty() {
            choices.push("- Remove model".into());
        }
        for m in selected.iter().skip(1) {
            choices.push(format!("\u{2191} {}", escape(m)));
        }
        for m in selected.iter().take(selected.len().saturating_sub(1)) {
            choices.push(format!("\u{2193} {}", escape(m)));
        }
        let Some(i) = prompt::select(console, "[bold]Edit models:[/]", &choices) else {
            break;
        };
        let action = choices[i].clone();
        if action == "Done" {
            break;
        }
        if action == "+ Add model" {
            if has_server {
                let escaped: Vec<String> = unselected.iter().map(|m| escape(m)).collect();
                if let Some(j) = prompt::select(console, "[bold]Add models[/]", &escaped) {
                    selected.push(unselected[j].clone());
                }
            } else if let Some(name) = prompt::text(console, "Model name:")
                && !name.trim().is_empty()
                && !selected.iter().any(|s| s.eq_ignore_ascii_case(&name))
            {
                selected.push(name);
            }
        } else if action == "- Remove model" {
            let escaped: Vec<String> = selected.iter().map(|m| escape(m)).collect();
            if let Some(j) = prompt::select(console, "Remove which model?", &escaped) {
                selected.remove(j);
            }
        } else if let Some(rest) = action.strip_prefix("\u{2191} ") {
            let model = remove(rest);
            if let Some(idx) = selected.iter().position(|m| *m == model)
                && idx > 0
            {
                let m = selected.remove(idx);
                selected.insert(idx - 1, m);
            }
        } else if let Some(rest) = action.strip_prefix("\u{2193} ") {
            let model = remove(rest);
            if let Some(idx) = selected.iter().position(|m| *m == model)
                && idx + 1 < selected.len()
            {
                let m = selected.remove(idx);
                selected.insert(idx + 1, m);
            }
        }
    }
    selected
}

/// `LlmServerConfigSection.TestConnectionAsync`.
async fn test_connection(console: &Console, config: &OrcaConfig) {
    crate::model_discovery::invalidate_cache();
    let models = crate::model_discovery::available_models(config).await;
    if models.is_empty() {
        console.markup_line("[red]Could not connect to LLM server.[/]");
        return;
    }
    console.markup_line(&format!(
        "[green]Connected! Found {} model(s):[/]",
        models.len()
    ));
    for m in &models {
        console.markup_line(&format!("  [cyan]{}[/]", escape(m)));
    }
}

/// `ImageGenConfigSection.FetchImageGenModelsAsync`: the image server's /v1/models ids, or
/// none when it can't be reached.
async fn fetch_image_gen_models(config: &OrcaConfig) -> Vec<String> {
    let base = config.image_gen.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return Vec::new();
    };
    let response = client
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", config.image_gen.api_key),
        )
        .send()
        .await;
    let Ok(response) = response.and_then(|r| r.error_for_status()) else {
        return Vec::new();
    };
    let Ok(doc) = response.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    doc.get("data")
        .and_then(|d| d.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("id").and_then(|id| id.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_table_lists_every_setting_with_profile_notes_hooks_and_the_file() {
        let (console, out) = Console::buffer();
        let mut config = OrcaConfig::default();
        config
            .hooks
            .pre_tool_hooks
            .insert("bash".into(), "echo hi".into());
        config.lm_studio.api_key = "sk-test-0123456789abcdefXYZ".into();
        let overlay = ProfileOverlay::new("fast", vec!["lmStudio.temperature".into()]);
        render_config_table_with_overlay(
            &console,
            &config,
            Path::new("/home/u/.openorca"),
            Some(&overlay),
        );
        let text = Console::buffer_text(&out);
        assert!(text.contains("Current Configuration"));
        for s in schema::all() {
            assert!(text.contains(s.label), "{}", s.key);
        }
        assert!(text.contains("0.7 (profile fast)"), "{text}");
        assert!(text.contains("sk-…XYZ"));
        assert!(!text.contains("0123456789"));
        assert!(text.contains("Pre-tool hooks"));
        assert!(text.contains("bash"));
        assert!(text.contains("/home/u/.openorca/config.json"));
    }

    #[test]
    fn save_keeps_the_profiles_values_out_of_config_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"lmStudio":{"temperature":0.3}}"#,
        )
        .unwrap();
        let mut config = OrcaConfig::default();
        config.lm_studio.temperature = 0.9; // the profile's
        config.session.max_sessions = 9; // edited in /config
        let overlay = ProfileOverlay::new("fast", vec!["lmStudio.temperature".into()]);
        save(&config, dir.path(), Some(&overlay)).unwrap();
        let mut m = ConfigManager::new(Some(dir.path()));
        m.load().unwrap();
        assert_eq!(m.config.lm_studio.temperature, 0.3);
        assert_eq!(m.config.session.max_sessions, 9);
    }
}
