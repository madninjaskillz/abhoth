//! Port of tests/OpenOrca.Tools.Tests/PiSubagentToolTests.cs. The C# delegate properties are
//! `PiSubagentHooks` fields here (no cancellation token), set with `PiSubagentTool::set_hooks`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_pi::lead_mode::{self, PiLeadModeTool};
use orca_tools_pi::subagent::{
    self, BoxFuture, PiFileOwner, PiSubagentHooks, PiSubagentOutcome, PiSubagentTask,
};
use orca_tools_pi::{PiSubagentTool, PiToolInfo};
use serde_json::{Value, json};
use tokio::sync::watch;

type Ran = Arc<Mutex<Vec<PiSubagentTask>>>;
type Succeeds = Arc<dyn Fn(&PiSubagentTask) -> bool + Send + Sync>;

fn args(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn outcome(agent: &str, ok: bool, output: impl Into<String>) -> PiSubagentOutcome {
    PiSubagentOutcome {
        agent: agent.into(),
        succeeded: ok,
        output: output.into(),
    }
}

fn ready<T: Send + 'static>(value: T) -> BoxFuture<T> {
    Box::pin(async move { value })
}

fn hooks(ran: &Ran, succeeds: Option<Succeeds>) -> PiSubagentHooks {
    let ran = ran.clone();
    PiSubagentHooks {
        run_one: Some(Arc::new(move |t: PiSubagentTask| {
            ran.lock().unwrap().push(t.clone());
            let ok = succeeds.as_ref().map(|f| f(&t)).unwrap_or(true);
            let output = if ok {
                format!("out:{}", t.task)
            } else {
                "boom".into()
            };
            ready(outcome(&t.agent, ok, output))
        })),
        available_agents: Some(Arc::new(|| {
            vec![
                "planner".into(),
                "reviewer".into(),
                "scout".into(),
                "worker".into(),
            ]
        })),
        ..Default::default()
    }
}

fn tool_with(h: PiSubagentHooks) -> PiSubagentTool {
    let tool = PiSubagentTool::default();
    tool.set_hooks(h);
    tool
}

fn tool(ran: &Ran) -> PiSubagentTool {
    tool_with(hooks(ran, None))
}

fn new_ran() -> Ran {
    Arc::new(Mutex::new(Vec::new()))
}

async fn exec(tool: &PiSubagentTool, json: &str) -> ToolResult {
    tool.execute(&args(json), &ToolContext::new(".")).await
}

fn tasks_of(ran: &Ran) -> Vec<String> {
    ran.lock().unwrap().iter().map(|t| t.task.clone()).collect()
}

/// A `run_parallel` hook that records the tasks it got and answers "ok" (or "ok:<task>").
#[allow(clippy::type_complexity)]
fn recording_parallel(
    with_task: bool,
) -> (
    Arc<Mutex<Option<Vec<PiSubagentTask>>>>,
    Arc<dyn Fn(Vec<PiSubagentTask>) -> BoxFuture<Vec<PiSubagentOutcome>> + Send + Sync>,
) {
    let got: Arc<Mutex<Option<Vec<PiSubagentTask>>>> = Arc::new(Mutex::new(None));
    let g = got.clone();
    let f = Arc::new(move |tasks: Vec<PiSubagentTask>| {
        *g.lock().unwrap() = Some(tasks.clone());
        let outs = tasks
            .iter()
            .map(|t| {
                outcome(
                    &t.agent,
                    true,
                    if with_task {
                        format!("ok:{}", t.task)
                    } else {
                        "ok".into()
                    },
                )
            })
            .collect();
        ready(outs)
    });
    (got, f)
}

fn prompt_for(names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    orca_tools_pi::system_prompt::build(
        &names,
        &orca_tools_pi::prompt_snippets(),
        &orca_tools_pi::prompt_guidelines(),
        &[],
        "/w",
        None,
        false,
    )
}

#[test]
fn metadata_is_pi_shaped() {
    let tool = PiSubagentTool::default();
    assert_eq!("subagent", tool.name());
    assert!(
        tool.description()
            .starts_with("Delegate tasks to specialized subagents with isolated context.")
    );
    assert!(tool.prompt_snippet().contains("scout"));
    assert_eq!(2, tool.prompt_guidelines().len());
    assert!(orca_tools_pi::prompt_snippets().contains_key("subagent"));
    // The prompt only lists it when it is advertised.
    let core = orca_tools_pi::CORE_TOOL_NAMES;
    let prompt = prompt_for(&core);
    assert!(!prompt.contains("subagent"));
    let mut with: Vec<&str> = core.to_vec();
    with.push("subagent");
    let prompt = prompt_for(&with);
    assert!(prompt.contains("- subagent: Delegate work to subagents"));
    // The delegation workflow is the Lead's addendum (SystemPromptBuilder), not a rule.
    assert!(!prompt.contains("ONE subagent call"));
    assert!(subagent::LEAD_INSTRUCTIONS.contains("ONE subagent call with a tasks array"));
    assert!(prompt.contains("label of at most 4 words"));
    assert!(prompt.contains("\"Agent A: found 3 failing tests\""));
}

#[tokio::test]
async fn cap_refuses_a_call_with_more_new_agents_than_are_left_follow_ups_still_go() {
    let ran = new_ran();
    let t = tool_with(PiSubagentHooks {
        cap_check: Some(Arc::new(|n| {
            (n > 1).then(|| {
                format!("Agent cap reached: only 1 more can start, and this call asks for {n}.")
            })
        })),
        follow_up: Some(Arc::new(|to, _task| {
            ready(outcome("worker", true, format!("Agent {to}: done")))
        })),
        ..hooks(&ran, None)
    });

    let refused = exec(
        &t,
        r#"{"tasks": [{"agent": "worker", "task": "a"}, {"agent": "worker", "task": "b"}]}"#,
    )
    .await;
    assert!(refused.is_error);
    assert!(refused.content.contains("Agent cap reached"));
    assert!(ran.lock().unwrap().is_empty());

    assert!(
        !exec(&t, r#"{"agent": "worker", "task": "a"}"#)
            .await
            .is_error
    );
    assert!(!exec(&t, r#"{"to": "A", "task": "more"}"#).await.is_error);
    assert_eq!(1, ran.lock().unwrap().len());
}

#[test]
fn swarm_instructions_name_the_cap_and_small_tasks() {
    let text = subagent::swarm_instructions(10);
    assert!(text.contains("up to 10 subagents"));
    assert!(text.contains("many small, well-scoped tasks with small contexts"));
    assert!(text.contains("one file group per agent"));
    assert!(text.contains("follow-ups"));
    assert!(subagent::swarm_instructions(0).contains("as many subagents as the work needs"));
}

#[tokio::test]
async fn budget_note_ends_each_result_including_follow_ups_and_errors() {
    let ran = new_ran();
    let t = tool_with(PiSubagentHooks {
        budget_note: Some(Arc::new(|| {
            Some("Agent budget: 7 of 10 new agents left".into())
        })),
        follow_up: Some(Arc::new(|to, _task| {
            ready(outcome("worker", true, format!("Agent {to}: done")))
        })),
        ..hooks(&ran, None)
    });
    let note = "Agent budget: 7 of 10 new agents left";
    assert!(
        exec(&t, r#"{"agent": "worker", "task": "a"}"#)
            .await
            .content
            .ends_with(note)
    );
    assert!(
        exec(&t, r#"{"to": "A", "task": "more"}"#)
            .await
            .content
            .ends_with(note)
    );
    assert!(
        exec(
            &t,
            r#"{"tasks": [{"agent": "worker", "task": "a"}, {"agent": "worker", "task": "b"}]}"#
        )
        .await
        .content
        .ends_with(note)
    );
}

#[tokio::test]
async fn no_budget_note_leaves_the_result_as_it_was() {
    let ran = new_ran();
    let result = exec(&tool(&ran), r#"{"agent": "worker", "task": "a"}"#).await;
    assert!(!result.content.contains("Agent budget"));
}

#[test]
fn swarm_instructions_split_large_pieces_into_file_groups() {
    let text = subagent::swarm_instructions(10);
    assert!(text.contains(
        "Split any piece you expect to be large (e.g. a whole UI) across several agents"
    ));
    assert!(text.contains("grid view and filters"));
    assert!(text.contains("never give one agent a whole UI"));
}

#[test]
fn leads_are_told_to_check_plausibility_and_use_real_samples() {
    assert!(subagent::VERIFY_CHECKLIST.contains("ground truth on this machine"));
    assert!(
        subagent::VERIFY_CHECKLIST
            .contains("empty or zero result as a failure unless the ground truth is also empty")
    );
    assert!(subagent::LEAD_INSTRUCTIONS.contains(subagent::REAL_INPUT_RULE));
    assert!(subagent::orchestrator_instructions(true).contains(subagent::REAL_INPUT_RULE));
    assert!(subagent::orchestrator_instructions(true).contains(subagent::PLAUSIBILITY_RULE));
    assert!(subagent::REAL_INPUT_RULE.contains("read one real file first"));
    assert!(subagent::REAL_INPUT_RULE.contains("invented fixtures do not count as verification"));
}

#[tokio::test]
async fn follow_up_sends_the_task_to_the_named_agent() {
    let ran = new_ran();
    let sent: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let s = sent.clone();
    let t = tool_with(PiSubagentHooks {
        follow_up: Some(Arc::new(move |to: String, task: String| {
            s.lock().unwrap().push((to.clone(), task));
            ready(outcome(
                "worker",
                true,
                format!("Agent {to} (Fix it): fixed"),
            ))
        })),
        ..hooks(&ran, None)
    });

    let result = exec(&t, r#"{"to": " A ", "task": "line 3 is wrong"}"#).await;

    assert!(!result.is_error);
    assert!(result.content.contains("fixed"));
    assert_eq!(
        vec![("A".to_string(), "line 3 is wrong".to_string())],
        *sent.lock().unwrap()
    );
    assert!(ran.lock().unwrap().is_empty());
    assert!(t.parameter_schema()["properties"].get("to").is_some());
    assert!(t.description().contains("follow-up"));
}

#[tokio::test]
async fn follow_up_reports_the_hosts_error_and_needs_a_host() {
    let ran = new_ran();
    let t = tool(&ran);
    let missing = exec(&t, r#"{"to": "A", "task": "x"}"#).await;
    assert!(missing.is_error);
    assert!(missing.content.contains("spawn a new agent"));

    t.set_hooks(PiSubagentHooks {
        follow_up: Some(Arc::new(|_, _| {
            ready(outcome("?", false, "Agent A is still running"))
        })),
        ..hooks(&ran, None)
    });
    let busy = exec(&t, r#"{"to": "A", "task": "x"}"#).await;
    assert!(busy.is_error);
    assert!(busy.content.contains("still running"));
}

#[test]
fn schema_has_an_optional_label_in_every_mode() {
    let schema = PiSubagentTool::default().parameter_schema();
    let props = &schema["properties"];
    assert_eq!("string", props["label"]["type"].as_str().unwrap());
    for mode in ["tasks", "chain"] {
        let item = &props[mode]["items"];
        assert!(item["properties"].get("label").is_some());
        assert_eq!(json!(["agent", "task"]), item["required"]);
    }
    assert!(
        PiSubagentTool::default()
            .description()
            .contains("progress bar")
    );
}

#[tokio::test]
async fn labels_reach_the_host_in_every_mode() {
    let ran = new_ran();
    exec(
        &tool(&ran),
        r#"{"agent":"scout","task":"find auth","label":"Find auth"}"#,
    )
    .await;
    exec(
        &tool(&ran),
        r#"{"tasks":[{"agent":"scout","task":"a","label":"Scan A"},{"agent":"scout","task":"b"}]}"#,
    )
    .await;
    exec(
        &tool(&ran),
        r#"{"chain":[{"agent":"scout","task":"c","label":"  "}]}"#,
    )
    .await;
    let ran = ran.lock().unwrap();
    assert_eq!(Some("Find auth"), ran[0].label.as_deref());
    assert!(
        ran.iter()
            .any(|t| t.task == "a" && t.label.as_deref() == Some("Scan A"))
    );
    assert!(ran.iter().any(|t| t.task == "b" && t.label.is_none()));
    let c: Vec<_> = ran.iter().filter(|t| t.task == "c").collect();
    assert_eq!(1, c.len());
    assert!(c[0].label.is_none()); // blank = derive from the task
}

#[tokio::test]
async fn single_returns_the_agent_output() {
    let ran = new_ran();
    let result = exec(&tool(&ran), r#"{"agent":"scout","task":"find auth"}"#).await;
    assert!(!result.is_error);
    assert_eq!("out:find auth", result.content);
    assert_eq!(
        vec![PiSubagentTask::new("scout", "find auth")],
        *ran.lock().unwrap()
    );
}

#[tokio::test]
async fn single_failure_is_an_error() {
    let ran = new_ran();
    let t = tool_with(hooks(&ran, Some(Arc::new(|_| false))));
    let result = exec(&t, r#"{"agent":"worker","task":"x"}"#).await;
    assert!(result.is_error);
    assert_eq!("Agent worker failed: boom", result.content);
}

#[tokio::test]
async fn parallel_reports_every_task() {
    let ran = new_ran();
    let t = tool_with(hooks(&ran, Some(Arc::new(|t| t.task != "b"))));
    let result = exec(
        &t,
        r#"{"tasks":[{"agent":"scout","task":"a"},{"agent":"reviewer","task":"b"}]}"#,
    )
    .await;
    assert!(!result.is_error);
    assert!(result.content.starts_with("Parallel: 1/2 succeeded"));
    assert!(
        result
            .content
            .contains("## [scout] task 1 completed\nout:a")
    );
    assert!(result.content.contains("## [reviewer] task 2 failed\nboom"));
    assert_eq!(2, ran.lock().unwrap().len());
}

#[tokio::test]
async fn long_reports_are_cut_in_the_middle_for_the_lead_but_chain_steps_get_them_whole() {
    let report = format!("SUMMARY {} FINAL", "x".repeat(6000));
    let ran = new_ran();
    let r = ran.clone();
    let rep = report.clone();
    let t = tool_with(PiSubagentHooks {
        run_one: Some(Arc::new(move |task: PiSubagentTask| {
            r.lock().unwrap().push(task.clone());
            let out = if task.task.starts_with("use") {
                "ok".to_string()
            } else {
                rep.clone()
            };
            ready(outcome(&task.agent, true, out))
        })),
        ..Default::default()
    });

    let single = exec(&t, r#"{"agent":"worker","task":"a"}"#).await;
    assert!(single.content.starts_with("SUMMARY "));
    assert!(single.content.ends_with(" FINAL"));
    assert!(
        single
            .content
            .contains("chars of this report cut; send the agent a follow-up if you need them")
    );
    assert!(single.content.encode_utf16().count() < subagent::MAX_REPORT_CHARS + 120);

    let parallel = exec(
        &t,
        r#"{"tasks":[{"agent":"worker","task":"a"},{"agent":"worker","task":"b"}]}"#,
    )
    .await;
    assert!(parallel.content.encode_utf16().count() < 2 * (subagent::MAX_REPORT_CHARS + 200));
    assert_eq!(2, parallel.content.matches(" FINAL").count());

    ran.lock().unwrap().clear();
    exec(
        &t,
        r#"{"chain":[{"agent":"scout","task":"look"},{"agent":"worker","task":"use {previous}"}]}"#,
    )
    .await;
    assert_eq!(format!("use {report}"), ran.lock().unwrap()[1].task);

    // Short reports pass through untouched.
    assert_eq!(
        "ok",
        exec(&t, r#"{"agent":"worker","task":"use it"}"#)
            .await
            .content
    );
}

#[tokio::test]
async fn chain_passes_previous_output_and_stops_on_failure() {
    let ran = new_ran();
    let result = exec(
        &tool(&ran),
        r#"{"chain":[{"agent":"scout","task":"look"},{"agent":"planner","task":"plan from {previous}"}]}"#,
    )
    .await;
    assert!(!result.is_error);
    assert_eq!("plan from out:look", ran.lock().unwrap()[1].task);
    assert!(result.content.ends_with("out:plan from out:look"));

    ran.lock().unwrap().clear();
    let t = tool_with(hooks(&ran, Some(Arc::new(|t| t.agent != "scout"))));
    let result = exec(
        &t,
        r#"{"chain":[{"agent":"scout","task":"look"},{"agent":"planner","task":"plan"}]}"#,
    )
    .await;
    assert!(result.is_error);
    assert_eq!("Chain stopped at step 1 (scout): boom", result.content);
    assert_eq!(1, ran.lock().unwrap().len());
}

#[tokio::test]
async fn exactly_one_mode_is_required() {
    for json in [
        r#"{}"#,
        r#"{"agent":"scout","task":"a","tasks":[{"agent":"scout","task":"b"}]}"#,
    ] {
        let result = exec(&tool(&new_ran()), json).await;
        assert!(result.is_error, "{json}");
        assert!(
            result
                .content
                .starts_with("Invalid parameters. Provide exactly one mode."),
            "{}",
            result.content
        );
        assert!(result.content.contains("scout"));
    }
}

#[tokio::test]
async fn unknown_agent_lists_the_available_ones() {
    let result = exec(&tool(&new_ran()), r#"{"agent":"explorer","task":"x"}"#).await;
    assert!(result.is_error);
    assert_eq!(
        "Unknown agent: \"explorer\". Available agents: \"planner\", \"reviewer\", \"scout\", \"worker\".",
        result.content
    );
}

#[tokio::test]
async fn too_many_parallel_tasks_is_rejected() {
    let tasks: Vec<String> = (0..9)
        .map(|i| format!(r#"{{"agent":"scout","task":"t{i}"}}"#))
        .collect();
    let ran = new_ran();
    let result = exec(
        &tool(&ran),
        &format!(r#"{{"tasks":[{}]}}"#, tasks.join(",")),
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.contains("Max is 8"));
    assert!(ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unwired_reports_unavailable() {
    let result = exec(
        &PiSubagentTool::default(),
        r#"{"agent":"scout","task":"x"}"#,
    )
    .await;
    assert!(result.is_error);
}

#[test]
fn schema_validates_like_pi() {
    let tool = PiSubagentTool::default();
    assert!(matches!(
        tool.prepare_arguments(r#"{"agent":"scout","task":"x"}"#),
        Some(Ok(_))
    ));
    assert!(matches!(
        tool.prepare_arguments(r#"{"tasks":[{"agent":"scout"}]}"#),
        Some(Err(_))
    ));
}

#[tokio::test]
async fn parallel_size_reaches_the_host_and_the_schema_and_prompts_ask_for_it() {
    let (got, parallel) = recording_parallel(false);
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(parallel),
        ..hooks(&new_ran(), None)
    });

    let result = exec(
        &t,
        r#"{"tasks":[{"agent":"worker","task":"a","size":250},{"agent":"worker","task":"b","size":"40"},{"agent":"worker","task":"c","size":-3},{"agent":"worker","task":"d"}]}"#,
    )
    .await;

    assert!(!result.is_error);
    let sizes: Vec<Option<i32>> = got
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.size)
        .collect();
    assert_eq!(vec![Some(250), Some(40), None, None], sizes);
    assert!(t.parameter_schema().to_string().contains("\"size\""));
    assert!(subagent::LEAD_INSTRUCTIONS.contains("biggest piece first"));
    assert!(subagent::LEAD_INSTRUCTIONS.contains("\"size\": 200"));
    assert!(subagent::orchestrator_instructions(true).contains("list the biggest piece first"));
}

#[test]
fn lead_prompts_put_tests_with_the_code_and_cap_new_test_files() {
    for text in [
        subagent::LEAD_INSTRUCTIONS.clone(),
        subagent::orchestrator_instructions(true),
        subagent::orchestrator_instructions(false),
    ] {
        assert!(text.contains("each worker writes and runs the tests for its own piece"));
        assert!(text.contains("never a separate tests-only or docs-only task"));
        assert!(text.contains("(at most 12 focused cases)"));
    }
}

#[tokio::test]
async fn parallel_after_resolves_ids_labels_and_numbers_for_the_host() {
    let (got, parallel) = recording_parallel(false);
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(parallel),
        ..hooks(&new_ran(), None)
    });

    let result = exec(
        &t,
        r#"{"tasks":[{"agent":"worker","id":"eng","task":"engine"},{"agent":"worker","label":"Board module","task":"board"},{"agent":"worker","task":"cli","after":["eng","board module"]},{"agent":"worker","task":"docs","after":[3]}]}"#,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let got = got.lock().unwrap().clone().unwrap();
    assert_eq!(None, got[0].after);
    assert_eq!(Some(vec![0, 1]), got[2].after);
    assert_eq!(Some(vec![2]), got[3].after);
}

#[tokio::test]
async fn parallel_after_bad_references_are_rejected() {
    for (json, error) in [
        (
            r#"{"tasks":[{"agent":"worker","task":"a","after":["nope"]},{"agent":"worker","task":"b"}]}"#,
            "names no task in this call",
        ),
        (
            r#"{"tasks":[{"agent":"worker","id":"a","task":"a","after":["a"]},{"agent":"worker","task":"b"}]}"#,
            "cannot wait for itself",
        ),
        (
            r#"{"tasks":[{"agent":"worker","id":"a","task":"a","after":["b"]},{"agent":"worker","id":"b","task":"b","after":["a"]}]}"#,
            "cycle",
        ),
    ] {
        let ran = new_ran();
        let result = exec(&tool(&ran), json).await;
        assert!(result.is_error, "{json}");
        assert!(result.content.contains(error), "{}", result.content);
        assert!(ran.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn parallel_after_without_a_host_scheduler_runs_dependencies_first() {
    let ran = new_ran();
    let result = exec(
        &tool(&ran),
        r#"{"tasks":[{"agent":"worker","task":"cli","after":["2"]},{"agent":"worker","task":"engine"}]}"#,
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(vec!["engine", "cli"], tasks_of(&ran));
    assert!(
        subagent::orchestrator_instructions(true)
            .contains("then 2-3 balanced pieces with after: [its id]")
    );
    assert!(subagent::LEAD_INSTRUCTIONS.contains("then 2-3 balanced pieces with after: [its id]"));
}

#[tokio::test]
async fn a_task_with_no_agent_goes_to_the_worker() {
    for json in [
        r#"{"task":"fix the parser"}"#,
        r#"{"agent":"  ","task":"fix the parser"}"#,
    ] {
        let ran = new_ran();
        let result = exec(&tool(&ran), json).await;
        assert!(!result.is_error, "{}", result.content);
        let ran = ran.lock().unwrap();
        assert_eq!(1, ran.len());
        assert_eq!(subagent::DEFAULT_AGENT, ran[0].agent);
    }
}

#[tokio::test]
async fn parallel_and_chain_tasks_with_no_agent_go_to_the_worker_and_after_still_resolves() {
    let ran = new_ran();
    let result = exec(
        &tool(&ran),
        r#"{"tasks":[{"task":"cli","after":["eng"]},{"id":"eng","agent":"scout","task":"engine"}]}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(vec!["engine", "cli"], tasks_of(&ran));
    let agents: Vec<String> = ran
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.agent.clone())
        .collect();
    assert_eq!(vec!["scout", "worker"], agents);

    ran.lock().unwrap().clear();
    let result = exec(
        &tool(&ran),
        r#"{"chain":[{"task":"look"},{"agent":"planner","task":"plan"}]}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let agents: Vec<String> = ran
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.agent.clone())
        .collect();
    assert_eq!(vec!["worker", "planner"], agents);
}

#[test]
fn lead_prompts_depend_only_on_the_contract_and_look_at_the_program() {
    // Launcher bench: the UI waited for the foundation and both discovery workers (~60% of the
    // run); nobody looked at the rendered app before answering.
    for text in [
        subagent::LEAD_INSTRUCTIONS.clone(),
        subagent::orchestrator_instructions(true),
        subagent::orchestrator_instructions(false),
    ] {
        assert!(text.contains(subagent::AFTER_RULE));
        assert!(text.contains("never make a piece wait for its siblings"));
    }
    // C# LookRule is VerifyChecklist.
    for text in [
        subagent::LEAD_INSTRUCTIONS.clone(),
        subagent::orchestrator_instructions(true),
    ] {
        assert!(text.contains(subagent::VERIFY_CHECKLIST.as_str()));
        assert!(text.contains("job screenshot"));
        assert!(text.contains("never a key that launches, buys or deletes something"));
    }
    assert!(!subagent::orchestrator_instructions(false).contains("job screenshot")); // no shell, no jobs
    assert!(
        PiSubagentTool::default().parameter_schema()["properties"]["tasks"]
            .to_string()
            .contains("never siblings it reaches through the foundation's interfaces")
    );
}

#[tokio::test]
async fn chain_with_after_runs_as_a_dependency_graph_so_siblings_start_together() {
    // launcher bench, local arm: a chain whose steps 2 and 3 named only the scaffold ran 1 -> 2 -> 3.
    let (got, parallel) = recording_parallel(true);
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(parallel),
        ..hooks(&new_ran(), None)
    });

    let result = exec(
        &t,
        r#"{"chain":[{"agent":"worker","label":"Scaffold + Core API","task":"scaffold"},{"agent":"worker","label":"Core discovery","after":["Scaffold + Core API"],"task":"discovery"},{"agent":"worker","label":"Dashboard UI","after":["Scaffold + Core API"],"task":"ui uses {previous}"}]}"#,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let got = got.lock().unwrap().clone().unwrap();
    assert_eq!(None, got[0].after);
    assert_eq!(Some(vec![0]), got[1].after);
    assert_eq!(Some(vec![0]), got[2].after);
    assert!(!got[2].task.contains("{previous}"));
    assert!(result.content.contains("ran as a dependency graph"));
    assert!(result.content.contains("step 2 after 1; step 3 after 1"));
    assert!(result.content.contains("Parallel: 3/3 succeeded"));
}

#[tokio::test]
async fn chain_with_after_a_step_without_after_waits_for_the_previous_step() {
    let (got, parallel) = recording_parallel(false);
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(parallel),
        ..hooks(&new_ran(), None)
    });

    let result = exec(
        &t,
        r#"{"chain":[{"id":"a","task":"a"},{"task":"b","after":[]},{"task":"c"},{"task":"d","after":["a"]}]}"#,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let got = got.lock().unwrap().clone().unwrap();
    assert_eq!(None, got[0].after);
    assert_eq!(None, got[1].after); // explicit empty after: starts at once
    assert_eq!(Some(vec![1]), got[2].after); // no after: waits for the step before it
    assert_eq!(Some(vec![0]), got[3].after);
}

#[tokio::test]
async fn chain_with_after_without_a_host_scheduler_still_runs_in_dependency_order_and_bad_refs_are_rejected()
 {
    let ran = new_ran();
    let result = exec(
        &tool(&ran),
        r#"{"chain":[{"task":"ui","after":["core"]},{"id":"core","task":"core","after":[]}]}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(vec!["core", "ui"], tasks_of(&ran));

    ran.lock().unwrap().clear();
    let bad = exec(
        &tool(&ran),
        r#"{"chain":[{"task":"a"},{"task":"b","after":["nope"]}]}"#,
    )
    .await;
    assert!(bad.is_error);
    assert!(bad.content.contains("names no task in this call"));
    assert!(ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn chain_without_after_stays_sequential() {
    let ran = new_ran();
    let called = Arc::new(Mutex::new(false));
    let c = called.clone();
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(Arc::new(move |_tasks| {
            *c.lock().unwrap() = true;
            ready(Vec::new())
        })),
        ..hooks(&ran, None)
    });
    let result = exec(
        &t,
        r#"{"chain":[{"task":"look"},{"task":"use {previous}"}]}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(!*called.lock().unwrap());
    assert_eq!("use out:look", ran.lock().unwrap()[1].task);
    assert!(
        t.parameter_schema()["properties"]["chain"]
            .to_string()
            .contains("the chain runs as a dependency graph")
    );
}

#[tokio::test]
async fn after_naming_a_finished_task_from_an_earlier_call_counts_as_met() {
    // Launcher re-run (Haiku): the Lead ran the foundation (id "F") alone, then named "F" in
    // the next call's after and got "names no task in this call".
    let (got, parallel) = recording_parallel(false);
    let t = tool_with(PiSubagentHooks {
        run_parallel: Some(parallel),
        ..hooks(&new_ran(), None)
    });
    assert!(
        !exec(
            &t,
            r#"{"tasks":[{"id":"F","agent":"worker","label":"Foundation","task":"models"}]}"#
        )
        .await
        .is_error
    );

    let result = exec(
        &t,
        r#"{"tasks":[{"id":"S","agent":"worker","task":"steam","after":["F"]},{"agent":"worker","task":"ui","after":["Foundation"]}]}"#,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let got = got.lock().unwrap().clone().unwrap();
    assert_eq!(None, got[0].after);
    assert_eq!(None, got[1].after);
    assert!(
        result.content.starts_with(
            "after named \"F\", \"Foundation\" from an earlier call: it had already finished"
        ),
        "{}",
        result.content
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn after_naming_an_earlier_agent_by_letter_uses_the_host_and_waits_while_it_runs() {
    let (running_tx, running_rx) = watch::channel(false);
    let (_done_tx, done_rx) = watch::channel(true);
    let ran = new_ran();
    let r = ran.clone();
    let t = Arc::new(tool_with(PiSubagentHooks {
        earlier_agent: Some(Arc::new(move |name: &str| match name {
            "A" => Some(done_rx.clone()),
            "B" => Some(running_rx.clone()),
            _ => None,
        })),
        run_parallel: Some(Arc::new(move |tasks: Vec<PiSubagentTask>| {
            r.lock().unwrap().extend(tasks.iter().cloned());
            ready(
                tasks
                    .iter()
                    .map(|t| outcome(&t.agent, true, "ok"))
                    .collect(),
            )
        })),
        ..hooks(&ran, None)
    }));

    let t2 = t.clone();
    let call = tokio::spawn(async move {
        exec(
            &t2,
            r#"{"tasks":[{"agent":"worker","task":"x","after":["A","B"]},{"agent":"worker","task":"y"}]}"#,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!call.is_finished()); // B is still running
    assert!(ran.lock().unwrap().is_empty());
    running_tx.send_replace(true);
    let result = call.await.unwrap();

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(2, ran.lock().unwrap().len());
    assert!(
        result.content.starts_with(
            "after named \"A\", \"B\" from an earlier call: this call waited for it to finish first."
        ),
        "{}",
        result.content
    );
    let bad = exec(
        &t,
        r#"{"tasks":[{"agent":"worker","task":"x","after":["Z"]},{"agent":"worker","task":"y"}]}"#,
    )
    .await;
    assert!(bad.is_error);
    assert!(
        bad.content
            .contains("names no task in this call and no earlier agent")
    );
}

fn owner(path: &str, agent: &str) -> PiFileOwner {
    PiFileOwner {
        path: path.into(),
        agent: agent.into(),
    }
}

#[tokio::test]
async fn new_code_task_naming_a_file_an_earlier_agent_wrote_is_sent_back_to_the_writer() {
    // Local launcher run: "Follow-up on your App engine work. The running app crashed..." went
    // to a new agent instead of {"to": "C"}.
    let ran = new_ran();
    let t = tool_with(PiSubagentHooks {
        file_owners: Some(Arc::new(|| {
            vec![
                owner("src/Launcher.App/Views/GridView.cs", "D"),
                owner("C:/repo/src/Launcher.App/Theme.cs", "C"),
            ]
        })),
        ..hooks(&ran, None)
    });

    let fix = exec(
        &t,
        r#"{"agent":"worker","task":"The app crashed in the grid: fix the font use in GridView.cs and Theme.cs"}"#,
    )
    .await;
    assert!(fix.is_error);
    assert!(
        fix.content
            .contains("GridView.cs (Agent D), Theme.cs (Agent C)"),
        "{}",
        fix.content
    );
    assert!(fix.content.contains("{\"to\": \"D\", \"task\":"));
    assert!(fix.content.contains("add \"new\": true"));
    assert!(ran.lock().unwrap().is_empty());

    // Opt-out, read-only agents, and names that only share a suffix go through.
    assert!(
        !exec(
            &t,
            r#"{"agent":"worker","new":true,"task":"Add a settings page next to GridView.cs"}"#
        )
        .await
        .is_error
    );
    assert!(
        !exec(&t, r#"{"agent":"scout","task":"Explain GridView.cs"}"#)
            .await
            .is_error
    );
    assert!(
        !exec(
            &t,
            r#"{"agent":"worker","task":"Write ListView.cs and View.cs"}"#
        )
        .await
        .is_error
    );
    assert_eq!(3, ran.lock().unwrap().len());
    assert!(
        t.parameter_schema()["properties"]["tasks"]
            .to_string()
            .contains("\"new\"")
    );
}

#[test]
fn owned_files_error_names_the_task_in_a_batch_and_matches_whole_file_names() {
    let owners = [owner("src/Input.cs", "B")];
    assert!(
        subagent::owned_files_error(
            &[PiSubagentTask::new("worker", "write MyInput.cs")],
            &owners
        )
        .is_none()
    );
    assert!(
        subagent::owned_files_error(
            &[PiSubagentTask {
                is_new: true,
                ..PiSubagentTask::new("worker", "fix Input.cs")
            }],
            &owners
        )
        .is_none()
    );
    assert!(
        subagent::owned_files_error(&[PiSubagentTask::new("worker", "fix Input.cs")], &[])
            .is_none()
    );
    let e = subagent::owned_files_error(
        &[
            PiSubagentTask::new("worker", "docs"),
            PiSubagentTask::new("worker", "fix src/Input.cs"),
        ],
        &owners,
    )
    .unwrap();
    assert!(
        e.starts_with("Task 2 changes code that earlier agents wrote"),
        "{e}"
    );
}

#[test]
fn lead_prompts_send_fixes_to_the_writer() {
    assert!(subagent::LEAD_INSTRUCTIONS.contains(subagent::FIX_RULE));
    assert!(
        subagent::orchestrator_instructions(true)
            .contains("Each result lists the files its agent wrote")
    );
}

#[test]
fn lead_prompts_have_both_shapes_and_use_after_only_for_code_that_must_exist() {
    // Coupled and big: a small foundation, then 2-3 balanced pieces with exact names; coupled and small: hands-on.
    for text in [
        subagent::LEAD_INSTRUCTIONS.clone(),
        subagent::orchestrator_instructions(true),
    ] {
        // multi-sales: a foundation piece and a report waiting on three parsers the spec fully defines made three waves.
        assert!(text.contains("If every piece can be written straight from the spec, start them all at once: no foundation piece and no after."));
        assert!(text.contains(
            "(e.g. a report reading the parsers' spec-defined output) runs alongside them"
        ));
        assert!(text.contains("Only when the spec leaves shared types or helpers undefined does a small foundation piece go first"));
        assert!(text.contains("names the exact functions, fields and messages it uses from the foundation, and the files it must not touch"));
        assert!(text.contains("(at most 12 focused cases)"));
        assert!(text.contains("never a separate tests-only or docs-only task"));
        // t3-cli: "or --store-style override if any" in a brief produced a non-spec flag and a fix-up turn.
        assert!(text.contains("no open invitations such as \"or an override if any\""));
    }
    assert!(
        lead_mode::AUTO_INSTRUCTIONS
            .contains("tightly coupled work under roughly 300 new lines even across several files")
    );
    assert!(
        PiLeadModeTool::default()
            .description()
            .contains("tightly coupled work under roughly 300 new lines")
    );
}

#[test]
fn auto_instructions_ask_for_parallel_reads_brief_planning_and_short_hands_on_answers() {
    // t3-cli: 5.7-7.7k thinking tokens and up to 7 one-read turns before the first spawn;
    // kvstore/logsummary: hands-on runs skip lead_mode, so these rules live in the system prompt.
    let text = lead_mode::AUTO_INSTRUCTIONS;
    assert!(text.contains("put every read you need into ONE turn"));
    assert!(
        text.contains("call lead_mode in your next turn together with any reads you still need")
    );
    assert!(text.contains("Think briefly. To delegate, plan only the split"));
    assert!(text.contains("do not work out code, formats or test cases yourself"));
    assert!(text.contains("answer in at most 3 short lines, no lists"));
    assert!(
        !PiLeadModeTool::default()
            .description()
            .contains("Call this FIRST")
    );
    assert!(
        PiSubagentTool::default()
            .description()
            .contains("name the files and the spec section instead of copying them")
    );
}
