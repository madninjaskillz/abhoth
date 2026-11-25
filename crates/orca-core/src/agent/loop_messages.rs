//! Messages the agent loop injects (`PromptConstants` / `AgentLoopRunner` / `ToolCallDispatcher`
//! strings used by the turn loop). Multi-line raw-string constants carry platform line breaks
//! when sent (see `prompts::platform_lines`); the single-line ones are concatenations in C#.

/// `PromptConstants.TruncatedToolCallMessage`.
pub const TRUNCATED_TOOL_CALL_MESSAGE: &str = "Your previous response was cut off mid-tool-call (the <tool_call> tag was never closed).
Your output exceeded the token limit, so that tool call was NOT executed.

Recover like this:
1. To change an existing file, use edit_file with a targeted edit instead of rewriting the whole file.
2. If the cut-off call was writing a new file that is genuinely too long for one response,
   split it: write the first part with write_file, then add the remaining parts with edit_file.
   (This is a fallback for oversized output only \u{2014} normally write complete files in one call.)
3. Do not repeat reasoning you already did \u{2014} go straight to the tool call.

Resume your task now.";

/// `PromptConstants.NativeNudgeMessage`.
pub const NATIVE_NUDGE_MESSAGE: &str =
    "IMPORTANT: You showed code or described an action but did NOT call any tools.
Nothing was executed \u{2014} only real function calls run.

Please re-do your response using the function calling API:
- To create/write a file: call write_file with the full content.
- To edit a file: call edit_file.
- To run a command: call bash.
- To read a file: call read_file.

Call the appropriate tool now.";

/// `PromptConstants.NudgeMessage` (text tool calling).
pub const NUDGE_MESSAGE: &str = r#"IMPORTANT: You showed code or described an action but did NOT use <tool_call> tags.
Your response will NOT be executed unless you wrap tool calls in <tool_call> tags.

Please re-do your response. Instead of showing code in markdown blocks, use the appropriate tool:
- To create/write a file: <tool_call>{"name": "write_file", "arguments": {"path": "...", "content": "..."}}</tool_call>
- To edit a file: <tool_call>{"name": "edit_file", "arguments": {"path": "...", "old_string": "...", "new_string": "..."}}</tool_call>
- To run a command: <tool_call>{"name": "bash", "arguments": {"command": "..."}}</tool_call>
- To read a file: <tool_call>{"name": "read_file", "arguments": {"path": "..."}}</tool_call>

You MUST output <tool_call> tags to take action. Do it now."#;

/// `PromptConstants.GetNudgeMessage` (OpenOrca profile).
pub fn nudge_message(native_tool_calling: bool) -> &'static str {
    if native_tool_calling {
        NATIVE_NUDGE_MESSAGE
    } else {
        NUDGE_MESSAGE
    }
}

/// `PromptConstants.RetryLoopRedirectMessage`.
pub const RETRY_LOOP_REDIRECT_MESSAGE: &str = "SYSTEM NOTICE: A tool has now failed 3 times in a row with the exact same error. You MUST NOT call that tool again with the same arguments. Read the error message and either: (1) fix the underlying problem first (e.g. run 'git init' with the bash tool if this is not a git repository), or (2) skip that operation entirely and move on. Do something DIFFERENT on your next turn.";

/// `PromptConstants.DoomLoopSkippedResult`.
pub const DOOM_LOOP_SKIPPED_RESULT: &str =
    "Not run: this exact call (same tool, same arguments) was already made several times in a row.";

/// `PromptConstants.DanglingCallResult`.
pub const DANGLING_CALL_RESULT: &str =
    "Not run: this call was not executed. Decide your next step from the results you already have.";

/// `PromptConstants.GetDoomLoopRedirectMessage`.
pub fn doom_loop_redirect_message(tool_name: &str) -> String {
    format!(
        "STOP: You are calling '{tool_name}' repeatedly with the same arguments and getting the same result. This is a doom loop. Try a completely different approach, use different arguments, or explain what you're stuck on."
    )
}

/// `PromptConstants.GetIterationHandoffMessage`.
pub fn iteration_handoff_message(reason: &str, facts: &str) -> String {
    format!(
        "You have reached the iteration limit for this turn ({reason}). Do NOT call any more tools.\n\
Write a handoff so the next turn can continue immediately. Use exactly these headings:\n\
## Done\n(what is finished and verified \u{2014} name the passing test commands)\n\
## In progress\n(what you were in the middle of, and what you learned about it)\n\
## Failing tests\n(each failing test and its error, or \"none\")\n\
## Next step\n(ONE concrete action: the file and function to change and how, or the exact command to run)\n\
Be specific and brief. Facts recorded by the harness this turn:\n{facts}"
    )
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
