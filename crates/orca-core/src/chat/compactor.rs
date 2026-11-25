//! The model summary (`IConversationCompactor` and the CLI's `ConversationCompactor`):
//! Pi-style compaction (Pi 0.87.1 core/compaction). Everything before a cut point that keeps
//! ~keepRecentTokens of the newest messages verbatim is summarized with Pi's system prompt and
//! checkpoint template (the UPDATE prompt when a checkpoint exists), and deterministic
//! `<read-files>`/`<modified-files>` lists are appended. The cut never separates a tool call
//! from its result; a cut inside a turn adds a turn-prefix summary. One implementation serves
//! auto-compaction, `/compact` and overflow recovery.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;

use super::compaction::{
    self, CompactionFileOps, CompactionPreparation, CompactionSettings, TOOL_RESULT_MAX_CHARS,
    TextToolCallParser, effective_keep_first_tokens,
};
use super::compaction_prompts::{
    self as prompts, NO_PRIOR_HISTORY, SPLIT_TURN_SEPARATOR, SUMMARIZATION_SYSTEM_PROMPT,
};
use super::conversation::Conversation;
use super::{cpt_or_default, len16};
use crate::client::{ChatMessage, ChatOptions, OpenAiClient};
use crate::config::{ContextConfig, OrcaConfig};

/// What a compaction told the user (the console lines of the C# compactor), in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionReport {
    pub notices: Vec<String>,
}

impl CompactionReport {
    fn notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.notices.push(text);
    }

    /// Whether any notice contains `needle`.
    pub fn mentions(&self, needle: &str) -> bool {
        self.notices.iter().any(|n| n.contains(needle))
    }
}

/// Compacts a conversation by summarizing older messages.
#[async_trait]
pub trait ConversationCompactor: Send + Sync {
    /// `context`: the context settings of the conversation's model; None = the app's.
    async fn compact_conversation(
        &self,
        conversation: &mut Conversation,
        extra_instructions: Option<&str>,
        context: Option<&ContextConfig>,
    ) -> CompactionReport;
}

/// The LLM summariser, over the session's [`OpenAiClient`].
pub struct LlmCompactor<'a> {
    client: &'a OpenAiClient,
    config: &'a OrcaConfig,
    actor: &'a str,
    text_tool_call_parser: Option<TextToolCallParser<'a>>,
}

/// Summaries smaller than this get lifted to it where the window has room.
const SMALL_WINDOW_FLOOR_TOKENS: i32 = 4096;

static THINK_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<think>.*?</think>").unwrap());
static THINK_UNCLOSED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<think>.*$").unwrap());

impl<'a> LlmCompactor<'a> {
    pub fn new(client: &'a OpenAiClient, config: &'a OrcaConfig, actor: &'a str) -> Self {
        Self {
            client,
            config,
            actor,
            text_tool_call_parser: None,
        }
    }

    /// Also read file operations from text-mode tool calls in assistant text.
    pub fn with_text_tool_call_parser(mut self, parser: TextToolCallParser<'a>) -> Self {
        self.text_tool_call_parser = Some(parser);
        self
    }

    /// Pi `compact`: one summary of the history (UPDATE prompt when a checkpoint exists), plus
    /// a turn-prefix summary merged underneath when the cut splits a turn.
    async fn generate_checkpoint(
        &self,
        prep: &CompactionPreparation,
        settings: &CompactionSettings,
        extra_instructions: Option<&str>,
    ) -> Result<String, String> {
        let summary_cap = (0.8 * f64::from(settings.reserve_tokens)) as i32;
        let history_prompt = || {
            prompts::build_summary_prompt(
                &compaction::serialize_conversation(&prep.messages_to_summarize),
                prep.previous_summary.as_deref(),
                extra_instructions,
            )
        };
        if prep.is_split_turn && !prep.turn_prefix_messages.is_empty() {
            let mut history = prep
                .previous_summary
                .clone()
                .unwrap_or_else(|| NO_PRIOR_HISTORY.to_string());
            if !prep.messages_to_summarize.is_empty() {
                history = self
                    .summarize(&history_prompt(), summary_cap, settings, "Summarization")
                    .await?;
            }
            let turn_prefix = self
                .summarize(
                    &prompts::build_turn_prefix_prompt(&compaction::serialize_conversation(
                        &prep.turn_prefix_messages,
                    )),
                    (0.5 * f64::from(settings.reserve_tokens)) as i32,
                    settings,
                    "Turn prefix summarization",
                )
                .await?;
            return Ok(format!("{history}{SPLIT_TURN_SEPARATOR}{turn_prefix}"));
        }
        self.summarize(&history_prompt(), summary_cap, settings, "Summarization")
            .await
    }

    async fn summarize(
        &self,
        prompt_text: &str,
        pi_max_tokens: i32,
        settings: &CompactionSettings,
        label: &str,
    ) -> Result<String, String> {
        let request = [
            ChatMessage::system(SUMMARIZATION_SYSTEM_PROMPT),
            ChatMessage::user(prompt_text),
        ];
        let options = ChatOptions {
            temperature: Some(0.3),
            max_output_tokens: Some(self.resolve_max_output_tokens(
                pi_max_tokens,
                prompt_text,
                settings,
            )),
            model: self.config.lm_studio.primary_model().map(str::to_string),
            ..ChatOptions::default()
        };
        let response = self
            .client
            .complete(self.actor, &request, &options)
            .await
            .map_err(|e| e.to_string())?;
        // Pi getSummarizationFailure: a length stop is a partial summary and must not become a
        // checkpoint.
        if response.finish_reason.as_deref() == Some("length") {
            return Err(format!(
                "{label} failed: generation hit the token cap and the summary is incomplete"
            ));
        }
        if !response.tool_calls.is_empty() {
            return Err(format!("{label} attempted to call a tool"));
        }
        // Thinking models may inline their reasoning; the checkpoint is the answer only.
        let text = THINK_BLOCK.replace_all(&response.text, "");
        let text = THINK_UNCLOSED.replace_all(&text, "");
        // The file lists are appended deterministically; drop any the model echoed.
        Ok(compaction::strip_file_lists(text.trim()))
    }

    /// Pi caps a summary at 0.8 x reserveTokens (0.5 x for a turn prefix). Small windows get a
    /// small reserve, so lift the cap to up to 4096 where the window has room after the prompt;
    /// never exceed the room left or maxTokens.
    fn resolve_max_output_tokens(
        &self,
        pi_max_tokens: i32,
        prompt_text: &str,
        settings: &CompactionSettings,
    ) -> i32 {
        let cpt = cpt_or_default(self.config.context.chars_per_token);
        let prompt_tokens =
            ((len16(SUMMARIZATION_SYSTEM_PROMPT) + len16(prompt_text)) as f32 / cpt) as i32;
        let room = settings.context_window - prompt_tokens - 256;
        let mut max = pi_max_tokens;
        if max < SMALL_WINDOW_FLOOR_TOKENS {
            max = max.max(room.min(SMALL_WINDOW_FLOOR_TOKENS));
        }
        if room >= 512 {
            max = max.min(room);
        }
        if let Some(cap) = self.config.lm_studio.max_tokens.filter(|c| *c > 0) {
            max = max.min(cap);
        }
        max.max(512)
    }

    /// Summarization failed: the window still has to shrink or the next request overflows.
    /// Stub old tool results deterministically, and squeeze the tail if that wasn't enough.
    fn fall_back_to_pruning(
        &self,
        conversation: &mut Conversation,
        prep: &CompactionPreparation,
        settings: &CompactionSettings,
        report: &mut CompactionReport,
    ) {
        let cpt = self.config.context.chars_per_token;
        if !settings.should_compact(conversation.estimate_context_tokens(cpt)) {
            return;
        }
        let reclaimed = conversation.prune_tool_results_before(prep.first_kept_index, 200);
        if reclaimed > 0 {
            tracing::warn!(
                "Compaction failed — pruned {reclaimed} chars of old tool results instead"
            );
        }
        self.squeeze_if_still_over(conversation, settings, 0, report);
    }

    /// True when tool results had to be truncated to fit.
    fn squeeze_if_still_over(
        &self,
        conversation: &mut Conversation,
        settings: &CompactionSettings,
        start_index: usize,
        report: &mut CompactionReport,
    ) -> bool {
        let cpt = self.config.context.chars_per_token;
        if !settings.should_compact(conversation.estimate_context_tokens(cpt)) {
            return false;
        }
        let reclaimed = conversation.truncate_tool_results_from(start_index, TOOL_RESULT_MAX_CHARS);
        if reclaimed > 0 {
            tracing::warn!(
                "Context still over budget after compaction — truncated {reclaimed} chars of tool output"
            );
            report.notice("Context still over budget — truncated large tool outputs.");
        }
        reclaimed > 0
    }
}

#[async_trait]
impl ConversationCompactor for LlmCompactor<'_> {
    async fn compact_conversation(
        &self,
        conversation: &mut Conversation,
        extra_instructions: Option<&str>,
        context: Option<&ContextConfig>,
    ) -> CompactionReport {
        let mut report = CompactionReport::default();
        let context = context.unwrap_or(&self.config.context);
        let settings = CompactionSettings::from_config(context);
        let cpt = self.config.context.chars_per_token;
        let prep = compaction::prepare(
            conversation.messages(),
            settings.keep_recent_tokens,
            cpt_or_default(cpt),
            self.text_tool_call_parser,
            effective_keep_first_tokens(context),
        );
        let tokens_before = conversation.estimate_context_tokens(cpt);
        let count_before = conversation.messages().len();

        let Some(prep) = prep else {
            // Everything left is the verbatim tail. If that alone overflows (huge tool output
            // in a small window), squeeze tool results once, as this compaction event.
            if !self.squeeze_if_still_over(conversation, &settings, 0, &mut report) {
                report.notice("Not enough messages to compact.");
            }
            return report;
        };

        match self
            .generate_checkpoint(&prep, &settings, extra_instructions)
            .await
        {
            Ok(summary) if summary.trim().is_empty() => {
                report.notice("Failed to generate summary — empty response.");
                self.fall_back_to_pruning(conversation, &prep, &settings, &mut report);
            }
            Ok(mut summary) => {
                let (read, modified) = prep.file_ops.compute_file_lists();
                summary.push_str(&CompactionFileOps::format(&read, &modified));
                conversation.apply_compaction(&summary, prep.first_kept_index, prep.head_end);
                self.squeeze_if_still_over(conversation, &settings, 1, &mut report);
                let tokens_after = conversation.estimate_context_tokens(cpt);
                tracing::info!(
                    "Compaction: {count_before} -> {} messages, ~{tokens_before} -> ~{tokens_after} tokens (split turn: {}, incremental: {})",
                    conversation.messages().len(),
                    prep.is_split_turn,
                    prep.previous_summary.is_some()
                );
                tracing::debug!("Compaction summary:\n{summary}");
                report.notice(format!(
                    "Compacted: {count_before} messages -> {} messages (~{tokens_before} -> ~{tokens_after} tokens)",
                    conversation.messages().len()
                ));
            }
            Err(e) => {
                tracing::error!("Failed to compact conversation: {e}");
                report.notice(format!("Failed to compact: {e}"));
                self.fall_back_to_pruning(conversation, &prep, &settings, &mut report);
            }
        }
        report
    }
}
