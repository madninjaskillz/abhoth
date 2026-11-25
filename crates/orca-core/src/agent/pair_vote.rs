//! Pair model programming (`PairModelVoter`, `PairVoteResult` and the loop's
//! `HandlePairVotingAsync`): with `lmStudio.pairModelProgramming` and two or more
//! `lmStudio.models`, every main-loop request is also sent (non-streaming) to each alternate
//! model; once the primary's stream ends, every model votes on the non-empty answers and the
//! winner's text and tool calls replace the primary's. With 2 models both must agree (a tie
//! keeps the primary); with 3+ the plurality wins, ties go to the primary when it is among them,
//! else to the lowest index.

use std::sync::Arc;

use serde_json::Value;
use tokio::task::JoinHandle;

use super::llm_tags;
use super::session::{AgentObserver, AgentSession};
use crate::chat::{head16, len16};
use crate::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, Role, ServedModels, ToolCall,
    client_for_model, request_parameters,
};

/// `PairOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    PrimaryConfirmed,
    AlternateSelected,
    DisagreementUsePrimary,
    VotingFailed,
}

/// One vote cast by a model (`ModelVote`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelVote {
    pub voter_index: usize,
    pub voter_model: String,
    pub choice_index: usize,
    pub choice_model: String,
}

/// `PairVoteResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct PairVoteResult {
    pub outcome: PairOutcome,
    pub winner_index: usize,
    pub winner_text: Option<String>,
    pub majority_agreed: bool,
    pub votes: Vec<ModelVote>,
    /// The winner's tool calls (its `WinnerContents`' function calls).
    pub winner_tool_calls: Option<Vec<ToolCall>>,
}

impl PairVoteResult {
    fn plain(outcome: PairOutcome) -> Self {
        Self {
            outcome,
            winner_index: 0,
            winner_text: None,
            majority_agreed: false,
            votes: Vec::new(),
            winner_tool_calls: None,
        }
    }
}

/// Vote tallies (`AgentLoopState.Pair*`, summed into `ReplState.Pair*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PairStats {
    pub primary_wins: i32,
    pub backup_wins: i32,
    pub disagreements: i32,
    pub backup_failures: i32,
}

impl PairStats {
    fn add(&mut self, other: &PairStats) {
        self.primary_wins += other.primary_wins;
        self.backup_wins += other.backup_wins;
        self.disagreements += other.disagreements;
        self.backup_failures += other.backup_failures;
    }
}

/// The alternates' requests, in model order (`List<Task<ChatResponse>>`).
pub(super) type PairTasks = Vec<JoinHandle<Result<ChatResponse, ChatError>>>;

impl AgentSession {
    /// `FireParallelRequests`: one request per non-primary model, started now. None when pair
    /// mode is off, fewer than two models are configured, or failover already moved on.
    pub(super) fn fire_parallel_requests(&self, options: &ChatOptions) -> Option<PairTasks> {
        let lm = &self.config.lm_studio;
        if !lm.pair_model_programming || lm.models.len() < 2 || self.model_index > 0 {
            return None;
        }
        let mut tasks = Vec::new();
        for (i, model) in lm.models.iter().enumerate().skip(1) {
            // Alternates get the same tools as the primary: a winning alternate that produced
            // tool calls must be able to keep the agent loop going.
            let mut pair_options = ChatOptions {
                temperature: options.temperature,
                max_output_tokens: options.max_output_tokens,
                model: Some(model.clone()),
                tools: options.tools.clone(),
                ..ChatOptions::default()
            };
            request_parameters::apply(&mut pair_options, &self.config, Some(model), true);
            tracing::info!("Pair mode: firing request for model #{}: {model}", i + 1);
            let client = self.pair_client(model);
            let messages = self.messages.clone();
            let actor = self.actor.clone();
            tasks.push(tokio::spawn(async move {
                client.complete(&actor, &messages, &pair_options).await
            }));
        }
        Some(tasks)
    }

    /// A client for one model (`LmStudioClientFactory.Create(model)`). Its served-model
    /// tracking is its own, so the alternates can't overwrite what served the Lead.
    fn pair_client(&self, model: &str) -> crate::client::OpenAiClient {
        client_for_model(&self.config, Arc::new(ServedModels::new()), model)
    }

    /// `CollectAndVoteAsync`: collect every alternate's answer and run the vote.
    pub(super) async fn collect_and_vote(
        &self,
        tasks: PairTasks,
        primary_text: &str,
    ) -> PairVoteResult {
        if tasks.is_empty() {
            return PairVoteResult::plain(PairOutcome::PrimaryConfirmed);
        }
        let count = tasks.len();
        // Index 0 = primary (text already in hand), 1..N = the alternates.
        let mut texts = vec![primary_text.to_string()];
        let mut calls: Vec<Option<Vec<ToolCall>>> = vec![None];
        let mut failed = 0;
        for (i, task) in tasks.into_iter().enumerate() {
            match task.await {
                Ok(Ok(r)) => {
                    texts.push(r.text);
                    calls.push(Some(r.tool_calls));
                }
                Ok(Err(e)) => {
                    tracing::warn!("Pair mode: model #{} request failed: {e}", i + 2);
                    texts.push(String::new());
                    calls.push(None);
                    failed += 1;
                }
                Err(e) => {
                    tracing::warn!("Pair mode: model #{} request failed: {e}", i + 2);
                    texts.push(String::new());
                    calls.push(None);
                    failed += 1;
                }
            }
        }
        // All alternates failed: the primary wins by default.
        if failed == count {
            return PairVoteResult::plain(PairOutcome::VotingFailed);
        }
        // Empty answers don't take part (indices stay stable).
        let valid: Vec<(String, usize)> = texts
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(i, t)| (t.clone(), i))
            .collect();
        if valid.len() < 2 {
            return PairVoteResult::plain(PairOutcome::PrimaryConfirmed);
        }
        let (winner, majority, votes) = self.run_multi_model_vote(&valid).await;
        tracing::info!(
            "Pair vote result: winner=model#{} majority={majority}",
            winner + 1
        );
        if winner == 0 {
            return PairVoteResult {
                majority_agreed: majority,
                votes,
                ..PairVoteResult::plain(PairOutcome::PrimaryConfirmed)
            };
        }
        // An alternate won, by plurality or by tie-break.
        PairVoteResult {
            outcome: if majority {
                PairOutcome::AlternateSelected
            } else {
                PairOutcome::DisagreementUsePrimary
            },
            winner_index: winner,
            winner_text: Some(texts[winner].clone()),
            majority_agreed: majority,
            votes,
            winner_tool_calls: calls[winner].clone(),
        }
    }

    /// `RunMultiModelVoteAsync`: every model votes on the answers (labelled A, B, ...).
    /// Returns the winning response index, whether a plurality chose it, and the votes.
    async fn run_multi_model_vote(
        &self,
        valid: &[(String, usize)],
    ) -> (usize, bool, Vec<ModelVote>) {
        let models = &self.config.lm_studio.models;
        let mut model_votes = Vec::new();
        let last_user = self.messages.iter().rev().find(|m| m.role == Role::User);
        let mut user_prompt = last_user
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        if len16(&user_prompt) > 500 {
            user_prompt = format!("{}...", head16(&user_prompt, 500));
        }
        let vote_prompt = build_vote_prompt(&user_prompt, valid);
        let vote_messages = vec![ChatMessage::user(vote_prompt)];
        // No model on the options: each voter's client names its own model.
        let vote_options = ChatOptions {
            temperature: Some(0.0),
            max_output_tokens: Some(1024),
            model_from_client: true,
            ..ChatOptions::default()
        };
        // All models vote, the primary first (by its explicit name, not the session client).
        let tasks: Vec<_> = models
            .iter()
            .map(|model| {
                let client = self.pair_client(model);
                let messages = vote_messages.clone();
                let options = vote_options.clone();
                let actor = self.actor.clone();
                tokio::spawn(async move { client.complete(&actor, &messages, &options).await })
            })
            .collect();

        // Each vote on its own: a failed voter must not discard the others.
        let mut votes: Vec<(usize, usize)> = Vec::new();
        for (i, task) in tasks.into_iter().enumerate() {
            let response = match task.await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!("Model #{} vote failed: {e}", i + 1);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Model #{} vote failed: {e}", i + 1);
                    continue;
                }
            };
            match parse_vote_choice(&response, valid.len()) {
                Some(choice) => {
                    // The letter maps back to the original response index.
                    let original = valid[choice].1;
                    votes.push((i, original));
                    model_votes.push(ModelVote {
                        voter_index: i,
                        voter_model: model_label(models, i),
                        choice_index: original,
                        choice_model: model_label(models, original),
                    });
                    tracing::info!(
                        "Model #{} voted for response {} (model #{})",
                        i + 1,
                        letter(choice),
                        original + 1
                    );
                }
                None => tracing::warn!("Model #{} returned unparseable vote", i + 1),
            }
        }
        if votes.is_empty() {
            return (0, false, model_votes);
        }
        let (winner, majority) = tally(&votes);
        (winner, majority, model_votes)
    }

    /// `HandlePairVotingAsync` + `ApplyPairBackupResult`: run the vote, report it, count the
    /// outcome, and put the winner's text and tool calls in `response` when an alternate won.
    pub(super) async fn handle_pair_voting(
        &mut self,
        tasks: PairTasks,
        response: &mut ChatResponse,
        observer: &mut dyn AgentObserver,
    ) {
        let result = self.collect_and_vote(tasks, &response.text).await;
        if !result.votes.is_empty() {
            observer.on_notice("Votes:");
            for v in &result.votes {
                observer.on_notice(&format!(
                    "  {} \u{2192} {}",
                    truncate_model_name(&v.voter_model),
                    truncate_model_name(&v.choice_model)
                ));
            }
        }
        let models = self.config.lm_studio.models.clone();
        let primary = truncate_model_name(models.first().map(String::as_str).unwrap_or_default());
        match result.outcome {
            PairOutcome::AlternateSelected => {
                self.turn_pair_stats.backup_wins += 1;
                let label = model_label(&models, result.winner_index);
                observer.on_notice(&format!(
                    "Winner: {} (majority)",
                    truncate_model_name(&label)
                ));
                self.apply_pair_backup_result(response, &result);
            }
            PairOutcome::PrimaryConfirmed => {
                if result.majority_agreed {
                    self.turn_pair_stats.primary_wins += 1;
                }
                observer.on_notice(&format!("Winner: {primary} (primary confirmed)"));
            }
            PairOutcome::DisagreementUsePrimary => {
                self.turn_pair_stats.disagreements += 1;
                if result.winner_text.is_some() && result.winner_index != 0 {
                    // The tie-break went to an alternate model.
                    let label = model_label(&models, result.winner_index);
                    observer.on_notice(&format!(
                        "Winner: {} (tie-break)",
                        truncate_model_name(&label)
                    ));
                    self.apply_pair_backup_result(response, &result);
                } else {
                    observer.on_notice(&format!(
                        "Winner: {primary} (tie-break \u{2014} primary wins)"
                    ));
                }
            }
            PairOutcome::VotingFailed => self.turn_pair_stats.backup_failures += 1,
        }
    }

    /// The winner's contents replace the primary's wholesale: its tool calls (if any) keep the
    /// loop going, its text becomes the reply. The primary's reasoning doesn't belong to it.
    fn apply_pair_backup_result(&mut self, response: &mut ChatResponse, result: &PairVoteResult) {
        let text = result.winner_text.clone().unwrap_or_default();
        self.last_assistant_response = Some(text.clone());
        response.text = text;
        response.reasoning.clear();
        // C# builds the message as `new ChatMessage(Assistant, "")` plus the winner's text.
        response.leading_empty_part = false;
        response.empty_first_part = true;
        response.tool_calls = result.winner_tool_calls.clone().unwrap_or_default();
    }

    /// `RenderFinalSummary`'s pair part: the turn's tallies, then add them to the session's.
    pub(super) fn finish_pair_stats(&mut self, observer: &mut dyn AgentObserver) {
        let t = std::mem::take(&mut self.turn_pair_stats);
        let total = t.primary_wins + t.backup_wins + t.disagreements;
        if self.config.lm_studio.pair_model_programming && total + t.backup_failures > 0 {
            let mut parts = Vec::new();
            if t.primary_wins > 0 {
                parts.push(format!("primary \u{2713} {}", t.primary_wins));
            }
            if t.backup_wins > 0 {
                parts.push(format!("backup \u{2713} {}", t.backup_wins));
            }
            if t.disagreements > 0 {
                parts.push(format!("disagreed {}", t.disagreements));
            }
            if t.backup_failures > 0 {
                parts.push(format!("failed {}", t.backup_failures));
            }
            observer.on_notice(&format!(
                "Pair mode: {total} vote{} \u{2014} {}",
                if total == 1 { "" } else { "s" },
                parts.join(" \u{b7} ")
            ));
        }
        self.pair_stats.add(&t);
    }
}

/// The vote request's prompt: the user's request (cut at 500 UTF-16 units) and the answers,
/// each cut at max(500, 2000 / count) units.
pub fn build_vote_prompt(user_prompt: &str, valid: &[(String, usize)]) -> String {
    let max_len = 500.max(2000 / valid.len().max(1));
    let block: Vec<String> = valid
        .iter()
        .enumerate()
        .map(|(i, (text, _))| {
            let text = if len16(text) > max_len {
                format!("{}...", head16(text, max_len))
            } else {
                text.clone()
            };
            format!("Response {}:\n{text}", letter(i))
        })
        .collect();
    let choices: Vec<String> = (0..valid.len())
        .map(|i| format!("{{\"choice\":\"{}\"}}", letter(i)))
        .collect();
    format!(
        "The user said: {user_prompt}\n\n{}\n\nWhich response better addresses the user's request? Reply with ONLY valid JSON and nothing else: {}",
        block.join("\n\n"),
        choices.join(", ")
    )
}

/// The tally: the most votes wins (`true` = by plurality); a tie at the top goes to the
/// primary when it is among them, else to the lowest index (`false`).
fn tally(votes: &[(usize, usize)]) -> (usize, bool) {
    let mut counts: Vec<(usize, usize)> = Vec::new();
    for &(_, choice) in votes {
        match counts.iter_mut().find(|(idx, _)| *idx == choice) {
            Some(c) => c.1 += 1,
            None => counts.push((choice, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top = counts[0].1;
    let tied: Vec<usize> = counts.iter().filter(|c| c.1 == top).map(|c| c.0).collect();
    if tied.len() > 1 {
        if tied.contains(&0) {
            return (0, false);
        }
        return (*tied.iter().min().expect("non-empty"), false);
    }
    (counts[0].0, true)
}

fn letter(i: usize) -> char {
    char::from(b'A' + i as u8)
}

fn model_label(models: &[String], i: usize) -> String {
    models
        .get(i)
        .cloned()
        .unwrap_or_else(|| format!("model #{}", i + 1))
}

/// `TruncateModelName`: "meta-llama/llama-4" -> "llama-4".
pub fn truncate_model_name(model: &str) -> &str {
    match model.rfind('/') {
        Some(i) => &model[i + 1..],
        None => model,
    }
}

/// `ParseVoteChoice`: the chosen answer's index (A = 0) from a vote reply: the `choice` of the
/// JSON object between the first `{` and the last `}`, else the first quoted letter.
pub fn parse_vote_choice(response: &ChatResponse, num_choices: usize) -> Option<usize> {
    let text = response.text.trim();
    let text = llm_tags::THINK_BLOCK.replace_all(text, "");
    let text = text.trim();

    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && end > start
        && let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&text[start..=end])
        && let Some(Value::String(choice)) = doc.get("choice")
    {
        let mut chars = choice.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let mut upper = c.to_uppercase();
            if let (Some(u), None) = (upper.next(), upper.next())
                && u >= 'A'
                && (u as u32) < 'A' as u32 + num_choices as u32
            {
                return Some((u as u32 - 'A' as u32) as usize);
            }
        }
    }

    // Fallback: a quoted letter.
    (0..num_choices).find(|&i| text.contains(&format!("\"{}\"", letter(i))))
}

/// The two-choice form (`ParseVoteChoice(response)`): "A", "B" or none.
pub fn parse_vote_choice_ab(response: &ChatResponse) -> Option<&'static str> {
    match parse_vote_choice(response, 2) {
        Some(0) => Some("A"),
        Some(1) => Some("B"),
        _ => None,
    }
}
