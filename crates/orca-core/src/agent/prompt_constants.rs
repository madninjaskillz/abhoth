//! Port of `PromptConstants.cs`: the fixed prompt texts the agent loop sends (nudges,
//! verification, summary, plan mode, Context7, next-prompt, stuck/doom-loop redirects).
//!
//! The C# multi-line constants are raw string literals, so they carry the line breaks of the
//! source checkout (CRLF in a Windows checkout). The constants here use `\n`; send them through
//! [`platform_lines`] wherever C# sends the constant unchanged. Constants C# builds by
//! concatenating `"\n"` (e.g. [`NEXT_PROMPT_INSTRUCTION`]) always use `\n`.

/// A multi-line prompt constant with platform line breaks (CRLF on Windows), as the C# raw
/// string literals carry them.
pub fn platform_lines(text: &str) -> String {
    text.replace('\n', orca_tools_common::NEWLINE)
}

/// `PromptConstants.PlanModeSystemPromptAddition`.
pub const PLAN_MODE_SYSTEM_PROMPT_ADDITION: &str = r#"*** PLAN MODE IS ACTIVE ***

You are currently in PLAN MODE. In this mode you MUST NOT make any changes.

ALLOWED actions:
- Use read-only tools (read_file, list_directory, glob, grep, think, web_fetch, web_search,
  get_process_output, task_list) to explore and understand the codebase
- Describe your plan step by step in your response text

BLOCKED actions:
- Writing, editing, or deleting files (write_file, edit_file, delete_file, move_file, copy_file)
- Running commands (bash, git_commit, git_push, git_pull, git_checkout, git_branch, git_stash)
- Any tool that modifies state

YOUR TASK in plan mode:
1. Explore the codebase using read-only tools to understand what exists
2. Produce a clear, numbered plan describing:
   - What files you would create, modify, or delete
   - What specific changes you would make (include code snippets where helpful)
   - What commands you would run
   - What verification steps you would take
3. Be specific: include file paths, function names, and line numbers
4. The user will review your plan and can approve, modify, or discard it"#;

/// `PromptConstants.PiPlanModeSystemPromptAddition`.
pub const PI_PLAN_MODE_SYSTEM_PROMPT_ADDITION: &str = r#"*** PLAN MODE IS ACTIVE ***

You are currently in PLAN MODE. In this mode you MUST NOT make any changes.

ALLOWED actions:
- Use read-only tools (read, and grep/find/ls when available) to explore and understand the codebase
- Describe your plan step by step in your response text

BLOCKED actions:
- Writing or editing files (write, edit)
- Running commands (bash)
- Any tool that modifies state

YOUR TASK in plan mode:
1. Explore the codebase using read-only tools to understand what exists
2. Produce a clear, numbered plan describing:
   - What files you would create, modify, or delete
   - What specific changes you would make (include code snippets where helpful)
   - What commands you would run
   - What verification steps you would take
3. Be specific: include file paths, function names, and line numbers
4. The user will review your plan and can approve, modify, or discard it"#;

/// `PromptConstants.TruncatedToolCallMessage`.
pub const TRUNCATED_TOOL_CALL_MESSAGE: &str = r#"Your previous response was cut off mid-tool-call (the <tool_call> tag was never closed).
Your output exceeded the token limit, so that tool call was NOT executed.

Recover like this:
1. To change an existing file, use edit_file with a targeted edit instead of rewriting the whole file.
2. If the cut-off call was writing a new file that is genuinely too long for one response,
   split it: write the first part with write_file, then add the remaining parts with edit_file.
   (This is a fallback for oversized output only — normally write complete files in one call.)
3. Do not repeat reasoning you already did — go straight to the tool call.

Resume your task now."#;

/// `PromptConstants.PiTruncatedToolCallMessage`.
pub const PI_TRUNCATED_TOOL_CALL_MESSAGE: &str = r#"Your previous response was cut off mid-tool-call.
Your output exceeded the token limit, so that tool call was NOT executed.

Recover like this:
1. To change an existing file, use edit with targeted edits instead of rewriting the whole file.
2. If the cut-off call was writing a new file that is genuinely too long for one response,
   split it: write the first part with write, then add the remaining parts with edit.
   (This is a fallback for oversized output only — normally write complete files in one call.)
3. Do not repeat reasoning you already did — go straight to the tool call.

Resume your task now."#;

/// `PromptConstants.NativeNudgeMessage`.
pub const NATIVE_NUDGE_MESSAGE: &str = r#"IMPORTANT: You showed code or described an action but did NOT call any tools.
Nothing was executed — only real function calls run.

Please re-do your response using the function calling API:
- To create/write a file: call write_file with the full content.
- To edit a file: call edit_file.
- To run a command: call bash.
- To read a file: call read_file.

Call the appropriate tool now."#;

/// `PromptConstants.PiNudgeMessage`.
pub const PI_NUDGE_MESSAGE: &str = r#"IMPORTANT: You showed code or described an action but did NOT call any tools.
Nothing was executed — only real function calls run.

Please re-do your response using the function calling API:
- To create/write a file: call write with the full content.
- To edit a file: call edit.
- To run a command: call bash.
- To read a file: call read.

Call the appropriate tool now."#;

/// `PromptConstants.NudgeMessage`.
pub const NUDGE_MESSAGE: &str = r#"IMPORTANT: You showed code or described an action but did NOT use <tool_call> tags.
Your response will NOT be executed unless you wrap tool calls in <tool_call> tags.

Please re-do your response. Instead of showing code in markdown blocks, use the appropriate tool:
- To create/write a file: <tool_call>{"name": "write_file", "arguments": {"path": "...", "content": "..."}}</tool_call>
- To edit a file: <tool_call>{"name": "edit_file", "arguments": {"path": "...", "old_string": "...", "new_string": "..."}}</tool_call>
- To run a command: <tool_call>{"name": "bash", "arguments": {"command": "..."}}</tool_call>
- To read a file: <tool_call>{"name": "read_file", "arguments": {"path": "..."}}</tool_call>

You MUST output <tool_call> tags to take action. Do it now."#;

/// `PromptConstants.VerificationMessage`.
pub const VERIFICATION_MESSAGE: &str = r#"VERIFICATION STEP — You MUST verify your implementation before finishing.

STEP 1: Use read_file to re-read every file you created or modified. Do NOT rely on memory.
   (Exception: a file you already re-read AFTER your last edit to it does not need reading again — read each file at most once in this step.)
STEP 2: For EACH requirement from the user's original request, trace through the code you just read:
   - Quote the specific line numbers that implement the requirement.
   - Mentally execute the code: what ACTUALLY happens at runtime? Follow variable assignments,
     function return values, and control flow step by step.
   - Common bugs to check for:
     * Score/counter incrementing every frame instead of only on specific events
     * Return values from functions not being used by the caller
     * Visual effects (fading, transparency) that don't actually change over time
     * Missing initial state (e.g., empty lists when something should start with an item)
     * Math that produces unexpected values (e.g., integer division by increasing index makes NEWER items darker)
STEP 3: If you find ANY issue, fix it NOW using edit_file. Then re-read the file to confirm the fix.
STEP 4: If you used build/test commands earlier (e.g., dotnet build, dotnet test, npm run build, pytest, cargo build),
   re-run them NOW to confirm the code still compiles and all tests pass. Edits made after the initial build
   may have introduced new errors (duplicate definitions, missing imports, syntax errors).
STEP 5: After all fixes, respond with "Verified." and nothing else.

IMPORTANT: Using the think tool alone is NOT sufficient. You MUST use read_file.
Do NOT run GUI/interactive programs with bash — they will time out.
Do NOT launch or relaunch GUI apps (games, windows) to verify them — they produce no
readable output. Verify with compile/syntax checks and code reading; ask the user to test visuals."#;

/// `PromptConstants.VerificationRetryMessage`.
pub const VERIFICATION_RETRY_MESSAGE: &str = r#"You said "Verified" but you did NOT actually use read_file to check the code.
Saying "Verified" without reading the files is NOT acceptable — you could be missing bugs.

You MUST call read_file NOW on every file you created or modified, then check each requirement.
If you find issues, fix them with edit_file. Do NOT skip this step."#;

/// `PromptConstants.PiVerificationRetryMessage`.
pub const PI_VERIFICATION_RETRY_MESSAGE: &str = r#"You said "Verified" but you did NOT actually use read to check the code.
Saying "Verified" without reading the files is NOT acceptable — you could be missing bugs.

You MUST call read NOW on every file you created or modified, then check each requirement.
If you find issues, fix them with edit. Do NOT skip this step."#;

/// `PromptConstants.PiVerificationMessage`.
pub const PI_VERIFICATION_MESSAGE: &str = r#"VERIFICATION STEP — You MUST verify your implementation before finishing.

STEP 1: Use read to re-read every file you created or modified. Do NOT rely on memory.
   (Exception: a file you already re-read AFTER your last edit to it does not need reading again — read each file at most once in this step.)
STEP 2: For EACH requirement from the user's original request, trace through the code you just read:
   - Quote the specific line numbers that implement the requirement.
   - Mentally execute the code: what ACTUALLY happens at runtime? Follow variable assignments,
     function return values, and control flow step by step.
   - Common bugs to check for:
     * Score/counter incrementing every frame instead of only on specific events
     * Return values from functions not being used by the caller
     * Visual effects (fading, transparency) that don't actually change over time
     * Missing initial state (e.g., empty lists when something should start with an item)
     * Math that produces unexpected values (e.g., integer division by increasing index makes NEWER items darker)
STEP 3: If you find ANY issue, fix it NOW using edit. Then re-read the file to confirm the fix.
STEP 4: If you used build/test commands earlier (e.g., dotnet build, dotnet test, npm run build, pytest, cargo build)
   AND you edited code after their last passing run, re-run them NOW with bash to confirm the code still compiles and
   all tests pass. If nothing changed since they last passed, do not run them again.
STEP 5: After all fixes, respond with "Verified." and nothing else.

IMPORTANT: Reasoning alone is NOT sufficient. You MUST use read.
Do NOT run GUI/interactive programs in the foreground with bash — they block until the timeout.
To see a GUI app (game, window), start it with bash background: true, then one job screenshot
(keys with shot steps, e.g. "shot Right shot Tab shot Esc shot"; size: "1280x720 1920x1080" and kill: true take both sizes and stop it) and look at the images."#;

/// `PromptConstants.PiDelegatedCheckMessage`.
pub const PI_DELEGATED_CHECK_MESSAGE: &str = r#"FINAL CHECK before you answer. The agents' reports are claims; check the result yourself:
1. ONE bash call that builds and runs the full test suite.
2. Run the program the way the user will, with the fixed checklist for its kind:
   - GUI app or game: start it with bash background: true, then ONE job screenshot that walks it and stops it:
     keys "shot <move focus> shot <each secondary view> shot <each filter or mode> shot <go back> shot",
     size: "1280x720 1920x1080", kill: true. That gives the start screen, a focus move, each secondary view, each
     filter or mode, the screen after going back, and a bigger window (it must still fill it). Never a key that
     launches, buys or deletes something. A size error from the screenshot is a bug in the program.
   - CLI tool: its main commands and one error case in one bash call, checking output and exit codes.
   - Server: start it in the background, hit the main endpoints and one bad request, stop it.
   - Plausibility: compare the counts and data you see against ground truth on this machine (list the files or
     records the program should have found) and treat an empty or zero result as a failure unless the ground
     truth is also empty. A parser whose tests use only invented fixtures is not verified: check it on real input.
3. If anything does not match the request, send the fix to the agent that wrote that code ({"to": "<its letter>", "task": "..."}) and re-check that step.
Run each check once: repeat it only after a fix changed the code.
Then answer in a few lines, saying what you checked (each step) and what you saw."#;

/// `PromptConstants.DelegatedCheckMessage`.
pub const DELEGATED_CHECK_MESSAGE: &str = r#"FINAL CHECK before you answer. The agents' reports are claims; check the result yourself:
1. One command that builds and runs the full test suite.
2. Run the program the way the user will (the spec's sample commands; a GUI app as a background process, checking its log/output) and compare with the request.
3. If anything is wrong, send the fix to the agent that wrote that code (spawn_agent with `to` set to its letter) and check again.
Then answer in a few lines, saying what you checked and what you saw."#;

/// `PromptConstants.SummaryRequestMessage`.
pub const SUMMARY_REQUEST_MESSAGE: &str = r#"All tool calls are complete. Provide a final summary of what you did.
Use markdown formatting (headers, bold, bullets, inline code) directly in your response — do NOT wrap it in a code fence.
Structure:
- **Summary**: 1–3 sentences describing what you did overall.
- **Changes made**: A bulleted list of files created, modified, or deleted, with brief descriptions.
- **Key findings**: Any notable discoveries, warnings, or issues encountered (omit if none).
- **Suggested next steps**: Actionable follow-ups the user might want to do (omit if none).
Keep it concise. Use `code` formatting for file paths and commands. Do NOT call any tools.

IMPORTANT: At the very end of your response, on its own line, add a <next-prompt> tag containing
a short follow-up prompt the user might type next (under 80 chars).
Example: <next-prompt>run the tests</next-prompt>
If there is no natural follow-up, omit the tag."#;

/// `PromptConstants.Context7SystemPromptAddition`.
pub const CONTEXT7_SYSTEM_PROMPT_ADDITION: &str = r#"LIBRARY DOCUMENTATION (Context7):
You have access to live, up-to-date library documentation via Context7. When the user asks about a specific
library, framework, or SDK (e.g., React, Express, Next.js, Django, etc.):
1. Call `mcp_resolve-library-id` with the library name to get its Context7 ID.
2. Call `mcp_get-library-docs` (or `mcp_query-docs`) with the resolved ID and the user's question as the query.
3. Use the returned documentation to answer — prefer it over your training data for API specifics, code examples, and migration guides.

Skip this for general programming concepts that don't change across versions (e.g., "what is a closure?")."#;

/// `PromptConstants.GetTruncatedToolCallMessage`.
pub fn get_truncated_tool_call_message(pi_profile: bool) -> &'static str {
    if pi_profile {
        PI_TRUNCATED_TOOL_CALL_MESSAGE
    } else {
        TRUNCATED_TOOL_CALL_MESSAGE
    }
}

/// `PromptConstants.GetNudgeMessage`: the nudge for the effective tool-calling mode and profile.
pub fn get_nudge_message(native_tool_calling: bool, pi_profile: bool) -> &'static str {
    if pi_profile {
        PI_NUDGE_MESSAGE
    } else if native_tool_calling {
        NATIVE_NUDGE_MESSAGE
    } else {
        NUDGE_MESSAGE
    }
}

/// `PromptConstants.GetVerificationRetryMessage`.
pub fn get_verification_retry_message(pi_profile: bool) -> &'static str {
    if pi_profile {
        PI_VERIFICATION_RETRY_MESSAGE
    } else {
        VERIFICATION_RETRY_MESSAGE
    }
}

/// `PromptConstants.GetDelegatedCheckMessage`.
pub fn get_delegated_check_message(pi_profile: bool) -> &'static str {
    if pi_profile {
        PI_DELEGATED_CHECK_MESSAGE
    } else {
        DELEGATED_CHECK_MESSAGE
    }
}

/// `PromptConstants.GetVerificationMessage`.
pub fn get_verification_message(pi_profile: bool) -> &'static str {
    if pi_profile {
        PI_VERIFICATION_MESSAGE
    } else {
        VERIFICATION_MESSAGE
    }
}

/// [`SUMMARY_REQUEST_MESSAGE`] with platform line breaks, as the C# build sends it.
pub fn summary_request_message() -> String {
    platform_lines(SUMMARY_REQUEST_MESSAGE)
}

/// `PromptConstants.NextPromptInstruction`: appended to every OpenOrca-profile system prompt.
/// Built by concatenation in C#, so its line breaks are always `\n`.
pub const NEXT_PROMPT_INSTRUCTION: &str = concat!(
    "SUGGESTED NEXT PROMPT:\n",
    "At the very end of every response, include a <next-prompt> tag with a short follow-up prompt ",
    "the user might type next (under 80 characters). Example: <next-prompt>run the tests</next-prompt>\n",
    "If there is no natural follow-up, omit the tag entirely. ",
    "The tag must appear AFTER all other content \u{2014} never inside <think> blocks or tool calls."
);

/// `PromptConstants.RetryLoopRedirectMessage`: a tool failed 3+ times identically.
pub const RETRY_LOOP_REDIRECT_MESSAGE: &str = concat!(
    "SYSTEM NOTICE: A tool has now failed 3 times in a row with the exact same error. ",
    "You MUST NOT call that tool again with the same arguments. ",
    "Read the error message and either: (1) fix the underlying problem first (e.g. run 'git init' with the bash tool if this is not a git repository), ",
    "or (2) skip that operation entirely and move on. ",
    "Do something DIFFERENT on your next turn."
);

/// `PromptConstants.DoomLoopSkippedResult`: recorded for each call the doom-loop guard skipped.
pub const DOOM_LOOP_SKIPPED_RESULT: &str =
    "Not run: this exact call (same tool, same arguments) was already made several times in a row.";

/// `PromptConstants.DanglingCallResult`: recorded for a call the harness never ran.
pub const DANGLING_CALL_RESULT: &str =
    "Not run: this call was not executed. Decide your next step from the results you already have.";

/// `PromptConstants.GetDoomLoopRedirectMessage`.
pub fn get_doom_loop_redirect_message(tool_name: &str) -> String {
    format!(
        "STOP: You are calling '{tool_name}' repeatedly with the same arguments and getting the same result. \
         This is a doom loop. Try a completely different approach, use different arguments, or explain what you're stuck on."
    )
}

/// `PromptConstants.GetStuckProbeMessage`: the model keeps probing (no file change) while the
/// latest test run keeps failing the same way. Strike 1 asks for a hypothesis and a decisive
/// change; strike 2 forbids further diagnostics.
pub fn get_stuck_probe_message<S: AsRef<str>>(
    strike: i32,
    calls: i32,
    test_command: &str,
    failure: &str,
    recent_calls: &[S],
    pi_profile: bool,
) -> String {
    let tried = recent_calls
        .iter()
        .map(|c| format!("  - {}", c.as_ref()))
        .collect::<Vec<_>>()
        .join("\n");
    if strike <= 1 {
        let change = if pi_profile {
            format!(
                "make ONE decisive change with edit or write \u{2014} if the fault is inside one function, rewrite that whole function from the spec/requirements instead of patching it \u{2014} then re-run `{test_command}` with bash."
            )
        } else {
            format!(
                "make ONE decisive code change \u{2014} if the fault is inside one function, rewrite that whole function from the spec/requirements instead of patching it \u{2014} then re-run `{test_command}`."
            )
        };
        return format!(
            "[Stuck] {calls} tool calls since your last file change, and `{test_command}` still fails the same way: {failure}.\n\
             Most recent calls:\n{tried}\n\
             More probing will not fix this. Stop investigating. In one sentence, state your single best hypothesis for the failure, then {change}"
        );
    }
    let next = if pi_profile {
        format!(
            "Your NEXT tool call must be write (or edit) replacing the failing function or section wholesale with a fresh implementation written from the requirements; then run `{test_command}` with bash."
        )
    } else {
        format!(
            "Your NEXT action must change the code: replace the failing function or section wholesale with a fresh implementation written from the requirements; then run `{test_command}`."
        )
    };
    format!(
        "[Stuck \u{2014} second warning] {calls} tool calls and still no file change; `{test_command}` still fails: {failure}.\n\
         Do NOT run another diagnostic, print, or inspection command. {next}"
    )
}

/// `PromptConstants.GetIterationHandoffMessage`: tools-off request when a turn ends on its
/// iteration budget; `facts` are harness-recorded (files changed, latest test results).
pub fn get_iteration_handoff_message(reason: &str, facts: &str) -> String {
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

/// `ToolCallDispatcher`: asked when a summary was cut off at the output limit.
pub const SUMMARY_CONTINUE_MESSAGE: &str = "Your response was cut off. Continue exactly where you stopped \u{2014} do NOT repeat anything you already wrote.";

/// `CliConstants.MaxSummaryContinuations`.
pub const MAX_SUMMARY_CONTINUATIONS: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_follows_mode_and_profile() {
        assert_eq!(get_nudge_message(true, false), NATIVE_NUDGE_MESSAGE);
        assert_eq!(get_nudge_message(false, false), NUDGE_MESSAGE);
        assert_eq!(get_nudge_message(false, true), PI_NUDGE_MESSAGE);
        assert!(!NATIVE_NUDGE_MESSAGE.contains("<tool_call>"));
    }

    #[test]
    fn stuck_probe_lists_recent_calls() {
        let m = get_stuck_probe_message(
            1,
            9,
            "pytest",
            "1 failed",
            &["bash ls", "read_file a"],
            false,
        );
        assert!(m.starts_with("[Stuck] 9 tool calls since your last file change, and `pytest`"));
        assert!(m.contains("Most recent calls:\n  - bash ls\n  - read_file a\n"));
        let m2 = get_stuck_probe_message::<&str>(2, 12, "pytest", "x", &[], true);
        assert!(m2.starts_with("[Stuck \u{2014} second warning] 12 tool calls"));
        assert!(m2.ends_with("then run `pytest` with bash."));
    }

    #[test]
    fn platform_lines_uses_os_newline() {
        let t = platform_lines("a\nb");
        assert_eq!(t, format!("a{}b", orca_tools_common::NEWLINE));
    }

    // ── LongTaskMonitorTests: StuckProbeMessage_* ──

    const STAGE1: &str = "python -m unittest tests.test_stage1";

    #[test]
    fn stuck_probe_message_pi_profile_names_only_pi_tools() {
        for strike in [1, 2] {
            let msg = get_stuck_probe_message(
                strike,
                9,
                STAGE1,
                "4 failing (test_strings)",
                &["bash: python - <<'EOF' \u{2026}"],
                true,
            );
            assert!(msg.contains("write"));
            assert!(msg.contains("bash"));
            for foreign in [
                "read_file",
                "write_file",
                "edit_file",
                "run_command",
                "think",
                "start_background_process",
            ] {
                assert!(!msg.contains(foreign), "{foreign}");
            }
            assert!(msg.contains(STAGE1));
        }
    }

    #[test]
    fn stuck_probe_message_neutral_names_no_tools() {
        let re = regex::Regex::new(r"\b(bash|edit|write|read)\b|_file\b").unwrap();
        for strike in [1, 2] {
            let msg = get_stuck_probe_message::<&str>(strike, 9, STAGE1, "4 failing", &[], false);
            assert!(!re.is_match(&msg), "{msg}");
        }
    }

    #[test]
    fn stuck_probe_message_second_strike_forbids_diagnostics() {
        let first = get_stuck_probe_message::<&str>(1, 8, STAGE1, "f", &[], true);
        let second = get_stuck_probe_message::<&str>(2, 16, STAGE1, "f", &[], true);
        assert!(first.contains("hypothesis"));
        assert!(second.contains("Do NOT run another diagnostic"));
    }
}
