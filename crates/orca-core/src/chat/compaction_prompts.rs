//! Pi coding agent's compaction strings, verbatim from Pi 0.87.1 (core/compaction/compaction.js,
//! core/compaction/utils.js, core/messages.js) via `CompactionPrompts.cs`. The Pi profile's
//! model is fine-tuned on Pi transcripts, so its checkpoints must look exactly like the ones it
//! was trained on. Do not reword. (These use `\n` on every platform, as the C# source does.)

/// utils.js SUMMARIZATION_SYSTEM_PROMPT.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\
\n\
Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// compaction.js SUMMARIZATION_PROMPT.
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\
\n\
Use this EXACT format:\n\
\n\
## Goal\n\
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\
\n\
## Constraints & Preferences\n\
- [Any constraints, preferences, or requirements mentioned by user]\n\
- [Or \"(none)\" if none were mentioned]\n\
\n\
## Progress\n\
### Done\n\
- [x] [Completed tasks/changes]\n\
\n\
### In Progress\n\
- [ ] [Current work]\n\
\n\
### Blocked\n\
- [Issues preventing progress, if any]\n\
\n\
## Key Decisions\n\
- **[Decision]**: [Brief rationale]\n\
\n\
## Next Steps\n\
1. [Ordered list of what should happen next]\n\
\n\
## Critical Context\n\
- [Any data, examples, or references needed to continue]\n\
- [Or \"(none)\" if not applicable]\n\
\n\
Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// compaction.js UPDATE_SUMMARIZATION_INSTRUCTIONS.
pub const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = "Update the existing structured summary with new information. RULES:\n\
- PRESERVE all existing information from the previous summary\n\
- ADD new progress, decisions, and context from the new messages\n\
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n\
- UPDATE \"Next Steps\" based on what was accomplished\n\
- PRESERVE exact file paths, function names, and error messages\n\
- If something is no longer relevant, you may remove it\n\
\n\
Use this EXACT format:\n\
\n\
## Goal\n\
[Preserve existing goals, add new ones if the task expanded]\n\
\n\
## Constraints & Preferences\n\
- [Preserve existing, add new ones discovered]\n\
\n\
## Progress\n\
### Done\n\
- [x] [Include previously done items AND newly completed items]\n\
\n\
### In Progress\n\
- [ ] [Current work - update based on progress]\n\
\n\
### Blocked\n\
- [Current blockers - remove if resolved]\n\
\n\
## Key Decisions\n\
- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\
\n\
## Next Steps\n\
1. [Update based on current state]\n\
\n\
## Critical Context\n\
- [Preserve important context, add new if needed]\n\
\n\
Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// compaction.js UPDATE_SUMMARIZATION_PROMPT (the intro, then
/// [`UPDATE_SUMMARIZATION_INSTRUCTIONS`]).
pub const UPDATE_SUMMARIZATION_PROMPT_INTRO: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\
\n";

/// compaction.js UPDATE_SUMMARIZATION_PROMPT.
pub fn update_summarization_prompt() -> String {
    format!("{UPDATE_SUMMARIZATION_PROMPT_INTRO}{UPDATE_SUMMARIZATION_INSTRUCTIONS}")
}

/// compaction.js TURN_PREFIX_SUMMARIZATION_PROMPT.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\
\n\
Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\
\n\
## Original Request\n\
[What did the user ask for?]\n\
\n\
## Progress So Far\n\
- [Key decisions and work completed in these messages]\n\
\n\
## Context Needed to Continue\n\
- [Information from these messages needed to understand the later work]\n\
\n\
Only summarize information explicitly present above. Do not infer or recreate later messages.";

/// messages.js COMPACTION_SUMMARY_PREFIX: how the checkpoint is shown to the model.
pub const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// messages.js COMPACTION_SUMMARY_SUFFIX.
pub const SUMMARY_SUFFIX: &str = "\n</summary>";

/// compaction.js: how a split turn's prefix summary is merged under the history summary.
pub const SPLIT_TURN_SEPARATOR: &str = "\n\n---\n\n**Turn Context (split turn):**\n\n";

/// compaction.js: history text when a split turn has nothing before it.
pub const NO_PRIOR_HISTORY: &str = "No prior history.";

/// Main summary prompt (initial or incremental update), with Pi's custom-focus suffix.
pub fn build_summary_prompt(
    conversation_text: &str,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let mut base = match previous_summary {
        Some(_) => update_summarization_prompt(),
        None => SUMMARIZATION_PROMPT.to_string(),
    };
    if let Some(ci) = custom_instructions.filter(|c| !c.trim().is_empty()) {
        base = format!("{base}\n\nAdditional focus: {ci}");
    }
    let mut prompt = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(prev) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        ));
    }
    prompt + &base
}

/// Turn-prefix prompt (compaction.js generateTurnPrefixSummary).
pub fn build_turn_prefix_prompt(conversation_text: &str) -> String {
    format!(
        "# Conversation\n{conversation_text}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    )
}
