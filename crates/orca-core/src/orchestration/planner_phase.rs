//! agent.planner (`PlannerPhase`): before the Lead's first turn, a read-only agent on a named
//! provider explores the workspace and writes a comprehensive plan. The plan is saved to
//! agent.planner.planFile in the workspace and handed to the Lead in its first message; the
//! Lead then runs as usual. A planner that fails leaves the prompt unchanged.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use super::cancel::CancelToken;
use super::context::AgentContext;
use super::orchestrator::{AgentOrchestrator, SpawnOptions};
use super::read_only_command;
use super::types::{AgentOrigin, AgentStatus};
use crate::agent::prompt_constants::platform_lines;
use crate::config::OrcaConfig;

pub struct PlannerPhase {
    config: OrcaConfig,
    orchestrator: Arc<AgentOrchestrator>,
    done: bool,
    /// Where the plan is written (the workspace).
    pub working_directory: PathBuf,
    /// The planner's run, once it ran.
    pub last_run: Option<Arc<AgentContext>>,
}

/// `PlannerConfig.Enabled`: a provider is named.
pub fn planner_enabled(config: &OrcaConfig) -> bool {
    config
        .agent
        .planner
        .as_ref()
        .and_then(|p| p.provider.as_deref())
        .is_some_and(|p| !p.trim().is_empty())
}

impl PlannerPhase {
    pub fn new(
        config: OrcaConfig,
        orchestrator: Arc<AgentOrchestrator>,
        working_directory: PathBuf,
    ) -> Self {
        Self {
            config,
            orchestrator,
            done: false,
            working_directory,
            last_run: None,
        }
    }

    /// The Lead's first message: `prompt`, with the plan when a planner is configured and
    /// succeeds (once per session; later turns pass through unchanged).
    pub async fn prepare_first_message(&mut self, prompt: &str, ct: &CancelToken) -> String {
        let planner = match self.config.agent.planner.clone() {
            Some(p) if !self.done && planner_enabled(&self.config) && !prompt.trim().is_empty() => {
                p
            }
            _ => return prompt.to_string(),
        };
        self.done = true;
        let provider = planner.provider.clone().unwrap_or_default();
        let cost_before = self.cost_of(&provider);
        let started = Instant::now();
        tracing::info!(
            "Planner: {provider} writes the plan (at most {} steps, {} s)",
            planner.max_steps,
            planner.timeout_seconds
        );
        let run = self
            .orchestrator
            .spawn_agent_with(
                &planner_task(prompt, &planner.plan_file, planner.max_steps),
                "planner",
                AgentOrchestrator::current_depth(),
                ct,
                SpawnOptions {
                    origin: AgentOrigin::Command,
                    label: Some("Write the plan".into()),
                    provider: Some(provider.clone()),
                    max_iterations: Some(planner.max_steps),
                    max_output_tokens: planner.max_tokens,
                    effort: planner.effort.clone(),
                    timeout_seconds: Some(planner.timeout_seconds),
                    read_only: true,
                    full_result: true,
                    ..SpawnOptions::default()
                },
            )
            .await;
        self.last_run = Some(run.clone());
        let plan = plan_text(&run);
        let provider_model = run.state().provider_model.clone();
        let cost = self.cost_of(&provider) - cost_before;
        tracing::info!(
            "Planner usage {provider} ({}): {} in {:.0} s, {} steps, ${cost:.4}, plan {} chars",
            provider_model.clone().unwrap_or_default(),
            run.status().as_str(),
            started.elapsed().as_secs_f64(),
            run.iteration_count(),
            plan.as_ref().map_or(0, |p| p.encode_utf16().count())
        );
        let Some(plan) = plan.filter(|p| !p.trim().is_empty()) else {
            tracing::warn!(
                "Planner did not produce a plan ({:?}: {}); the Lead starts without one",
                run.status(),
                run.error().unwrap_or_default()
            );
            return prompt.to_string();
        };
        let path = self.working_directory.join(&planner.plan_file);
        let write = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&path, format!("{plan}\n")));
        if let Err(e) = write {
            tracing::warn!("Could not save the plan to {}: {e}", path.display());
        }
        with_plan(
            prompt,
            &plan,
            &planner.plan_file,
            provider_model.as_deref().unwrap_or(&provider),
        )
    }
}

impl PlannerPhase {
    /// `CostOf`: what `provider` has cost this session so far (USD).
    fn cost_of(&self, provider: &str) -> f64 {
        self.orchestrator
            .usage_ledger()
            .snapshot()
            .iter()
            .filter(|u| u.provider.eq_ignore_ascii_case(provider))
            .filter_map(|u| u.cost_usd)
            .sum()
    }
}

/// The plan in the planner's result: a finished run's answer, or the answer it wrote at its
/// step or time limit (without the "[Partial result …]" line); None otherwise.
pub fn plan_text(run: &AgentContext) -> Option<String> {
    let text = run.result()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    match run.status() {
        AgentStatus::Completed => Some(text),
        AgentStatus::CompletedPartial if text.starts_with("[Partial result") => {
            let rest = text
                .find('\n')
                .map(|i| text[i + 1..].trim().to_string())
                .unwrap_or_default();
            (!rest.is_empty()).then_some(rest)
        }
        _ => None,
    }
}

/// The planner's brief: the user's task and what the plan must contain.
pub fn planner_task(prompt: &str, plan_file: &str, max_steps: i32) -> String {
    let head = platform_lines(&format!(
        "Write a comprehensive implementation plan for the task below. You are read-only: read, grep, find, ls and bash
commands that only look (dir, ls, cat, type, git log, reg query, Get-ChildItem). Do not write or change files;
your final answer is the plan itself (it will be saved to {plan_file} and given to the lead developer, who
delegates the work to several agents on a smaller local model, so be explicit and concrete).
You have {max_steps} steps (one step = one round of tool calls) and no extensions: batch several commands
into each step, keep a few steps for writing, and end with the plan as plain text with no tool calls.

"
    ));
    let tail = platform_lines(
        "

Read the task's spec files first, then look at the machine where it matters (e.g. the real data files, install
locations and formats the program must handle). The plan must have these sections:
1. Architecture and the file and folder layout (every file to create, one line each on its role).
2. Interfaces and contracts between parts: exact type, method and field names and signatures, data formats,
   and which part owns which files.
3. Task breakdown sized for agents (each a few hundred lines at most, one file group each), with dependencies
   and what can run in parallel; split a large part such as a whole UI into several tasks.
4. Risks, including real data formats to sample from this machine: say where the real files are and what
   they look like (quote a short real example), so parsers and fixtures follow real input, not invented formats.
5. A verification checklist: build and tests, running the program as the user will, and plausibility checks
   of counts and data against ground truth on this machine.

Task:
",
    );
    format!(
        "{head}{}{tail}{prompt}",
        read_only_command::allowed_summary()
    )
}

/// The Lead's first message: the task, then the plan to follow.
pub fn with_plan(prompt: &str, plan: &str, plan_file: &str, model: &str) -> String {
    let middle = platform_lines(&format!(
        "

A planner ({model}) has explored this workspace and written the plan below (also saved as {plan_file}). Follow it:
use its file layout, contracts and task breakdown when you delegate (give each agent the parts of the plan it
needs), and use its verification checklist at the end. Deviate only where the plan is wrong or the spec says
otherwise, and say so in your answer.

<plan>
"
    ));
    format!("{prompt}{middle}{plan}{}", platform_lines("\n</plan>"))
}
