//! The agent loop's file-log lines read as C#'s (D141): each run writes the event kinds that
//! `bench/run.py report` and the parity analysis count, matched here with their own patterns.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::ToolContext;
use orca_core::agent::{AgentSession, NullObserver};
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::logging::FileLog;
use orca_testkit::{FakeOpenAi, Reply};
use regex::Regex;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

fn session(server: &FakeOpenAi, work: &std::path::Path, native: bool) -> AgentSession {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec!["model-a".into()];
    config.lm_studio.native_tool_calling = native;
    config.agent.repeat_call_notes = false;
    config.permissions.auto_approve_all = true;
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let registry = orca_cli::build_registry(&config);
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(work));
    s.retry_base_delay = Duration::from_millis(1);
    s.failover_delay = Duration::from_millis(1);
    s.set_system_prompt("system".into());
    s
}

/// Runs `turn` with the file log layer installed and returns the log's text.
async fn logged<F: std::future::Future<Output = ()>>(turn: F) -> String {
    let home = tempfile::tempdir().unwrap();
    let log = FileLog::open(home.path());
    let subscriber =
        tracing_subscriber::registry().with(orca_cli::file_log::FileLogLayer::new(log.clone()));
    {
        // #[tokio::test] runs on one thread, so the default covers the whole turn.
        let _guard = tracing::subscriber::set_default(subscriber);
        turn.await;
    }
    let file = std::fs::read_dir(log.dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::read_to_string(file).unwrap()
}

/// How many lines match `pattern`.
fn count(log: &str, pattern: &str) -> usize {
    let rx = Regex::new(pattern).unwrap();
    log.lines().filter(|l| rx.is_match(l)).count()
}

fn assert_kind(log: &str, pattern: &str, expected: usize) {
    assert_eq!(
        count(log, pattern),
        expected,
        "{pattern}\n--- log ---\n{log}"
    );
}

#[tokio::test]
async fn a_native_turn_logs_requests_tools_errors_verification_and_the_end() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("read_file", json!({"path": "missing.txt"})),
        Reply::tool("write_file", json!({"path": "f.txt", "content": "alpha\n"})),
        Reply::text("Done."),
        Reply::tool("read_file", json!({"path": "f.txt"})),
        Reply::text("Verified."),
        Reply::text("I wrote f.txt with the single line alpha and read it back to check it."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), true);
    s.config.agent.verification_mode = "auto".into();
    let log = logged(async {
        s.run_turn("write f.txt", &mut NullObserver).await.unwrap();
    })
    .await;
    let requests = server.requests().len();
    assert_eq!(requests, 6, "{log}");

    // bench/run.py LOG_PATTERNS and its per-line metrics.
    assert_kind(
        &log,
        r"\[DBG\] AgentLoopRunner: Starting streaming request to LLM\.\.\.$",
        requests,
    );
    assert_kind(
        &log,
        r"AgentLoopRunner: === Agent loop iteration \d+/\d+ ===",
        6,
    );
    assert_kind(
        &log,
        r"Streaming complete: \d+ tokens, \d+ chars, \d+ content items",
        6,
    );
    assert_kind(
        &log,
        r"\[INF\] AgentLoopRunner: Executing 1 native tool call\(s\)",
        3,
    );
    assert_kind(
        &log,
        r"requesting verification before (summary|finishing)",
        1,
    );
    assert_kind(&log, r"Verification complete — requesting final summary", 1);
    assert_kind(&log, r"requesting final summary", 1);
    assert_kind(
        &log,
        r"No tool calls — ending agent loop \(nudge attempts: 0\)",
        1,
    );
    // analyze.py: the Lead's calls.
    assert_kind(
        &log,
        r"AgentLoopRunner: Native FunctionCallContent received: read_file \(id: \S+\)",
        2,
    );
    assert_kind(&log, r"AgentLoopRunner: Native function calls: 1$", 3);
    // The tool pipeline's lines, under C#'s `Program` category.
    assert_kind(
        &log,
        r"\[DBG\] Program: ExecuteToolAsync called: write_file args: ",
        1,
    );
    assert_kind(
        &log,
        r"\[DBG\] Program: Tool resolved: read_file \(risk: ReadOnly\)",
        2,
    );
    assert_kind(
        &log,
        r"\[DBG\] Program: Tool read_file completed: isError=True \(\d+ chars\)",
        1,
    );
    assert_kind(&log, r"\[WRN\] Program: Tool read_file error: ", 1);
    assert_kind(
        &log,
        r"\[DBG\] ToolCallExecutor: Preparing tool: write_file args: ",
        1,
    );
    assert_kind(
        &log,
        r"\[DBG\] ToolCallExecutor: Tool write_file returned \d+ chars in \d+\.\ds",
        1,
    );
    assert_kind(
        &log,
        r"ToolCallExecutor: Added tool result message to conversation \(callId: ",
        3,
    );
    assert_kind(
        &log,
        r"AgentLoopRunner: Conversation has \d+ messages, sending to LLM",
        6,
    );
    assert_kind(
        &log,
        r"AgentLoopRunner: Native tool calling: True, tool count: \d+",
        6,
    );
    assert_kind(&log, r"AgentLoopRunner: First token received from LLM", 6);
}

#[tokio::test]
async fn a_text_turn_logs_nudges_parsing_and_the_wrap_up() {
    let server = FakeOpenAi::start(vec![
        Reply::text(
            "I'll create the file src/test.cs with this content:\n```csharp\nConsole.WriteLine(\"hello\");\n```",
        ),
        Reply::text(
            "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"f.txt\"}}\n</tool_call>",
        ),
        Reply::text("The file f.txt holds one line that says alpha, nothing more at all."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("f.txt"), "alpha\n").unwrap();
    let mut s = session(&server, work.path(), false);
    s.config.agent.verification_mode = "off".into();
    let log = logged(async {
        s.run_turn("read f.txt", &mut NullObserver).await.unwrap();
    })
    .await;

    assert_kind(&log, r"Nudging model|Truncated tool call detected", 1);
    assert_kind(
        &log,
        r"\[INF\] AgentLoopRunner: Nudging model \(attempt 1\) — response has code blocks but no tool calls",
        1,
    );
    assert_kind(&log, r"Executing (\d+) (?:native|text-parsed) tool call", 1);
    assert_kind(
        &log,
        r"AgentLoopRunner: Executing 1 text-parsed tool call\(s\)",
        1,
    );
    assert_kind(
        &log,
        r"AgentLoopRunner: Parsed function calls from text: \d+",
        3,
    );
    assert_kind(
        &log,
        r"\[INF\] AgentLoopRunner: Text-parsed tool call: read_file args: ",
        1,
    );
    assert_kind(
        &log,
        r"AgentLoopRunner: No tool calls found\. Full text \(\d+ chars\), first 500: ",
        2,
    );
    assert_kind(
        &log,
        r"ToolCallExecutor: Injected tool results as user message \(\d+ chars\)",
        1,
    );
    assert_kind(&log, r"Starting streaming request to LLM", 3);
    // A substantive wrap-up ends the turn without the "ending agent loop" line (as in C#).
    assert_kind(
        &log,
        r"AgentLoopRunner: Final message already contains a wrap-up \(\d+ chars\) — no summary request",
        1,
    );
    assert_kind(&log, r"No tool calls — ending agent loop", 0);
}

#[tokio::test]
async fn loop_guards_log_doom_loops_and_identical_failures() {
    let read = || Reply::tool("read_file", json!({"path": "f.txt"}));
    let fail = || Reply::tool("read_file", json!({"path": "nope.txt"}));
    let server = FakeOpenAi::start(vec![
        read(),
        read(),
        read(),
        fail(),
        fail(),
        Reply::text("I could not read nope.txt, it does not exist in this folder at all."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("f.txt"), "x\n").unwrap();
    let mut s = session(&server, work.path(), true);
    s.config.agent.verification_mode = "off".into();
    let log = logged(async {
        s.run_turn("check", &mut NullObserver).await.unwrap();
    })
    .await;

    assert_kind(
        &log,
        r"Doom loop detected for tool|Force-breaking agent loop|redirect model after \d+ identical tool failures",
        1,
    );
    assert_kind(
        &log,
        r"\[WRN\] AgentLoopRunner: Doom loop detected for tool 'read_file' — ",
        1,
    );
    assert_kind(
        &log,
        r"\[WRN\] ToolCallExecutor: Doom loop detected: read_file called 3\+ times",
        1,
    );
    assert_kind(
        &log,
        r"\[WRN\] ToolCallExecutor: Tool read_file has failed 2 times with same error — warning appended",
        1,
    );
    assert_kind(&log, r"\[WRN\] Program: Tool read_file error: ", 2);
}

#[tokio::test]
async fn an_empty_reply_is_logged() {
    // Text mode: the empty stream, the server error probe (answers 200, reports nothing), the
    // same-model retry.
    let server = FakeOpenAi::start(vec![
        Reply::empty(),
        Reply::empty(),
        Reply::text("Hello there, answering your greeting now with a full sentence."),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), false);
    let log = logged(async {
        s.run_turn("hi", &mut NullObserver).await.unwrap();
    })
    .await;
    assert_kind(&log, r"No tokens received from LLM", 1);
    assert_kind(
        &log,
        r"\[WRN\] AgentLoopRunner: No tokens received from LLM — empty response$",
        1,
    );
    assert_kind(&log, r"Starting streaming request to LLM", 2);
}
