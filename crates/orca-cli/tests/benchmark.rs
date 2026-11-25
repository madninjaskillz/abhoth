//! `/benchmark` against the fake server: the three prompts run in a temp folder, the script is
//! run and checked, and the session comes back as it was.

use std::sync::Arc;

use orca_abstractions::ToolContext;
use orca_cli::commands::benchmark_cmd::{self, BENCHMARK_PROMPTS, BenchmarkArgs};
use orca_cli::repl::state::ReplState;
use orca_cli::ui::Console;
use orca_core::agent::AgentSession;
use orca_core::client::{ChatMessage, ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::json;

const SCRIPT: &str = "def main():\n    primes = [n for n in range(2, 101) if all(n % d for d in range(2, n))]\n    with open('output.txt', 'w') as f:\n        f.write(chr(10).join(str(p) for p in primes) + chr(10))\n\nif __name__ == '__main__':\n    main()\n";

fn session(server: &FakeOpenAi, work: &std::path::Path, model: &str) -> AgentSession {
    let mut config = OrcaConfig::default();
    config.lm_studio.base_url = server.base_url();
    config.lm_studio.api_key = "k".into();
    config.lm_studio.models = vec![model.into(), "model-b".into()];
    config.lm_studio.native_tool_calling = true;
    config.agent.verification_mode = "off".into();
    config.permissions.auto_approve_all = true;
    let client = client_from_config(&config, Arc::new(ServedModels::new())).unwrap();
    let registry = orca_cli::build_registry(&config);
    let mut s = AgentSession::new(config, client, registry, ToolContext::new(work));
    s.set_system_prompt("system".into());
    s.messages.push(ChatMessage::user("earlier"));
    s
}

fn python_available() -> bool {
    let python = if cfg!(windows) { "python" } else { "python3" };
    std::process::Command::new(python)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn a_run_sends_the_three_prompts_in_a_temp_folder_checks_the_script_and_restores_the_session()
{
    let wrap_up =
        "Done: main.py is in place and writes the primes as asked, nothing else to change.";
    let server = FakeOpenAi::start(vec![
        Reply::tool("write_file", json!({"path": "main.py", "content": SCRIPT})),
        Reply::text(wrap_up),
        Reply::text(wrap_up),
        Reply::text(wrap_up),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), "model-a");
    let before = s.messages.clone();
    let (console, out) = Console::buffer();
    let args = BenchmarkArgs {
        runs_per_model: 1,
        model_filter: None,
        mode_pair: false,
    };
    benchmark_cmd::run(&console, &mut s, &ReplState::default(), &args)
        .await
        .unwrap();
    let text = Console::buffer_text(&out);

    // The session is as it was.
    assert_eq!(s.messages, before);
    assert_eq!(s.ctx.cwd, work.path());
    assert_eq!(s.config.lm_studio.models, vec!["model-a", "model-b"]);
    assert!(!work.path().join("main.py").exists());

    let reqs = server.requests();
    assert_eq!(reqs.len(), 4);
    let first = reqs[0].body["messages"].as_array().unwrap();
    assert_eq!(first.len(), 2, "a fresh conversation: system + prompt 1");
    assert_eq!(first[1]["content"], BENCHMARK_PROMPTS[0]);
    let system = first[0]["content"].as_str().unwrap();
    assert!(system.contains("orca-benchmark-model-a-run1-"), "{system}");
    assert_eq!(
        reqs[2].body["messages"].as_array().unwrap().last().unwrap()["content"],
        BENCHMARK_PROMPTS[1]
    );
    assert_eq!(
        reqs[3].body["messages"].as_array().unwrap().last().unwrap()["content"],
        BENCHMARK_PROMPTS[2]
    );

    assert!(text.contains("Starting model benchmark (1 run(s) per model)..."));
    assert!(text.contains("Benchmarking current model: model-a"));
    assert!(text.contains("model-a — Run 1/1"));
    assert!(text.contains("Prompt 1/3") && text.contains("Prompt 3/3"));
    assert!(text.contains("1 tool call(s)"), "{text}");
    assert!(text.contains("0 tool call(s)"));
    assert!(
        !text.contains("nothing else to change"),
        "model text is hidden"
    );
    assert!(text.contains("Benchmark Results"));
    if python_available() {
        assert!(text.contains("Running: python main.py"), "{text}");
        assert!(text.contains("PASS — All 25 primes correct"), "{text}");
        assert!(text.contains("1/1"));
    }
}

#[tokio::test]
async fn no_entry_point_is_a_failed_run_and_pair_mode_needs_two_models() {
    let wrap_up = "Nothing was written here; I only described the change in words for you.";
    let server = FakeOpenAi::start(vec![
        Reply::text(wrap_up),
        Reply::text(wrap_up),
        Reply::text(wrap_up),
    ])
    .await;
    let work = tempfile::tempdir().unwrap();
    let mut s = session(&server, work.path(), "model-c");
    let (console, out) = Console::buffer();
    let args = BenchmarkArgs {
        runs_per_model: 1,
        model_filter: None,
        mode_pair: false,
    };
    benchmark_cmd::run(&console, &mut s, &ReplState::default(), &args)
        .await
        .unwrap();
    let text = Console::buffer_text(&out);
    assert!(text.contains("No Python entry point found."), "{text}");
    assert!(text.contains("No Python entry point found"));
    assert!(text.contains("N/A"));

    // The fake server lists one model at /v1/models by default.
    let pair = BenchmarkArgs {
        runs_per_model: 1,
        model_filter: Some(vec![]),
        mode_pair: true,
    };
    let (console, out) = Console::buffer();
    benchmark_cmd::run(&console, &mut s, &ReplState::default(), &pair)
        .await
        .unwrap();
    let text = Console::buffer_text(&out);
    assert!(text.contains("Starting pair benchmark (every ordered model combination)..."));
    assert!(
        text.contains("Requires at least 2 models. Found: "),
        "{text}"
    );
}
