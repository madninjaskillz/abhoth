//! `RepeatCallNote`: notices a command or read that repeats an earlier one to no effect (the
//! same tool with the same arguments, no write or edit call in between, and the same result,
//! digits ignored). The result then gets a one-line note so the model stops re-running checks
//! on unchanged code. Nothing is blocked: a repeat can be a legitimate poll.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::client::{ChatMessage, Role, ToolCall};
use crate::config::dotnet_json;

/// Tools whose repeats are noted: commands and reads (case-insensitive).
pub const WATCHED_TOOLS: &[&str] = &[
    "bash",
    "read",
    "read_file",
    "grep",
    "find",
    "ls",
    "glob",
    "list_directory",
    "run_command",
    "execute_command",
];

/// Calls that change files; one in between means a repeat is a real re-check.
pub const WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "write_file",
    "edit_file",
    "multi_edit",
    "create_file",
    "delete_file",
    "move_file",
    "rename_file",
    "copy_file",
    "apply_patch",
];

fn in_set(set: &[&str], name: &str) -> bool {
    set.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// The note to append to `result` of `call`, or None. The history must already hold the
/// assistant message that made the call, plus the results of the calls before it in that
/// message.
pub fn note_for(history: &[ChatMessage], call: &ToolCall, result: &str) -> Option<String> {
    if !in_set(WATCHED_TOOLS, &call.name) || call.id.is_empty() {
        return None;
    }
    let key = args_key(&call.arguments);
    let mut steps = 0;
    let mut seen_current = false;
    for i in (0..history.len()).rev() {
        let msg = &history[i];
        if msg.role != Role::Assistant {
            continue;
        }
        let calls = &msg.tool_calls;
        let at = if seen_current {
            None
        } else {
            calls.iter().position(|c| c.id == call.id)
        };
        if let Some(at) = at {
            seen_current = true;
            // Calls before this one in the same message have run already.
            if calls[..at].iter().any(|c| in_set(WRITE_TOOLS, &c.name)) {
                return None;
            }
            continue;
        }
        if !seen_current {
            continue; // newer than the call (shouldn't happen)
        }
        steps += 1;
        // Latest first: a later call in a message ran after the earlier ones.
        for earlier in calls.iter().rev() {
            if in_set(WRITE_TOOLS, &earlier.name) {
                return None;
            }
            if !earlier.name.eq_ignore_ascii_case(&call.name) || args_key(&earlier.arguments) != key
            {
                continue;
            }
            let previous = find_result(history, i, &earlier.id)?;
            if normalize(previous) != normalize(result) {
                return None;
            }
            return Some(format!(
                "\n\n[Repeat: the same {} call as {steps} step{} ago, with no write or edit since, gave the same result. Running it again will not change it: change the code first, or move on.]",
                call.name,
                if steps == 1 { "" } else { "s" }
            ));
        }
    }
    None
}

fn find_result<'a>(history: &'a [ChatMessage], from: usize, call_id: &str) -> Option<&'a str> {
    if call_id.is_empty() {
        return None;
    }
    history[from + 1..]
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call_id))
        .map(|m| m.content.as_deref().unwrap_or(""))
}

/// The arguments with sorted keys, serialized; "" for none.
fn args_key(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(o)) if !o.is_empty() => {
            let sorted: BTreeMap<&String, &Value> = o.iter().collect();
            dotnet_json::to_string_dotnet(&sorted).unwrap_or_default()
        }
        _ => String::new(),
    }
}

static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// The result with digits and whitespace runs collapsed (timings, counters, temp names), and
/// without an earlier repeat note.
pub fn normalize(text: &str) -> String {
    let without_note = match text.find("\n\n[Repeat: ") {
        Some(i) => &text[..i],
        None => text,
    };
    let digits = DIGITS.replace_all(without_note, "#");
    WHITESPACE.replace_all(&digits, " ").trim().to_string()
}
