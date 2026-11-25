//! Ported from `PiProfileTests.cs` and `PiLoopIntegrationTests.cs` (toolProfile "pi"): the
//! system prompt, tool exposure, pipeline behaviour and the loop's Pi wording.

// The ported tests set config fields one by one, as the C# tests do.
#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use orca_abstractions::{Tool, ToolCapabilities, ToolContext, ToolResult, ToolRiskLevel};
use orca_core::agent::prompt_constants::{self as pc, platform_lines};
use orca_core::agent::run_evidence::{RunOutcome, classify};
use orca_core::agent::system_prompt::{PromptTool, SystemPromptBuilder};
use orca_core::agent::{AgentSession, NullObserver, ToolCallRecord, ToolRegistry};
use orca_core::client::{ServedModels, ToolCall, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::config::prompt_manager::PromptManager;
use orca_testkit::{FakeOpenAi, Reply};
use orca_tools_pi::{PiContextFile, PiToolsetOptions, subagent, system_prompt};
use serde_json::{Value, json};

fn pi_config() -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.tool_profile = "pi".into();
    config.lm_studio.native_tool_calling = true;
    config.permissions.auto_approve_all = true;
    config
}

fn core_names() -> Vec<String> {
    orca_tools_pi::CORE_TOOL_NAMES.map(str::to_string).to_vec()
}

// --- System prompt -----------------------------------------------------------------------

/// Pi 0.87.1's rendering for the four default tools, minus the <docs> section.
#[test]
fn pi_prompt_matches_upstream_layout() {
    let got = system_prompt::build(
        &core_names(),
        &orca_tools_pi::prompt_snippets(),
        &orca_tools_pi::prompt_guidelines(),
        &[],
        "/work/app",
        None,
        false,
    );
    let want = concat!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\n",
        "<tools>\n",
        "- read: Read file contents\n",
        "- bash: Execute bash commands (ls, grep, find, etc.)\n",
        "- edit: Make precise file edits with exact text replacement, including multiple disjoint edits in one call\n",
        "- write: Create or overwrite files\n\n",
        "In addition to the tools above, you may have access to other custom tools depending on the project.\n",
        "</tools>\n\n",
        "<rules>\n",
        "- Use bash for file operations like ls, rg, find\n",
        "- Use read to examine files instead of cat or sed.\n",
        "- You can inspect PI_* environment variables for current model and session details.\n",
        "- Stop only processes you started (job kill, or kill by PID), never by name (taskkill /IM, pkill, killall, Stop-Process -Name): that also closes the user's own copies\n",
        "- Use edit for precise changes (edits[].oldText must match exactly)\n",
        "- When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls\n",
        "- Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.\n",
        "- Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.\n",
        "- Use write only for new files or complete rewrites.\n",
        "- Be concise in your responses\n",
        "- Show file paths clearly when working with files\n",
        "</rules>\n\n",
        "<cwd>\n/work/app\n</cwd>"
    );
    assert_eq!(got, want);
}

#[test]
fn pi_prompt_project_context_addendum_and_extras() {
    let names: Vec<String> = [
        "read",
        "bash",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
        "mcp_thing",
    ]
    .map(str::to_string)
    .to_vec();
    let got = system_prompt::build(
        &names,
        &orca_tools_pi::prompt_snippets(),
        &orca_tools_pi::prompt_guidelines(),
        &[
            PiContextFile {
                path: "/w/AGENTS.md".into(),
                content: "Be kind.".into(),
            },
            PiContextFile {
                path: "/w/CLAUDE.md".into(),
                content: "Be brief.".into(),
            },
        ],
        r"C:\w",
        Some("Extra rule."),
        false,
    );
    // With grep/find/ls the shell file-op rule disappears; tools without a snippet are not listed.
    assert!(!got.contains("Use bash for file operations"));
    assert!(got.contains("- ls: List directory contents\n\nIn addition"));
    assert!(!got.contains("mcp_thing"));
    assert!(got.ends_with(concat!(
        "\n\n<addendum>\nExtra rule.\n</addendum>\n\n<project_context>\nProject-specific instructions and guidelines:\n\n",
        "<project_instructions path=\"/w/AGENTS.md\">\nBe kind.\n</project_instructions>\n\n",
        "<project_instructions path=\"/w/CLAUDE.md\">\nBe brief.\n</project_instructions>\n</project_context>\n\n<cwd>\nC:/w\n</cwd>"
    )));
}

struct PromptFixture {
    prompts: tempfile::TempDir,
}

impl PromptFixture {
    fn new() -> Self {
        Self {
            prompts: tempfile::tempdir().unwrap(),
        }
    }
    fn builder(&self) -> SystemPromptBuilder {
        SystemPromptBuilder::new(
            PromptManager::with_dir(self.prompts.path()),
            None,
            std::env::current_dir().unwrap(),
        )
    }
}

/// The Pi toolset as the prompt sees it (name and description).
fn pi_prompt_tools() -> Vec<PromptTool> {
    orca_tools_pi::create_toolset(&PiToolsetOptions::default())
        .iter()
        .map(|t| PromptTool::new(t.name(), t.description(), json!({})))
        .collect()
}

#[test]
fn system_prompt_builder_pi_profile_uses_pi_prompt_only() {
    let f = PromptFixture::new();
    let builder = f.builder();
    let prompt = builder.get_system_prompt(&pi_config(), Some(&pi_prompt_tools()), false);

    assert!(prompt.starts_with(&format!(
        "{}\n\n<tools>\n- read: Read file contents\n- bash:",
        system_prompt::PREAMBLE
    )));
    let cwd = std::env::current_dir()
        .unwrap()
        .display()
        .to_string()
        .replace('\\', "/");
    assert!(prompt.ends_with(&format!("<cwd>\n{cwd}\n</cwd>")));
    assert!(!prompt.contains("next-prompt"));
    assert!(!prompt.contains("PROJECT MEMORY"));
    assert!(!prompt.contains("think"));
    assert_eq!(builder.get_prompt_tool_mode(Some(&prompt)), Some(true));
}

#[test]
fn system_prompt_builder_pi_profile_lead_gets_the_delegation_workflow_only_with_subagent() {
    let f = PromptFixture::new();
    let builder = f.builder();
    let core = pi_prompt_tools();
    let without = builder.get_system_prompt(&pi_config(), Some(&core), false);
    assert!(!without.contains("<addendum>"));

    let sub = subagent::PiSubagentTool::default();
    let mut with_tools = core.clone();
    with_tools.push(PromptTool::new(sub.name(), sub.description(), json!({})));
    let with = builder.get_system_prompt(&pi_config(), Some(&with_tools), false);
    assert!(with.contains(&format!(
        "<addendum>\n{}\n</addendum>",
        platform_lines(&subagent::LEAD_INSTRUCTIONS)
    )));
    assert!(with.contains("Make ONE subagent call with a tasks array"));
    assert!(with.contains("Do only trivial work directly"));
}

#[test]
fn system_prompt_builder_pi_profile_plan_mode_names_pi_tools_only() {
    let f = PromptFixture::new();
    let prompt = f
        .builder()
        .get_system_prompt(&pi_config(), Some(&pi_prompt_tools()), true);
    assert!(prompt.contains(&platform_lines(pc::PI_PLAN_MODE_SYSTEM_PROMPT_ADDITION)));
    assert!(!prompt.contains("read_file"));
    assert!(!prompt.contains("task_list"));
}

// --- Exposure ----------------------------------------------------------------------------

struct FakeTool(&'static str);

#[async_trait::async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "fake tool"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }
    fn parameter_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
}

/// C# registers fakes plus `PiToolset.Create` and filters with `ToolExposure`; here
/// `pi_profile::register` plus `tool_exposure::is_advertised` (as `build_registry` wires it). `pi.agentTools` is off because the C# test's registry
/// has no agent tools, and the bash default timeout is 0 as in `PiToolsetOptions`.
fn advertised_names(config: &OrcaConfig) -> (ToolRegistry, Vec<String>) {
    let mut r = ToolRegistry::new();
    for name in [
        "read_file",
        "write_file",
        "edit_file",
        "bash",
        "grep",
        "think",
        "task_list",
        "mcp_docs_search",
    ] {
        r.register(Arc::new(FakeTool(name)));
    }
    orca_cli::pi_profile::register(&mut r, config);
    let exposure = config.clone();
    r.set_advertise_filter(Arc::new(move |name: &str| {
        orca_core::agent::tool_exposure::is_advertised(name, &exposure)
    }));
    let names = r.advertised().map(|t| t.name().to_string()).collect();
    (r, names)
}

#[test]
fn exposure_pi_profile_advertises_only_pi_tools_in_pi_order_plus_mcp() {
    if orca_tools_pi::shell::resolve(None).is_err() {
        return; // needs a bash for the Pi bash tool to replace OpenOrca's
    }
    let mut config = pi_config();
    config.pi.agent_tools = false;
    config.pi.bash_timeout_seconds = 0;
    let (_, names) = advertised_names(&config);
    assert_eq!(
        names,
        ["read", "bash", "edit", "write", "job", "mcp_docs_search"]
    );

    config.pi.extra_tools = true;
    config.tools.extra_tools = vec!["think".into()];
    let (r, names) = advertised_names(&config);
    assert_eq!(
        names[..8],
        ["read", "bash", "edit", "write", "grep", "find", "ls", "job"]
    );
    assert!(names.contains(&"think".to_string()));
    assert!(names.contains(&"mcp_docs_search".to_string()));
    assert!(!names.contains(&"read_file".to_string()));

    // Pi's bash/grep replace OpenOrca's same-named tools; the schema the model sees is Pi's.
    let bash = r.resolve("bash").unwrap();
    assert_eq!(
        orca_tools_pi::json::to_compact(&bash.parameter_schema()),
        r#"{"type":"object","required":["command"],"properties":{"command":{"type":"string","description":"Shell command to execute"},"timeout":{"type":"number","description":"Timeout in seconds (max 600)"},"background":{"type":"boolean","description":"Run as a background job and return its id at once (servers, apps, watchers)"}}}"#
    );
}

#[test]
fn exposure_default_profile_unchanged() {
    let r = orca_cli::build_registry(&OrcaConfig::default());
    let names: Vec<String> = r.advertised().map(|t| t.name().to_string()).collect();
    assert!(names.contains(&"read_file".to_string()));
    assert!(!names.contains(&"find".to_string()));
}

// --- Pipeline ----------------------------------------------------------------------------

fn pi_session(config: OrcaConfig, work: &std::path::Path) -> AgentSession {
    let mut config = config;
    config.lm_studio.base_url = "http://127.0.0.1:9".into();
    config.lm_studio.models = vec!["m".into()];
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let mut r = ToolRegistry::new();
    r.register(Arc::new(orca_tools_pi::PiReadTool));
    r.register(Arc::new(orca_tools_pi::PiEditTool));
    r.register(Arc::new(orca_tools_pi::PiWriteTool));
    AgentSession::new(config, client, r, ToolContext::new(work))
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

#[tokio::test]
async fn pipeline_pi_validation_error_is_pi_text_verbatim() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    let result = s
        .execute_tool_call(&call("write", json!({"path": "a.txt"})), &mut NullObserver)
        .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "Validation failed for tool \"write\":\n  - content: must have required properties content\n\nReceived arguments:\n{\n  \"path\": \"a.txt\"\n}"
    );
}

#[tokio::test]
async fn pipeline_pi_tool_errors_have_no_host_prefix_and_unknown_tool_uses_pi_text() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    let missing = work.path().join("nope.txt").display().to_string();
    let result = s
        .execute_tool_call(&call("read", json!({ "path": missing })), &mut NullObserver)
        .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!("ENOENT: no such file or directory, access '{missing}'")
    );

    let unknown = s
        .execute_tool_call(&call("frobnicate", json!({})), &mut NullObserver)
        .await;
    assert_eq!(unknown.content, "Tool frobnicate not found");
}

#[tokio::test]
async fn pipeline_pi_edit_is_content_modifying_read_before_edit_applies() {
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("seed.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let file_s = file.display().to_string();
    let mut config = pi_config();
    config.pi.read_before_edit = true;
    let mut s = pi_session(config, work.path());
    let edit = call(
        "edit",
        json!({"path": file_s, "edits": [{"oldText": "alpha", "newText": "beta"}]}),
    );

    let blocked = s.execute_tool_call(&edit, &mut NullObserver).await;
    assert!(blocked.is_error);
    assert!(
        blocked.content.contains("Use read first"),
        "{}",
        blocked.content
    );

    let read = s
        .execute_tool_call(&call("read", json!({ "path": file_s })), &mut NullObserver)
        .await;
    assert_eq!(read.content, "alpha\n");

    let edited = s.execute_tool_call(&edit, &mut NullObserver).await;
    assert!(!edited.is_error, "{}", edited.content);
    assert_eq!(
        edited.content,
        format!("Successfully replaced 1 block(s) in {file_s}.")
    );
    assert!(s.files_modified.contains(&file_s));

    // `ToolExecutionPipeline.IsContentModifying` is private in Rust: the capabilities decide.
    let content_modifying = |t: &dyn Tool| {
        t.capabilities()
            .contains(ToolCapabilities::MODIFIES_FILES | ToolCapabilities::MODIFIES_FILE_CONTENT)
    };
    assert!(content_modifying(&orca_tools_pi::PiEditTool));
    assert!(content_modifying(&orca_tools_pi::PiWriteTool));
    assert!(!content_modifying(&orca_tools_pi::PiReadTool));
}

#[tokio::test]
async fn pipeline_pi_read_before_edit_is_off_by_default() {
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("seed2.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let file_s = file.display().to_string();
    let config = pi_config();
    assert!(!config.pi.read_before_edit);
    let mut s = pi_session(config, work.path());
    let result = s
        .execute_tool_call(
            &call("write", json!({"path": file_s, "content": "new\n"})),
            &mut NullObserver,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, format!("Successfully wrote to {file_s}"));
}

// --- PiLoopIntegrationTests ----------------------------------------------------------------

// MutationKeys_PiEditAndWrite_KeyedByResolvedPath_ReadAndBashNone and
// ParallelPiEdits_SameFile_RunInCallOrder: not ported. The Rust loop runs a response's calls
// one after another (there is no FileMutationScheduler yet); Pi's write/edit additionally
// serialise per resolved path inside the tools (`with_file_lock`).
// RetryDetection_PiTool_ResultVerbatim_FailuresStillCounted: not ported as such (it swaps the
// executor's tool delegate, which Rust has no seam for); verbatim Pi results are covered by
// the pipeline tests above and cross-check scenario 64.

#[tokio::test]
async fn pi_edit_success_advances_mutation_epoch_failure_does_not() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.py"), "x = 1\n").unwrap();
    let mut s = pi_session(pi_config(), work.path());
    let failing = call(
        "edit",
        json!({"path": "a.py", "edits": [{"oldText": "missing", "newText": "y"}]}),
    );
    let r = s.execute_tool_call(&failing, &mut NullObserver).await;
    assert!(r.is_error);
    assert_eq!(s.mutation_epoch, 0);

    let ok = call(
        "edit",
        json!({"path": "a.py", "edits": [{"oldText": "x = 1", "newText": "x = 2"}]}),
    );
    let r = s.execute_tool_call(&ok, &mut NullObserver).await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(s.mutation_epoch, 1);
}

fn record(name: &str, args: Value, result: &str, is_error: bool) -> ToolCallRecord {
    ToolCallRecord {
        name: name.into(),
        arguments: Some(args.to_string()),
        result: Some(result.into()),
        is_error,
        duration_ms: 0,
    }
}

#[test]
fn evidence_pi_bash_passing_test_run_after_pi_edit_skips_verification() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    s.tool_call_history.push(record(
        "read",
        json!({"path": "calc.py"}),
        "def add(a, b):\n    return a - b\n",
        false,
    ));
    s.tool_call_history.push(record(
        "edit",
        json!({"path": "calc.py", "edits": [{"oldText": "a - b", "newText": "a + b"}]}),
        "Successfully replaced 1 block(s) in calc.py.",
        false,
    ));
    // Pi bash: stdout+stderr combined, no "[stderr] " prefix, nothing appended on exit 0.
    s.tool_call_history.push(record(
        "bash",
        json!({"command": "cd /work && python -m unittest -q"}),
        ".\n----------------------------------------------------------------------\nRan 1 test in 0.000s\n\nOK",
        false,
    ));
    assert!(s.turn_modified_files());
    assert!(s.has_recent_test_pass_evidence());
}

#[test]
fn evidence_pi_bash_test_command_exit0_no_recognisable_summary_still_pass() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    s.tool_call_history.push(record(
        "write",
        json!({"path": "app.js", "content": "x"}),
        "Successfully wrote 1 bytes to app.js",
        false,
    ));
    s.tool_call_history.push(record(
        "bash",
        json!({"command": "node test_app.js"}),
        "(no output)",
        false,
    ));
    assert!(s.has_recent_test_pass_evidence());
}

#[test]
fn evidence_pi_bash_failing_run_command_exited_with_code_is_failure() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    s.tool_call_history.push(record(
        "edit",
        json!({"path": "calc.py"}),
        "Successfully replaced 1 block(s) in calc.py.",
        false,
    ));
    s.tool_call_history.push(record(
        "bash",
        json!({"command": "python -m pytest -q"}),
        "F.\n=== FAILURES ===\nE   assert 1 == 5\n1 failed, 1 passed in 0.02s\n\nCommand exited with code 1",
        true,
    ));
    assert!(!s.has_recent_test_pass_evidence());
    let tail = s.recent_test_failure().expect("a failure");
    assert!(tail.contains("Command exited with code 1"));
    assert_eq!(
        classify(
            Some("weird runner noise\n\nCommand exited with code 2"),
            true,
            Some(&json!({"command": "npm test"}).to_string())
        ),
        RunOutcome::Fail
    );
}

#[test]
fn evidence_pi_write_scratch_then_deleted_keeps_evidence() {
    let work = tempfile::tempdir().unwrap();
    let mut s = pi_session(pi_config(), work.path());
    s.tool_call_history
        .push(record("edit", json!({"path": "ledger.py"}), "ok", false));
    s.tool_call_history.push(record(
        "write",
        json!({"path": "check.py", "content": "x"}),
        "ok",
        false,
    ));
    s.tool_call_history.push(record(
        "bash",
        json!({"command": "python check.py"}),
        "all checks ok",
        false,
    ));
    s.tool_call_history.push(record(
        "bash",
        json!({"command": "rm check.py"}),
        "(no output)",
        false,
    ));
    // C# also records an OpenOrca delete_file of the scratch file; this registry has no
    // delete tool, so the record is not a mutation either way.
    s.tool_call_history.push(record(
        "delete_file",
        json!({"path": "check.py"}),
        "deleted",
        false,
    ));
    assert!(s.has_recent_test_pass_evidence());
}

/// Injected loop messages name only Pi tools.
#[test]
fn pi_profile_messages_name_no_open_orca_only_tools() {
    let re = regex::Regex::new(r"\b(read_file|edit_file|write_file|multi_edit|delete_file|list_directory|think|task_list|print|ask_user|spawn_agents?|start_background_process|get_process_output)\b").unwrap();
    for (name, text) in [
        ("PiNudgeMessage", pc::get_nudge_message(true, true)),
        ("PiNudgeText", pc::get_nudge_message(false, true)),
        (
            "PiTruncatedToolCallMessage",
            pc::get_truncated_tool_call_message(true),
        ),
        ("PiVerificationMessage", pc::get_verification_message(true)),
        (
            "PiVerificationRetryMessage",
            pc::get_verification_retry_message(true),
        ),
        (
            "PiPlanModeSystemPromptAddition",
            pc::PI_PLAN_MODE_SYSTEM_PROMPT_ADDITION,
        ),
        ("RetryLoopRedirectMessage", pc::RETRY_LOOP_REDIRECT_MESSAGE),
        ("SummaryRequestMessage", pc::SUMMARY_REQUEST_MESSAGE),
    ] {
        assert!(
            !re.is_match(text),
            "{name} mentions '{}'",
            re.find(text).unwrap().as_str()
        );
    }
}

#[test]
fn default_profile_messages_unchanged() {
    assert_eq!(
        pc::get_verification_message(false),
        pc::VERIFICATION_MESSAGE
    );
    assert_eq!(pc::get_nudge_message(true, false), pc::NATIVE_NUDGE_MESSAGE);
    assert_eq!(pc::get_nudge_message(false, false), pc::NUDGE_MESSAGE);
    assert_eq!(
        pc::get_truncated_tool_call_message(false),
        pc::TRUNCATED_TOOL_CALL_MESSAGE
    );
    assert_eq!(
        pc::get_verification_retry_message(false),
        pc::VERIFICATION_RETRY_MESSAGE
    );
}

/// The user text of the last message of a request body.
fn last_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn served_session(server: &FakeOpenAi, work: &std::path::Path) -> AgentSession {
    let mut config = pi_config();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["m".into()];
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let registry = orca_cli::build_registry(&config);
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(work));
    s.set_system_prompt("system".into());
    s
}

/// `Dispatcher_PiProfile_InjectsPiVerificationThenPiRetry`, through a turn: after an edit,
/// "Done." gets Pi's verification message and "Verified." (with no read) Pi's retry message.
#[tokio::test]
async fn dispatcher_pi_profile_injects_pi_verification_then_pi_retry() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.py"), "x = 1\n").unwrap();
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "edit",
            json!({"path": "a.py", "edits": [{"oldText": "x = 1", "newText": "x = 2"}]}),
        ),
        Reply::text("Done."),
        Reply::text("Verified."),
        Reply::text("I changed x to 2 in a.py and checked the file afterwards for you."),
    ])
    .await;
    let mut s = served_session(&server, work.path());
    s.config.agent.verification_mode = "always".into();
    s.run_turn("Change x", &mut NullObserver).await.unwrap();
    let requests = server.requests();
    assert!(requests.len() >= 3, "{} requests", requests.len());
    assert_eq!(
        last_user_text(&requests[2].body),
        platform_lines(pc::PI_VERIFICATION_MESSAGE)
    );
    assert_eq!(
        last_user_text(&requests[3].body),
        platform_lines(pc::PI_VERIFICATION_RETRY_MESSAGE)
    );
}

#[tokio::test]
async fn dispatcher_pi_profile_nudge_uses_pi_tool_names() {
    let work = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start(vec![
        Reply::text(
            "Here is the file:\n```python\ndef main():\n    print('hi')\n```\nSave this as main.py.",
        ),
        Reply::text("I could not save it myself, so please create main.py with that code."),
    ])
    .await;
    let mut s = served_session(&server, work.path());
    s.run_turn("Write main.py", &mut NullObserver)
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(
        last_user_text(&requests[1].body),
        platform_lines(pc::PI_NUDGE_MESSAGE)
    );
}
