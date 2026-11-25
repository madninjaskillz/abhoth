//! Port of tests/OpenOrca.Tools.Tests/PiProgressToolTests.cs.

use std::sync::{Arc, Mutex};

use orca_abstractions::{Tool, ToolContext, ToolRiskLevel};
use orca_tools_pi::{
    CORE_TOOL_NAMES, PiProgressTool, prompt_guidelines, prompt_snippets, system_prompt,
};
use serde_json::Value;

fn args(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

/// What the report hook received: (percent, note).
type Reported = (i32, Option<String>);

fn ctx() -> ToolContext {
    ToolContext::new(std::env::temp_dir())
}

#[test]
fn metadata_is_pi_shaped() {
    let tool = PiProgressTool::default();
    assert_eq!(tool.name(), "progress");
    assert_eq!(
        tool.description(),
        "Update your progress bar shown to the user. percent is your estimate for the whole task (0-100); note is what you are doing now, at most 4 words."
    );
    assert_eq!(tool.risk_level(), ToolRiskLevel::ReadOnly);
    let schema = tool.parameter_schema();
    let props = &schema["properties"];
    assert_eq!(props["percent"]["type"].as_str(), Some("number"));
    assert_eq!(props["note"]["type"].as_str(), Some("string"));
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    assert_eq!(required, ["percent"]);

    // Listed in the Pi prompt only when advertised.
    let core: Vec<String> = CORE_TOOL_NAMES.iter().map(|s| s.to_string()).collect();
    let prompt = system_prompt::build(
        &core,
        &prompt_snippets(),
        &prompt_guidelines(),
        &[],
        "/w",
        None,
        false,
    );
    assert!(!prompt.contains("progress"), "{prompt}");
    let mut with_progress = core.clone();
    with_progress.push("progress".into());
    let prompt = system_prompt::build(
        &with_progress,
        &prompt_snippets(),
        &prompt_guidelines(),
        &[],
        "/w",
        None,
        false,
    );
    assert!(
        prompt.contains("- progress: Show the user how far along you are (progress bar)"),
        "{prompt}"
    );
    assert!(
        prompt.contains("- Use progress at real milestones of longer tasks"),
        "{prompt}"
    );
}

#[tokio::test]
async fn reports_percent_and_note() {
    let cases: [(&str, i32, Option<&str>); 4] = [
        (
            r#"{"percent":40,"note":"reading tests"}"#,
            40,
            Some("reading tests"),
        ),
        (r#"{"percent":"75%"}"#, 75, None),
        (r#"{"percent":0.5}"#, 50, None),
        (r#"{"percent":250,"note":" "}"#, 100, None),
    ];
    for (json, percent, note) in cases {
        let got: Arc<Mutex<Option<Reported>>> = Arc::new(Mutex::new(None));
        let tool = PiProgressTool::default();
        let sink = got.clone();
        tool.set_report(Some(Arc::new(move |p, n| {
            *sink.lock().unwrap() = Some((p, n.map(str::to_string)));
            format!("Progress recorded: {p}%.")
        })));
        let result = tool.execute(&args(json), &ctx()).await;
        assert!(!result.is_error, "{json}: {}", result.content);
        assert_eq!(result.content, format!("Progress recorded: {percent}%."));
        assert_eq!(
            *got.lock().unwrap(),
            Some((percent, note.map(str::to_string))),
            "{json}"
        );
    }
}

#[tokio::test]
async fn bad_percent_is_rejected() {
    for json in [r#"{}"#, r#"{"percent":-5}"#, r#"{"percent":"lots"}"#] {
        let tool = PiProgressTool::default();
        tool.set_report(Some(Arc::new(|_, _| "x".to_string())));
        let result = tool.execute(&args(json), &ctx()).await;
        assert!(result.is_error, "{json}");
        assert_eq!(result.content, "percent must be a number from 0 to 100.");
    }
}

#[tokio::test]
async fn unwired_fails_cleanly() {
    let result = PiProgressTool::default()
        .execute(&args(r#"{"percent":10}"#), &ctx())
        .await;
    assert!(result.is_error);
}
