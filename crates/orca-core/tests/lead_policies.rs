//! Lead policies that depend on agents, ported from `tests/OpenOrca.Cli.Tests`:
//! `AutoLeadModeTests.cs` (the `LeadModePolicy` checks), `AgentUiTests.PlanMode_AllowsReadOnly
//! SubagentTypes_Only` and `DelegatedCheckMessageTests.cs`, and the
//! `ToolCallExecutorTests` delegated-check tests (`DelegatedWithoutCheck`, `DelegatesCodeWork`).
//!
//! Not ported from `AutoLeadModeTests.cs`: the tool filtering (`FilterForLead`,
//! `WithoutLeadMode`), the lead_mode tool and its chooser (`LeadModeChooser`, `PiLeadModeTool`),
//! the Pi/OpenOrca prompt addenda and the Lead bar label belong to the Pi tools and the UI,
//! which are ported elsewhere. `DelegateGateTests.cs` needs slots (ported with slots).

use std::sync::Arc;

use orca_abstractions::{Tool, ToolContext, ToolResult, ToolRiskLevel};
use orca_core::agent::policies::{
    AutoLeadVerdict, LEAD_MODE_DELEGATE, LEAD_MODE_HANDS_ON, check_auto_lead, check_plan_mode_with,
};
use orca_core::agent::prompt_constants::PI_DELEGATED_CHECK_MESSAGE;
use orca_core::agent::{
    ToolCallRecord, ToolRegistry, delegated_without_check, delegates_code_work,
};
use orca_core::config::OrcaConfig;
use orca_core::orchestration::types::{AgentTypeDefinition, AgentTypeRegistry};
use serde_json::{Value, json};

fn config(profile: &str, mode: &str) -> OrcaConfig {
    let mut config = OrcaConfig {
        tool_profile: profile.into(),
        ..OrcaConfig::default()
    };
    config.lm_studio.native_tool_calling = true;
    config.agent.lead_mode = mode.into();
    config
}

fn auto() -> OrcaConfig {
    config("pi", "auto")
}

fn blocked_message(v: &AutoLeadVerdict) -> Option<&str> {
    match v {
        AutoLeadVerdict::Blocked(b) => Some(&b.message),
        _ => None,
    }
}

#[test]
fn auto_refuses_writes_only_after_delegate() {
    // (chosen mode, tool, blocked); lead_mode not advertised, as in the C# policy without it.
    let cases: &[(Option<&str>, &str, bool)] = &[
        (None, "write", false), // not chosen yet: works as hands-on
        (Some("hands-on"), "write", false),
        (Some("answer"), "edit", false),
        (Some("delegate"), "write", true),
        (Some("delegate"), "edit", true),
        (Some("delegate"), "bash", false), // bash stays for checks
        (Some("delegate"), "read", false),
        (Some("delegate"), "subagent", false),
    ];
    for &(chosen, tool, blocked) in cases {
        let verdict = check_auto_lead(&auto(), chosen, false, tool);
        let message = blocked_message(&verdict);
        assert_eq!(message.is_some(), blocked, "{chosen:?} {tool}");
        if let Some(m) = message {
            assert!(m.contains("You chose delegate"), "{m}");
            assert!(m.contains("call lead_mode with hands-on"), "{m}");
        }
    }
}

#[test]
fn auto_before_a_choice_changes_and_delegation_wait_for_lead_mode() {
    let cases: &[(&str, bool)] = &[
        ("write", false), // counts as choosing hands-on (below)
        ("edit", false),
        ("subagent", true),
        ("spawn_agents", true),
        ("read", false),
        ("bash", false),
        ("lead_mode", false),
    ];
    for &(tool, blocked) in cases {
        let verdict = check_auto_lead(&auto(), None, true, tool);
        let message = blocked_message(&verdict);
        assert_eq!(message.is_some(), blocked, "{tool}");
        if let Some(m) = message {
            assert!(m.contains("size this request first. Call lead_mode"), "{m}");
        }
        // Without lead_mode advertised (e.g. a profile without agent tools) nothing waits.
        assert!(
            blocked_message(&check_auto_lead(&auto(), None, false, tool)).is_none(),
            "{tool}"
        );
    }
}

#[test]
fn auto_a_write_before_any_choice_chooses_hands_on_and_runs() {
    // The Lead often writes the file first; refusing it made it generate the whole file again.
    for tool in ["write", "edit", "write_file"] {
        let verdict = check_auto_lead(&auto(), None, true, tool);
        let AutoLeadVerdict::ChooseHandsOn(reason) = verdict else {
            panic!("{tool}: {verdict:?}");
        };
        assert!(reason.contains(tool), "{reason}");
        // Later writes in the request run as hands-on; delegating is allowed too (a mode is set).
        let chosen = Some(LEAD_MODE_HANDS_ON);
        assert!(matches!(
            check_auto_lead(&auto(), chosen, true, "edit"),
            AutoLeadVerdict::Run
        ));
        assert!(matches!(
            check_auto_lead(&auto(), chosen, true, "subagent"),
            AutoLeadVerdict::Run
        ));
    }
}

#[test]
fn auto_reads_and_delegation_before_a_choice_do_not_choose_a_mode() {
    assert!(matches!(
        check_auto_lead(&auto(), None, true, "read"),
        AutoLeadVerdict::Run
    ));
    assert!(matches!(
        check_auto_lead(&auto(), None, true, "subagent"),
        AutoLeadVerdict::Blocked(_)
    ));
}

#[test]
fn forced_modes_never_apply_the_auto_policy() {
    // (Rust-only) hands-on / orchestrator leads skip the auto checks entirely.
    for mode in ["hands-on", "orchestrator"] {
        let c = config("pi", mode);
        assert!(matches!(
            check_auto_lead(&c, Some(LEAD_MODE_DELEGATE), true, "write"),
            AutoLeadVerdict::Run
        ));
        assert!(matches!(
            check_auto_lead(&c, None, true, "subagent"),
            AutoLeadVerdict::Run
        ));
    }
}

/// A stand-in for the Pi tools of the C# test: only the name and risk level matter.
struct NamedTool(&'static str, ToolRiskLevel);

#[async_trait::async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        self.0
    }
    fn risk_level(&self) -> ToolRiskLevel {
        self.1
    }
    fn parameter_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::default()
    }
}

#[test]
fn plan_mode_allows_read_only_subagent_types_only() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NamedTool("read", ToolRiskLevel::ReadOnly)));
    registry.register(Arc::new(NamedTool("subagent", ToolRiskLevel::Moderate)));
    let mut types = AgentTypeRegistry::new();
    types.register(AgentTypeDefinition::new(
        "lookup",
        "read only",
        Some(vec!["read".into()]),
        "x.md",
    ));
    types.register(AgentTypeDefinition::new("fixer", "writes", None, "x.md"));
    let check = |args: &str| check_plan_mode_with(true, &registry, "subagent", args, Some(&types));

    assert!(check(r#"{"agent":"lookup","task":"x"}"#).is_none());
    assert!(check(r#"{"tasks":[{"agent":"lookup","task":"x"}]}"#).is_none());
    assert!(
        check(r#"{"chain":[{"agent":"lookup","task":"x"},{"agent":"fixer","task":"y"}]}"#)
            .is_some()
    );
}

#[test]
fn pi_check_has_the_checklist_and_the_plausibility_step() {
    let text = PI_DELEGATED_CHECK_MESSAGE;
    assert!(text.contains("shot <move focus>"));
    assert!(text.contains("ground truth on this machine"));
    assert!(text.contains("empty or zero result as a failure"));
    assert!(text.contains("invented fixtures is not verified"));
}

// ── LeadModeTests (policy parts) ────────────────────────────────────────────
// The tool-list filtering (`FilterForLead`) is applied where the Lead's tools are advertised
// through `is_denied_for_lead`; the orchestrator instructions and prompt addenda are Pi/prompt
// tests ported with those.

#[test]
fn lead_mode_defaults_to_auto_with_bash() {
    let c = OrcaConfig::default();
    assert_eq!(c.agent.lead_mode, "auto");
    assert!(orca_core::orchestration::is_auto_lead(&c));
    assert!(c.agent.lead_bash);
    assert!(!orca_core::agent::policies::is_orchestrator_lead(&c));
}

#[test]
fn hands_on_keeps_every_tool() {
    let c = config("pi", "hands-on");
    for tool in ["read", "bash", "edit", "write", "subagent", "progress"] {
        assert!(!orca_core::agent::policies::is_denied_for_lead(&c, tool));
    }
    assert!(orca_core::agent::policies::check_lead_mode(&c, "write").is_none());
}

#[test]
fn orchestrator_pi_drops_write_and_edit_keeps_read_search_shell_and_delegation() {
    use orca_core::agent::policies::is_denied_for_lead;
    let c = config("pi", "orchestrator");
    let lead = [
        "read", "bash", "edit", "write", "grep", "find", "ls", "subagent", "progress",
    ];
    let kept: Vec<&str> = lead
        .into_iter()
        .filter(|t| !is_denied_for_lead(&c, t))
        .collect();
    assert_eq!(
        kept,
        ["read", "bash", "grep", "find", "ls", "subagent", "progress"]
    );
    let mut strict = c.clone();
    strict.agent.lead_bash = false;
    let kept: Vec<&str> = lead
        .into_iter()
        .filter(|t| !is_denied_for_lead(&strict, t))
        .collect();
    assert_eq!(kept, ["read", "grep", "find", "ls", "subagent", "progress"]);
}

#[test]
fn orchestrator_refuses_a_write_call_anyway_with_a_pointer_to_subagent() {
    use orca_core::agent::policies::check_lead_mode;
    let c = config("pi", "orchestrator");
    let blocked = check_lead_mode(&c, "edit").expect("edit is refused");
    assert!(blocked.is_error);
    assert!(blocked.message.contains("subagent"), "{}", blocked.message);
    assert!(check_lead_mode(&c, "bash").is_none());
    let mut strict = c.clone();
    strict.agent.lead_bash = false;
    assert!(check_lead_mode(&strict, "bash").is_some());
    assert!(check_lead_mode(&c, "read").is_none());
}

#[test]
fn reviewer_agents_get_a_smaller_iteration_budget() {
    use orca_core::orchestration::iterations_for;
    let cases: &[(&str, Option<i32>, i32)] = &[
        ("reviewer", None, 15),
        ("Review", None, 15),
        ("worker", None, 40),
        ("scout", None, 40),
        ("reviewer", Some(25), 25),
    ];
    for &(ty, type_override, expected) in cases {
        let mut c = OrcaConfig::default();
        assert_eq!(iterations_for(&c, ty, type_override), expected, "{ty}");
        c.agent.reviewer_max_iterations = 0;
        assert_eq!(
            iterations_for(&c, ty, type_override),
            type_override.unwrap_or(40),
            "{ty}"
        );
    }
}

// ── ToolCallExecutorTests: delegated work needs a final check ───────────────

fn record(name: &str, arguments: &str, result: &str, is_error: bool) -> ToolCallRecord {
    ToolCallRecord {
        name: name.into(),
        arguments: Some(arguments.into()),
        result: Some(result.into()),
        is_error,
        duration_ms: 0,
    }
}

#[test]
fn delegated_without_check_true_until_a_passing_run_after_the_last_delegation() {
    let mut history: Vec<ToolCallRecord> = Vec::new();
    assert!(!delegated_without_check(&history)); // nothing delegated

    history.push(record(
        "bash",
        r#"{"command":"dotnet test"}"#,
        "Passed!  - Failed: 0, Passed: 5",
        false,
    ));
    history.push(record(
        "subagent",
        r#"{"tasks":[{"agent":"worker","task":"build it"}]}"#,
        "Parallel: 1/1 succeeded",
        false,
    ));
    assert!(delegated_without_check(&history)); // the earlier pass came before the agents

    history.push(record(
        "bash",
        r#"{"command":"dotnet test"}"#,
        "Failed!  - Failed: 2, Passed: 3",
        true,
    ));
    assert!(delegated_without_check(&history)); // a failing run is not a check that passed

    history.push(record(
        "bash",
        r#"{"command":"dotnet test"}"#,
        "Passed!  - Failed: 0, Passed: 5",
        false,
    ));
    assert!(!delegated_without_check(&history));

    history.push(record(
        "subagent",
        r#"{"to":"A","task":"fix the filter"}"#,
        "Agent A (UI): fixed",
        false,
    ));
    assert!(delegated_without_check(&history)); // a follow-up changed code again
}

#[test]
fn delegates_code_work_only_for_agents_that_can_change_code() {
    let cases: &[(&str, bool)] = &[
        (r#"{"agent":"scout","task":"find the parser"}"#, false),
        (
            r#"{"tasks":[{"agent":"scout","task":"a"},{"agent":"reviewer","task":"b"}]}"#,
            false,
        ),
        (
            r#"{"tasks":[{"agent":"scout","task":"a"},{"agent":"worker","task":"b"}]}"#,
            true,
        ),
        (r#"{"chain":[{"task":"no agent means the worker"}]}"#, true),
        (r#"{"to":"B","task":"continue"}"#, true),
        (r#"{"agent_type":"explore","task":"look"}"#, false),
        (r#"{"agent_type":"general","task":"build"}"#, true),
        ("not json", true),
    ];
    for &(args, expected) in cases {
        assert_eq!(delegates_code_work(Some(args)), expected, "{args}");
    }
}
