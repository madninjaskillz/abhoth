//! Messages the turn loop injects: the `AgentLoopRunner` / `ToolCallExecutor` strings, plus
//! re-exports of the `PromptConstants` ones it uses. Multi-line raw-string constants carry platform line breaks
//! when sent (see `prompts::platform_lines`); the single-line ones are concatenations in C#.

pub use super::prompt_constants::{
    DANGLING_CALL_RESULT, DOOM_LOOP_SKIPPED_RESULT, NATIVE_NUDGE_MESSAGE, NUDGE_MESSAGE,
    RETRY_LOOP_REDIRECT_MESSAGE, TRUNCATED_TOOL_CALL_MESSAGE,
    get_doom_loop_redirect_message as doom_loop_redirect_message,
    get_iteration_handoff_message as iteration_handoff_message,
};

/// `PromptConstants.GetNudgeMessage` (OpenOrca profile).
pub fn nudge_message(native_tool_calling: bool) -> &'static str {
    super::prompt_constants::get_nudge_message(native_tool_calling, false)
}

/// The failing-test gate (`AgentLoopRunner`): refuse to wind down over a red run.
pub fn test_failure_message(failure_tail: &str) -> String {
    format!(
        "STOP \u{2014} the task is NOT complete: the most recent test/build run FAILED. Fix the code and re-run the tests until they pass.\nFailure output:\n{failure_tail}"
    )
}

/// Sent after a thinking runaway was cancelled.
pub const RUNAWAY_MESSAGE: &str = "Your last response was cut off because it was entirely internal reasoning with no action. Do NOT deliberate further. Immediately output the tool call (or the final answer) for your next step.";

/// `ToolCallExecutor.CommitTextResults`: tool results of a text-mode round, as one user message.
pub fn text_tool_results_message(parts: &[(String, String)]) -> String {
    let combined: Vec<String> = parts
        .iter()
        .map(|(name, result)| format!("[Tool result for {name}]\n{result}"))
        .collect();
    format!(
        "Here are the results of the tool calls you made:\n\n{}\n\nContinue with the task based on these results. If you need to use more tools, use <tool_call> tags.",
        combined.join("\n\n")
    )
}

/// `AgentLoopRunner` task reminder.
pub fn task_reminder(brief: &str, files: &str, iteration: i32, budget: i32) -> String {
    format!(
        "[Task reminder] Original request: \"{brief}\"\nFiles modified so far: {files}\nIteration {iteration}/{budget}. Stay focused on the original request; verify before summarizing."
    )
}

/// `AgentLoopRunner` budget advisory.
pub fn budget_advisory(remaining: i32, extendable: bool) -> String {
    format!(
        "[Budget] Only {remaining} iterations remain. Prioritize finishing the task and verifying your work over further exploration.{}",
        if extendable {
            " (The budget is extended only while your changes measurably move the tests forward.)"
        } else {
            ""
        }
    )
}

/// `ToolCallExecutor.ApplyRetryDetection` second identical failure.
pub const RETRY_WARNING_SUFFIX: &str = "\n\nWARNING: This tool call has failed 2 times with the same error. Do NOT retry with the same arguments. Try a different approach or tool.";

/// `ToolCallExecutor.ApplyRetryDetection` third identical failure.
pub fn retry_stop_message(count: u32, error: &str) -> String {
    format!(
        "STOP: This tool call has failed {count} times with the same error. You MUST use a different approach. Original error: {error}"
    )
}
