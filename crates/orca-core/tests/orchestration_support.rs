//! Ported from the pure parts of `tests/OpenOrca.Core.Tests/Orchestration`:
//! `AgentPromptLoaderTests.cs`, `AgentTypeRegistryTests.cs`, `BriefFilesTests.cs`,
//! `ReadOnlyCommandTests.cs`, `CustomAgentLoaderTests.cs`, `WorkflowTests.cs` (parse, loader and
//! template tests), and from `AgentSupportTests.cs`, `AgentFooterTests.cs`,
//! `PeerReplyGuardTests.cs` and `AgentUserMessageTests.cs` the tests that need no orchestrator.
//! Tests that run agents through `AgentOrchestrator` are ported with the orchestrator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use orca_core::orchestration::activity;
use orca_core::orchestration::brief_files;
use orca_core::orchestration::custom_agents::CustomAgentLoader;
use orca_core::orchestration::labels;
use orca_core::orchestration::palette;
use orca_core::orchestration::prompt_loader::AgentPromptLoader;
use orca_core::orchestration::read_only_command;
use orca_core::orchestration::repetition::{self, RepetitionDetector};
use orca_core::orchestration::transcript::{AgentTranscript, AgentTranscriptKind};
use orca_core::orchestration::types::{AgentTypeDefinition, AgentTypeRegistry};
use orca_core::orchestration::workflow::{WorkflowDefinition, WorkflowLoader, substitute_template};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ── AgentPromptLoaderTests ──────────────────────────────────────────────────

fn load(type_name: &str, task: &str, cwd: &str, platform: &str) -> Option<String> {
    let def = AgentTypeRegistry::new().resolve(type_name).unwrap();
    AgentPromptLoader::load_prompt(&def, task, cwd, platform)
}

#[test]
fn load_prompt_all_built_in_types_return_non_null() {
    for type_name in ["explore", "plan", "bash", "review", "general"] {
        let prompt = load(type_name, "test task", "/tmp", "Linux");
        assert!(prompt.is_some_and(|p| !p.is_empty()), "{type_name}");
    }
}

#[test]
fn strip_frontmatter_crlf_line_endings_stripped() {
    // Regression: Windows-authored agent files leaked YAML frontmatter into the prompt.
    let content = "---\r\nname: winagent\r\ndescription: test\r\n---\r\nYou are an agent.\r\n";
    let stripped = AgentPromptLoader::strip_frontmatter(content);
    assert!(!stripped.contains("name: winagent"));
    assert!(stripped.contains("You are an agent."));
}

#[test]
fn strip_frontmatter_lf_line_endings_stripped() {
    let content = "---\nname: agent\n---\nBody here.\n";
    let stripped = AgentPromptLoader::strip_frontmatter(content);
    assert!(!stripped.contains("name: agent"));
    assert!(stripped.contains("Body here."));
}

#[test]
fn strip_frontmatter_no_frontmatter_unchanged() {
    let content = "Just a prompt body with --- a divider later.\n";
    assert_eq!(content, AgentPromptLoader::strip_frontmatter(content));
}

#[test]
fn load_prompt_substitutes_task_variable() {
    let prompt = load("explore", "find all controllers", "/tmp", "Linux").unwrap();
    assert!(prompt.contains("find all controllers"));
    assert!(!prompt.contains("{{TASK}}"));
}

#[test]
fn load_prompt_substitutes_cwd_variable() {
    let prompt = load("explore", "test", "/home/user/project", "Linux").unwrap();
    assert!(prompt.contains("/home/user/project"));
    assert!(!prompt.contains("{{CWD}}"));
}

#[test]
fn load_prompt_substitutes_platform_variable() {
    let prompt = load("explore", "test", "/tmp", "Windows 11").unwrap();
    assert!(prompt.contains("Windows 11"));
    assert!(!prompt.contains("{{PLATFORM}}"));
}

#[test]
fn load_prompt_missing_resource_returns_null() {
    let fake = AgentTypeDefinition::new("fake", "does not exist", None, "nonexistent.md");
    assert_eq!(
        AgentPromptLoader::load_prompt(&fake, "test", "/tmp", "Linux"),
        None
    );
}

#[test]
fn load_prompt_explore_prompt_contains_read_only_constraints() {
    let prompt = load("explore", "test task", "/tmp", "Linux").unwrap();
    assert!(prompt.contains("read_file"));
    assert!(prompt.contains("glob"));
    assert!(prompt.contains("grep"));
}

#[test]
fn load_prompt_bash_prompt_contains_bash_tool() {
    let prompt = load("bash", "run tests", "/tmp", "Linux").unwrap();
    assert!(prompt.contains("bash"));
    assert!(prompt.to_lowercase().contains("command execution"));
}

#[test]
fn load_prompt_from_file_strips_frontmatter_and_leading_space() {
    // Not in the C# tests: the file-path branch of LoadPrompt (custom agents).
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("custom.md");
    std::fs::write(
        &file,
        "---\r\nname: c\r\n---\r\n\r\n  Do {{TASK}} in {{CWD}}.",
    )
    .unwrap();
    let def = AgentTypeDefinition::new("c", "c", None, file.to_string_lossy());
    assert_eq!(
        AgentPromptLoader::load_prompt(&def, "x", "/w", "os").as_deref(),
        Some("Do x in /w.")
    );
}

// ── AgentTypeRegistryTests ──────────────────────────────────────────────────

#[test]
fn resolve_known_type_returns_definition() {
    let result = AgentTypeRegistry::new().resolve("explore").unwrap();
    assert_eq!("explore", result.name);
}

#[test]
fn resolve_is_case_insensitive() {
    let result = AgentTypeRegistry::new().resolve("EXPLORE").unwrap();
    assert_eq!("explore", result.name);
}

#[test]
fn resolve_unknown_type_returns_null() {
    assert_eq!(AgentTypeRegistry::new().resolve("nonexistent"), None);
}

#[test]
fn get_default_returns_general() {
    let result = AgentTypeRegistry::new().get_default();
    assert_eq!("general", result.name);
    assert_eq!(result.allowed_tools, None);
}

#[test]
fn get_all_returns_at_least_five_built_in_types() {
    assert_eq!(5, AgentTypeRegistry::new().get_all().len());
}

#[test]
fn resolve_all_built_in_types_exist() {
    for type_name in ["explore", "plan", "bash", "review", "general"] {
        let result = AgentTypeRegistry::new().resolve(type_name).unwrap();
        assert_eq!(type_name, result.name);
        assert!(!result.description.is_empty());
        assert!(!result.prompt_resource_name.is_empty());
    }
}

fn tools_of(type_name: &str) -> Option<Vec<String>> {
    AgentTypeRegistry::new()
        .resolve(type_name)
        .unwrap()
        .allowed_tools
}

#[test]
fn explore_type_has_read_only_tools() {
    let tools = tools_of("explore").unwrap();
    assert!(tools.contains(&"read_file".to_string()));
    assert!(tools.contains(&"glob".to_string()));
    assert!(tools.contains(&"grep".to_string()));
    assert!(!tools.contains(&"bash".to_string()));
    assert!(!tools.contains(&"write_file".to_string()));
}

#[test]
fn bash_type_has_execution_tools() {
    let tools = tools_of("bash").unwrap();
    assert!(tools.contains(&"bash".to_string()));
    assert!(!tools.contains(&"write_file".to_string()));
    assert!(!tools.contains(&"glob".to_string()));
}

#[test]
fn general_type_has_unrestricted_tools() {
    assert_eq!(tools_of("general"), None);
}

#[test]
fn review_type_has_git_and_read_tools() {
    let tools = tools_of("review").unwrap();
    assert!(tools.contains(&"git_diff".to_string()));
    assert!(tools.contains(&"git_log".to_string()));
    assert!(tools.contains(&"read_file".to_string()));
    assert!(!tools.contains(&"bash".to_string()));
}

#[test]
fn plan_type_has_research_tools() {
    let tools = tools_of("plan").unwrap();
    assert!(tools.contains(&"web_search".to_string()));
    assert!(tools.contains(&"web_fetch".to_string()));
    assert!(tools.contains(&"read_file".to_string()));
    assert!(!tools.contains(&"bash".to_string()));
}

#[test]
fn get_all_keeps_built_in_order_and_custom_overrides_in_place() {
    // Not in the C# tests: Dictionary order of GetAll / GetAllNames.
    let mut registry = AgentTypeRegistry::new();
    registry.register(AgentTypeDefinition::new("extra", "x", None, "x.md"));
    registry.register(AgentTypeDefinition::new(
        "PLAN",
        "custom plan",
        None,
        "p.md",
    ));
    let all: Vec<String> = registry
        .get_all()
        .iter()
        .map(|d| d.description.clone())
        .collect();
    assert_eq!(all[1], "custom plan");
    assert_eq!(all[5], "x");
    assert_eq!(
        registry.get_all_names(),
        strings(&["explore", "plan", "bash", "review", "general", "extra"])
    );
    registry.clear_custom();
    assert_eq!(registry.get_all().len(), 5);
}

// ── BriefFilesTests ─────────────────────────────────────────────────────────

struct BriefDir {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl BriefDir {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        Self { _tmp: tmp, dir }
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

#[test]
fn named_files_are_appended_in_order_of_mention_only_when_they_exist() {
    let d = BriefDir::new();
    d.write("spec.md", "# Spec\nrule 1");
    d.write("src/board.py", "def board(): pass\n");
    let brief = "Build `src/board.py` per spec.md (section 2). Write tests/test_board.py. See ./spec.md again.";

    let text = brief_files::append(brief, &d.dir);

    assert!(text.starts_with(brief));
    let board = text
        .find("<file path=\"src/board.py\">\ndef board(): pass\n</file>")
        .unwrap_or(0);
    let spec = text
        .find("<file path=\"spec.md\">\n# Spec\nrule 1\n</file>")
        .unwrap_or(0);
    assert!(board > 0 && spec > board, "{text}");
    assert!(!text.contains("test_board.py\"")); // not written yet
    assert_eq!(1, text.matches("path=\"spec.md\"").count());
}

#[test]
fn a_brief_without_files_is_unchanged() {
    let d = BriefDir::new();
    assert_eq!(
        "Refactor the state handling. Version 1.2 is fine.",
        brief_files::append("Refactor the state handling. Version 1.2 is fine.", &d.dir)
    );
}

#[test]
fn files_outside_the_working_directory_are_not_read() {
    let d = BriefDir::new();
    let outside = d.dir.parent().unwrap().join("secret-1234.txt");
    std::fs::write(&outside, "secret").unwrap();
    let brief = format!("Read ../secret-1234.txt and {}", outside.display());
    assert_eq!(brief, brief_files::append(&brief, &d.dir));
}

#[test]
fn caps_per_file_total_and_file_count() {
    let d = BriefDir::new();
    d.write("big.md", &"a".repeat(20_000));
    d.write("b.txt", &"b".repeat(11_000));
    d.write("c.txt", &"c".repeat(3_000));
    let text = brief_files::append("big.md b.txt c.txt", &d.dir);
    assert!(text.contains(&format!(
        "<file path=\"big.md\" shown=\"first {} of 20000 chars: read the rest\">",
        brief_files::MAX_CHARS_PER_FILE
    )));
    // 12,000 + 11,000 leaves 1,000 for c.txt, which is cut to that.
    assert!(
        text.contains("<file path=\"c.txt\" shown=\"first 1000 of 3000 chars: read the rest\">")
    );
    let len = text.encode_utf16().count();
    assert!(
        (brief_files::MAX_TOTAL_CHARS..=brief_files::MAX_TOTAL_CHARS + 600).contains(&len),
        "{len}"
    );

    for i in 0..8 {
        d.write(&format!("f{i}.py"), "x");
    }
    let brief = (0..8)
        .map(|i| format!("f{i}.py"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        brief_files::MAX_FILES,
        brief_files::find(&brief, &d.dir).len()
    );
}

#[test]
fn preload_brief_files_defaults_on() {
    assert!(orca_core::config::model::AgentConfig::default().preload_brief_files);
}

// ── ReadOnlyCommandTests ────────────────────────────────────────────────────

#[test]
fn reading_commands_run() {
    for command in [
        r#"dir /s /b "C:\Program Files (x86)\Steam\steamapps""#,
        "ls -la",
        "cat SPEC.md | grep -n Steam | head -20",
        "git -C repo log --oneline -5",
        "git status && git diff --stat",
        r"reg query HKCU\Software\Valve\Steam /v SteamPath",
        r#"powershell -Command "Get-ChildItem 'C:\Games' | Select-Object Name, Length""#,
        r"cmd /c dir C:\ProgramData\Epic",
        "find . -name '*.acf' -maxdepth 3",
        "ls missing 2>/dev/null; echo done",
        "type notes.txt 2>nul",
        "tree /f src",
        "dotnet --list-sdks",
        "pwd; ls -la; python --version 2>&1; py --version 2>&1; python3 --version 2>&1",
        "head -c 400 appmanifest_440.acf",
        // Quoted text is data: pipes and angle brackets inside quotes are not shell syntax.
        "ls ~/.nuget/packages 2>/dev/null | grep -i -E 'xunit|monogame|stb'",
        "grep -i -h -o -e '<Identity[^/]*/>' -e 'DefaultDisplayName=\"[^\"]*\"' C:/XboxGames/*/Content/MicrosoftGame.config",
        "grep -c \"a|b\" notes.txt; echo '$(not run)'",
        "unzip -l templates.nupkg | grep -i spritefont",
        "ls ~/.nuget/packages; powershell -Command \"Get-ChildItem 'C:\\Program Files (x86)\\Steam' | Select-Object Name\"",
    ] {
        assert_eq!(
            read_only_command::command_refusal(command),
            None,
            "{command}"
        );
    }
}

#[test]
fn writing_commands_are_refused() {
    for (command, reason) in [
        ("echo hi > a.txt", "redirection"),
        ("cat a >> b", "redirection"),
        ("rm -rf build", "'rm'"),
        ("mkdir plans", "'mkdir'"),
        ("dotnet build", "dotnet only"),
        ("dotnet new console", "dotnet only"),
        ("sed -i s/a/b/ file.cs", "sed -i"),
        ("find . -name '*.tmp' -delete", "-delete"),
        ("git checkout main", "git only"),
        ("git branch -D old", "only to list"),
        ("ls && rm x", "'rm'"),
        ("cat $(ls)", "substitution"),
        ("powershell -Command \"Remove-Item x\"", "not read-only"),
        (
            "powershell -Command \"Get-Content a | Out-File b\"",
            "not read-only",
        ),
        ("pwsh -c \"Start-Process notepad\"", "not read-only"),
        ("cmd /c del x", "'del'"),
        ("python script.py", "python only"),
        ("py -c \"open('x','w')\"", "py only"),
        ("curl https://example.com -o x", "'curl'"),
        ("grep 'a|b' x > out.txt", "redirection"),
        ("echo \"$(rm x)\"", "substitution"),
        ("grep 'x' a | rm y", "'rm'"),
        ("unzip pkg.zip", "unzip only"),
        // The fablehaiku planner's refusal: escaped quotes inside powershell -Command "..." are
        // ambiguous (bash escape vs cmd path + closing quote), so they are refused with advice.
        (
            "ls ~/.nuget/packages; powershell -Command \"Get-ChildItem \\\"C:\\Program Files\\\" | Select Name\"",
            "use single quotes",
        ),
        ("dir \"C:\\games\\\" & del x", "escaped double quotes"),
    ] {
        let refusal = read_only_command::command_refusal(command)
            .unwrap_or_else(|| panic!("not refused: {command}"));
        assert!(refusal.contains("read-only"), "{refusal}");
        assert!(refusal.contains(reason), "{command}: {refusal}");
    }
}

#[test]
fn refusal_messages_are_exact() {
    // Not in the C# tests: the whole text, including the nested cmd /c quirk.
    assert_eq!(
        read_only_command::command_refusal("rm x").unwrap(),
        "Error: you are read-only (planning): 'rm' is not a read-only command. Refused: rm x. Use commands that only look (dir, ls, cat, type, grep, find, tree, git log, reg query, Get-ChildItem, Get-Content)."
    );
    assert!(
        read_only_command::command_refusal("cmd /c del x")
            .unwrap()
            .starts_with("Error: you are read-only (planning): Error: you are read-only (planning): 'del' is not a read-only command. Refused: del x.")
    );
    assert!(
        read_only_command::command_refusal("git checkout main")
            .unwrap()
            .contains("git only with: log, status, show, diff, ls-files, branch,")
    );
    assert_eq!(
        read_only_command::command_refusal("   ").as_deref(),
        Some("Error: empty command.")
    );
}

#[test]
fn allowed_summary_lists_what_the_check_allows() {
    let summary = read_only_command::allowed_summary();
    assert!(summary.contains("git "));
    assert!(summary.contains("log"));
    assert!(summary.contains("reg query"));
    assert!(summary.contains("dotnet --info|--list-runtimes|--list-sdks|--version"));
    assert!(summary.contains("2>/dev/null"));
    assert!(summary.contains("shell variables"));
    // Every program the summary names as a plain bash program really runs.
    let lines: Vec<&str> = summary
        .split('\n')
        .filter(|l| l.contains("bash programs:"))
        .collect();
    assert_eq!(lines.len(), 1);
    let line = lines[0];
    let programs: Vec<&str> = line[line.find(':').unwrap() + 1..line.find(';').unwrap()]
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    assert!(programs.len() > 30);
    for p in programs {
        let command = match p {
            "set" | "attrib" => p.to_string(),
            "certutil" => "certutil -hashfile x".to_string(),
            _ => format!("{p} x"),
        };
        assert_eq!(
            read_only_command::command_refusal(&command),
            None,
            "{command}"
        );
    }
    assert_eq!(summary.contains("\r\n"), cfg!(windows));
}

#[test]
fn tools_only_look_and_list() {
    use read_only_command::refusal;
    assert_eq!(refusal("read", r#"{"path": "SPEC.md"}"#), None);
    assert_eq!(refusal("grep", r#"{"pattern": "x"}"#), None);
    assert_eq!(refusal("bash", r#"{"command": "dir"}"#), None);
    assert!(
        refusal("write", r#"{"path": "a", "content": "b"}"#)
            .unwrap()
            .contains("'write' is not available")
    );
    assert!(
        refusal("edit", "{}")
            .unwrap()
            .contains("'edit' is not available")
    );
    assert!(
        refusal("job", "{}")
            .unwrap()
            .contains("'job' is not available")
    );
    assert!(
        refusal("subagent", "{}")
            .unwrap()
            .contains("'subagent' is not available")
    );
    assert!(
        refusal("bash", r#"{"command": "dir", "background": true}"#)
            .unwrap()
            .contains("no background jobs")
    );
    assert!(
        refusal("bash", r#"{"command": "echo x > PLAN.md"}"#)
            .unwrap()
            .contains("redirection")
    );
}

// ── CustomAgentLoaderTests ──────────────────────────────────────────────────

fn parse(content: &str, path: &str) -> Option<AgentTypeDefinition> {
    CustomAgentLoader::parse_agent_content(content, path, None).0
}

#[test]
fn parse_agent_content_valid_frontmatter_returns_definition() {
    let content = "---\nname: security\ndescription: Security analysis specialist\ntools: [read_file, grep, think]\n---\n\nYou are a security analysis specialist.\n## Task\n{{TASK}}";
    let def = parse(content, "/fake/security.md").unwrap();
    assert_eq!("security", def.name);
    assert_eq!("Security analysis specialist", def.description);
    assert_eq!(
        def.allowed_tools,
        Some(strings(&["read_file", "grep", "think"]))
    );
    assert_eq!(def.prompt_resource_name, "/fake/security.md");
}

#[test]
fn parse_agent_content_optional_override_keys_parsed() {
    let content = "---\nname: heavy\ndescription: Long-running agent on a bigger model\ntools: [read_file, grep]\nmodel: lmstudio/qwen2.5-coder-32b\nmax_iterations: 40\ntimeout: 900\n---\n\nBody text.";
    let def = parse(content, "/fake/heavy.md").unwrap();
    assert_eq!(def.model.as_deref(), Some("lmstudio/qwen2.5-coder-32b"));
    assert_eq!(def.max_iterations, Some(40));
    assert_eq!(def.timeout_seconds, Some(900));
}

#[test]
fn parse_agent_content_override_keys_absent_or_invalid_are_null() {
    let content = "---\nname: plain\ndescription: No overrides\ntools: [read_file]\nmax_iterations: zero\ntimeout: -5\n---\n\nBody text.";
    let def = parse(content, "/fake/plain.md").unwrap();
    assert_eq!(def.model, None);
    assert_eq!(def.max_iterations, None);
    assert_eq!(def.timeout_seconds, None);
}

#[test]
fn parse_agent_content_block_list_tools_parsed() {
    let content = "---\nname: reviewer\ndescription: Code reviewer\ntools:\n  - read_file\n  - grep\n---\n\nBody text.";
    let def = parse(content, "/fake/reviewer.md").unwrap();
    assert_eq!(def.allowed_tools, Some(strings(&["read_file", "grep"])));
}

#[test]
fn parse_agent_content_crlf_line_endings_parsed() {
    let content = "---\r\nname: winagent\r\ndescription: CRLF-authored agent\r\ntools: [read_file]\r\n---\r\n\r\nBody text.\r\n";
    let def = parse(content, "/fake/winagent.md").unwrap();
    assert_eq!("winagent", def.name);
    assert_eq!("CRLF-authored agent", def.description);
}

#[test]
fn parse_agent_content_crlf_block_list_and_model_parsed() {
    // Not in the C# tests: C# raw strings carry CRLF in a Windows checkout.
    let content = "---\r\nname: w\r\ndescription: d\r\ntools:\r\n  - \"read_file\"\r\n  - grep\r\nmodel: 'm1'\r\nmax_iterations: 7\r\n---\r\nBody";
    let def = parse(content, "/fake/w.md").unwrap();
    assert_eq!(def.allowed_tools, Some(strings(&["read_file", "grep"])));
    assert_eq!(def.model.as_deref(), Some("m1"));
    // `^max_iterations:\s*(\d+)\s*$`: the \r is matched by \s* before $.
    assert_eq!(def.max_iterations, Some(7));
}

#[test]
fn parse_agent_content_missing_tools_reports_error() {
    let content = "---\nname: broken\ndescription: No tools field\n---\n\nBody text.";
    let (def, error) = CustomAgentLoader::parse_agent_content(content, "/fake/broken.md", None);
    assert_eq!(def, None);
    assert!(error.unwrap().contains("tools"));
}

#[test]
fn parse_agent_content_all_tools_unknown_reports_error() {
    let content = "---\nname: ghost\ndescription: All tools invalid\ntools: [not_a_tool, also_fake]\n---\n\nBody text.";
    let registered = strings(&["read_file"]);
    let (def, error) =
        CustomAgentLoader::parse_agent_content(content, "/fake/ghost.md", Some(&registered));
    assert_eq!(def, None);
    assert_eq!(
        error.as_deref(),
        Some("no valid tools remain (unknown: not_a_tool, also_fake)")
    );
}

#[test]
fn parse_agent_content_some_tools_unknown_returns_definition_with_warning() {
    let content = "---\nname: partial\ndescription: One bad tool\ntools: [read_file, not_a_tool]\n---\n\nBody text.";
    let registered = strings(&["read_file"]);
    let (def, error) =
        CustomAgentLoader::parse_agent_content(content, "/fake/partial.md", Some(&registered));
    assert_eq!(def.unwrap().allowed_tools, Some(strings(&["read_file"])));
    assert_eq!(error.as_deref(), Some("ignoring unknown tools: not_a_tool"));
}

#[test]
fn parse_agent_content_missing_name_returns_null() {
    let content = "---\ndescription: No name agent\ntools: [read_file]\n---\n\nBody text.";
    assert_eq!(parse(content, "/fake/noname.md"), None);
}

#[test]
fn parse_agent_content_missing_tools_returns_null() {
    let content = "---\nname: bad\ndescription: No tools\n---\n\nBody text.";
    assert_eq!(parse(content, "/fake/bad.md"), None);
}

#[test]
fn parse_agent_content_no_frontmatter_returns_null() {
    assert_eq!(
        parse("Just plain markdown, no frontmatter.", "/fake/plain.md"),
        None
    );
}

#[test]
fn parse_agent_content_validates_tool_names_filters_invalid() {
    let content = "---\nname: restricted\ndescription: Only valid tools\ntools: [read_file, nonexistent_tool, grep]\n---\n\nBody.";
    let valid = strings(&["read_file", "grep", "bash"]);
    let def = CustomAgentLoader::parse_agent_content(content, "/fake/restricted.md", Some(&valid))
        .0
        .unwrap();
    assert_eq!(def.allowed_tools, Some(strings(&["read_file", "grep"])));
}

#[test]
fn parse_agent_content_all_tools_invalid_returns_null() {
    let content =
        "---\nname: empty\ndescription: No valid tools\ntools: [fake1, fake2]\n---\n\nBody.";
    let valid = strings(&["read_file", "grep"]);
    assert_eq!(
        CustomAgentLoader::parse_agent_content(content, "/fake/empty.md", Some(&valid)).0,
        None
    );
}

#[test]
fn substitute_template_vars_replaces_all_vars() {
    let template = "Task: {{TASK}}\nCWD: {{CWD}}\nPlatform: {{PLATFORM}}";
    let result =
        CustomAgentLoader::substitute_template_vars(template, "do stuff", "/home/user", "linux");
    assert_eq!("Task: do stuff\nCWD: /home/user\nPlatform: linux", result);
}

#[test]
fn discover_agents_project_dir_finds_agents() {
    // C# sets the process's current directory; here the directory is passed in.
    let tmp = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let orca_dir = tmp.path().join(".orca").join("agents");
    std::fs::create_dir_all(&orca_dir).unwrap();
    // A .git dir so find_project_root detects the project.
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    std::fs::write(
        orca_dir.join("test-agent.md"),
        "---\nname: test-agent\ndescription: Test agent\ntools: [read_file, think]\n---\n\nYou are a test agent.\n{{TASK}}",
    )
    .unwrap();
    std::fs::write(orca_dir.join("broken.md"), "no frontmatter").unwrap();

    let mut warnings = Vec::new();
    let agents = CustomAgentLoader::discover_agents(config.path(), tmp.path(), None, &mut |w| {
        warnings.push(w)
    });

    assert!(agents.contains_key("test-agent"));
    assert_eq!("Test agent", agents["test-agent"].description);
    assert_eq!(
        warnings,
        [
            "Skipping custom agent file 'broken.md': missing YAML frontmatter (--- delimited block at the top of the file)"
        ]
    );
}

#[test]
fn discover_agents_project_overrides_global() {
    // Not in the C# tests: global then project, same name (any case) overridden.
    let tmp = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let global = config.path().join("agents");
    let project = tmp.path().join(".orca").join("agents");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        global.join("a.md"),
        "---\nname: Sec\ndescription: global\ntools: [read_file]\n---\nx",
    )
    .unwrap();
    std::fs::write(
        project.join("a.md"),
        "---\nname: sec\ndescription: project\ntools: [read_file]\n---\nx",
    )
    .unwrap();
    let agents = CustomAgentLoader::discover_agents(config.path(), tmp.path(), None, &mut |_| {});
    assert_eq!(agents.len(), 1);
    assert_eq!(agents["Sec"].description, "project");
}

#[test]
fn register_custom_custom_overrides_built_in_on_resolve() {
    let mut registry = AgentTypeRegistry::new();
    let custom = AgentTypeDefinition::new(
        "custom-override-test",
        "Custom override test agent",
        Some(strings(&["read_file"])),
        "/fake/override.md",
    );
    let mut customs = IndexMap::new();
    customs.insert("custom-override-test".to_string(), custom);
    registry.register_custom(&customs);

    let resolved = registry.resolve("custom-override-test").unwrap();
    assert_eq!("Custom override test agent", resolved.description);
    // Custom types are listed in get_all.
    assert!(
        registry
            .get_all()
            .iter()
            .any(|a| a.name == "custom-override-test")
    );
}

#[test]
fn get_all_includes_both_built_in_and_custom() {
    let mut registry = AgentTypeRegistry::new();
    registry.register(AgentTypeDefinition::new(
        "security",
        "Custom security agent",
        Some(strings(&["read_file"])),
        "/fake/security.md",
    ));
    let all = registry.get_all();
    for name in ["security", "explore", "general"] {
        assert!(all.iter().any(|a| a.name == name), "{name}");
    }
}

#[test]
fn get_all_names_includes_custom_names() {
    let mut registry = AgentTypeRegistry::new();
    registry.register(AgentTypeDefinition::new(
        "perf",
        "Performance agent",
        Some(strings(&["read_file"])),
        "/fake/perf.md",
    ));
    let names = registry.get_all_names();
    assert!(names.contains(&"perf".to_string()));
    assert!(names.contains(&"explore".to_string()));
}

#[test]
fn parse_agent_content_quoted_tools_parsed_correctly() {
    let content = "---\nname: quoted\ndescription: Agent with quoted tools\ntools: [\"read_file\", \"grep\", \"bash\"]\n---\n\nBody.";
    let def = parse(content, "/fake/quoted.md").unwrap();
    assert_eq!(
        def.allowed_tools,
        Some(strings(&["read_file", "grep", "bash"]))
    );
}

// ── WorkflowTests (parse, loader, template) ─────────────────────────────────

#[test]
fn parse_valid_workflow_succeeds() {
    let wf = WorkflowDefinition::parse(
        r#"{
          "name": "demo",
          "description": "test",
          "stages": [
            { "name": "one", "agents": [ { "task": "do {{input}}", "agentType": "explore" } ] }
          ]
        }"#,
    )
    .unwrap();
    assert_eq!("demo", wf.name);
    assert_eq!(1, wf.stages.len());
    assert_eq!("explore", wf.stages[0].agents[0].agent_type);
}

#[test]
fn parse_invalid_workflow_reports_error() {
    for (json, hint) in [
        (r#"{ "stages": [] }"#, "name"),
        (r#"{ "name": "x", "stages": [] }"#, "stage"),
        (
            r#"{ "name": "x", "stages": [ { "agents": [] } ] }"#,
            "stage",
        ),
        (
            r#"{ "name": "x", "stages": [ { "agents": [ { "task": "" } ] } ] }"#,
            "task",
        ),
        ("not json at all", ""),
    ] {
        let error = WorkflowDefinition::parse(json).unwrap_err();
        assert!(error.to_lowercase().contains(hint), "{json}: {error}");
    }
}

#[test]
fn parse_accepts_comments_trailing_commas_and_any_case() {
    // Not in the C# tests: the C# JsonSerializerOptions (case-insensitive names, comments,
    // trailing commas) and the "general" default agent type.
    let wf = WorkflowDefinition::parse(
        "{\n  // a comment\n  \"NAME\": \"c\", /* block, ] */\n  \"Stages\": [ { \"AGENTS\": [ { \"Task\": \"t // not a comment\", }, ], }, ],\n}",
    )
    .unwrap();
    assert_eq!(wf.name, "c");
    assert_eq!(wf.stages[0].agents[0].task, "t // not a comment");
    assert_eq!(wf.stages[0].agents[0].agent_type, "general");
    assert!(WorkflowDefinition::parse(r#"{ "name": 5 }"#).is_err());
}

#[test]
fn substitute_template_replaces_input_and_stage_outputs() {
    let result = substitute_template(
        "Answer {{input}} using:\n{{stage.1.output}}\nand {{STAGE.2.Output}}",
        "the question",
        &strings(&["first result", "second result"]),
    );
    assert!(result.contains("the question"));
    assert!(result.contains("first result"));
    assert!(result.contains("second result"));
    assert!(!result.contains("{{"));
}

#[test]
fn built_ins_present_and_valid() {
    let built_ins = WorkflowLoader::built_ins();
    for name in ["verify", "explore-wide", "test-fix"] {
        assert!(built_ins.iter().any(|w| w.name == name), "{name}");
    }
    for w in &built_ins {
        assert!(!w.stages.is_empty());
        for a in w.stages.iter().flat_map(|s| &s.agents) {
            assert!(!a.task.trim().is_empty());
        }
    }
    assert!(built_ins[0].stages[0].agents[0].task.contains(
        "(use git_status and git_diff). Look for bugs, missing error handling, and inconsistencies"
    ));
}

#[test]
fn loader_discovers_built_ins_then_global_then_project() {
    // Not in the C# tests: discovery order, overrides and warnings.
    let tmp = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let global = config.path().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("v.json"),
        r#"{ "name": "VERIFY", "stages": [ { "agents": [ { "task": "mine" } ] } ] }"#,
    )
    .unwrap();
    std::fs::write(global.join("bad.json"), r#"{ "name": "x", "stages": [] }"#).unwrap();
    let mut warnings = Vec::new();
    let all = WorkflowLoader::discover(config.path(), tmp.path(), &mut |w| warnings.push(w));
    assert_eq!(
        all.keys().cloned().collect::<Vec<_>>(),
        strings(&["verify", "explore-wide", "test-fix"])
    );
    assert_eq!(all["verify"].stages[0].agents[0].task, "mine");
    assert_eq!(
        warnings,
        [
            "Skipping workflow 'bad.json': workflow needs at least one stage, and every stage at least one agent"
        ]
    );
}

// Runner_TwoStages_ChainsOutputsIntoLaterTemplates runs agents through the orchestrator: see
// tests/workflow_runner.rs.

// ── AgentSupportTests: activity + progress parsing ──────────────────────────

#[test]
fn describe_summarizes_tool_calls() {
    for (tool, args, expected) in [
        (
            "read",
            r#"{"path":"src/Foo.cs","offset":10}"#,
            "read src/Foo.cs",
        ),
        ("read_file", r#"{"path":"README.md"}"#, "read README.md"),
        (
            "bash",
            r#"{"command":"dotnet test\n--no-build"}"#,
            "bash dotnet test",
        ),
        (
            "edit",
            r#"{"path":"a.py","oldText":"x","newText":"y"}"#,
            "edit a.py",
        ),
        ("grep", r#"{"pattern":"TODO"}"#, "grep TODO"),
        (
            "subagent",
            r#"{"agent":"scout","task":"look"}"#,
            "subagent scout",
        ),
        ("mystery_tool", "not json", "mystery_tool"),
    ] {
        assert_eq!(
            expected,
            activity::describe(tool, Some(args), Some(Path::new("C:/repo")))
        );
    }
}

#[test]
fn describe_makes_paths_relative_to_cwd_and_truncates() {
    assert_eq!(
        "read src/a.cs",
        activity::describe(
            "read",
            Some(r#"{"path":"C:\\repo\\src\\a.cs"}"#),
            Some(Path::new("C:\\repo"))
        )
    );
    let args = format!(r#"{{"command":"{}"}}"#, "x".repeat(200));
    let long_cmd = activity::describe("bash", Some(&args), None);
    assert_eq!(activity::MAX_CHARS, long_cmd.encode_utf16().count());
    assert!(long_cmd.ends_with('…'));
}

#[test]
fn try_parse_progress_reads_the_last_marker() {
    for (text, fraction, note) in [
        ("[progress 40%] reading tests", 0.4, "reading tests"),
        ("intro\n[Progress: 3/4] almost there", 0.75, "almost there"),
        ("Progress: 25% — scanning", 0.25, "scanning"),
        ("[progress 10%] a\n[progress 60%] b", 0.6, "b"),
    ] {
        let (f, n) = activity::try_parse_progress(Some(text)).unwrap();
        assert!((f - fraction).abs() < 0.0005, "{text}");
        assert_eq!(n.as_deref(), Some(note));
    }
}

#[test]
fn try_parse_progress_rejects_non_markers() {
    for text in [
        "no marker here",
        "[progress 140%] nope",
        "[progress 5/4] nope",
        "we made progress today",
    ] {
        assert_eq!(activity::try_parse_progress(Some(text)), None, "{text}");
    }
}

#[test]
fn strip_progress_markers_keeps_the_report() {
    for (text, expected) in [
        ("[progress 100%] found it", "found it"),
        ("  [Progress 2/3]\nstep two done ", "step two done"),
        ("we made progress today", "we made progress today"),
    ] {
        assert_eq!(expected, activity::strip_progress_markers(Some(text)));
    }
}

#[test]
fn is_agent_tool_ignores_case() {
    assert!(activity::is_agent_tool("Spawn_Agent"));
    assert!(!activity::is_agent_tool("read"));
}

#[test]
fn transcript_keeps_the_streamed_text_until_the_step_is_recorded() {
    // A viewer opened mid-stream must see the whole step so far, not just later deltas.
    let transcript = AgentTranscript::new();
    transcript.raise_delta("[progress 30%] reading al");
    transcript.raise_delta("pha.txt");
    assert_eq!(
        "[progress 30%] reading alpha.txt",
        transcript.streaming_text()
    );

    transcript.add(AgentTranscriptKind::ToolResult, "file contents", false);
    assert!(!transcript.streaming_text().is_empty()); // results don't end the step's text

    transcript.add(
        AgentTranscriptKind::AssistantText,
        "[progress 30%] reading alpha.txt",
        false,
    );
    assert!(transcript.streaming_text().is_empty());
}

#[test]
fn transcript_cuts_long_text_and_survives_a_panicking_listener() {
    // Not in the C# tests: the "{N:N0} more chars" note and the listener guard.
    let t = AgentTranscript::new();
    t.on_appended(Box::new(|_| panic!("viewer bug")));
    t.add(AgentTranscriptKind::ToolResult, &"r".repeat(2_000), false);
    let e = &t.snapshot()[0];
    assert_eq!(e.text, format!("{}\n… (1,200 more chars)", "r".repeat(800)));
    t.add_tool_call("x", "bash", &"a".repeat(7000));
    assert_eq!(
        t.snapshot()[1].args.as_deref().map(|a| a.chars().count()),
        Some(6001)
    );
}

// ── AgentSupportTests: Pi profile agent types ───────────────────────────────

#[test]
fn pi_profile_swaps_built_ins_for_pi_agents_with_aliases_and_pi_tools() {
    let mut registry = AgentTypeRegistry::new();
    registry.use_pi_profile(false);

    assert!(registry.pi_profile());
    assert_eq!("worker", registry.get_default().name);
    assert_eq!("scout", registry.resolve("explore").unwrap().name);
    assert_eq!("planner", registry.resolve("plan").unwrap().name);
    assert_eq!("reviewer", registry.resolve("review").unwrap().name);
    assert_eq!(
        Some(strings(&["read", "bash", "job"])),
        registry.resolve("scout").unwrap().allowed_tools
    );
    assert_eq!(None, registry.resolve("worker").unwrap().allowed_tools);
    let mut names = registry.get_all_names();
    names.sort();
    assert_eq!(strings(&["planner", "reviewer", "scout", "worker"]), names);

    registry.use_pi_profile(true);
    assert_eq!(
        Some(strings(&["read", "bash", "grep", "find", "ls", "job"])),
        registry.resolve("scout").unwrap().allowed_tools
    );

    // Custom agents still win.
    registry.register(AgentTypeDefinition::new(
        "scout",
        "custom",
        Some(strings(&["read"])),
        "x.md",
    ));
    assert_eq!("custom", registry.resolve("scout").unwrap().description);
}

#[test]
fn pi_agent_prompts_are_embedded() {
    for (resource, marker) in [
        ("pi-scout.md", "## Start Here"),
        ("pi-planner.md", "## Plan"),
        ("pi-reviewer.md", "## Critical"),
        ("pi-worker.md", "## Files Changed"),
    ] {
        let prompt = AgentPromptLoader::load_prompt(
            &AgentTypeDefinition::new("x", "x", None, resource),
            "task",
            "/w",
            "os",
        )
        .unwrap();
        assert!(prompt.contains(marker), "{resource}");
    }
}

#[test]
fn pi_worker_prompt_asks_for_every_error_case_of_its_spec_section() {
    // multi-sales: the CSV worker skipped "bad header" / "blank input" and failed a hidden test.
    let prompt = AgentPromptLoader::load_prompt(
        &AgentTypeDefinition::new("x", "x", None, "pi-worker.md"),
        "task",
        "/w",
        "os",
    )
    .unwrap();
    assert!(prompt.contains("implement and test every error case the spec's section lists for your piece (malformed header, empty or blank input, and so on)"));
}

// ── AgentFooterTests: palette + labels ──────────────────────────────────────

#[test]
fn palette_fixed_colour_letter_pairs() {
    assert_eq!(
        [
            "blue", "green", "yellow", "purple", "orange", "red", "brown", "white"
        ],
        palette::NAMES
    );
    assert_eq!(
        "ABCDEFGHIJKMNOPQRSTUVWXYZ",
        palette::LETTERS.iter().collect::<String>()
    );
    assert_eq!('D', palette::letter_of(3));
    assert_eq!(135, palette::xterm_of(3));
    assert_eq!(2, palette::slot_of('c'));
    assert_eq!(-1, palette::slot_of('L'));
    assert!(!palette::XTERM.contains(&palette::LEAD_XTERM));
    // Letters past H cycle through the eight colours: I is blue again.
    assert_eq!('I', palette::letter_of(8));
    assert_eq!(palette::xterm_of(0), palette::xterm_of(8));
    assert_eq!('M', palette::letter_of(11));
}

#[test]
fn palette_assign_letter_never_reuses_a_letter_and_avoids_held_colours() {
    assert_eq!(0, palette::assign_letter(&[], &[]));
    // A finished long ago: B next, not A again.
    assert_eq!(1, palette::assign_letter(&[0], &[]));
    // A..H used, A (blue) still on the footer: I (blue) is skipped for J (green).
    let a_to_h: Vec<i32> = (0..8).collect();
    assert_eq!(9, palette::assign_letter(&a_to_h, &[0]));
    // Every unused letter's colour is held: the first unused letter anyway.
    assert_eq!(3, palette::assign_letter(&[0, 1, 2], &a_to_h));
    // All 25 letters spent: the oldest letter not on the footer.
    let all: Vec<i32> = (0..palette::LETTER_COUNT).collect();
    assert_eq!(1, palette::assign_letter(&all, &[0]));
}

#[test]
fn palette_assign_takes_the_first_free_colour_and_wraps_when_full() {
    assert_eq!(0, palette::assign(&[]));
    assert_eq!(1, palette::assign(&[0, 2]));
    assert_eq!(3, palette::assign(&[0, 1, 2]));
    let mut full: Vec<i32> = (0..8).collect();
    full.push(0);
    assert_eq!(1, palette::assign(&full));
}

#[test]
fn palette_negative_slots_and_hold() {
    // Not in the C# tests: C#'s ((slot % n) + n) % n normalisation and the hold times.
    assert_eq!(palette::name_of(-1), "white");
    assert_eq!(palette::color_of(-9), 7);
    assert_eq!(palette::letter_of(-1), 'Z');
    assert_eq!(
        palette::hold_after_finish(orca_core::orchestration::types::AgentOrigin::User),
        palette::USER_HOLD
    );
    assert_eq!(
        palette::hold_after_finish(Default::default()),
        std::time::Duration::from_secs(4)
    );
}

#[test]
fn labels_given_or_derived_at_most_four_words() {
    for (label, task, ty, expected) in [
        (
            Some("Scan auth code"),
            "anything",
            "explore",
            "Scan auth code",
        ),
        (
            Some("  Run   the whole test suite now  "),
            "x",
            "bash",
            "Run the whole test",
        ),
        (
            None,
            "Find all usages of AgentOrchestrator in src",
            "explore",
            "Find all usages",
        ),
        (
            None,
            "Please review the diff on this branch.",
            "review",
            "Review the diff",
        ),
        (
            None,
            "You are to survey alpha.txt and report its marker",
            "explore",
            "Survey alpha.txt",
        ),
        (None, "", "explore", "Explore"),
        (Some(""), "   ", "", "Agent"),
        (None, "run\nthe tests", "bash", "Run"),
    ] {
        let derived = labels::derive(label, Some(task), Some(ty));
        assert_eq!(expected, derived);
        assert!(derived.split(' ').count() <= labels::MAX_WORDS);
    }
}

#[test]
fn labels_drop_control_and_wide_characters_and_cap_length() {
    assert_eq!("Scan code", labels::normalize(Some("Scan\x1b 🚀 code")));
    let long_label = labels::normalize(Some(
        "Supercalifragilisticexpialidocious antidisestablishmentarianism words here",
    ));
    assert!(
        long_label.chars().count() <= labels::MAX_CHARS,
        "{long_label}"
    );
}

// ── PeerReplyGuardTests: repetition ─────────────────────────────────────────

#[test]
fn find_repeat_finds_a_short_unit_filling_the_tail() {
    let text = format!(
        "assert parse('1.2.3+01.2') == Version(1{}",
        "-1.2.3".repeat(600)
    );
    assert_eq!(
        repetition::find_repeat(&text, repetition::WINDOW, repetition::MAX_PERIOD).as_deref(),
        Some("-1.2.3")
    );
    assert_eq!(
        repetition::find_repeat(
            &"x".repeat(3000),
            repetition::WINDOW,
            repetition::MAX_PERIOD
        )
        .as_deref(),
        Some("x")
    );
}

#[test]
fn find_repeat_ignores_normal_code_and_short_runs() {
    let code = (0..200)
        .map(|i| format!("    assert compare('1.{i}.0', '1.{}.0') == -1", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let find = |t: &str| repetition::find_repeat(t, repetition::WINDOW, repetition::MAX_PERIOD);
    assert_eq!(find(&code), None);
    // A short run (under the window) is not enough.
    assert_eq!(find(&format!("{code}{}", "ab".repeat(500))), None);
    assert_eq!(find("short"), None);
}

#[test]
fn detector_checks_streamed_chunks_content_and_reasoning_separately() {
    let mut d = RepetitionDetector::new();
    for i in 0..100 {
        assert!(!d.add_content(Some(&format!("line {i}: some normal text that changes\n"))));
    }
    // Reasoning repetition is found even though the content is fine.
    let mut hit = false;
    for _ in 0..1000 {
        if hit {
            break;
        }
        hit = d.add_reasoning(Some("wait, "));
    }
    assert!(hit);
    assert_eq!(d.found(), Some("wait, "));
    assert!(d.add_content(Some("anything")));
}

// ── AgentUserMessageTests: transcript numbering ─────────────────────────────

#[test]
fn transcript_numbers_entries_and_counts_the_stream() {
    let t = AgentTranscript::new();
    let ends = Arc::new(Mutex::new(Vec::<i64>::new()));
    let sink = Arc::clone(&ends);
    t.on_stream_delta(Box::new(move |_, end| sink.lock().unwrap().push(end)));
    t.add(AgentTranscriptKind::Task, "task", false);
    t.raise_delta("Hel");
    t.raise_delta("lo");
    let mid = t.capture();
    assert_eq!("Hello", mid.streaming);
    assert_eq!(5, mid.stream_end);
    assert_eq!(vec![3i64, 5], *ends.lock().unwrap());
    t.add_tool_call("read a.cs", "read_file", "{\"path\":\"a.cs\"}");
    let after = t.capture();
    assert_eq!("", after.streaming); // the step's text is superseded…
    assert_eq!(5, after.stream_end); // …but the count keeps rising
    assert_eq!(
        vec![0i64, 1],
        after.entries.iter().map(|e| e.n).collect::<Vec<_>>()
    );
    assert_eq!(after.entries[1].tool.as_deref(), Some("read_file"));

    for i in 0..AgentTranscript::MAX_ENTRIES + 3 {
        t.add(AgentTranscriptKind::Status, &format!("s{i}"), false);
    }
    let full = t.capture();
    assert_eq!(AgentTranscript::MAX_ENTRIES, full.entries.len());
    assert_eq!(full.dropped as i64, full.entries[0].n); // numbers survive dropping
    assert_eq!(t.dropped_count(), full.dropped);
}
