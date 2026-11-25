//! The host side of sub-agents (`AgentManager`, the model-tool parts): runs spawns for the
//! model's tools (spawn_agent, spawn_agents, the Pi profile's subagent), prints their start and
//! finish lines, formats the result handed back to the parent, and keeps background hand-offs
//! (peer mode) until the main agent collects them. One per session.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::cancel::CancelToken;
use super::context::AgentContext;
use super::events::AgentEvent;
use super::orchestrator::{AgentOrchestrator, ParallelAgentSpec, SpawnOptions, take16};
use super::types::{AgentOrigin, AgentStatus};
use crate::config::dotnet_json::NEWLINE;

/// One task of the Pi profile's subagent tool (`PiSubagentTask`).
#[derive(Debug, Clone, Default)]
pub struct SubagentTask {
    pub agent: String,
    pub task: String,
    pub label: Option<String>,
    pub size: Option<i32>,
    pub after: Option<Vec<usize>>,
}

/// What a Pi-style spawn hands back (`PiSubagentOutcome`): the agent type, success, and the
/// text for the Lead (it leads with the agent's letter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentOutcome {
    pub agent: String,
    pub ok: bool,
    pub output: String,
}

/// Where start/finish lines go (stderr in the CLI; nothing in JSON mode).
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

type BackgroundRun = tokio::task::JoinHandle<Vec<SubagentOutcome>>;

pub struct AgentManager {
    orchestrator: Arc<AgentOrchestrator>,
    sink: Mutex<Option<LineSink>>,
    finished_user_agents: Arc<Mutex<VecDeque<Arc<AgentContext>>>>,
    background: Mutex<Vec<BackgroundRun>>,
    background_ct: Mutex<CancelToken>,
    /// True when a live agent panel shows agents; without it each sub-agent tool call is
    /// printed as an activity line.
    panel_active: Arc<Mutex<Box<dyn Fn() -> bool + Send + Sync>>>,
}

impl AgentManager {
    pub fn new(orchestrator: Arc<AgentOrchestrator>) -> Arc<Self> {
        let me = Arc::new(Self {
            orchestrator: orchestrator.clone(),
            sink: Mutex::new(None),
            finished_user_agents: Arc::new(Mutex::new(VecDeque::new())),
            background: Mutex::new(Vec::new()),
            background_ct: Mutex::new(CancelToken::new()),
            panel_active: Arc::new(Mutex::new(Box::new(|| false))),
        });
        let weak = Arc::downgrade(&me);
        orchestrator.subscribe(move |e| {
            if let Some(m) = weak.upgrade() {
                m.on_agent_event(e);
            }
        });
        me
    }

    pub fn orchestrator(&self) -> &Arc<AgentOrchestrator> {
        &self.orchestrator
    }

    /// Where the start/finish lines go; None = nowhere.
    pub fn set_line_sink(&self, sink: Option<LineSink>) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }

    pub fn set_panel_active(&self, f: impl Fn() -> bool + Send + Sync + 'static) {
        *self.panel_active.lock().unwrap_or_else(|e| e.into_inner()) = Box::new(f);
    }

    // ── model tools ──────────────────────────────────────────────────────────

    /// spawn_agent: run one agent and return its result text, tagged with its letter.
    pub async fn run_spawn_tool(
        &self,
        task: &str,
        agent_type: &str,
        label: Option<&str>,
        ct: &CancelToken,
    ) -> String {
        let depth = AgentOrchestrator::current_depth();
        self.print_line(
            &format!("    {agent_type}: {}", preview(task, 80)),
            depth,
        );
        let context = self
            .orchestrator
            .spawn_agent_with(
                task,
                agent_type,
                depth,
                ct,
                SpawnOptions {
                    label: label.map(str::to_string),
                    ..SpawnOptions::default()
                },
            )
            .await;
        self.print_finish(&context, depth);
        tool_result_text(&context)
    }

    /// spawn_agents: run agents in parallel and return a markdown digest.
    pub async fn run_parallel_tool(
        &self,
        specs: &[ParallelAgentSpec],
        ct: &CancelToken,
    ) -> Result<String, String> {
        // Children run one level below the depth ceiling so they can't fan out again.
        let caller_depth = AgentOrchestrator::current_depth();
        let config = self.orchestrator.config();
        let depth = caller_depth.max(config.agent.max_spawn_depth - 1);
        self.print_line(
            &format!(
                "    {} agents in parallel (max {} concurrent)",
                specs.len(),
                config.agent.max_concurrent
            ),
            caller_depth,
        );
        let contexts = self.orchestrator.spawn_parallel(specs, depth, ct).await?;
        let mut digest = String::new();
        digest.push_str(&format!("Ran {} agents in parallel:{NEWLINE}", contexts.len()));
        for (c, spec) in contexts.iter().zip(specs) {
            self.print_finish(c, caller_depth);
            digest.push_str(NEWLINE);
            digest.push_str(&format!(
                "## {} ({} iterations){NEWLINE}",
                result_header(c),
                c.iteration_count()
            ));
            digest.push_str(&format!("Task: {}{NEWLINE}", preview(&spec.task, 100)));
            digest.push_str(NEWLINE);
            let s = c.state();
            let body = s
                .result
                .clone()
                .or(s.error.clone())
                .unwrap_or_else(|| "(no result)".into());
            drop(s);
            digest.push_str(&body);
            digest.push_str(NEWLINE);
        }
        Ok(digest)
    }

    /// spawn_agent with "to": a follow-up to a finished agent.
    pub async fn run_follow_up_tool(&self, to: &str, task: &str, ct: &CancelToken) -> String {
        match self.run_follow_up_core(to, task, ct).await {
            Ok(c) => tool_result_text(&c),
            Err(e) => e,
        }
    }

    async fn run_follow_up_core(
        &self,
        to: &str,
        task: &str,
        ct: &CancelToken,
    ) -> Result<Arc<AgentContext>, String> {
        let depth = AgentOrchestrator::current_depth();
        self.print_line(
            &format!("    follow-up to {to}: {}", preview(task, 80)),
            depth,
        );
        let c = self.orchestrator.follow_up(to, task, ct).await?;
        self.print_finish(&c, depth);
        Ok(c)
    }

    /// Pi profile subagent tool: one task.
    pub async fn run_pi_one(&self, task: &SubagentTask, ct: &CancelToken) -> SubagentOutcome {
        let depth = AgentOrchestrator::current_depth();
        self.print_line(
            &format!("    {}: {}", task.agent, preview(&task.task, 80)),
            depth,
        );
        let c = self
            .orchestrator
            .spawn_agent_with(
                &task.task,
                &task.agent,
                self.pi_child_depth(depth),
                ct,
                SpawnOptions {
                    label: task.label.clone(),
                    ..SpawnOptions::default()
                },
            )
            .await;
        self.print_finish(&c, depth);
        to_outcome(&c, self.orchestrator.cwd())
    }

    /// Pi profile subagent tool: parallel tasks (bounded by agent.maxConcurrent).
    pub async fn run_pi_parallel(
        &self,
        tasks: &[SubagentTask],
        ct: &CancelToken,
    ) -> Result<Vec<SubagentOutcome>, String> {
        let depth = AgentOrchestrator::current_depth();
        self.print_line(
            &format!(
                "    {} tasks in parallel (max {} concurrent)",
                tasks.len(),
                self.orchestrator.config().agent.max_concurrent
            ),
            depth,
        );
        let contexts = self
            .orchestrator
            .spawn_parallel(&to_specs(tasks), self.pi_child_depth(depth), ct)
            .await?;
        for c in &contexts {
            self.print_finish(c, depth);
        }
        Ok(contexts
            .iter()
            .map(|c| to_outcome(c, self.orchestrator.cwd()))
            .collect())
    }

    /// Pi follow-up ("to" mode).
    pub async fn run_pi_follow_up(
        &self,
        to: &str,
        task: &str,
        ct: &CancelToken,
    ) -> SubagentOutcome {
        match self.run_follow_up_core(to, task, ct).await {
            Ok(c) => to_outcome(&c, self.orchestrator.cwd()),
            Err(e) => SubagentOutcome {
                agent: "?".into(),
                ok: false,
                output: e,
            },
        }
    }

    fn pi_child_depth(&self, caller_depth: i32) -> i32 {
        caller_depth.max(self.orchestrator.config().agent.max_spawn_depth - 1)
    }

    // ── peer mode: background hand-offs ──────────────────────────────────────

    /// Helpers started in the background that the main agent has not collected yet.
    pub fn has_background_work(&self) -> bool {
        !self
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Start a hand-off in the background and return at once.
    pub fn start_pi_background(self: &Arc<Self>, tasks: Vec<SubagentTask>) -> String {
        let depth = AgentOrchestrator::current_depth();
        let ct = self
            .background_ct
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.print_line(
            &format!(
                "    {} helper(s) in the background (max {} concurrent)",
                tasks.len(),
                self.orchestrator.config().agent.max_concurrent
            ),
            depth,
        );
        let names: Vec<String> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| t.label.clone().unwrap_or_else(|| format!("task {}", i + 1)))
            .collect();
        let count = tasks.len();
        let me = self.clone();
        let child_depth = self.pi_child_depth(depth);
        let run = tokio::spawn(async move {
            match me
                .orchestrator
                .spawn_parallel(&to_specs(&tasks), child_depth, &ct)
                .await
            {
                Ok(contexts) => {
                    for c in &contexts {
                        me.print_finish(c, depth);
                    }
                    contexts
                        .iter()
                        .map(|c| to_outcome(c, me.orchestrator.cwd()))
                        .collect()
                }
                Err(e) => {
                    tracing::warn!("Background helpers failed: {e}");
                    vec![SubagentOutcome {
                        agent: "worker".into(),
                        ok: false,
                        output: format!("Background helpers failed to start: {e}"),
                    }]
                }
            }
        });
        self.background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(run);
        format!(
            "Started {count} helper{} in the background: {}.",
            if count == 1 { "" } else { "s" },
            names.join(", ")
        )
    }

    /// Wait for every background hand-off; their outcomes in start order, or None when none ran.
    /// Cancelling `ct` cancels the helpers.
    pub async fn wait_pi_background(&self, ct: &CancelToken) -> Option<Vec<SubagentOutcome>> {
        let pending: Vec<BackgroundRun> =
            std::mem::take(&mut *self.background.lock().unwrap_or_else(|e| e.into_inner()));
        if pending.is_empty() {
            return None;
        }
        let all = futures_util::future::join_all(pending);
        tokio::select! {
            results = all => Some(results.into_iter().flat_map(|r| r.unwrap_or_default()).collect()),
            _ = ct.cancelled() => {
                self.cancel_background();
                None
            }
        }
    }

    /// End of the main agent's turn: background helpers it never collected are waited for and
    /// their reports handed back as a new message. `format`: the Pi tool's outcome formatter.
    pub async fn collect_background(
        &self,
        ct: &CancelToken,
        format: &(dyn Fn(&[SubagentOutcome]) -> String + Send + Sync),
    ) -> Option<String> {
        if !self.has_background_work() {
            return None;
        }
        tracing::info!(
            "Main agent ended its turn with background helpers running; waiting for them"
        );
        let outcomes = self.wait_pi_background(ct).await?;
        Some(format!(
            "[Your background helpers have finished; you had not collected them. Their reports follow. Check their work together with yours against the request, fix what fails (follow-ups to the writer), then finish.]\n\n{}",
            format(&outcomes)
        ))
    }

    /// Cancel the background helpers.
    pub fn cancel_background(&self) {
        let mut ct = self
            .background_ct
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ct.cancel();
        *ct = CancelToken::new();
    }

    // ── user agents ──────────────────────────────────────────────────────────

    /// /agent: start a background agent.
    pub fn start_user_agent(&self, task: &str, agent_type: &str) -> Arc<AgentContext> {
        self.orchestrator.start_background_agent(task, agent_type, None)
    }

    /// Results of background agents that finished since the last call, for the main agent's
    /// next turn (None when there are none). Each is handed over once.
    pub fn drain_finished_user_agent_notes(&self) -> Option<String> {
        let user_notes = self.orchestrator.drain_user_agent_notes();
        let mut q = self
            .finished_user_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return user_notes;
        }
        let mut sb = String::new();
        if let Some(n) = user_notes {
            sb.push_str(&n);
            sb.push_str(NEWLINE);
            sb.push_str(NEWLINE);
        }
        while let Some(c) = q.pop_front() {
            sb.push_str(&format!(
                "[{} \u{2014} background agent #{} ({}) {} \u{2014} task: {}]{NEWLINE}",
                c.tag(),
                c.number,
                c.agent_type,
                status_word(&c),
                preview(&c.task, 200)
            ));
            let s = c.state();
            sb.push_str(
                &s.result
                    .clone()
                    .or(s.error.clone())
                    .unwrap_or_else(|| "(no result)".into()),
            );
            drop(s);
            sb.push_str(NEWLINE);
            sb.push_str(NEWLINE);
        }
        Some(sb.trim_end().to_string())
    }

    fn on_agent_event(&self, e: &AgentEvent) {
        match e {
            AgentEvent::Completed { agent_id, .. } => {
                if let Some(user) = self.orchestrator.find_agent(agent_id)
                    && user.origin == AgentOrigin::User
                    && user.status() != AgentStatus::Cancelled
                {
                    self.finished_user_agents
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(user);
                }
            }
            AgentEvent::ToolCallStarted {
                agent_id,
                tool_name,
                activity,
                ..
            } => {
                let panel = (self.panel_active.lock().unwrap_or_else(|e| e.into_inner()))();
                if !panel
                    && let Some(agent) = self.orchestrator.find_agent(agent_id)
                    && agent.origin != AgentOrigin::User
                {
                    self.print_line(
                        &format!(
                            "    \u{2502} #{} {} \u{b7} {}",
                            agent.number,
                            agent.agent_type,
                            activity.as_deref().unwrap_or(tool_name)
                        ),
                        agent.caller_depth,
                    );
                }
            }
            _ => {}
        }
    }

    fn print_finish(&self, c: &AgentContext, depth: i32) {
        let s = c.state();
        let elapsed = s
            .completed_at
            .unwrap_or_else(std::time::SystemTime::now)
            .duration_since(s.started_at.unwrap_or(std::time::UNIX_EPOCH))
            .unwrap_or_default();
        let (status, iterations, error) = (s.status, s.iteration_count, s.error.clone());
        drop(s);
        let elapsed = format_elapsed(elapsed);
        let tag = format!("{} {}{}", c.tag(), c.label(), model_part(c));
        let line = match status {
            AgentStatus::Completed => format!(
                "    \u{2713} #{} {} agent {tag} ({iterations} iterations, {elapsed})",
                c.number, c.agent_type
            ),
            AgentStatus::CompletedPartial => format!(
                "    \u{26a0} #{} {} agent {tag} partial \u{2014} {}, {elapsed}",
                c.number,
                c.agent_type,
                c.limit_description()
            ),
            AgentStatus::Cancelled => {
                format!("    \u{2298} #{} {} agent cancelled {tag}", c.number, c.agent_type)
            }
            _ => format!(
                "    \u{2717} #{} {} agent {tag} {}",
                c.number,
                c.agent_type,
                error.unwrap_or_default()
            ),
        };
        self.print_line(&line, depth);
    }

    fn print_line(&self, line: &str, depth: i32) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sink) = sink {
            let indent = " ".repeat((depth.max(0) * 2) as usize);
            sink(&format!("{indent}{line}"));
        }
    }
}

fn to_specs(tasks: &[SubagentTask]) -> Vec<ParallelAgentSpec> {
    tasks
        .iter()
        .map(|t| ParallelAgentSpec {
            task: t.task.clone(),
            agent_type: t.agent.clone(),
            label: t.label.clone(),
            size: t.size,
            after: t.after.clone(),
        })
        .collect()
}

/// "1m 05s" / "12s" (`AgentPanelLayout.FormatElapsed`).
pub fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// What spawn_agent hands back: the header, a blank line, then the result or the failure.
pub fn tool_result_text(c: &AgentContext) -> String {
    let s = c.state();
    let body = match s.status {
        AgentStatus::Completed | AgentStatus::CompletedPartial => s
            .result
            .clone()
            .unwrap_or_else(|| "(no result)".into()),
        _ => format!("Sub-agent failed: {}", s.error.clone().unwrap_or_default()),
    };
    drop(s);
    format!("{}\n\n{body}", result_header(c))
}

/// "Agent A (Scan auth code · explore) — completed".
pub fn result_header(c: &AgentContext) -> String {
    format!(
        "{} ({} \u{b7} {}{}) \u{2014} {}",
        c.tag(),
        c.label(),
        c.agent_type,
        model_part(c),
        status_word(c)
    )
}

/// " · haiku" / " · local" in a mixed swarm, else "".
pub fn model_part(c: &AgentContext) -> String {
    match c.model_tag() {
        Some(m) if !m.is_empty() => format!(" \u{b7} {m}"),
        _ => String::new(),
    }
}

pub fn status_word(c: &AgentContext) -> String {
    match c.status() {
        AgentStatus::Completed => "completed".into(),
        AgentStatus::CompletedPartial => {
            if c.stopped_at_time_limit() {
                "partial (time limit)".into()
            } else {
                "partial (iteration limit)".into()
            }
        }
        AgentStatus::Cancelled => "cancelled".into(),
        _ => format!("failed: {}", c.error().unwrap_or_default()),
    }
}

/// An agent that stopped at its step limit: INCOMPLETE up front, how to continue it, its report.
pub fn partial_outcome_text(c: &AgentContext) -> String {
    format!(
        "{} ({}{}): INCOMPLETE, stopped at its {} with work remaining. To finish it, send a follow-up to the same agent: {{\"to\": \"{}\", \"task\": \"Continue: <its Remaining list>\"}}; it keeps its context and gets a fresh step and time budget.\n{}",
        c.tag(),
        c.label(),
        model_part(c),
        c.limit_description(),
        c.letter(),
        c.result().unwrap_or_default()
    )
}

/// The Pi outcome of a finished agent.
pub fn to_outcome(c: &AgentContext, cwd: &std::path::Path) -> SubagentOutcome {
    let files = files_line(c, cwd);
    match c.status() {
        AgentStatus::Completed => SubagentOutcome {
            agent: c.agent_type.clone(),
            ok: true,
            output: format!(
                "{} ({}{}):\n{}{files}",
                c.tag(),
                c.label(),
                model_part(c),
                c.result().unwrap_or_default()
            ),
        },
        AgentStatus::CompletedPartial => SubagentOutcome {
            agent: c.agent_type.clone(),
            ok: true,
            output: format!("{}{files}", partial_outcome_text(c)),
        },
        _ => SubagentOutcome {
            agent: c.agent_type.clone(),
            ok: false,
            output: format!(
                "{} ({}{}): {}{files}",
                c.tag(),
                c.label(),
                model_part(c),
                c.error()
                    .or(c.result())
                    .unwrap_or_else(|| "unknown error".into())
            ),
        },
    }
}

/// "[Files Agent C wrote: …]" so the Lead sends fixes to the writer.
pub fn files_line(c: &AgentContext, cwd: &std::path::Path) -> String {
    let files = c.files_written();
    if files.is_empty() {
        return String::new();
    }
    let cwd_s = cwd.display().to_string();
    let shown: Vec<String> = files
        .iter()
        .take(15)
        .map(|f| {
            let p = std::path::Path::new(f);
            let rel = if p.is_absolute()
                && f.len() >= cwd_s.len()
                && f[..cwd_s.len()].replace('\\', "/").eq_ignore_ascii_case(&cwd_s.replace('\\', "/"))
            {
                p.strip_prefix(cwd)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| f.clone())
            } else {
                f.clone()
            };
            rel.replace('\\', "/")
        })
        .collect();
    format!(
        "\n[Files {} wrote: {}{}]",
        c.tag(),
        shown.join(", "),
        if files.len() > 15 {
            format!(", \u{2026} ({} more)", files.len() - 15)
        } else {
            String::new()
        }
    )
}

/// One line, at most `max` UTF-16 units ("..." when cut).
pub fn preview(text: &str, max: usize) -> String {
    let one = text.replace(['\r', '\n'], " ");
    if one.encode_utf16().count() > max {
        format!("{}...", take16(&one, max.saturating_sub(3)))
    } else {
        one
    }
}
