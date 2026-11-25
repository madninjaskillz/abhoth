//! Pi-style `subagent` custom tool (`PiSubagentTool`, after Pi's subagent extension): delegate
//! work to specialized agents with isolated context: single (agent + task), parallel (tasks),
//! chain (sequential, `{previous}` = prior output) and follow-up (to + task). The host runs the
//! agents through [`PiSubagentHooks`]; this module owns the Pi-facing contract: schema, prompt
//! snippet and guidelines, mode handling and the result text.
//!
//! Gap: the Rust CLI has no agent orchestrator yet, so no host wires `run_one`; the tool then
//! answers "Subagents are not available in this session." (C#'s text for an unwired tool).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use tokio::sync::watch;

use crate::{PiToolInfo, fail, fail_with, get_string};

pub const TOOL_NAME: &str = "subagent";
pub const MAX_PARALLEL_TASKS: usize = 8;
/// The agent a task without one goes to.
pub const DEFAULT_AGENT: &str = "worker";
/// Most of one agent's report the Lead gets back (UTF-16 units, as C# counts).
pub const MAX_REPORT_CHARS: usize = 3000;

/// One delegated task (`PiSubagentTask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSubagentTask {
    pub agent: String,
    pub task: String,
    pub label: Option<String>,
    pub size: Option<i32>,
    /// Indices of tasks in the same call that must finish first.
    pub after: Option<Vec<usize>>,
    pub is_new: bool,
}

impl PiSubagentTask {
    pub fn new(agent: impl Into<String>, task: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            task: task.into(),
            label: None,
            size: None,
            after: None,
            is_new: false,
        }
    }
}

/// A file an earlier agent wrote, and that agent's letter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiFileOwner {
    pub path: String,
    pub agent: String,
}

/// What one sub-agent run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSubagentOutcome {
    pub agent: String,
    pub succeeded: bool,
    pub output: String,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Done signal of an earlier agent or call: `true` once it finished.
pub type DoneSignal = watch::Receiver<bool>;

/// Looks up an earlier call's task or agent by name.
pub type EarlierLookup<'a> = &'a dyn Fn(&str) -> Option<DoneSignal>;

/// The host side of the tool (the C# delegate properties). All optional; without `run_one`
/// the tool reports that subagents are not available.
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
pub struct PiSubagentHooks {
    pub run_one: Option<Arc<dyn Fn(PiSubagentTask) -> BoxFuture<PiSubagentOutcome> + Send + Sync>>,
    pub run_parallel:
        Option<Arc<dyn Fn(Vec<PiSubagentTask>) -> BoxFuture<Vec<PiSubagentOutcome>> + Send + Sync>>,
    pub follow_up:
        Option<Arc<dyn Fn(String, String) -> BoxFuture<PiSubagentOutcome> + Send + Sync>>,
    pub available_agents: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    pub earlier_agent: Option<Arc<dyn Fn(&str) -> Option<DoneSignal> + Send + Sync>>,
    pub cap_check: Option<Arc<dyn Fn(usize) -> Option<String> + Send + Sync>>,
    pub hand_off_check: Option<Arc<dyn Fn() -> BoxFuture<Option<String>> + Send + Sync>>,
    pub start_background: Option<Arc<dyn Fn(Vec<PiSubagentTask>) -> String + Send + Sync>>,
    pub wait_background:
        Option<Arc<dyn Fn() -> BoxFuture<Option<Vec<PiSubagentOutcome>>> + Send + Sync>>,
    pub budget_note: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub file_owners: Option<Arc<dyn Fn() -> Vec<PiFileOwner> + Send + Sync>>,
}

/// Host settings that shape the description, schema and prompt metadata.
#[derive(Debug, Clone)]
pub struct PiSubagentSettings {
    /// "scout (...), worker (...)": set by the host from its agent registry.
    pub agent_list: String,
    pub peer_mode: bool,
    pub offers_wait: bool,
    pub orchestrator_lead: bool,
    pub auto_lead: bool,
}

pub const DEFAULT_AGENT_LIST: &str = "scout (fast read-only recon; returns files with line ranges for handoff), planner (implementation plans), reviewer (code review), worker (full capabilities, can edit files)";

impl Default for PiSubagentSettings {
    fn default() -> Self {
        Self {
            agent_list: DEFAULT_AGENT_LIST.into(),
            peer_mode: false,
            offers_wait: false,
            orchestrator_lead: false,
            auto_lead: false,
        }
    }
}

const WAIT_PROPERTY: &str = "\"wait\":{\"type\":\"boolean\",\"description\":\"Set true (alone) to wait for the helpers you started in the background and get their reports.\"},";

const SCHEMA_WITHOUT_WAIT: &str = r#"{"type":"object","properties":{"to":{"type":"string","description":"Follow-up mode: the letter of an agent that already finished (e.g. \"A\"); task is the follow-up. The agent resumes with its own context."},"agent":{"type":"string","description":"Name of the agent to invoke (for single mode; default worker)"},"task":{"type":"string","description":"Task to delegate (for single mode)"},"label":{"type":"string","description":"Short label for the progress bar, at most 4 words (for single mode, optional)"},"new":{"type":"boolean","description":"Set true only when a task names files an earlier agent wrote but is really new work for a new agent (fixes go to the writer with to)"},"tasks":{"type":"array","items":{"type":"object","properties":{"agent":{"type":"string","description":"Name of the agent to invoke (default worker)"},"task":{"type":"string","description":"Task to delegate to the agent"},"label":{"type":"string","description":"Short label for the progress bar, at most 4 words (optional)"},"size":{"type":"integer","description":"Rough size: lines of code this task writes (optional). The biggest task starts first on a model copy of its own."},"id":{"type":"string","description":"Short id that other tasks can name in after (optional; the label works too)"},"new":{"type":"boolean","description":"Set true only when this task names files an earlier agent wrote but is really new work (fixes go to the writer with to)"},"after":{"type":"array","items":{"type":"string"},"description":"Ids, labels or 1-based numbers of tasks in this call that must finish first (optional; an earlier agent's letter also works and counts as met once it has finished). This task then starts at once and gets their reports. Name only tasks whose code this one cannot be written without (usually just the foundation), never siblings it reaches through the foundation's interfaces."}},"required":["agent","task"]},"description":"Array of {agent, task, label?, size?, id?, after?} for parallel execution, biggest task first"},"chain":{"type":"array","items":{"type":"object","properties":{"agent":{"type":"string","description":"Name of the agent to invoke (default worker)"},"task":{"type":"string","description":"Task with optional {previous} placeholder for prior output"},"label":{"type":"string","description":"Short label for the progress bar, at most 4 words (optional)"},"id":{"type":"string","description":"Short id that other steps can name in after (optional)"},"after":{"type":"array","items":{"type":"string"},"description":"Optional: ids, labels or 1-based numbers of the steps this step needs. If any step has after, the chain runs as a dependency graph: each step starts as soon as the steps it names finish (steps without after wait for the step before them)."}},"required":["agent","task"]},"description":"Array of {agent, task, label?} for sequential execution (with after: a dependency graph)"}}}"#;

pub struct PiSubagentTool {
    settings: PiSubagentSettings,
    description: String,
    schema_json: String,
    hooks: RwLock<PiSubagentHooks>,
    /// Ids and labels of tasks from earlier calls (lowercased), done when their call finished.
    earlier_names: Mutex<HashMap<String, DoneSignal>>,
}

impl Default for PiSubagentTool {
    fn default() -> Self {
        Self::new(PiSubagentSettings::default())
    }
}

impl PiSubagentTool {
    pub fn new(settings: PiSubagentSettings) -> Self {
        let description = describe(&settings);
        let schema_json = if settings.offers_wait {
            let at = SCHEMA_WITHOUT_WAIT[1..].find('{').unwrap() + 2;
            format!(
                "{}{WAIT_PROPERTY}{}",
                &SCHEMA_WITHOUT_WAIT[..at],
                &SCHEMA_WITHOUT_WAIT[at..]
            )
        } else {
            SCHEMA_WITHOUT_WAIT.to_string()
        };
        Self {
            settings,
            description,
            schema_json,
            hooks: RwLock::new(PiSubagentHooks::default()),
            earlier_names: Mutex::new(HashMap::new()),
        }
    }

    pub fn settings(&self) -> &PiSubagentSettings {
        &self.settings
    }

    /// Wire (or replace) the host side.
    pub fn set_hooks(&self, hooks: PiSubagentHooks) {
        *self.hooks.write().unwrap() = hooks;
    }

    fn find_earlier(&self, hooks: &PiSubagentHooks, name: &str) -> Option<DoneSignal> {
        let name = name.trim();
        if let Some(rx) = self.earlier_names.lock().unwrap().get(&name.to_lowercase()) {
            return Some(rx.clone());
        }
        hooks.earlier_agent.as_ref().and_then(|f| f(name))
    }

    fn register_call(&self, names: &[(Option<String>, Option<String>)]) -> CallDone {
        let (tx, rx) = watch::channel(false);
        let mut map = self.earlier_names.lock().unwrap();
        for (id, label) in names {
            for n in [id, label].into_iter().flatten() {
                if !n.trim().is_empty() {
                    map.insert(n.trim().to_lowercase(), rx.clone());
                }
            }
        }
        CallDone(tx)
    }
}

/// Marks a call's names as finished when dropped (C# `finally { thisCall.TrySetResult(); }`).
struct CallDone(watch::Sender<bool>);

impl Drop for CallDone {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

fn describe(s: &PiSubagentSettings) -> String {
    let modes = format!(
        "parallel (tasks array, up to {MAX_PARALLEL_TASKS}), chain (sequential, {{previous}} in a task is replaced by the prior step's output), "
    );
    if s.peer_mode {
        return format!(
            "Hand pieces of the work to helper agents: the same model as you, each in its own context, running at the same time as you. Modes: single (agent + task), {modes}follow-up (to + task: more work for a helper that already finished, by its letter; it keeps its context){}Helpers cannot see this conversation but can read every file: name the files and the spec section instead of copying them. Each helper gets a lettered progress bar (Agent A, B, ...); label names it. Agents: {}",
            if s.offers_wait {
                ", wait (wait: true alone: get the reports of helpers started in the background). Single and parallel calls return at once and the helpers run in the background while you keep working. "
            } else {
                ". "
            },
            s.agent_list
        );
    }
    format!(
        "Delegate tasks to specialized subagents with isolated context. Use parallel mode to run the independent pieces of multi-part work at once. Modes: single (agent + task), {modes}follow-up (to + task: more work for an agent that already finished, by its letter; it keeps its context). Subagents cannot see this conversation but can read every file: name the files and the spec section instead of copying them. Each subagent gets a lettered progress bar (Agent A, B, ...); label names it. Agents: {}",
        s.agent_list
    )
}

impl PiToolInfo for PiSubagentTool {
    fn prompt_snippet(&self) -> String {
        if self.settings.peer_mode {
            "Hand independent pieces of the work to helper agents that run while you work: worker (writes and changes code), scout (investigates), planner, reviewer".into()
        } else {
            "Delegate work to subagents with isolated context: worker (writes and changes code), scout (investigates), planner, reviewer".into()
        }
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        let s = &self.settings;
        if s.peer_mode {
            return vec![
                "Give each helper task a label of at most 4 words; it names the helper's progress bar.".into(),
                "Helper results start with the helper's letter: when you report what a helper did, start with it, e.g. \"Agent A: added the parser\". If a helper failed or stopped early, send it a follow-up ({\"to\": \"A\", \"task\": ...}) or finish that piece yourself.".into(),
            ];
        }
        let tail = if s.orchestrator_lead {
            "If an agent failed or stopped early, send it a follow-up ({\"to\": \"A\", \"task\": ...}) or give the piece to a new worker."
        } else if s.auto_lead {
            "If an agent failed or stopped early, send it a follow-up ({\"to\": \"A\", \"task\": ...}) or give the piece to a new worker (when you are hands-on, you may finish it yourself)."
        } else {
            "If an agent failed or stopped early, finish that piece yourself."
        };
        vec![
            "Give each subagent task a label of at most 4 words; it names the agent's progress bar.".into(),
            format!(
                "Subagent results start with the agent's letter: when you report what an agent did, start with it, e.g. \"Agent A: found 3 failing tests\". {tail}"
            ),
        ]
    }
}

#[async_trait::async_trait]
impl Tool for PiSubagentTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn parameter_schema(&self) -> Value {
        crate::schema(&self.schema_json)
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::LONG_RUNNING
    }
    fn permission_arg_name(&self) -> &str {
        "agent"
    }
    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1F465}",
            short_description: "Delegate to subagents",
            summary_arg_keys: &["agent"],
            aliases: &[],
        })
    }
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(
            self.name(),
            &self.schema_json,
            arguments_json,
        ))
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let hooks = self.hooks.read().unwrap().clone();
        let Some(run_one) = hooks.run_one.clone() else {
            return fail("Subagents are not available in this session.");
        };
        let with_budget = |result: ToolResult| -> ToolResult {
            match hooks.budget_note.as_ref().and_then(|f| f()) {
                Some(note) if !note.is_empty() => ToolResult {
                    content: format!("{}\n\n{note}", result.content),
                    ..result
                },
                _ => result,
            }
        };

        // {"wait": true} alone: collect the helpers started in the background.
        if self.settings.offers_wait
            && is_true(args, "wait")
            && blank(get_string(args, "task")).is_none()
            && blank(get_string(args, "to")).is_none()
            && args.get("tasks").is_none()
            && args.get("chain").is_none()
        {
            let waited = match &hooks.wait_background {
                Some(f) => f().await,
                None => None,
            };
            return match waited {
                None => ToolResult::success("No helpers are running in the background."),
                Some(w) => with_budget(ToolResult::success(format!(
                    "Background helpers: {}",
                    format_parallel(&w)
                ))),
            };
        }

        let single_agent = agent_or_default(get_string(args, "agent"));
        let single_task = get_string(args, "task");
        let single_label = get_string(args, "label");
        let mut tasks = read_tasks(args, "tasks");
        let mut earlier_waits: Vec<DoneSignal> = Vec::new();
        let mut earlier_named: Vec<String> = Vec::new();
        let earlier = |name: &str| self.find_earlier(&hooks, name);
        if let Some(e) = resolve_after(
            args,
            &mut tasks,
            "tasks",
            false,
            Some(&earlier),
            &mut earlier_waits,
            &mut earlier_named,
        ) {
            return fail_with(e, ToolErrorCode::InvalidArguments);
        }
        let mut chain = read_tasks(args, "chain");
        let chain_graph = !chain.is_empty() && chain_has_dependencies(args);
        if chain_graph {
            if let Some(e) = resolve_after(
                args,
                &mut chain,
                "chain",
                true,
                Some(&earlier),
                &mut earlier_waits,
                &mut earlier_named,
            ) {
                return fail_with(e, ToolErrorCode::InvalidArguments);
            }
            for t in &mut chain {
                t.task = t.task.replace(
                    "{previous}",
                    "(the reports of the tasks you waited for are below)",
                );
            }
        }
        let to = get_string(args, "to");

        let has_follow_up = blank(to).is_some() && blank(single_task).is_some();
        let has_single = !has_follow_up && blank(single_task).is_some();
        let modes = usize::from(has_follow_up)
            + usize::from(has_single)
            + usize::from(!tasks.is_empty())
            + usize::from(!chain.is_empty());
        if modes != 1 {
            let available = hooks
                .available_agents
                .as_ref()
                .map(|f| f())
                .filter(|a| !a.is_empty())
                .map(|a| a.join(", "))
                .unwrap_or_else(|| "none".into());
            return fail_with(
                format!(
                    "Invalid parameters. Provide exactly one mode.\nAvailable agents: {available}"
                ),
                ToolErrorCode::InvalidArguments,
            );
        }

        if has_follow_up {
            let Some(follow_up) = &hooks.follow_up else {
                return fail(
                    "Follow-ups are not available in this session; spawn a new agent with the full context.",
                );
            };
            let outcome = follow_up(
                to.unwrap_or("").trim().to_string(),
                single_task.unwrap_or("").to_string(),
            )
            .await;
            return with_budget(if outcome.succeeded {
                ToolResult::success(for_lead(Some(&outcome.output)))
            } else {
                fail(for_lead(Some(&outcome.output)))
            });
        }

        let all: Vec<PiSubagentTask> = if has_single {
            vec![PiSubagentTask {
                label: blank(single_label).map(str::to_string),
                is_new: is_true(args, "new"),
                ..PiSubagentTask::new(single_agent, single_task.unwrap_or(""))
            }]
        } else if !tasks.is_empty() {
            tasks.clone()
        } else {
            chain.clone()
        };
        if let Some(unknown) = unknown_agent(&hooks, &all) {
            return fail_with(unknown, ToolErrorCode::InvalidArguments);
        }
        if let Some(owners) = hooks.file_owners.as_ref().map(|f| f())
            && !owners.is_empty()
            && let Some(owned) = owned_files_error(&all, &owners)
        {
            return fail_with(owned, ToolErrorCode::InvalidArguments);
        }
        if let Some(capped) = hooks.cap_check.as_ref().and_then(|f| f(all.len())) {
            return fail_with(capped, ToolErrorCode::InvalidArguments);
        }
        if let Some(check) = &hooks.hand_off_check
            && let Some(no_helper) = check().await
        {
            return fail(no_helper);
        }

        // Peer mode: single and parallel hand-offs start in the background and return at once.
        if self.settings.offers_wait
            && let (Some(start), Some(_)) = (&hooks.start_background, &hooks.wait_background)
            && (has_single || !tasks.is_empty())
        {
            if tasks.len() > MAX_PARALLEL_TASKS {
                return fail_with(
                    format!(
                        "Too many parallel tasks ({}). Max is {MAX_PARALLEL_TASKS}.",
                        tasks.len()
                    ),
                    ToolErrorCode::InvalidArguments,
                );
            }
            wait_all(&mut earlier_waits).await;
            return with_budget(ToolResult::success(format!(
                "{}\n{BACKGROUND_STARTED_NOTE}",
                start(all)
            )));
        }

        let earlier_note = if earlier_named.is_empty() {
            None
        } else {
            let mut distinct: Vec<String> = Vec::new();
            for n in &earlier_named {
                if !distinct.iter().any(|d| d.eq_ignore_ascii_case(n)) {
                    distinct.push(n.clone());
                }
            }
            Some(format!(
                "after named {} from an earlier call: {}",
                distinct
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                if earlier_waits.is_empty() {
                    "it had already finished, so that dependency counted as met."
                } else {
                    "this call waited for it to finish first."
                }
            ))
        };
        let names = if has_single {
            vec![(None, single_label.map(str::to_string))]
        } else {
            call_names(args, if tasks.is_empty() { "chain" } else { "tasks" }, &all)
        };
        let _this_call = self.register_call(&names);
        wait_all(&mut earlier_waits).await;

        let result = if has_single {
            let outcome = run_one(all[0].clone()).await;
            if outcome.succeeded {
                ToolResult::success(for_lead(Some(&outcome.output)))
            } else {
                fail(format!(
                    "Agent {} failed: {}",
                    outcome.agent,
                    for_lead(Some(&outcome.output))
                ))
            }
        } else if !tasks.is_empty() {
            if tasks.len() > MAX_PARALLEL_TASKS {
                fail_with(
                    format!(
                        "Too many parallel tasks ({}). Max is {MAX_PARALLEL_TASKS}.",
                        tasks.len()
                    ),
                    ToolErrorCode::InvalidArguments,
                )
            } else {
                let outcomes = match &hooks.run_parallel {
                    Some(f) => f(tasks).await,
                    None => run_in_dependency_order(&tasks, run_one.clone()).await,
                };
                ToolResult::success(format_parallel(&outcomes))
            }
        } else if chain_graph {
            if chain.len() > MAX_PARALLEL_TASKS {
                fail_with(
                    format!(
                        "Too many tasks ({}). Max is {MAX_PARALLEL_TASKS}.",
                        chain.len()
                    ),
                    ToolErrorCode::InvalidArguments,
                )
            } else {
                let outcomes = match &hooks.run_parallel {
                    Some(f) => f(chain.clone()).await,
                    None => run_in_dependency_order(&chain, run_one.clone()).await,
                };
                ToolResult::success(format!(
                    "{}\n\n{}",
                    chain_graph_note(&chain),
                    format_parallel(&outcomes)
                ))
            }
        } else {
            run_chain(&chain, &run_one).await
        };
        with_budget(match earlier_note {
            Some(note) if !result.is_error => {
                ToolResult::success(format!("{note}\n\n{}", result.content))
            }
            _ => result,
        })
    }
}

async fn wait_all(waits: &mut [DoneSignal]) {
    for rx in waits {
        let _ = rx.wait_for(|done| *done).await;
    }
}

/// Host without its own scheduler: run each task once the tasks in its `after` list are done.
async fn run_in_dependency_order(
    tasks: &[PiSubagentTask],
    run_one: Arc<dyn Fn(PiSubagentTask) -> BoxFuture<PiSubagentOutcome> + Send + Sync>,
) -> Vec<PiSubagentOutcome> {
    let signals: Vec<(watch::Sender<bool>, DoneSignal)> =
        tasks.iter().map(|_| watch::channel(false)).collect();
    let mut handles = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        let deps: Vec<DoneSignal> = task
            .after
            .iter()
            .flatten()
            .map(|&d| signals[d].1.clone())
            .collect();
        let done = signals[i].0.clone();
        let run = run_one.clone();
        let task = task.clone();
        handles.push(tokio::spawn(async move {
            let mut deps = deps;
            wait_all(&mut deps).await;
            let outcome = run(task).await;
            done.send_replace(true);
            outcome
        }));
    }
    let mut outcomes = Vec::new();
    for (h, t) in handles.into_iter().zip(tasks) {
        outcomes.push(h.await.unwrap_or_else(|e| PiSubagentOutcome {
            agent: t.agent.clone(),
            succeeded: false,
            output: e.to_string(),
        }));
    }
    outcomes
}

async fn run_chain(
    chain: &[PiSubagentTask],
    run_one: &Arc<dyn Fn(PiSubagentTask) -> BoxFuture<PiSubagentOutcome> + Send + Sync>,
) -> ToolResult {
    let mut previous = String::new();
    let mut sb = String::new();
    for (i, step) in chain.iter().enumerate() {
        let step = PiSubagentTask {
            task: step.task.replace("{previous}", &previous),
            ..step.clone()
        };
        let outcome = run_one(step.clone()).await;
        if !outcome.succeeded {
            return fail(format!(
                "Chain stopped at step {} ({}): {}",
                i + 1,
                step.agent,
                for_lead(Some(&outcome.output))
            ));
        }
        sb.push_str(&format!(
            "## Step {}: {}\n{}\n\n",
            i + 1,
            step.agent,
            for_lead(Some(&outcome.output))
        ));
        previous = outcome.output;
    }
    ToolResult::success(sb.trim_end().to_string())
}

/// The (id, label) of each task in a tasks/chain array, for later calls' after.
fn call_names(
    args: &Value,
    key: &str,
    tasks: &[PiSubagentTask],
) -> Vec<(Option<String>, Option<String>)> {
    let items = task_items(args, key);
    tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            (
                items
                    .get(i)
                    .and_then(|it| get_string(it, "id"))
                    .map(str::to_string),
                t.label.clone(),
            )
        })
        .collect()
}

/// The array items that carry a non-blank task.
fn task_items<'a>(args: &'a Value, key: &str) -> Vec<&'a Value> {
    match args.get(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter(|item| blank(get_string(item, "task")).is_some())
            .collect(),
        _ => Vec::new(),
    }
}

/// Turns each task's "after" (ids, labels or 1-based numbers of tasks in the same call) into
/// indices on `tasks`. Returns an error for an unknown or self reference or a cycle.
pub fn resolve_after(
    args: &Value,
    tasks: &mut [PiSubagentTask],
    key: &str,
    previous_by_default: bool,
    earlier: Option<EarlierLookup<'_>>,
    waits: &mut Vec<DoneSignal>,
    named: &mut Vec<String>,
) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let Some(Value::Array(_)) = args.get(key) else {
        return None;
    };
    let items = task_items(args, key);
    let find = |r: &str, tasks: &[PiSubagentTask]| -> Option<usize> {
        let r = r.trim();
        for (i, item) in items.iter().enumerate() {
            if get_string(item, "id").is_some_and(|id| id.trim().eq_ignore_ascii_case(r)) {
                return Some(i);
            }
        }
        for (i, t) in tasks.iter().enumerate() {
            if t.label
                .as_deref()
                .is_some_and(|l| l.trim().eq_ignore_ascii_case(r))
            {
                return Some(i);
            }
        }
        match r.trim_start_matches('#').trim().parse::<i64>() {
            Ok(n) if n >= 1 && n as usize <= tasks.len() => Some(n as usize - 1),
            _ => None,
        }
    };
    for i in 0..items.len().min(tasks.len()) {
        let Some(after) = items[i].get("after") else {
            if previous_by_default && i > 0 {
                tasks[i].after = Some(vec![i - 1]);
            }
            continue;
        };
        let refs: Vec<String> = match after {
            Value::Array(a) => a
                .iter()
                .map(|e| match e {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                })
                .collect(),
            Value::String(s) => vec![s.clone()],
            Value::Number(n) => vec![n.to_string()],
            _ => Vec::new(),
        };
        let mut idx: Vec<usize> = Vec::new();
        for r in refs.iter().filter(|r| !r.trim().is_empty()) {
            let Some(d) = find(r, tasks) else {
                if let Some(prior) = earlier.and_then(|f| f(r)) {
                    named.push(r.trim().to_string());
                    if !*prior.borrow() {
                        waits.push(prior);
                    }
                    continue;
                }
                return Some(format!(
                    "Task {} has after \"{r}\", which names no task in this call and no earlier agent. Use a task's id, label or number, or an earlier agent's letter.",
                    i + 1
                ));
            };
            if d == i {
                return Some(format!("Task {} cannot wait for itself.", i + 1));
            }
            if !idx.contains(&d) {
                idx.push(d);
            }
        }
        if !idx.is_empty() {
            tasks[i].after = Some(idx);
        }
    }
    // Cycle check (Kahn).
    let mut indeg: Vec<usize> = tasks
        .iter()
        .map(|t| t.after.as_ref().map_or(0, Vec::len))
        .collect();
    let mut queue: std::collections::VecDeque<usize> =
        (0..tasks.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(n) = queue.pop_front() {
        seen += 1;
        for j in 0..tasks.len() {
            if tasks[j].after.as_ref().is_some_and(|a| a.contains(&n)) {
                indeg[j] -= 1;
                if indeg[j] == 0 {
                    queue.push_back(j);
                }
            }
        }
    }
    (seen != tasks.len())
        .then(|| "The tasks' after lists form a cycle, so no task in it could ever start.".into())
}

/// Whether any chain step names "after": the chain is then a dependency graph.
pub fn chain_has_dependencies(args: &Value) -> bool {
    match args.get("chain") {
        Some(Value::Array(arr)) => arr
            .iter()
            .any(|item| item.as_object().is_some_and(|o| o.contains_key("after"))),
        _ => false,
    }
}

/// What the Lead is told when its chain ran as a graph: which steps waited for which.
pub fn chain_graph_note(chain: &[PiSubagentTask]) -> String {
    let waits: Vec<String> = chain
        .iter()
        .enumerate()
        .map(|(i, t)| match &t.after {
            Some(a) if !a.is_empty() => format!(
                "step {} after {}",
                i + 1,
                a.iter()
                    .map(|d| (d + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            _ => format!("step {} at once", i + 1),
        })
        .collect();
    format!(
        "Chain steps named after, so they ran as a dependency graph (each step started as soon as the steps it waits for finished): {}.",
        waits.join("; ")
    )
}

/// A parallel call's outcomes as the caller sees them (`FormatParallel` / `FormatOutcomes`).
pub fn format_parallel(outcomes: &[PiSubagentOutcome]) -> String {
    let ok = outcomes.iter().filter(|o| o.succeeded).count();
    let mut sb = format!("Parallel: {ok}/{} succeeded", outcomes.len());
    for (i, o) in outcomes.iter().enumerate() {
        sb.push_str(&format!(
            "\n\n## [{}] task {} {}\n{}",
            o.agent,
            i + 1,
            if o.succeeded { "completed" } else { "failed" },
            for_lead(Some(&o.output))
        ));
    }
    sb
}

fn unknown_agent(hooks: &PiSubagentHooks, tasks: &[PiSubagentTask]) -> Option<String> {
    let available = hooks.available_agents.as_ref().map(|f| f())?;
    if available.is_empty() {
        return None;
    }
    for t in tasks {
        if !available.iter().any(|a| a.eq_ignore_ascii_case(&t.agent)) {
            return Some(format!(
                "Unknown agent: \"{}\". Available agents: {}.",
                t.agent,
                available
                    .iter()
                    .map(|a| format!("\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    None
}

/// An agent's report as the Lead sees it: a placeholder when empty, the middle cut past
/// [`MAX_REPORT_CHARS`] (UTF-16 units).
pub fn for_lead(s: Option<&str>) -> String {
    let s = match s {
        Some(s) if !s.trim().is_empty() => s,
        _ => "(no output)",
    };
    let s = crate::arguments::dotnet_trim(s);
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= MAX_REPORT_CHARS {
        return s.to_string();
    }
    const TAIL: usize = 800;
    let head = MAX_REPORT_CHARS - TAIL;
    let cut = units.len() - head - TAIL;
    let first = String::from_utf16_lossy(&units[..head]);
    let last = String::from_utf16_lossy(&units[units.len() - TAIL..]);
    format!(
        "{}\n[... {cut} chars of this report cut; send the agent a follow-up if you need them ...]\n{}",
        first.trim_end(),
        last.trim_start()
    )
}

fn blank(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

/// A boolean flag given as true or "true".
fn is_true(item: &Value, name: &str) -> bool {
    match item.get(name) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Agents that only read: their tasks may name any file.
const READ_ONLY_AGENTS: [&str; 6] = ["scout", "planner", "reviewer", "explore", "plan", "review"];

/// A new code task that names a file an earlier agent wrote (and that agent can still take a
/// follow-up) is sent back once with the owners' letters, unless it says "new": true.
pub fn owned_files_error(tasks: &[PiSubagentTask], owners: &[PiFileOwner]) -> Option<String> {
    if owners.is_empty() {
        return None;
    }
    for (i, t) in tasks.iter().enumerate() {
        if t.is_new
            || READ_ONLY_AGENTS
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&t.agent))
        {
            continue;
        }
        let text = t.task.replace('\\', "/");
        // GroupBy path (case-insensitive), keep each group's last owner, in first-seen order.
        let mut hits: Vec<&PiFileOwner> = Vec::new();
        for o in owners.iter().filter(|o| mentions(&text, &o.path)) {
            match hits
                .iter_mut()
                .find(|h| h.path.eq_ignore_ascii_case(&o.path))
            {
                Some(h) => *h = o,
                None => hits.push(o),
            }
        }
        hits.truncate(6);
        if hits.is_empty() {
            continue;
        }
        let first = &hits[0].agent;
        return Some(format!(
            "{} changes code that earlier agents wrote and can still take a follow-up: {}. Send the fix to the agent that wrote it instead, e.g. {{\"to\": \"{first}\", \"task\": \"<what is wrong and what to change>\"}}: it keeps its context, so say only what is new. If this really is new work for a new agent, add \"new\": true to the task.",
            if tasks.len() > 1 {
                format!("Task {}", i + 1)
            } else {
                "This task".into()
            },
            hits.iter()
                .map(|h| format!("{} (Agent {})", file_name(&h.path), h.agent))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    None
}

/// `Path.GetFileName` (both separators on Windows).
fn file_name(path: &str) -> &str {
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    path.rsplit(seps).next().unwrap_or(path)
}

/// The task names the file: its path, or its file name when that is specific (has an
/// extension, 6+ chars), as a whole word (so "View.cs" does not hit "GridView.cs"). The C#
/// lookarounds `(?<![\w.-])` / `(?![\w-])` are checked by hand.
fn mentions(text: &str, path: &str) -> bool {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches(['.', '/']);
    if !p.is_empty() && text.to_lowercase().contains(&p.to_lowercase()) {
        return true;
    }
    let name = file_name(p);
    if name.encode_utf16().count() < 6 || !name.contains('.') {
        return false;
    }
    let Ok(re) = RegexBuilder::new(&regex::escape(name))
        .case_insensitive(true)
        .build()
    else {
        return false;
    };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut pos = 0;
    while let Some(m) = re.find_at(text, pos) {
        let before_ok = text[..m.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !(word(c) || c == '.' || c == '-'));
        let after_ok = text[m.end()..]
            .chars()
            .next()
            .is_none_or(|c| !(word(c) || c == '-'));
        if before_ok && after_ok {
            return true;
        }
        pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// A task's "size" (an integer, or a number in a string); `None` when absent or not positive.
pub fn get_size(item: &Value) -> Option<i32> {
    let d = match item.get("size")? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (d > 0.0 && d < i32::MAX as f64).then(|| crate::job_tool::round_half_even(d) as i32)
}

fn agent_or_default(agent: Option<&str>) -> String {
    blank(agent).unwrap_or(DEFAULT_AGENT).to_string()
}

fn read_tasks(args: &Value, name: &str) -> Vec<PiSubagentTask> {
    let Some(Value::Array(arr)) = args.get(name) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let task = blank(get_string(item, "task"))?;
            Some(PiSubagentTask {
                label: blank(get_string(item, "label")).map(str::to_string),
                size: get_size(item),
                is_new: is_true(item, "new"),
                ..PiSubagentTask::new(agent_or_default(get_string(item, "agent")), task)
            })
        })
        .collect()
}

// ---- prompt texts the host adds (all with "\n" line breaks) ---------------------------------

/// Refusal when a hand-off finds no free model copy ({0} free, {1} needed).
pub const NO_HELPER_FREE_NOTE: &str = "No helper can start now: only {0} model copy/copies are free for helpers and a hand-off needs {1} (a helper sharing a copy halves its speed and yours). Do this work yourself.";

/// What a background hand-off returns, after the line naming the helpers.
pub const BACKGROUND_STARTED_NOTE: &str = "They run while you work: do your own piece now, without touching the files their briefs own. When it is done, call subagent with {\"wait\": true} to get their reports, then integrate and check everything together. (If you end your turn first, their reports are handed to you then.)";

pub const AFTER_RULE: &str = "A piece's after names only the pieces whose code it cannot be written without, usually just the foundation: once the foundation defines the interfaces, every other piece starts together after it, including one that consumes the others' results through those interfaces (e.g. a UI showing what the sources find); never make a piece wait for its siblings.";

pub const FIX_RULE: &str = "Send each fix back to the agent that wrote that code, as a follow-up with its letter: {\"to\": \"A\", \"task\": \"<what fails and what to change>\"}; it keeps its context, so say only what is new. Each result lists the files its agent wrote. Use a new worker only for new work.";

pub const PLAUSIBILITY_RULE: &str = "Plausibility: compare the counts and data you see against ground truth on this machine (list the files or records the program should have found, e.g. with one bash command) and treat an empty or zero result as a failure unless the ground truth is also empty.";

pub const REAL_INPUT_RULE: &str = "A parser and its test fixtures must follow a real sample: each brief for code that reads a file format says where real input lives on this machine (or the official docs) and tells the worker to read one real file first; never an invented format. Tests built only on invented fixtures do not count as verification.";

pub const NO_REPEAT_RULE: &str = "Do not repeat work: run a build or check again only when code changed since it last passed, never run the same command again hoping for a different result, and do not re-read a file you just read or wrote.";

/// The fixed final check, by kind of program (`VerifyChecklist`, also `LookRule`).
pub static VERIFY_CHECKLIST: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Final verification checklist, by kind of program. GUI app or game: start it with bash background: true, then ONE job screenshot that walks it with shot steps in keys and stops it: keys \"shot <move focus> shot <open each secondary view the spec lists> shot <each filter or mode> shot <go back> shot\", size: \"1280x720 1920x1080\", kill: true (the start screen, one after a focus move, each secondary view, each filter or mode, one after going back, then the bigger window, which checks it still fills it; use a second call only if the spec has more views than fit in 8 shots; never a key that launches, buys or deletes something). A size error from the screenshot is a bug in the program, not in the check. CLI tool: run its main commands and one error case (bad argument or missing file) in one bash call, checking output and exit codes. Server: start it with bash background: true, hit the main endpoints and one bad request in one bash call, then stop it with job kill. {PLAUSIBILITY_RULE} Then say in your answer what you checked and what each step showed; for anything that does not match the spec, send the fix to the agent that wrote it and re-check only that step."
    )
});

/// The Lead's delegation workflow (`LeadInstructions`, a C# raw string: "\n" line breaks here).
pub static LEAD_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "You lead a team of subagents. Before you write any code, decide: does the task need 3 or more files written or changed, or investigation plus implementation? If so, delegate instead of writing them yourself:
1. Read the spec, then split the work into pieces, one per file or component.
2. Make ONE subagent call with a tasks array: a worker per piece of code (a scout per investigation), each with a label of at most 4 words, a size (rough lines of code it writes) and a self-contained task: the goal, the files it owns, the exact interfaces and rules copied from the spec, constraints, and how to check it is done. List the biggest piece first: it starts first, on a model copy of its own. Tests and docs go with the code: each worker writes and runs the tests for its own piece, in its own test file (at most 12 focused cases), and updates the docs its piece changes; never a separate tests-only or docs-only task. Shape the split by what the spec defines. If every piece can be written straight from the spec, start them all at once: no foundation piece and no after. A piece that uses other pieces only through interfaces the spec defines (e.g. a report reading the parsers' spec-defined output) runs alongside them. Only when the spec leaves shared types or helpers undefined does a small foundation piece go first, then 2-3 balanced pieces with after: [its id]; each of their briefs names the exact functions, fields and messages it uses from the foundation, and the files it must not touch. {AFTER_RULE} Ask only for what the spec asks: no extra options, flags or files, and no open invitations such as \"or an override if any\". Subagents cannot see this conversation.
   {REAL_INPUT_RULE}
3. When the results come back, run the full test suite and the spec's sample command in ONE bash call (at most 2 more short probes, only for rules the tests miss). {FIX_RULE} {} {NO_REPEAT_RULE} Then answer in about 5 lines.
Example: {{\"tasks\": [{{\"agent\": \"worker\", \"label\": \"Build parser module\", \"size\": 200, \"task\": \"Create parser.py with parse(text) -> list[Row] ... Done when: python -m unittest tests.test_parser passes.\"}}, {{\"agent\": \"worker\", \"label\": \"Build report command\", \"size\": 80, \"task\": \"Create report.py ...\"}}]}}
Do only trivial work directly: a one-line edit, a single lookup, a quick answer.",
        *VERIFY_CHECKLIST
    )
});

/// The main agent's system-prompt addendum in peer mode (agent.peerMode).
pub fn peer_instructions(background: bool) -> String {
    format!(
        "You do the work yourself. Helper agents (the same model as you) can take pieces of it at the same time through the subagent tool, but each starts cold and has to be briefed and checked, so:\n- Work alone on small tasks (a fix, one or two files), on sequential or tightly coupled work (every part needs the same new code), and on reading or exploring you need for your own decisions.\n- Hand off only when the task splits into independent pieces that can run at the same time: separate files, modules or tests, with no file edited by two pieces (e.g. eight similar modules). Then make ONE subagent call with a tasks array, one worker per piece, biggest first, each with a label of at most 4 words and a self-contained brief: the goal, the files it owns (and must not go beyond), the interfaces it uses or provides, where its rules are (point to the spec section), and the command that must pass. Helpers cannot see this conversation but can read every file.\n{}- When the helpers are done, integrate their work, then run the full test suite and the spec's sample command in ONE bash call. Send each fix to the helper that wrote that code ({{\"to\": \"A\", \"task\": \"<what fails and what to change>\"}}) or make it yourself if it is small.\n- Before you finish, check that every piece the task asks for exists and passes. A helper that stopped early (a partial report: time, step or output limit) left its piece unfinished: finish it yourself.\n- If the subagent call says no helper can start now, do that work yourself.",
        if background {
            "- The call returns at once and the helpers run in the background. Keep one piece for yourself and do it meanwhile, without touching their files; then call subagent with {\"wait\": true} for their reports.\n"
        } else {
            "- The call returns when every helper has finished; do your own piece before or after it, never in their files.\n"
        }
    )
}

/// The intro of a helper's prompt in peer mode.
pub const PEER_WORKER_INTRO: &str = "You are a peer agent: another agent on the same task (the same model as you) handed you one piece of it and is working on other pieces at the same time. Finish your piece and report back. You run in an isolated context: you cannot see that agent's conversation, and it sees only your final report, so keep that report short.";

/// A helper's role prompt in peer mode (`PeerRole`).
pub fn peer_role(role: &str) -> String {
    const WORKER_INTRO: &str = "You are a worker agent. A lead agent gave you one piece of a larger task. You run in an isolated context: you cannot see the lead's conversation, and the lead sees only your final report. The lead is the slow, shared part of the team, so keep that report short.";
    static RES: LazyLock<[(Regex, &str); 4]> = LazyLock::new(|| {
        [
            (Regex::new(r"\b[Aa] lead agent\b").unwrap(), "another agent"),
            (
                Regex::new(r"\b[Tt]he lead agent\b").unwrap(),
                "the agent that handed you this piece",
            ),
            (Regex::new(r"\b[Tt]he lead's\b").unwrap(), "that agent's"),
            (Regex::new(r"\b[Tt]he lead\b").unwrap(), "that agent"),
        ]
    });
    let mut text = role
        .replace(WORKER_INTRO, PEER_WORKER_INTRO)
        .replace(
            "The lead agent reads your writeup on its own, slower slot,",
            "The agent that handed you this piece reads your writeup,",
        )
        .replace("the lead gave you", "the brief gave you")
        .replace("the lead's number", "the brief's number");
    for (re, with) in RES.iter() {
        text = re.replace_all(&text, regex::NoExpand(with)).into_owned();
    }
    text
}

/// The orchestrator Lead's workflow (agent.leadMode = "orchestrator").
pub fn orchestrator_instructions(bash: bool) -> String {
    let mut s = String::from(
        "You are the orchestrator of a team of subagents. You cannot create, edit or delete files: you have no write or edit tool",
    );
    if bash {
        s.push_str(", and you must never change files through bash (no redirects, sed -i, rm, mv, git checkout or scripts that write)");
    }
    s.push_str(". Every change to the code, even a one-line fix, is made by a worker.\n");
    s.push_str("1. Read the spec and the code you need, then split the work into pieces, one per file or component.\n");
    s.push_str("2. Make ONE subagent call with a tasks array: a worker per piece of code (a scout per investigation), each with a label of at most 4 words, a size (rough lines of code it writes; list the biggest piece first, it starts first on a model copy of its own) and a short, self-contained brief (at most about 120 words): the goal, the files it owns, where its rules are (point to the spec section, e.g. \"SPEC.md section gems/board.py\"; do not copy it), only the interfaces between pieces that the spec leaves open, and \"Done when: <command> passes\". Never work out expected values or outputs yourself for a brief; the spec and the tests are the reference. Subagents cannot see this conversation, but they can read every file. Tests and docs go with the code: each worker writes and runs the tests for its own piece, in its own test file (at most 12 focused cases), and updates the docs its piece changes; never a separate tests-only or docs-only task. Shape the split by what the spec defines. If every piece can be written straight from the spec, start them all at once: no foundation piece and no after. A piece that uses other pieces only through interfaces the spec defines (e.g. a report reading the parsers' spec-defined output) runs alongside them. Only when the spec leaves shared types or helpers undefined does a small foundation piece go first, then 2-3 balanced pieces with after: [its id]; each of their briefs names the exact functions, fields and messages it uses from the foundation, and the files it must not touch. ");
    s.push_str(AFTER_RULE);
    s.push_str(" Ask only for what the spec asks: no extra options, flags or files, and no open invitations such as \"or an override if any\".\n");
    if bash {
        s.push_str(&format!("3. Final check: ONE bash call that runs the full test suite and the spec's sample command(s) together. {} At most 1 more short probe, only for a spec rule the tests do not cover. {NO_REPEAT_RULE} When these pass you are done: do not spawn a reviewer unless the user asks for a review, and answer at once in at most 5 short lines (what was built, by which agent, the test result, what you saw).\n", *VERIFY_CHECKLIST));
    } else {
        s.push_str("3. Review the results: have a worker run the full test suite and the spec's sample command(s) and report the exact failures within 10 steps (you have no shell). When they pass you are done: do not spawn a reviewer unless the user asks for a review, and answer in about 5 lines.\n");
    }
    s.push_str("4. Send each fix back to the agent that wrote that code, as a follow-up with its letter: {\"to\": \"A\", \"task\": \"test_x fails: expected 3, got 2; fix ... in board.py\"}. It keeps its context, so say only what is new. Each result lists the files its agent wrote. Use a new worker only for new work. Repeat until the checks pass.\n");
    s.push_str("Example: {\"tasks\": [{\"agent\": \"worker\", \"label\": \"Build parser module\", \"size\": 200, \"task\": \"Create parser.py as SPEC.md section Parser says. Only that file. Done when: python -m unittest tests.test_parser passes.\"}, {\"agent\": \"worker\", \"label\": \"Build report command\", \"size\": 80, \"task\": \"Create report.py ...\"}]}\n");
    s.push_str(REAL_INPUT_RULE);
    s.push('\n');
    s.push_str("If the user only asks a question or wants an explanation and nothing needs to change, answer it directly without subagents.");
    s
}

/// The Lead's extra rules in swarm mode (agent.providerSchedule / maxAgents); 0 = no cap.
pub fn swarm_instructions(max_agents: i32) -> String {
    format!(
        "Swarm mode (this overrides the piece counts above): {}Prefer many small, well-scoped tasks with small contexts over a few big ones: one file group per agent (one component and its tests), a clear contract (the exact files, types, functions and fields it provides and the ones it uses from other pieces) and a short brief (at most about 120 words) that names only the files it needs; a small agent with a tight brief finishes faster and makes fewer mistakes than a big one. Split any piece you expect to be large (e.g. a whole UI) across several agents, each owning a file group with a clear contract (for a UI e.g. home view, grid view and filters, input and navigation, theme and assets, with the shared state type and its fields named in every brief); never give one agent a whole UI or app layer. Spawn freely up to the cap: start every piece the contracts allow at once (a tasks array, up to {MAX_PARALLEL_TASKS} per call), and keep a few agents in reserve for fixes and late work. Agents run on different models (each result names its model), so every brief must stand on its own. Send each fix to the agent that wrote the code ({{\"to\": \"A\", ...}}). When the cap is reached, send new work as follow-ups to existing agents. Final verification as above.",
        if max_agents > 0 {
            format!(
                "you may create up to {max_agents} subagents in this session (finished ones count; follow-ups with to do not). "
            )
        } else {
            "you may create as many subagents as the work needs. ".into()
        }
    )
}
