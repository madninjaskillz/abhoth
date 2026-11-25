//! The Pi profile's agent tools backed by the orchestrator (`AgentWiring.RegisterPiAgentTools`
//! and `WireTools`' Pi part, `LeadModeChooser`, `AgentManager.PeerHandOffBlockAsync`):
//! `subagent` runs through the [`AgentManager`], `progress` reports to the orchestrator's Lead
//! bar, and `lead_mode` records the Lead's choice for the current request.

use std::sync::Arc;

use orca_core::agent::ToolRegistry;
use orca_core::config::{OrcaConfig, delegate_gate};
use orca_core::orchestration::{
    AgentManager, AgentOrchestrator, CancelToken, SubagentOutcome, SubagentTask,
};
use orca_core::slots::SlotRuntime;
use orca_tools_common::NEWLINE;
use orca_tools_pi::lead_mode::{self, ChooseFn, PiLeadModeTool};
use orca_tools_pi::progress::PiProgressTool;
use orca_tools_pi::subagent::{
    self, BoxFuture, PiFileOwner, PiSubagentHooks, PiSubagentOutcome, PiSubagentSettings,
    PiSubagentTask, PiSubagentTool,
};

/// Free model copies, asked of the slot registry (`SlotAllocator.FreeWholeCopiesAsync`).
pub type FreeCopiesFn = Arc<dyn Fn() -> BoxFuture<Option<Vec<String>>> + Send + Sync>;

/// lead_mode result prefix when a delegate choice fell back to hands-on.
pub const NO_FREE_COPY_NOTE: &str = "Delegate is not available now: no model copy is free for a worker (a worker sharing a copy halves its speed and yours), so this request is hands-on.";

/// `TooFewFreeCopiesNote` ({0} free, {1} needed).
pub fn too_few_free_copies_note(free: usize, needed: i32) -> String {
    format!(
        "Delegate is not available now: only {free} model copy/copies are free for workers and delegating needs {needed} (workers would run one after another), so this request is hands-on."
    )
}

/// `FreeCopiesNote`: how many workers can run at once.
pub fn free_copies_note(free: usize) -> String {
    format!(
        "{free} model copy/copies are free for workers: each worker gets a whole copy and the rest wait their turn, so split the work into at most {free} piece(s) per subagent call."
    )
}

/// `SharedLeadCopyNote`: workers that can run at once, on free copies and sharing the Lead's.
pub fn shared_lead_copy_note(at_once: usize, free: usize) -> String {
    format!(
        "{at_once} worker(s) can run at once: {free} on free model copies and 1 sharing your copy (it slows you while it runs, so leave the waiting to it). Split the work into at most {at_once} piece(s) per subagent call; more pieces wait their turn."
    )
}

/// `LeadModeChooser.Instructions`: the lead_mode result for `mode` in this profile.
pub fn instructions(config: &OrcaConfig, mode: &str) -> String {
    match mode {
        lead_mode::DELEGATE => {
            let mut s = String::from(
                "Delegate for this request: you cannot write or edit files until the user's next message; bash stays for checks.\n",
            );
            if orca_tools_pi::is_pi_profile(&config.tool_profile) {
                s.push_str(&subagent::orchestrator_instructions(true));
            } else {
                s.push_str(&orca_core::agent::system_prompt::orchestrator_section(true));
            }
            if orca_core::orchestration::is_swarm(config) {
                s.push_str(NEWLINE);
                s.push_str(&subagent::swarm_instructions(config.agent.max_agents));
            }
            s
        }
        lead_mode::HANDS_ON => lead_mode::HANDS_ON_INSTRUCTIONS.to_string(),
        _ => lead_mode::ANSWER_INSTRUCTIONS.to_string(),
    }
}

/// `LeadModeChooser.Create`: stores the choice for the current request (the orchestrator's
/// Lead request mode, which the policies and the footer read), logs it and answers with the
/// mode's instructions. With slots.wholeCopyPerAgent a delegate choice needs enough free model
/// copies (`free_copies`), else it falls back to hands-on.
pub fn lead_mode_chooser(
    config: OrcaConfig,
    orchestrator: Option<Arc<AgentOrchestrator>>,
    free_copies: Option<FreeCopiesFn>,
) -> ChooseFn {
    let config = Arc::new(config);
    Arc::new(move |mode: String, reason: Option<String>| {
        let config = config.clone();
        let orchestrator = orchestrator.clone();
        let free_copies = free_copies.clone();
        Box::pin(async move {
            let mut mode = mode;
            let mut reason = reason;
            let mut downgrade: Option<String> = None;
            let whole_copies = config
                .slots
                .as_ref()
                .is_some_and(|s| s.enabled && s.whole_copy_per_agent);
            if mode == lead_mode::DELEGATE
                && whole_copies
                && let Some(free_copies) = &free_copies
            {
                let free =
                    match tokio::time::timeout(std::time::Duration::from_secs(5), free_copies())
                        .await
                    {
                        Ok(f) => f,
                        Err(_) => {
                            tracing::debug!("Free-copy check failed; delegation allowed");
                            None
                        }
                    };
                let needed = delegate_gate::copies_needed(&config);
                let copies = config.slots.as_ref().map_or(0, |s| s.copies.len());
                match free {
                    Some(free) if (free.len() as i32) < needed => {
                        tracing::info!(
                            "Lead asked to delegate but {} model copy/copies are free for workers (need {needed}) \u{2014} hands-on instead",
                            free.len()
                        );
                        downgrade = Some(if free.is_empty() {
                            NO_FREE_COPY_NOTE.to_string()
                        } else {
                            too_few_free_copies_note(free.len(), needed)
                        });
                        mode = lead_mode::HANDS_ON.to_string();
                        reason = Some(format!(
                            "{} (delegate refused: {} free model copy/copies, need {needed})",
                            reason.as_deref().unwrap_or("no reason given"),
                            free.len()
                        ));
                    }
                    Some(free) if delegate_gate::shares_lead_copy(&config) && copies > 1 => {
                        downgrade = Some(shared_lead_copy_note(free.len() + 1, free.len()));
                    }
                    Some(free) if !free.is_empty() => {
                        downgrade = Some(free_copies_note(free.len()));
                    }
                    _ => {}
                }
            }

            let previous = orchestrator.as_ref().and_then(|o| o.lead_request_mode());
            if let Some(o) = &orchestrator {
                o.set_lead_request_mode(Some(&mode));
            }
            let was = match previous {
                Some(p) if p != mode => format!(" (was {p})"),
                _ => String::new(),
            };
            tracing::info!(
                "Lead mode for this request: {mode}{was} \u{2014} {}",
                reason.as_deref().unwrap_or("no reason given")
            );
            let text = instructions(&config, &mode);
            match downgrade {
                None => text,
                Some(d) if mode == lead_mode::HANDS_ON => format!("{d}\n{text}"),
                Some(d) => format!("{text}\n{d}"),
            }
        }) as BoxFuture<String>
    })
}

/// `AgentManager.PeerHandOffBlock`: the refusal for `free` free copies when `min_free` are
/// needed, or None.
pub fn peer_hand_off_block(free: usize, min_free: i32) -> Option<String> {
    let needed = min_free.max(0);
    if free as i32 >= needed {
        return None;
    }
    tracing::info!("Hand-off refused: {free} model copy/copies free for helpers, need {needed}");
    Some(
        subagent::NO_HELPER_FREE_NOTE
            .replace("{0}", &free.to_string())
            .replace("{1}", &needed.to_string()),
    )
}

/// `AgentManager.PeerHandOffBlockAsync`: peer mode with slots.wholeCopyPerAgent refuses a
/// hand-off while too few model copies are free for helpers.
pub async fn peer_hand_off_block_for(
    config: &OrcaConfig,
    slots: Option<&Arc<SlotRuntime>>,
) -> Option<String> {
    let whole_copies = config
        .slots
        .as_ref()
        .is_some_and(|s| s.enabled && s.whole_copy_per_agent);
    if !config.agent.peer_mode || !whole_copies {
        return None;
    }
    let allocator = slots?.allocator.clone();
    let free = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        allocator.free_whole_copies(),
    )
    .await
    {
        Ok(f) => f,
        Err(_) => {
            tracing::debug!("Free-copy check failed; hand-off allowed");
            return None;
        }
    };
    peer_hand_off_block(free.len(), delegate_gate::copies_needed(config))
}

fn to_task(t: PiSubagentTask) -> SubagentTask {
    SubagentTask {
        agent: t.agent,
        task: t.task,
        label: t.label,
        size: t.size,
        after: t.after,
    }
}

fn to_pi(o: SubagentOutcome) -> PiSubagentOutcome {
    PiSubagentOutcome {
        agent: o.agent,
        succeeded: o.ok,
        output: o.output,
    }
}

/// "scout (...), worker (...)" from the orchestrator's agent types (custom ones included),
/// ordered by name.
pub fn agent_list(orchestrator: &AgentOrchestrator) -> String {
    let mut types = orchestrator.type_registry().get_all();
    types.sort_by(|a, b| a.name.cmp(&b.name));
    types
        .iter()
        .map(|t| format!("{} ({})", t.name, t.description))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The subagent tool's host side over `manager`.
pub fn subagent_hooks(
    manager: &Arc<AgentManager>,
    config: &OrcaConfig,
    slots: Option<Arc<SlotRuntime>>,
    offers_wait: bool,
) -> PiSubagentHooks {
    let o = manager.orchestrator().clone();
    let mut hooks = PiSubagentHooks::default();
    let m = manager.clone();
    hooks.run_one = Some(Arc::new(move |task: PiSubagentTask| {
        let m = m.clone();
        Box::pin(async move { to_pi(m.run_pi_one(&to_task(task), &CancelToken::new()).await) })
    }));
    let m = manager.clone();
    hooks.run_parallel = Some(Arc::new(move |tasks: Vec<PiSubagentTask>| {
        let m = m.clone();
        Box::pin(async move {
            let tasks: Vec<SubagentTask> = tasks.into_iter().map(to_task).collect();
            match m.run_pi_parallel(&tasks, &CancelToken::new()).await {
                Ok(outcomes) => outcomes.into_iter().map(to_pi).collect(),
                Err(e) => vec![PiSubagentOutcome {
                    agent: "?".into(),
                    succeeded: false,
                    output: e,
                }],
            }
        })
    }));
    let m = manager.clone();
    hooks.follow_up = Some(Arc::new(move |to: String, task: String| {
        let m = m.clone();
        Box::pin(async move { to_pi(m.run_pi_follow_up(&to, &task, &CancelToken::new()).await) })
    }));
    let oc = o.clone();
    hooks.cap_check = Some(Arc::new(move |n: usize| oc.cap_error(n as i32)));
    // Peer mode: hand-offs are refused while too few model copies are free for helpers.
    let config_h = config.clone();
    hooks.hand_off_check = Some(Arc::new(move || {
        let config = config_h.clone();
        let slots = slots.clone();
        Box::pin(async move { peer_hand_off_block_for(&config, slots.as_ref()).await })
    }));
    if offers_wait {
        let m = manager.clone();
        hooks.start_background = Some(Arc::new(move |tasks: Vec<PiSubagentTask>| {
            m.start_pi_background(tasks.into_iter().map(to_task).collect())
        }));
        let m = manager.clone();
        hooks.wait_background = Some(Arc::new(move || {
            let m = m.clone();
            Box::pin(async move {
                m.wait_pi_background(&CancelToken::new())
                    .await
                    .map(|v| v.into_iter().map(to_pi).collect())
            })
        }));
    }
    let oc = o.clone();
    hooks.budget_note = Some(Arc::new(move || oc.budget_note()));
    // Fix-ups go back to the writer: the files each finished agent wrote.
    let oc = o.clone();
    hooks.file_owners = Some(Arc::new(move || {
        let mut done: Vec<_> = oc
            .get_all_agents()
            .into_iter()
            .filter(|c| !c.is_active() && c.retain_conversation && !c.messages().is_empty())
            .collect();
        done.sort_by_key(|c| c.completed_at());
        done.iter()
            .flat_map(|c| {
                let letter = c.letter().to_string();
                c.files_written().into_iter().map(move |f| PiFileOwner {
                    path: f,
                    agent: letter.clone(),
                })
            })
            .collect()
    }));
    // "after" naming an agent from an earlier call: by letter or label, done once it finished.
    let oc = o.clone();
    hooks.earlier_agent = Some(Arc::new(move |name: &str| {
        let c = oc.find_agent(name).or_else(|| {
            oc.get_all_agents()
                .into_iter()
                .rev()
                .find(|a| a.label().eq_ignore_ascii_case(name.trim()))
        })?;
        let (tx, rx) = tokio::sync::watch::channel(!c.is_active());
        if c.is_active() {
            tokio::spawn(async move {
                c.wait().await;
                let _ = tx.send(true);
            });
        }
        Some(rx)
    }));
    let oc = o;
    hooks.available_agents = Some(Arc::new(move || {
        let mut names: Vec<String> = oc
            .type_registry()
            .get_all()
            .into_iter()
            .map(|t| t.name)
            .collect();
        names.sort();
        names
    }));
    hooks
}

/// `WireTools`' Pi part plus the lead_mode chooser: replaces the unwired Pi agent tools in
/// `registry` (same place, so the advertised order stays) with tools backed by `manager`.
/// No-op outside the Pi profile or without pi.agentTools.
pub fn wire_pi_agent_tools(
    registry: &mut ToolRegistry,
    manager: &Arc<AgentManager>,
    config: &OrcaConfig,
    slots: Option<&Arc<SlotRuntime>>,
) {
    if !orca_tools_pi::is_pi_profile(&config.tool_profile) || !config.pi.agent_tools {
        return;
    }
    let o = manager.orchestrator().clone();
    let agent = &config.agent;
    let auto_lead = orca_core::orchestration::is_auto_lead(config);
    let settings = PiSubagentSettings {
        agent_list: agent_list(&o),
        peer_mode: agent.peer_mode,
        offers_wait: agent.peer_mode && agent.peer_background,
        orchestrator_lead: orca_core::agent::policies::is_orchestrator_lead(config),
        auto_lead,
    };
    let offers_wait = settings.offers_wait;
    let tool = PiSubagentTool::new(settings);
    tool.set_hooks(subagent_hooks(manager, config, slots.cloned(), offers_wait));
    registry.replace(Arc::new(tool));

    let progress = PiProgressTool::default();
    let oc = o.clone();
    progress.set_report(Some(Arc::new(move |percent: i32, note: Option<&str>| {
        oc.report_progress(percent, note)
    })));
    registry.replace(Arc::new(progress));

    if auto_lead {
        registry.replace(Arc::new(wired_lead_mode(config, o, slots)));
    }
}

/// `PiLeadModeTool` with the Lead's chooser (`LeadModeChooser.Create`).
fn wired_lead_mode(
    config: &OrcaConfig,
    o: Arc<AgentOrchestrator>,
    slots: Option<&Arc<SlotRuntime>>,
) -> PiLeadModeTool {
    let tool = PiLeadModeTool::default();
    let free: Option<FreeCopiesFn> = slots.map(|rt| {
        let allocator = rt.allocator.clone();
        Arc::new(move || {
            let allocator = allocator.clone();
            Box::pin(async move { Some(allocator.free_whole_copies().await) })
                as BoxFuture<Option<Vec<String>>>
        }) as FreeCopiesFn
    });
    tool.set_choose(Some(lead_mode_chooser(config.clone(), Some(o), free)));
    tool
}

/// `ToolWiring`: the OpenOrca profile in `agent.leadMode` "auto" sizes each request with
/// lead_mode too (the Pi profile registers it with its agent tools), when the Lead can spawn.
pub fn register_auto_lead_mode(
    registry: &mut ToolRegistry,
    manager: &Arc<AgentManager>,
    config: &OrcaConfig,
    slots: Option<&Arc<SlotRuntime>>,
) {
    if !orca_core::orchestration::is_auto_lead(config)
        || registry.resolve(lead_mode::TOOL_NAME).is_some()
        || registry.resolve("spawn_agents").is_none()
    {
        return;
    }
    let o = manager.orchestrator().clone();
    registry.register(Arc::new(wired_lead_mode(config, o, slots)));
}

/// `AgentWiring.PiPromptFactory`: the Pi profile's sub-agent prompt, built like Pi's subagent
/// extension does it: Pi's own system prompt for the tools the agent sees, with the agent's
/// role appended as the addendum. The task arrives as the user message. Outside the Pi profile
/// None (the type's own prompt), except in peer mode (the type's prompt as a peer's role).
pub fn pi_prompt_factory(
    config: &OrcaConfig,
    cwd: std::path::PathBuf,
) -> orca_core::orchestration::SystemPromptFactory {
    use orca_core::orchestration::prompt_loader::AgentPromptLoader;
    use orca_tools_pi::{PiContextFile, system_prompt};
    let config = config.clone();
    let platform = orca_core::agent::system_prompt::os_version_string();
    Arc::new(move |type_def, tools, task| {
        let cwd_text = cwd.display().to_string();
        if !orca_tools_pi::is_pi_profile(&config.tool_profile) {
            // Peer mode, OpenOrca profile: the type's own prompt without the lead-agent wording.
            if !config.agent.peer_mode {
                return None;
            }
            return AgentPromptLoader::load_prompt(type_def, task, &cwd_text, &platform)
                .map(|typed| subagent::peer_role(&typed));
        }
        let mut role = AgentPromptLoader::load_prompt(type_def, task, &cwd_text, &platform)
            .unwrap_or_else(|| {
                "You are a focused sub-agent. Complete the task and report the result concisely."
                    .to_string()
            });
        if config.agent.peer_mode {
            role = subagent::peer_role(&role);
        }
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        let context: Vec<PiContextFile> = orca_core::config::project_instructions::load_all(&cwd)
            .into_iter()
            .map(|f| PiContextFile {
                path: f.full_path.display().to_string(),
                content: f.content,
            })
            .collect();
        // With the progress tool its <rules> entry covers progress; otherwise the free marker.
        let mut addendum = if names
            .iter()
            .any(|n| n == orca_tools_pi::progress::TOOL_NAME)
        {
            role.trim().to_string()
        } else {
            format!(
                "{}\n\n{}",
                role.trim(),
                orca_core::orchestration::PROGRESS_HINT
            )
        };
        if let Some(append) = config.pi.append_system_prompt.as_deref()
            && !append.trim().is_empty()
        {
            addendum = format!("{}\n\n{addendum}", append.trim());
        }
        // Adaptive reasoning runs the thinking steps at medium effort; this is the low effort's
        // sentence.
        if config.agent.reasoning.is_adaptive(false) && config.agent.reasoning.adaptive_brief_rule {
            addendum.push_str("\n\n");
            addendum.push_str(orca_core::chat::adaptive_reasoning::BRIEF_RULE);
        }
        Some(system_prompt::build(
            &names,
            &orca_tools_pi::prompt_snippets(),
            &orca_tools_pi::prompt_guidelines(),
            &context,
            &cwd_text,
            Some(&addendum),
            config.pi.batch_calls_rule,
        ))
    })
}
