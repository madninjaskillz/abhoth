//! The orchestrator behind the REPL's and the web UI's sub-agent features: `/review`,
//! `/workflow`, `/agent`, `/agents` ([`ReplAgentHost`]) and the agent tabs and bars of
//! `--hostui` sessions ([`WebAgentHost`], `LiveSessionAdapter`'s agent part).

use std::sync::Arc;

use async_trait::async_trait;
use orca_core::config::OrcaConfig;
use orca_core::live::LiveAgent;
use orca_core::orchestration::transcript::{AgentTranscriptEntry, AgentTranscriptKind};
use orca_core::orchestration::types::AgentStatus;
use orca_core::orchestration::{
    AgentContext, AgentManager, AgentMessageResult, AgentOrchestrator, LEAD_ID, can_resume,
};
use orca_webui::live::{LiveAgentEntry, LiveAgentMessageResult, LiveAgentTranscript};

use crate::commands::{AgentHost, AgentRunOutcome};

/// The REPL's sub-agents: the session's [`AgentManager`].
pub struct ReplAgentHost {
    manager: Arc<AgentManager>,
}

impl ReplAgentHost {
    pub fn new(manager: Arc<AgentManager>) -> Self {
        Self { manager }
    }
}

/// What a finished run reports to `/review`.
pub fn run_outcome(c: &AgentContext) -> AgentRunOutcome {
    let status = c.status();
    AgentRunOutcome {
        completed: status == AgentStatus::Completed,
        status: status.as_str().to_string(),
        iterations: c.iteration_count().max(0) as u32,
        result: c.result(),
        error: c.error(),
    }
}

#[async_trait]
impl AgentHost for ReplAgentHost {
    async fn spawn_agent(&self, task: &str, agent_type: &str) -> Result<AgentRunOutcome, String> {
        let c = self
            .manager
            .orchestrator()
            .spawn_agent(task, agent_type)
            .await;
        Ok(run_outcome(&c))
    }

    fn manager(&self) -> Option<Arc<AgentManager>> {
        Some(self.manager.clone())
    }
}

/// Most of a message the user sends an agent from its tab.
pub const MAX_AGENT_MESSAGE_CHARS: usize = 50_000;

/// The web UI's agent tabs and bars over the session's orchestrator.
pub struct WebAgentHost {
    orchestrator: Arc<AgentOrchestrator>,
    config: OrcaConfig,
    in_turn: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl WebAgentHost {
    /// `in_turn`: whether the Lead's turn runs now (its bar's status).
    pub fn new(
        orchestrator: Arc<AgentOrchestrator>,
        config: OrcaConfig,
        in_turn: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            orchestrator,
            config,
            in_turn,
        }
    }

    /// Publish on `bus`: the agent bars whenever an agent changes (`OnAgentEvent` +
    /// `FlushAgents`), and a notice when an agent the user continued from its tab replies
    /// (`OnUserAgentReplied`).
    /// `decorate`: the session's view of the list (a bridge's Lead bar), `None` as is.
    pub fn attach(
        self: &Arc<Self>,
        bus: Arc<orca_core::live::LiveEventBus>,
        decorate: Option<orca_webui::bridge::AgentsDecorator>,
    ) {
        let me = Arc::downgrade(self);
        let b = bus.clone();
        self.orchestrator.subscribe(move |_| {
            let Some(me) = me.upgrade() else { return };
            if !b.is_active() {
                return;
            }
            let mut agents = orca_webui::live::AgentHost::agents(&*me);
            if let Some(d) = &decorate {
                agents = d(agents);
            }
            b.publish(
                orca_core::live::types::AGENTS,
                serde_json::json!({ "agents": agents }),
            );
        });
        self.orchestrator
            .on_user_agent_replied(move |agent: &AgentContext, _note: &str| {
                bus.publish(
                    orca_core::live::types::NOTICE,
                    serde_json::json!({"level": "info", "text": reply_notice(agent)}),
                );
            });
    }

    fn by_id(&self, id: &str) -> Option<Arc<AgentContext>> {
        if id.is_empty() {
            return None;
        }
        self.orchestrator
            .get_all_agents()
            .into_iter()
            .find(|a| a.id == id)
    }
}

/// The main chat's line when an agent the user continued replies.
pub fn reply_notice(agent: &AgentContext) -> String {
    let mut reply = agent
        .result()
        .or_else(|| agent.error())
        .unwrap_or_else(|| "(no reply)".into());
    // The note for the Lead repeats what the user wrote; the user knows.
    if let Some(at) = reply.find("\n\n[User told this agent directly") {
        reply.truncate(at);
    }
    if orca_tools_common::utf16_len(&reply) > 600 {
        let mut cut = String::new();
        let mut units = 0;
        for ch in reply.chars() {
            if units + ch.len_utf16() > 600 {
                break;
            }
            units += ch.len_utf16();
            cut.push(ch);
        }
        reply = format!("{cut}\u{2026}");
    }
    format!(
        "You messaged {} ({}); its reply:\n{reply}\n\nThe Lead gets this with your next message.",
        agent.tag(),
        agent.label()
    )
}

fn display_label(a: &AgentContext) -> String {
    let label = a.label();
    if label.trim().is_empty() {
        a.tag()
    } else {
        label
    }
}

/// `LiveSessionAdapter.ToLive`.
pub fn to_live(agent_id: &str, e: &AgentTranscriptEntry) -> LiveAgentEntry {
    LiveAgentEntry {
        agent_id: agent_id.to_string(),
        n: e.n,
        at: {
            let d =
                e.at.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
            orca_webui::util::iso_from_unix(d.as_secs() as i64, d.subsec_nanos())
        },
        kind: match e.kind {
            AgentTranscriptKind::Task => "task",
            AgentTranscriptKind::AssistantText => "assistant",
            AgentTranscriptKind::ToolCall => "tool",
            AgentTranscriptKind::ToolResult => "toolResult",
            AgentTranscriptKind::UserMessage => "message",
            _ => "status",
        }
        .to_string(),
        text: e.text.clone(),
        is_error: e.is_error,
        tool: e.tool.clone(),
        args: e.args.clone(),
        from: e.from.clone(),
    }
}

#[async_trait]
impl orca_webui::live::AgentHost for WebAgentHost {
    /// `BuildAgents`: the Lead's bar, then running helpers and the six most recent finished.
    fn agents(&self) -> Vec<LiveAgent> {
        let o = &self.orchestrator;
        let model = self.config.lm_studio.primary_model().map(str::to_string);
        let mut list = vec![LiveAgent {
            id: LEAD_ID.into(),
            letter: "L".into(),
            label: "Lead".into(),
            kind: o.lead_request_mode().unwrap_or_else(|| "main".into()),
            status: if (self.in_turn)() { "running" } else { "idle" }.into(),
            progress: o.lead_progress(),
            note: o.lead_note(),
            activity: None,
            slot: None,
            model: model.clone(),
            is_lead: true,
            requested_model: model.clone(),
            served_model: None,
        }];
        let all = o.get_all_agents();
        let mut finished: Vec<_> = all.iter().filter(|a| !a.is_active()).cloned().collect();
        finished.sort_by_key(|a| std::cmp::Reverse(a.completed_at()));
        let shown = all
            .iter()
            .filter(|a| a.is_active())
            .cloned()
            .chain(finished.into_iter().take(6))
            .take(16);
        for a in shown {
            let (slot, configured) = {
                let s = a.state();
                (
                    s.slot.clone().or_else(|| s.last_slot.clone()),
                    s.provider_model
                        .clone()
                        .or_else(|| s.model_tag.clone())
                        .or_else(|| model.clone()),
                )
            };
            list.push(LiveAgent {
                id: a.id.clone(),
                letter: a.letter().to_string(),
                label: display_label(&a),
                kind: a.agent_type.clone(),
                status: a.status().as_str().to_lowercase(),
                progress: Some(if a.is_active() {
                    a.estimated_progress()
                } else {
                    1.0
                }),
                note: a.progress_note(),
                activity: a.current_activity(),
                slot,
                model: configured.clone(),
                is_lead: false,
                requested_model: configured,
                served_model: None,
            });
        }
        list
    }

    async fn transcript(&self, agent_id: &str) -> Option<LiveAgentTranscript> {
        let a = self.by_id(agent_id)?;
        let c = a.transcript.capture();
        let active = a.is_active();
        Some(LiveAgentTranscript {
            agent_id: a.id.clone(),
            letter: a.letter().to_string(),
            label: display_label(&a),
            kind: a.agent_type.clone(),
            status: a.status().as_str().to_lowercase(),
            task: a.task.clone(),
            entries: c.entries.iter().map(|e| to_live(&a.id, e)).collect(),
            dropped: c.dropped as i32,
            streaming: c.streaming,
            stream_end: c.stream_end,
            can_message: active || can_resume(&a),
            can_cancel: active,
            result: if active {
                None
            } else {
                a.result().or_else(|| a.error())
            },
        })
    }

    async fn send_message(&self, agent_id: &str, text: &str) -> LiveAgentMessageResult {
        if text.trim().is_empty() {
            return LiveAgentMessageResult::new(false, "empty", Some("Empty message."));
        }
        let len = orca_tools_common::utf16_len(text);
        if len > MAX_AGENT_MESSAGE_CHARS {
            return LiveAgentMessageResult::new(
                false,
                "tooLong",
                Some(&format!(
                    "Message too long ({} characters; the limit is {}).",
                    orca_webui::util::group_thousands(len),
                    orca_webui::util::group_thousands(MAX_AGENT_MESSAGE_CHARS)
                )),
            );
        }
        let Some(a) = self.by_id(agent_id) else {
            return LiveAgentMessageResult::new(
                false,
                "notFound",
                Some("No such agent in this session."),
            );
        };
        let tag = a.tag();
        match self.orchestrator.send_message(&a.id, text.trim()) {
            AgentMessageResult::Queued => LiveAgentMessageResult::new(
                true,
                "queued",
                Some(&format!("{tag} gets it at its next turn.")),
            ),
            AgentMessageResult::Resumed => LiveAgentMessageResult::new(
                true,
                "resumed",
                Some(&format!("{tag} is continuing with your message.")),
            ),
            AgentMessageResult::NotAccepting => LiveAgentMessageResult::new(
                false,
                "notAccepting",
                Some(&format!(
                    "{tag} finished and kept no conversation, so it can't be continued; ask the Lead instead."
                )),
            ),
            AgentMessageResult::NotFound => LiveAgentMessageResult::new(
                false,
                "notFound",
                Some("No such agent in this session."),
            ),
        }
    }

    async fn cancel(&self, agent_id: &str) -> bool {
        match self.by_id(agent_id) {
            Some(a) => self.orchestrator.cancel_agent(&a.id),
            None => false,
        }
    }
}
