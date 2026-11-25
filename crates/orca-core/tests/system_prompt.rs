//! Ported from `SystemPromptBuilderTests.cs` (OpenOrca profile), plus Rust-only checks of the
//! peer/orchestrator rewrites and the platform string.

// The ported tests set config fields one by one, as the C# tests do.
#![allow(clippy::field_reassign_with_default)]

use std::path::PathBuf;

use orca_core::agent::prompt_constants::{
    CONTEXT7_SYSTEM_PROMPT_ADDITION, NEXT_PROMPT_INSTRUCTION, PLAN_MODE_SYSTEM_PROMPT_ADDITION,
    platform_lines,
};
use orca_core::agent::system_prompt::{
    PEER_SECTION, PromptTool, SystemPromptBuilder, apply_orchestrator_mode, apply_peer_mode,
    orchestrator_section, os_version_string, unix_os_version,
};
use orca_core::config::OrcaConfig;
use orca_core::config::memory_manager::MemoryManager;
use orca_core::config::prompt_manager::PromptManager;
use serde_json::json;

/// Isolated prompts dir: never reads the developer's real `~/.openorca/prompts`.
struct Fixture {
    prompts: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            prompts: tempfile::tempdir().unwrap(),
        }
    }

    fn builder(&self, memory: Option<MemoryManager>) -> SystemPromptBuilder {
        SystemPromptBuilder::new(PromptManager::with_dir(self.prompts.path()), memory, cwd())
    }
}

/// C# uses `Directory.GetCurrentDirectory()`.
fn cwd() -> PathBuf {
    std::env::current_dir().unwrap()
}

fn tool(name: &str, description: &str) -> PromptTool {
    PromptTool::new(
        name,
        description,
        json!({"type": "object", "properties": {}}),
    )
}

fn some_tools() -> Vec<PromptTool> {
    vec![tool("bash", "Run commands")]
}

fn native_config(model: &str) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    config.lm_studio.models = vec![model.to_string()];
    config
}

fn prompt(config: &OrcaConfig, tools: Option<&[PromptTool]>, plan_mode: bool) -> String {
    Fixture::new()
        .builder(None)
        .get_system_prompt(config, tools, plan_mode)
}

// --- Tool-calling protocol follows the effective mode ---

#[test]
fn native_mode_prompt_does_not_teach_text_tool_calls() {
    let f = Fixture::new();
    let builder = f.builder(None);
    let p = builder.get_system_prompt(&native_config("qwen3-coder"), Some(&some_tools()), false);

    assert!(p.contains("function calling API"));
    assert!(!p.contains("<tool_call>"));
    assert!(p.contains("UNDERSTAND FIRST, THEN BUILD"));
    assert_eq!(builder.get_prompt_tool_mode(Some(&p)), Some(true));
}

#[test]
fn text_mode_prompt_teaches_tool_call_tags() {
    let f = Fixture::new();
    let builder = f.builder(None);
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = false;
    let p = builder.get_system_prompt(&config, Some(&some_tools()), false);

    assert!(p.contains("<tool_call>"));
    assert!(p.contains("UNDERSTAND FIRST, THEN BUILD"));
    assert_eq!(builder.get_prompt_tool_mode(Some(&p)), Some(false));
}

#[test]
fn native_config_but_model_proved_unsupported_uses_text_protocol() {
    let f = Fixture::new();
    let builder = f.builder(None);
    builder.remember_native_tools_support("qwen3-coder", false);
    let p = builder.get_system_prompt(&native_config("qwen3-coder"), Some(&some_tools()), false);
    assert!(p.contains("<tool_call>"));
}

#[test]
fn native_config_no_tools_resolves_to_text() {
    let f = Fixture::new();
    assert!(
        !f.builder(None)
            .resolve_native_tool_calling(&native_config("qwen3-coder"), None)
    );
}

#[test]
fn get_prompt_tool_mode_unknown_prompt_returns_null() {
    let f = Fixture::new();
    assert_eq!(
        f.builder(None)
            .get_prompt_tool_mode(Some("You are a custom prompt.")),
        None
    );
}

#[test]
fn iteration_limit_is_injected_from_config() {
    let mut config = OrcaConfig::default();
    config.agent.main_loop_max_iterations = 37;
    let p = prompt(&config, None, false);

    assert!(p.contains("up to 37 iterations"));
    assert!(!p.contains("25 iterations"));
    assert!(!p.contains("{{MAX_ITERATIONS}}"));
}

#[test]
fn prompt_never_advises_skeleton_first() {
    for native in [true, false] {
        let f = Fixture::new();
        let p = f.builder(None).get_system_prompt_for_mode(
            &native_config("gpt-oss-20b"),
            Some(&some_tools()),
            false,
            native,
        );
        assert!(!p.to_lowercase().contains("skeleton first"));
        assert!(!p.contains("~200 lines"));
    }
}

// --- Basic prompt generation ---

#[test]
fn get_system_prompt_async_returns_non_empty_prompt() {
    assert!(
        !prompt(&OrcaConfig::default(), None, false)
            .trim()
            .is_empty()
    );
}

#[test]
fn get_system_prompt_async_contains_cwd() {
    let p = prompt(&OrcaConfig::default(), None, false);
    assert!(p.contains(&cwd().display().to_string()));
}

// --- Tool list ---

#[test]
fn get_system_prompt_async_no_tools_shows_no_tools_message() {
    assert!(prompt(&OrcaConfig::default(), None, false).contains("no tools available"));
}

#[test]
fn get_system_prompt_async_with_tools_lists_tool_names() {
    let tools = [tool("my_tool", "A test tool")];
    let p = prompt(&OrcaConfig::default(), Some(&tools), false);
    assert!(p.contains("my_tool"));
    assert!(p.contains("A test tool"));
}

// --- Plan mode ---

#[test]
fn get_system_prompt_async_plan_mode_appends_plan_instructions() {
    let p = prompt(&OrcaConfig::default(), None, true);
    assert!(p.contains(&platform_lines(PLAN_MODE_SYSTEM_PROMPT_ADDITION)));
}

#[test]
fn get_system_prompt_async_no_plan_mode_does_not_append_plan_instructions() {
    let p = prompt(&OrcaConfig::default(), None, false);
    assert!(!p.contains(&platform_lines(PLAN_MODE_SYSTEM_PROMPT_ADDITION)));
}

// --- Thinking budget ---

#[test]
fn get_system_prompt_async_thinking_budget_appends_advisory() {
    let mut config = OrcaConfig::default();
    config.thinking.budget_tokens = 500;
    let p = prompt(&config, None, false);
    assert!(p.contains("Thinking budget"));
    assert!(p.contains("500"));
}

#[test]
fn get_system_prompt_async_no_thinking_budget_no_advisory() {
    let mut config = OrcaConfig::default();
    config.thinking.budget_tokens = 0;
    assert!(!prompt(&config, None, false).contains("Thinking budget"));
}

// --- Sandbox mode ---

#[test]
fn get_system_prompt_async_sandbox_mode_appends_restriction() {
    let mut config = OrcaConfig::default();
    config.sandbox_mode = true;
    assert!(prompt(&config, None, false).contains("SANDBOX MODE"));
}

#[test]
fn get_system_prompt_async_no_sandbox_mode_no_restriction() {
    assert!(!prompt(&OrcaConfig::default(), None, false).contains("SANDBOX MODE"));
}

// --- Directory restriction ---

#[test]
fn get_system_prompt_async_allowed_directory_appends_restriction() {
    let mut config = OrcaConfig::default();
    config.allowed_directory = Some("/restricted/path".into());
    let p = prompt(&config, None, false);
    assert!(p.contains("DIRECTORY RESTRICTION"));
    assert!(p.contains("/restricted/path"));
}

#[test]
fn get_system_prompt_async_no_allowed_directory_no_restriction() {
    assert!(!prompt(&OrcaConfig::default(), None, false).contains("DIRECTORY RESTRICTION"));
}

// --- Next prompt instruction ---

#[test]
fn get_system_prompt_async_always_includes_next_prompt_instruction() {
    assert!(prompt(&OrcaConfig::default(), None, false).contains(NEXT_PROMPT_INSTRUCTION));
}

// --- Context7 detection ---

#[test]
fn get_system_prompt_async_with_context7_tool_appends_context7_instructions() {
    let tools = [tool("mcp_resolve-library-id", "Resolve a library ID")];
    let p = prompt(&OrcaConfig::default(), Some(&tools), false);
    assert!(p.contains(&platform_lines(CONTEXT7_SYSTEM_PROMPT_ADDITION)));
}

#[test]
fn get_system_prompt_async_without_context7_tool_no_context7_instructions() {
    let tools = [tool("bash", "Run commands")];
    let p = prompt(&OrcaConfig::default(), Some(&tools), false);
    assert!(!p.contains(&platform_lines(CONTEXT7_SYSTEM_PROMPT_ADDITION)));
}

// --- Combined modes ---

#[test]
fn get_system_prompt_async_all_modes_active_includes_all() {
    let mut config = OrcaConfig::default();
    config.sandbox_mode = true;
    config.allowed_directory = Some("/test".into());
    config.thinking.budget_tokens = 1000;
    let p = prompt(&config, None, true);
    assert!(p.contains("SANDBOX MODE"));
    assert!(p.contains("DIRECTORY RESTRICTION"));
    assert!(p.contains("Thinking budget"));
    assert!(p.contains(&platform_lines(PLAN_MODE_SYSTEM_PROMPT_ADDITION)));
}

// --- Auto memory is opt-in ---

fn memory_with(config: &OrcaConfig, dir: &tempfile::TempDir) -> MemoryManager {
    std::fs::write(
        dir.path().join("20260101-abcdef.md"),
        "- LEARNED: always use tabs",
    )
    .unwrap();
    MemoryManager::with_global_dir(config, dir.path(), true)
}

#[test]
fn auto_memory_not_in_prompt_by_default() {
    let mem = tempfile::tempdir().unwrap();
    let config = OrcaConfig::default();
    let p = Fixture::new()
        .builder(Some(memory_with(&config, &mem)))
        .get_system_prompt(&config, Some(&some_tools()), false);

    assert!(!config.memory.auto_memory_enabled);
    assert!(!p.contains("LEARNED"));
}

#[test]
fn auto_memory_include_in_prompt_appends_memory() {
    let mem = tempfile::tempdir().unwrap();
    let mut config = OrcaConfig::default();
    config.memory.include_in_prompt = true;
    let p = Fixture::new()
        .builder(Some(memory_with(&config, &mem)))
        .get_system_prompt(&config, Some(&some_tools()), false);

    assert!(p.contains("PROJECT MEMORY (auto-learned):\n- LEARNED: always use tabs"));
}

// AutoMemory_PiProfile_GoesInProjectContextOnly: not ported, the Pi profile's prompt
// (BuildPiPromptAsync) comes with the Pi toolset in phase 3.

// --- Rust-only ---

#[test]
fn orchestrator_mode_needs_a_spawn_tool_and_replaces_delegation() {
    let mut config = OrcaConfig::default();
    config.agent.lead_mode = "Orchestrator".into();
    let without = prompt(&config, Some(&some_tools()), false);
    assert!(without.contains("AGENT DELEGATION"));
    assert!(!without.contains("ORCHESTRATOR MODE"));

    let tools = [tool("spawn_agents", "Spawn agents"), tool("bash", "Run")];
    let p = prompt(&config, Some(&tools), false);
    assert!(!p.contains("AGENT DELEGATION"));
    assert!(!p.contains("Do trivial things yourself"));
    assert!(p.contains("4. Have agents make every change (see ORCHESTRATOR MODE)"));
    assert!(p.contains(&orchestrator_section(true)));
    assert!(p.ends_with(NEXT_PROMPT_INSTRUCTION));
}

#[test]
fn peer_mode_swaps_delegation_for_helper_agents() {
    let mut config = OrcaConfig::default();
    config.agent.peer_mode = true;
    let tools = [tool("spawn_agents", "Spawn agents")];
    let p = prompt(&config, Some(&tools), false);
    assert!(!p.contains("AGENT DELEGATION"));
    assert!(p.contains(&format!("{PEER_SECTION}\n\nPROGRESS:")));
    assert!(p.contains("it moves your own bar in the footer"));
    assert!(p.contains("(see HELPER AGENTS)"));

    let no_spawn = prompt(&config, Some(&some_tools()), false);
    assert!(!no_spawn.contains("HELPER AGENTS \u{2014}"));
    assert!(!no_spawn.contains("AGENT DELEGATION"));
}

#[test]
fn peer_and_orchestrator_rewrites_without_sections() {
    assert_eq!(
        apply_peer_mode("plain", true),
        format!("plain\n\n{PEER_SECTION}")
    );
    assert_eq!(apply_peer_mode("plain", false), "plain");
    assert_eq!(
        apply_orchestrator_mode("  3. Execute with tools (write_file, ...)", false),
        format!(
            "  4. Have agents make every change (see ORCHESTRATOR MODE)\n\n{}",
            orchestrator_section(false)
        )
    );
}

#[test]
fn lead_mode_tool_adds_lead_mode_section() {
    let tools = [tool("lead_mode", "Choose how to work")];
    let p = prompt(&OrcaConfig::default(), Some(&tools), false);
    assert!(p.contains("\n\nLEAD MODE (auto):\nYou lead a team of subagents."));
}

#[test]
fn project_instructions_are_appended() {
    let proj = tempfile::tempdir().unwrap();
    std::fs::create_dir(proj.path().join(".git")).unwrap();
    std::fs::write(proj.path().join("AGENTS.md"), "Use tabs.\r\n").unwrap();
    let f = Fixture::new();
    let builder =
        SystemPromptBuilder::new(PromptManager::with_dir(f.prompts.path()), None, proj.path());
    let p = builder.get_system_prompt(&OrcaConfig::default(), None, false);
    assert!(p.contains(
        "\n\nPROJECT INSTRUCTIONS:\n\n--- AGENTS.md ---\nUse tabs.\n\nSUGGESTED NEXT PROMPT:"
    ));
}

#[test]
fn platform_matches_dotnet_os_version() {
    let v = os_version_string();
    if cfg!(windows) {
        assert!(v.starts_with("Microsoft Windows NT 10.0."), "{v}");
        assert!(v.ends_with(".0"), "{v}");
    } else {
        assert!(v.starts_with("Unix "), "{v}");
    }
    assert_eq!(unix_os_version("6.8.0-45-generic"), "Unix 6.8.0.45");
    assert_eq!(unix_os_version("24.1.0"), "Unix 24.1.0.0");
    assert_eq!(unix_os_version(""), "Unix 0.0.0.0");
}
