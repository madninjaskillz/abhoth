//! The C# log categories of the agent loop's lines (D141).
//!
//! The file log names a line by the last `::` segment of its `tracing` target, so these
//! targets make the loop's lines read as the C# ones do (`AgentLoopRunner: Starting streaming
//! request to LLM...`). The bench reporter (`bench/run.py report`) and the parity analysis
//! match on category and wording, so a line ported here keeps both.

/// `AgentLoopRunner`: the Lead's turn loop.
pub const LOOP: &str = "orca_core::agent::AgentLoopRunner";
/// `StreamingResponseProcessor`'s lines (C# writes them under the loop's logger).
pub const STREAM: &str = LOOP;
/// `ToolCallDispatcher`'s lines (C# writes them under the loop's logger too).
pub const DISPATCHER: &str = LOOP;
/// `ToolCallExecutor`: one round of tool calls.
pub const EXECUTOR: &str = "orca_core::agent::ToolCallExecutor";
/// `ToolExecutionPipeline`: C# creates its logger from `Program`'s, so its lines read
/// `Program: ...`.
pub const PIPELINE: &str = "orca_core::agent::Program";

/// `ErrorContentLogMaxChars` / `LogPreviewMaxChars`.
pub const PREVIEW_MAX_CHARS: usize = 500;
/// `LogTailMaxChars`.
pub const TAIL_MAX_CHARS: usize = 300;
/// The argument previews of "Preparing tool" and "ExecuteToolAsync called".
pub const ARGS_MAX_CHARS: usize = 200;

/// The first `max` characters, with "..." when cut (`s[..max] + "..."`).
pub fn head_ellipsis(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// The first `max` characters (`s[..max]`), no marker.
pub fn head(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The last `max` characters (`s[^max..]`).
pub fn tail(s: &str, max: usize) -> &str {
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    match s.char_indices().nth(n - max) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

/// .NET's `bool.ToString()`.
pub fn dotnet_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_by_characters() {
        assert_eq!(head_ellipsis("abcdef", 3), "abc...");
        assert_eq!(head_ellipsis("abc", 3), "abc");
        assert_eq!(head("é—xyz", 2), "é—");
        assert_eq!(tail("abcdef", 2), "ef");
        assert_eq!(tail("ab", 5), "ab");
        assert_eq!(crate::logging::short_category(LOOP), "AgentLoopRunner");
        assert_eq!(crate::logging::short_category(EXECUTOR), "ToolCallExecutor");
        assert_eq!(crate::logging::short_category(PIPELINE), "Program");
    }
}
