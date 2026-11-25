//! Context management for [`AgentSession`]: the parts of `AgentLoopRunner` that track the
//! prompt size (`RecordPromptUsage`), compact before a request (`CompactIfNeededAsync`) and
//! recover from a server-reported overflow (`TryRecoverFromOverflowAsync`).
//!
//! The session keeps its history as `Vec<ChatMessage>` (system prompt first) plus a
//! [`ContextUsage`]; each operation works on a [`Conversation`] built from a copy of them and
//! writes the result back, so a cancelled compaction leaves the history as it was.

use super::session::AgentSession;
use crate::chat::compactor::{CompactionReport, ConversationCompactor, LlmCompactor};
use crate::chat::conversation::{ContextMark, Conversation};
use crate::chat::{CompactionAction, CompactionSettings, n0, planner};
use crate::client::{ChatError, Usage, context_overflow};

impl AgentSession {
    /// The history as a [`Conversation`] (a copy, with the usage bookkeeping).
    pub fn conversation(&self) -> Conversation {
        Conversation::from_api_messages(self.messages.clone(), self.context_usage.clone())
    }

    /// Store a conversation's history and bookkeeping back into the session.
    pub fn set_conversation(&mut self, conversation: Conversation) {
        let (messages, usage) = conversation.into_api_messages();
        self.messages = messages;
        self.context_usage = usage;
    }

    /// Take right before a request is sent; give to [`Self::record_prompt_usage`] after.
    pub fn context_mark(&self) -> ContextMark {
        self.conversation().mark()
    }

    /// The server's prompt size for the request taken at `mark` becomes the base of later
    /// estimates (only messages appended after it are estimated).
    pub fn record_prompt_usage(&mut self, mark: ContextMark, usage: &Usage) {
        let Some(tokens) = Conversation::prompt_tokens_of(usage) else {
            return;
        };
        let mut conversation = self.conversation();
        conversation.record_prompt_usage_at(tokens, mark);
        self.context_usage = conversation.usage().clone();
    }

    /// Best available size of the next request (server usage plus an estimate of what was
    /// appended since, or a calibrated full estimate).
    pub fn estimate_context_tokens(&self) -> i32 {
        self.conversation()
            .estimate_context_tokens(self.config.context.chars_per_token)
    }

    /// `CompactIfNeededAsync`: Pi's trigger, checked before every request (tool-heavy loops
    /// overflow mid-turn). The check never touches history; past the planned point the
    /// planner decides whether a rewrite pays. Returns true when the history was rewritten.
    /// Notices go to [`AgentSession::compaction_notices`].
    pub async fn maybe_compact(&mut self) -> bool {
        let context = &self.config.context;
        if !context.auto_compact_enabled {
            return false;
        }
        let settings = CompactionSettings::from_config(context);
        let mut conversation = self.conversation();
        let tokens = conversation.estimate_context_tokens(context.chars_per_token);
        tracing::debug!(target: crate::agent::log_lines::LOOP,
            "Context ~{tokens} tokens ({}); planned compaction at {}, hard limit {}",
            if conversation.has_usage_base() {
                "server usage + estimate"
            } else {
                "estimate"
            },
            settings.planned_trigger_tokens,
            settings.trigger_tokens()
        );
        if !settings.should_plan_compaction(tokens) && !settings.should_compact(tokens) {
            return false;
        }
        let plan = planner::plan(&conversation, &settings, context, None, None);
        if plan.action == CompactionAction::None {
            tracing::debug!(target: crate::agent::log_lines::LOOP, "Compaction check: {plan}");
            return false;
        }
        tracing::info!(target: crate::agent::log_lines::LOOP,
            "Auto-compacting (window {}, planned {}, hard {}; keep ~{} recent): {plan}",
            settings.context_window,
            settings.planned_trigger_tokens,
            settings.trigger_tokens(),
            settings.keep_recent_tokens
        );
        let (rewritten, report) = {
            let compactor = LlmCompactor::new(&self.client, &self.config, &self.actor)
                .with_text_tool_call_parser(&parse_text_tool_calls);
            planner::apply(
                &mut conversation,
                &plan,
                &self.config.context,
                Some(&compactor as &dyn ConversationCompactor),
            )
            .await
        };
        tracing::info!(target: crate::agent::log_lines::LOOP,
            "Compaction done: ~{} -> ~{} tokens ({})",
            plan.tokens_before,
            conversation.estimate_context_tokens(self.config.context.chars_per_token),
            plan.action
        );
        self.compaction_notices.extend(report.notices);
        self.set_conversation(conversation);
        rewritten
    }

    /// `/compact`: summarize now (the same compactor as auto-compaction).
    pub async fn compact_now(&mut self, extra_instructions: Option<&str>) -> CompactionReport {
        let mut conversation = self.conversation();
        let report = LlmCompactor::new(&self.client, &self.config, &self.actor)
            .with_text_tool_call_parser(&parse_text_tool_calls)
            .compact_conversation(&mut conversation, extra_instructions, None)
            .await;
        self.set_conversation(conversation);
        report
    }

    /// `TryRecoverFromOverflowAsync`: the server rejected the prompt as too long. Retrying the
    /// same prompt cannot help, so compact, then retry, once per turn
    /// ([`AgentSession::overflow_recovery_used`]; the loop clears it when a turn starts). When
    /// the error states a real window smaller than the configured one, it is adopted so
    /// proactive compaction triggers at the right size from now on. True when the caller
    /// should retry the request.
    pub async fn recover_from_overflow(&mut self, err: &ChatError) -> bool {
        if self.overflow_recovery_used || !context_overflow::is_overflow_error(err) {
            return false;
        }
        self.overflow_recovery_used = true;

        let configured = self.config.context.context_window_size;
        if let Some(reported) = context_overflow::try_parse_context_size_error(err)
            && reported < configured
        {
            self.notice(format!(
                "Server context window is {} tokens (configured {}) — using the server's.",
                n0(i64::from(reported)),
                n0(i64::from(configured))
            ));
            self.config.context.context_window_size = reported;
        }
        if !self.config.context.auto_compact_enabled {
            self.notice("Context window exceeded and auto-compaction is disabled — run /compact, or enable context.autoCompactEnabled.");
            return false;
        }
        tracing::warn!(target: crate::agent::log_lines::LOOP, "Context overflow reported by server — compacting and retrying once: {err}");
        self.notice("Context window exceeded — compacting the conversation and retrying...");

        let cpt = self.config.context.chars_per_token;
        let mut conversation = self.conversation();
        let before = conversation.estimate_token_count(cpt);
        conversation.prune_old_tool_results(self.config.context.compact_preserve_last_n, 200);
        let report = LlmCompactor::new(&self.client, &self.config, &self.actor)
            .with_text_tool_call_parser(&parse_text_tool_calls)
            .compact_conversation(&mut conversation, None, None)
            .await;
        let after = conversation.estimate_token_count(cpt);
        self.compaction_notices.extend(report.notices);
        self.set_conversation(conversation);
        if after >= before {
            self.notice(
                "Compaction could not shrink the conversation — giving up on this request.",
            );
            return false;
        }
        true
    }

    fn notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.compaction_notices.push(text);
    }
}

/// The session's LLM summariser as an owned compactor (`IConversationCompactor` in DI), for
/// sub-agents' compaction: requests go through the session's client (in an agent's flow, its
/// slot when sticky slots are on) labelled with the agent's id.
pub struct SessionCompactor {
    client: crate::client::OpenAiClient,
    config: crate::config::OrcaConfig,
}

impl SessionCompactor {
    pub fn new(client: crate::client::OpenAiClient, config: crate::config::OrcaConfig) -> Self {
        Self { client, config }
    }
}

#[async_trait::async_trait]
impl ConversationCompactor for SessionCompactor {
    async fn compact_conversation(
        &self,
        conversation: &mut Conversation,
        extra_instructions: Option<&str>,
        context: Option<&crate::config::ContextConfig>,
    ) -> CompactionReport {
        let actor = crate::orchestration::AgentOrchestrator::current_agent()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| "lead".to_string());
        LlmCompactor::new(&self.client, &self.config, &actor)
            .with_text_tool_call_parser(&parse_text_tool_calls)
            .compact_conversation(conversation, extra_instructions, context)
            .await
    }
}

/// Tool calls written as text, for the compactor's file lists (C# passes its ToolCallParser).
fn parse_text_tool_calls(text: &str) -> Vec<crate::client::ToolCall> {
    super::tool_call_parser::parse_tool_calls_from_text(text)
        .into_iter()
        .map(|p| crate::client::ToolCall {
            id: p.id,
            name: p.name,
            arguments: serde_json::Value::Object(p.arguments).to_string(),
        })
        .collect()
}
