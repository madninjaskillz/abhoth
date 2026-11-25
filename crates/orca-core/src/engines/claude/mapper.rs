//! Turns Claude Code's stream-json output (`claude -p --output-format stream-json --verbose
//! --include-partial-messages`) into engine events (`ClaudeStreamMapper`). One mapper per
//! bridge session; it remembers tool calls so results can be paired with their names and
//! arguments, and which messages streamed partial text so the full message isn't shown twice.
//! Messages of sub-agents (`parent_tool_use_id` set) are skipped: only the Task call shows.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::engines::bridge_tools::{self, is_file_write_tool};
use crate::engines::json::{int_of, prop, raw_text, str_of};
use crate::engines::paths;
use crate::engines::{AgentEngineEvent, EngineTurnCompleted};

struct PendingTool {
    name: String,
    original_name: String,
    args_json: String,
    file_path: Option<String>,
}

pub struct ClaudeStreamMapper {
    cwd: Option<String>,
    tools: HashMap<String, PendingTool>,
    streamed_messages: HashSet<String>,
    turn_text: String,
    current_stream_message: Option<String>,
    last_message_text: Option<String>,
    last_message_id: Option<String>,
    session_id: Option<String>,
}

impl ClaudeStreamMapper {
    pub fn new(working_directory: Option<&str>) -> Self {
        Self {
            cwd: working_directory.map(str::to_string),
            tools: HashMap::new(),
            streamed_messages: HashSet::new(),
            turn_text: String::new(),
            current_stream_message: None,
            last_message_text: None,
            last_message_id: None,
            session_id: None,
        }
    }

    /// The session id the CLI last reported.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// All of this turn's answer text, paragraphs between messages.
    pub fn turn_text(&self) -> &str {
        &self.turn_text
    }

    /// Adds text the bridge produced itself (a plan from ExitPlanMode) to the turn's answer.
    pub fn append_turn_text(&mut self, text: &str) {
        if !self.turn_text.is_empty() {
            self.turn_text.push_str("\n\n");
        }
        self.turn_text.push_str(text);
    }

    /// Start a new turn's text (the session's tool memory is kept).
    pub fn begin_turn(&mut self) {
        self.turn_text.clear();
        self.last_message_text = None;
        self.last_message_id = None;
    }

    /// Map one stdout line. Non-JSON lines and unknown types map to nothing.
    pub fn map_line(&mut self, line: &str) -> Vec<AgentEngineEvent> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.map(&v),
            Err(_) => Vec::new(),
        }
    }

    pub fn map(&mut self, msg: &Value) -> Vec<AgentEngineEvent> {
        if !msg.is_object() {
            return Vec::new();
        }
        if let Some(sid) = str_of(msg, "session_id").filter(|s| !s.is_empty()) {
            self.session_id = Some(sid.to_string());
        }
        // A sub-agent's own traffic: the parent's Task call and result already show it.
        if matches!(prop(msg, "parent_tool_use_id"), Some(Value::String(_))) {
            return Vec::new();
        }

        let mut events = Vec::new();
        match str_of(msg, "type") {
            Some("system") => Self::map_system(msg, &mut events),
            Some("stream_event") => {
                if let Some(ev) = prop(msg, "event") {
                    self.map_stream_event(ev, &mut events);
                }
            }
            Some("assistant") => {
                if let Some(m) = prop(msg, "message") {
                    self.map_assistant(m, &mut events);
                }
            }
            Some("user") => self.map_user(msg, &mut events),
            Some("rate_limit_event") => {
                if let Some(rl) = prop(msg, "rate_limit_info")
                    && let Some(status) = str_of(rl, "status")
                    && !status.eq_ignore_ascii_case("allowed")
                {
                    let kind = str_of(rl, "rateLimitType")
                        .map(|k| format!(" ({k})"))
                        .unwrap_or_default();
                    events.push(AgentEngineEvent::Notice {
                        level: "warning".into(),
                        text: format!("Claude Code rate limit: {status}{kind}"),
                    });
                }
            }
            Some("result") => events.push(self.map_result(msg)),
            _ => {}
        }
        events
    }

    fn map_system(msg: &Value, events: &mut Vec<AgentEngineEvent>) {
        match str_of(msg, "subtype") {
            Some("init") => events.push(AgentEngineEvent::SessionStarted {
                session_id: str_of(msg, "session_id").map(str::to_string),
                model: str_of(msg, "model").map(str::to_string),
                working_directory: str_of(msg, "cwd").map(str::to_string),
            }),
            Some("thinking_tokens") => events.push(AgentEngineEvent::ThinkingProgress(int_of(
                msg,
                "estimated_tokens",
            ))),
            Some("compact_boundary") => events.push(AgentEngineEvent::Notice {
                level: "info".into(),
                text: "Claude Code compacted the conversation.".into(),
            }),
            Some("api_retry" | "api_error") => events.push(AgentEngineEvent::Notice {
                level: "warning".into(),
                text: format!(
                    "Claude Code: {}",
                    str_of(msg, "error")
                        .or_else(|| str_of(msg, "message"))
                        .unwrap_or("retrying the API request")
                ),
            }),
            _ => {}
        }
    }

    fn map_stream_event(&mut self, ev: &Value, events: &mut Vec<AgentEngineEvent>) {
        match str_of(ev, "type") {
            Some("message_start") => {
                self.current_stream_message = prop(ev, "message")
                    .and_then(|m| str_of(m, "id"))
                    .map(str::to_string);
            }
            Some("content_block_start") => {
                if let Some(block) = prop(ev, "content_block")
                    && matches!(
                        str_of(block, "type"),
                        Some("thinking" | "redacted_thinking")
                    )
                {
                    events.push(AgentEngineEvent::ThinkingProgress(None));
                }
            }
            Some("content_block_delta") => {
                let Some(delta) = prop(ev, "delta") else {
                    return;
                };
                match str_of(delta, "type") {
                    Some("text_delta") => {
                        if let Some(text) = str_of(delta, "text").filter(|t| !t.is_empty()) {
                            self.mark_streamed();
                            events.push(AgentEngineEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        self.mark_streamed();
                        match str_of(delta, "thinking").filter(|t| !t.is_empty()) {
                            Some(thinking) => {
                                events.push(AgentEngineEvent::ThinkingDelta(thinking.to_string()))
                            }
                            None => events.push(AgentEngineEvent::ThinkingProgress(int_of(
                                delta,
                                "estimated_tokens",
                            ))),
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn mark_streamed(&mut self) {
        if let Some(id) = &self.current_stream_message {
            self.streamed_messages.insert(id.clone());
        }
    }

    fn map_assistant(&mut self, message: &Value, events: &mut Vec<AgentEngineEvent>) {
        let id = str_of(message, "id").map(str::to_string);
        let streamed = id
            .as_ref()
            .is_some_and(|i| self.streamed_messages.contains(i));
        let Some(Value::Array(content)) = prop(message, "content") else {
            return;
        };

        // One API message arrives as several assistant lines (one per content block).
        if id.is_none() || id != self.last_message_id {
            self.last_message_id = id;
            self.last_message_text = None;
        }

        for block in content {
            match str_of(block, "type") {
                Some("text") => {
                    let text = str_of(block, "text").unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    if !self.turn_text.is_empty() && self.last_message_text.is_none() {
                        self.turn_text.push_str("\n\n");
                    }
                    self.turn_text.push_str(text);
                    self.last_message_text
                        .get_or_insert_with(String::new)
                        .push_str(text);
                    if !streamed {
                        events.push(AgentEngineEvent::TextDelta(text.to_string()));
                    }
                }
                Some("thinking") => {
                    if !streamed
                        && let Some(thinking) = str_of(block, "thinking").filter(|t| !t.is_empty())
                    {
                        events.push(AgentEngineEvent::ThinkingDelta(thinking.to_string()));
                    }
                }
                Some("tool_use") => {
                    let tool_id = str_of(block, "id").unwrap_or("").to_string();
                    let original = str_of(block, "name").unwrap_or("tool").to_string();
                    let name = bridge_tools::from_claude(&original);
                    let input = prop(block, "input");
                    let args = bridge_tools::normalize_args(input, self.cwd.as_deref());
                    let mut file = bridge_tools::file_path_of(input);
                    if let (Some(f), Some(cwd)) = (&file, &self.cwd)
                        && !paths::is_path_fully_qualified(f)
                    {
                        file = Some(paths::get_full_path(&paths::combine(cwd, f)));
                    }
                    self.tools.insert(
                        tool_id.clone(),
                        PendingTool {
                            name: name.clone(),
                            original_name: original.clone(),
                            args_json: args.clone(),
                            file_path: file,
                        },
                    );
                    events.push(AgentEngineEvent::ToolCall {
                        id: tool_id,
                        name,
                        original_name: original,
                        args_json: args,
                    });
                }
                _ => {}
            }
        }
    }

    fn map_user(&mut self, msg: &Value, events: &mut Vec<AgentEngineEvent>) {
        let Some(Value::Array(content)) = prop(msg, "message").and_then(|m| prop(m, "content"))
        else {
            return;
        };
        let details = prop(msg, "tool_use_result");

        for block in content {
            if str_of(block, "type") != Some("tool_result") {
                continue;
            }
            let id = str_of(block, "tool_use_id").unwrap_or("").to_string();
            let is_error = prop(block, "is_error") == Some(&Value::Bool(true));
            let mut text = result_text(block);
            let call = self.tools.remove(&id);
            let name = call
                .as_ref()
                .map_or_else(|| "tool".to_string(), |c| c.name.clone());

            if !is_error
                && let Some(call) = &call
                && is_file_write_tool(&name)
            {
                let display = call.file_path.as_deref().map_or_else(
                    || "file".to_string(),
                    |fp| bridge_tools::relative(fp, self.cwd.as_deref()),
                );
                if let Some(diff) = bridge_tools::diff_from_structured_patch(details, &display) {
                    text = diff;
                } else if call.original_name == "Write"
                    && let Some(d) = details.filter(|d| d.is_object())
                    && let Some(written) = str_of(d, "content")
                {
                    let n = count_lines(written);
                    text = format!(
                        "Wrote {n} line{} to {display}",
                        if n == 1 { "" } else { "s" }
                    );
                }
            }

            events.push(AgentEngineEvent::ToolResult {
                id: id.clone(),
                name: name.clone(),
                result: text,
                is_error,
                args_json: call.as_ref().map(|c| c.args_json.clone()),
            });
            if !is_error
                && let Some(path) = call.and_then(|c| c.file_path)
                && is_file_write_tool(&name)
            {
                events.push(AgentEngineEvent::FileEdited {
                    path,
                    tool_call_id: id,
                });
            }
        }
    }

    fn map_result(&self, msg: &Value) -> AgentEngineEvent {
        let subtype = str_of(msg, "subtype");
        let is_error = prop(msg, "is_error") == Some(&Value::Bool(true));
        let terminal = str_of(msg, "terminal_reason");
        let outcome = if !is_error && subtype == Some("success") {
            "success"
        } else if matches!(
            terminal,
            Some("aborted_tools" | "aborted_streaming" | "interrupted")
        ) {
            "cancelled"
        } else {
            "error"
        };
        let (mut input, mut output) = (None, None);
        if let Some(usage) = prop(msg, "usage").filter(|u| u.is_object()) {
            input = Some(
                int_of(usage, "input_tokens").unwrap_or(0)
                    + int_of(usage, "cache_read_input_tokens").unwrap_or(0)
                    + int_of(usage, "cache_creation_input_tokens").unwrap_or(0),
            );
            output = int_of(usage, "output_tokens");
        }
        let cost = prop(msg, "total_cost_usd").and_then(Value::as_f64);
        let duration = prop(msg, "duration_ms").and_then(Value::as_i64);
        let final_text = str_of(msg, "result")
            .map(str::to_string)
            .or_else(|| self.last_message_text.clone());
        let mut detail = if outcome == "success" {
            None
        } else {
            subtype.or(terminal).map(str::to_string)
        };
        if outcome == "error"
            && is_error
            && let Some(err) = str_of(msg, "result").filter(|e| !e.is_empty())
        {
            detail = Some(err.to_string());
        }
        AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
            outcome: outcome.into(),
            detail,
            session_id: str_of(msg, "session_id")
                .map(str::to_string)
                .or_else(|| self.session_id.clone()),
            final_text,
            cost_usd: cost,
            input_tokens: input,
            output_tokens: output,
            duration_ms: duration,
            num_turns: int_of(msg, "num_turns"),
        })
    }
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.trim_end_matches('\n').split('\n').count()
    }
}

fn result_text(block: &Value) -> String {
    let Some(c) = prop(block, "content") else {
        return String::new();
    };
    match c {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(parts) => {
            let mut sb = String::new();
            for part in parts {
                match str_of(part, "type") {
                    Some("text") => {
                        if !sb.is_empty() {
                            sb.push('\n');
                        }
                        sb.push_str(str_of(part, "text").unwrap_or(""));
                    }
                    Some("image") => {
                        if !sb.is_empty() {
                            sb.push('\n');
                        }
                        sb.push_str("[image]");
                    }
                    _ => {}
                }
            }
            sb
        }
        other => raw_text(other),
    }
}
