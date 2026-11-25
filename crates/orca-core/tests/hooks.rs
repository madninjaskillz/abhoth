//! Ported from tests/OpenOrca.Core.Tests/HookRunnerTests.cs, plus the hook environment and
//! background post-hooks.

use std::sync::Arc;
use std::time::Duration;

use orca_core::config::HooksConfig;
use orca_core::hooks::HookRunner;

fn runner(config: HooksConfig) -> HookRunner {
    HookRunner::new(config)
}

fn exit1() -> &'static str {
    if cfg!(windows) { "exit /b 1" } else { "exit 1" }
}

#[tokio::test]
async fn run_pre_hook_async_returns_true_when_no_hook_configured() {
    assert!(
        runner(HooksConfig::default())
            .run_pre_hook("bash", "{}")
            .await
    );
}

#[tokio::test]
async fn run_pre_hook_async_runs_specific_hook() {
    let mut c = HooksConfig::default();
    c.pre_tool_hooks.insert("bash".into(), "echo ok".into());
    assert!(runner(c).run_pre_hook("bash", "{}").await);
}

#[tokio::test]
async fn run_pre_hook_async_blocks_tool_when_hook_exits_non_zero() {
    let mut c = HooksConfig::default();
    c.pre_tool_hooks.insert("bash".into(), exit1().into());
    assert!(!runner(c).run_pre_hook("bash", "{}").await);
}

#[tokio::test]
async fn run_pre_hook_async_wildcard_hook_matches_any_tool() {
    let mut c = HooksConfig::default();
    c.pre_tool_hooks.insert("*".into(), "echo wildcard".into());
    assert!(runner(c).run_pre_hook("any_tool_name", "{}").await);
}

#[tokio::test]
async fn run_pre_hook_async_specific_hook_takes_precedence_over_wildcard() {
    let mut c = HooksConfig::default();
    c.pre_tool_hooks.insert("bash".into(), exit1().into()); // blocks
    c.pre_tool_hooks.insert("*".into(), "echo ok".into()); // allows
    let r = runner(c);
    assert!(!r.run_pre_hook("bash", "{}").await);
    assert!(r.run_pre_hook("other_tool", "{}").await);
}

#[tokio::test]
async fn run_post_hook_async_does_not_throw_when_no_hook_configured() {
    runner(HooksConfig::default())
        .run_post_hook("bash", "{}", "result", false)
        .await;
}

#[tokio::test]
async fn run_post_hook_async_does_not_throw_when_hook_fails() {
    let mut c = HooksConfig::default();
    c.post_tool_hooks.insert("bash".into(), exit1().into());
    runner(c).run_post_hook("bash", "{}", "result", false).await;
}

// ── Beyond the C# tests ──

/// The hook gets the tool name, arguments, result and error flag in its environment.
#[tokio::test]
async fn post_hook_in_background_sees_the_environment_and_can_be_awaited() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hook.txt");
    let command = if cfg!(windows) {
        format!(
            "echo %ORCA_TOOL_NAME%^|%ORCA_TOOL_ERROR%^|%ORCA_TOOL_RESULT%> \"{}\"",
            out.display()
        )
    } else {
        format!(
            "echo \"$ORCA_TOOL_NAME|$ORCA_TOOL_ERROR|$ORCA_TOOL_RESULT|$ORCA_TOOL_ARGS\" > '{}'",
            out.display()
        )
    };
    let mut c = HooksConfig::default();
    c.post_tool_hooks.insert("*".into(), command);
    let r = Arc::new(runner(c));
    r.run_post_hook_in_background("write_file", r#"{"path":"a"}"#, "done", true);
    r.wait_for_pending_post_hooks(Duration::from_secs(20)).await;
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("write_file|True|done"), "{text}");
    if !cfg!(windows) {
        assert!(text.contains(r#"{"path":"a"}"#), "{text}");
    }
}

#[tokio::test]
async fn pre_hook_sees_no_result_variable() {
    let mut c = HooksConfig::default();
    // Exits 0 only when ORCA_TOOL_RESULT is unset and ORCA_TOOL_ERROR is False.
    let command = if cfg!(windows) {
        "if defined ORCA_TOOL_RESULT (exit /b 1) else if not \"%ORCA_TOOL_ERROR%\"==\"False\" (exit /b 2) else (exit /b 0)"
    } else {
        "[ -z \"${ORCA_TOOL_RESULT+x}\" ] && [ \"$ORCA_TOOL_ERROR\" = False ]"
    };
    c.pre_tool_hooks.insert("bash".into(), command.into());
    assert!(runner(c).run_pre_hook("bash", "{}").await);
}
