//! Turns Codex app-server notifications (JSON-RPC, protocol v2) into engine events
//! (`CodexStreamMapper`):
//! * `item/agentMessage/delta`, `item/plan/delta` -> streamed answer text
//! * `item/reasoning/*Delta` -> thinking text
//! * `commandExecution` items -> bash tool cards (command, output, exit code)
//! * `fileChange` items -> write_file / edit_file cards with the diff, plus file-edit markers
//! * `dynamicToolCall` / `mcpToolCall` / `webSearch` items -> their tool cards
//! * `thread/tokenUsage/updated` + `turn/completed` -> the turn's end
//!
//! Stateful per thread: [`CodexStreamMapper::begin_turn`] before each turn.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::engines::bridge_tools;
use crate::engines::json::{long_of, prop, raw_text, str_of, to_compact_dotnet};
use crate::engines::paths;
use crate::engines::{AgentEngineEvent, EngineTurnCompleted};

/// (OpenOrca name, CLI name, arguments) of a tool call.
pub type CallSpec = (String, String, String);

pub struct CodexStreamMapper {
    cwd: Option<String>,
    turn_text: String,
    streamed_items: HashSet<String>,
    open_calls: IndexMap<String, (String, String)>,
    file_changes: HashMap<String, Value>,
    current_text_item: Option<String>,
    input_before: i64,
    output_before: i64,
    input_total: i64,
    output_total: i64,
    /// The active turn's id (from turn/started or the turn/start response).
    pub turn_id: Option<String>,
}

impl CodexStreamMapper {
    pub fn new(working_directory: Option<&str>) -> Self {
        Self {
            cwd: working_directory.map(str::to_string),
            turn_text: String::new(),
            streamed_items: HashSet::new(),
            open_calls: IndexMap::new(),
            file_changes: HashMap::new(),
            current_text_item: None,
            input_before: 0,
            output_before: 0,
            input_total: 0,
            output_total: 0,
            turn_id: None,
        }
    }

    /// The turn's answer text so far (all agent messages and plans, blank-line separated).
    pub fn turn_text(&self) -> &str {
        &self.turn_text
    }

    pub fn begin_turn(&mut self) {
        self.turn_text.clear();
        self.streamed_items.clear();
        self.open_calls.clear();
        self.current_text_item = None;
        self.turn_id = None;
        self.input_before = self.input_total;
        self.output_before = self.output_total;
    }

    /// The changes of a fileChange item seen in item/started (for its approval request).
    pub fn file_changes_of(&self, item_id: &str) -> Option<&Value> {
        self.file_changes.get(item_id)
    }

    /// The OpenOrca call (name, args) for an item, once item/started was seen.
    pub fn call_of(&self, item_id: &str) -> Option<&(String, String)> {
        self.open_calls.get(item_id)
    }

    pub fn map(&mut self, msg: &Value) -> Vec<AgentEngineEvent> {
        let mut events = Vec::new();
        let null = Value::Null;
        let p = prop(msg, "params").unwrap_or(&null);
        match str_of(msg, "method") {
            Some("turn/started") => {
                if let Some(id) = prop(p, "turn").and_then(|t| str_of(t, "id")) {
                    self.turn_id = Some(id.to_string());
                }
            }
            Some("item/agentMessage/delta" | "item/plan/delta") => {
                self.text(str_of(p, "itemId"), str_of(p, "delta"), &mut events);
            }
            Some("item/reasoning/summaryTextDelta" | "item/reasoning/textDelta") => {
                if let Some(thought) = str_of(p, "delta").filter(|d| !d.is_empty()) {
                    events.push(AgentEngineEvent::ThinkingDelta(thought.to_string()));
                }
            }
            Some("item/started") => {
                if let Some(item) = prop(p, "item") {
                    self.item_started(item, &mut events);
                }
            }
            Some("item/completed") => {
                if let Some(item) = prop(p, "item") {
                    self.item_completed(item, &mut events);
                }
            }
            Some("thread/tokenUsage/updated") => {
                if let Some(total) = prop(p, "tokenUsage").and_then(|u| prop(u, "total")) {
                    self.input_total = long_of(total, "inputTokens").unwrap_or(0);
                    self.output_total = long_of(total, "outputTokens").unwrap_or(0);
                }
            }
            Some("error") => {
                let err = prop(p, "error").and_then(|e| str_of(e, "message"));
                let retry = prop(p, "willRetry") == Some(&Value::Bool(true));
                if let Some(err) = err.filter(|e| !e.is_empty()) {
                    events.push(AgentEngineEvent::Notice {
                        level: if retry { "warning" } else { "error" }.into(),
                        text: format!("Codex: {err}{}", if retry { " (retrying)" } else { "" }),
                    });
                }
            }
            Some("turn/completed") => {
                if let Some(turn) = prop(p, "turn") {
                    let thread = str_of(p, "threadId").map(str::to_string);
                    self.turn_completed(thread, turn, &mut events);
                }
            }
            _ => {}
        }
        events
    }

    fn text(
        &mut self,
        item_id: Option<&str>,
        delta: Option<&str>,
        events: &mut Vec<AgentEngineEvent>,
    ) {
        let Some(delta) = delta.filter(|d| !d.is_empty()) else {
            return;
        };
        if let Some(id) = item_id {
            if self.current_text_item.as_deref() != Some(id) {
                if !self.turn_text.is_empty() {
                    self.turn_text.push_str("\n\n");
                    events.push(AgentEngineEvent::TextDelta("\n\n".into()));
                }
                self.current_text_item = Some(id.to_string());
            }
            self.streamed_items.insert(id.to_string());
        }
        self.turn_text.push_str(delta);
        events.push(AgentEngineEvent::TextDelta(delta.to_string()));
    }

    fn item_started(&mut self, item: &Value, events: &mut Vec<AgentEngineEvent>) {
        let id = str_of(item, "id").unwrap_or("").to_string();
        let Some((name, original, args)) = self.call_for(item) else {
            return;
        };
        self.open_calls
            .insert(id.clone(), (name.clone(), args.clone()));
        events.push(AgentEngineEvent::ToolCall {
            id,
            name,
            original_name: original,
            args_json: args,
        });
    }

    fn item_completed(&mut self, item: &Value, events: &mut Vec<AgentEngineEvent>) {
        let id = str_of(item, "id").unwrap_or("").to_string();
        let kind = str_of(item, "type");
        if matches!(kind, Some("agentMessage" | "plan")) {
            if !self.streamed_items.contains(&id)
                && let Some(text) = str_of(item, "text").filter(|t| !t.is_empty())
            {
                let text = text.to_string();
                self.text(Some(&id), Some(&text), events);
            }
            return;
        }

        let open = match self.open_calls.get(&id) {
            Some(o) => o.clone(),
            None => {
                // Completed without a start we saw (resumed mid-item): show it whole.
                let Some((name, original, args)) = self.call_for(item) else {
                    return;
                };
                events.push(AgentEngineEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    original_name: original,
                    args_json: args.clone(),
                });
                (name, args)
            }
        };
        self.open_calls.shift_remove(&id);
        let (name, args) = open;

        let status = str_of(item, "status");
        let result = |result: String, is_error: bool| AgentEngineEvent::ToolResult {
            id: id.clone(),
            name: name.clone(),
            result,
            is_error,
            args_json: Some(args.clone()),
        };
        match kind {
            Some("commandExecution") => {
                let output = str_of(item, "aggregatedOutput")
                    .unwrap_or("")
                    .trim_end()
                    .to_string();
                let exit = prop(item, "exitCode")
                    .and_then(Value::as_i64)
                    .map(|e| e as i32);
                let failed_exit = exit.is_some_and(|e| e != 0);
                let is_error = matches!(status, Some("failed" | "declined")) || failed_exit;
                let text = if status == Some("declined") {
                    "Declined in OpenOrca.".to_string()
                } else if failed_exit {
                    format!(
                        "{}(exit code {})",
                        if output.is_empty() {
                            String::new()
                        } else {
                            format!("{output}\n")
                        },
                        exit.unwrap_or(0)
                    )
                } else if !output.is_empty() {
                    output
                } else {
                    "(no output)".to_string()
                };
                events.push(result(text, is_error));
            }
            Some("fileChange") => {
                let ok = status == Some("completed");
                let text = if ok {
                    self.diff_summary(item)
                } else if status == Some("declined") {
                    "Declined in OpenOrca.".to_string()
                } else {
                    "The patch failed to apply.".to_string()
                };
                events.push(result(text, !ok));
                if ok && let Some(Value::Array(changes)) = prop(item, "changes") {
                    for c in changes {
                        if let Some(path) = str_of(c, "path").filter(|p| !p.is_empty()) {
                            events.push(AgentEngineEvent::FileEdited {
                                path: path.to_string(),
                                tool_call_id: id.clone(),
                            });
                        }
                    }
                }
                self.file_changes.remove(&id);
            }
            Some("dynamicToolCall") => {
                let success = prop(item, "success") != Some(&Value::Bool(false));
                events.push(result(
                    content_text(item, "contentItems"),
                    !success || status == Some("failed"),
                ));
            }
            Some("mcpToolCall") => {
                let error = prop(item, "error");
                let failed = status == Some("failed") || error.is_some_and(Value::is_object);
                let text = if failed && let Some(err) = error {
                    str_of(err, "message").unwrap_or("failed").to_string()
                } else if let Some(r) = prop(item, "result") {
                    content_text(r, "content")
                } else {
                    String::new()
                };
                events.push(result(text, failed));
            }
            _ => {
                let failed = status == Some("failed");
                events.push(result(
                    if failed { "failed" } else { "done" }.to_string(),
                    failed,
                ));
            }
        }
    }

    fn turn_completed(
        &mut self,
        thread_id: Option<String>,
        turn: &Value,
        events: &mut Vec<AgentEngineEvent>,
    ) {
        // Items the CLI never finished (an interrupted command): close their cards.
        for (id, (name, args)) in self.open_calls.drain(..) {
            events.push(AgentEngineEvent::ToolResult {
                id,
                name,
                result: "Interrupted.".into(),
                is_error: true,
                args_json: Some(args),
            });
        }

        let status = str_of(turn, "status");
        let outcome = match status {
            Some("completed") => "success",
            Some("interrupted") => "cancelled",
            _ => "error",
        };
        let mut detail = match prop(turn, "error") {
            Some(err) if err.is_object() => str_of(err, "message").map(str::to_string),
            _ => None,
        };
        if detail.is_none() {
            detail = if outcome == "error" {
                Some(format!("turn {}", status.unwrap_or("failed")))
            } else {
                status.map(str::to_string)
            };
        }
        let duration = prop(turn, "durationMs").and_then(Value::as_i64);
        let input = (self.input_total - self.input_before).max(0) as i32;
        let output = (self.output_total - self.output_before).max(0) as i32;
        events.push(AgentEngineEvent::TurnCompleted(EngineTurnCompleted {
            outcome: outcome.into(),
            detail,
            session_id: thread_id,
            final_text: Some(self.turn_text.clone()),
            cost_usd: None,
            input_tokens: Some(input),
            output_tokens: Some(output),
            duration_ms: duration,
            num_turns: None,
        }));
        self.input_before = self.input_total;
        self.output_before = self.output_total;
    }

    fn call_for(&mut self, item: &Value) -> Option<CallSpec> {
        let id = str_of(item, "id").unwrap_or("").to_string();
        match str_of(item, "type")? {
            "commandExecution" => {
                let mut args = Map::new();
                args.insert("command".into(), Value::String(display_command(item)));
                if let Some(cwd) = str_of(item, "cwd").filter(|c| !c.is_empty())
                    && !same_path(cwd, self.cwd.as_deref())
                {
                    args.insert("cwd".into(), Value::String(cwd.to_string()));
                }
                Some((
                    "bash".into(),
                    "shell".into(),
                    to_compact_dotnet(&Value::Object(args)),
                ))
            }
            "fileChange" => {
                let changes = prop(item, "changes").filter(|c| c.is_array())?.clone();
                let call = file_change_call(&changes, self.cwd.as_deref());
                self.file_changes.insert(id, changes);
                Some(call)
            }
            "dynamicToolCall" => {
                let tool = str_of(item, "tool").unwrap_or("tool").to_string();
                let args = prop(item, "arguments").map_or_else(|| "{}".to_string(), raw_text);
                Some((tool.clone(), tool, args))
            }
            "mcpToolCall" => {
                let server = str_of(item, "server").unwrap_or("mcp");
                let tool = str_of(item, "tool").unwrap_or("tool").to_string();
                let args = prop(item, "arguments").map_or_else(|| "{}".to_string(), raw_text);
                Some((format!("mcp__{server}__{tool}"), tool, args))
            }
            "webSearch" => {
                let mut args = Map::new();
                args.insert(
                    "query".into(),
                    Value::String(str_of(item, "query").unwrap_or("").to_string()),
                );
                Some((
                    "web_search".into(),
                    "web_search".into(),
                    to_compact_dotnet(&Value::Object(args)),
                ))
            }
            _ => None,
        }
    }

    fn diff_summary(&self, item: &Value) -> String {
        let Some(Value::Array(changes)) = prop(item, "changes") else {
            return "Patched.".into();
        };
        let mut sb = String::new();
        for c in changes {
            let path = bridge_tools::relative(str_of(c, "path").unwrap_or(""), self.cwd.as_deref());
            let kind = prop(c, "kind").and_then(|k| str_of(k, "type"));
            let diff = str_of(c, "diff").unwrap_or("").trim_end_matches('\n');
            if !sb.is_empty() {
                sb.push('\n');
            }
            if kind == Some("add") {
                let lines = if diff.is_empty() {
                    0
                } else {
                    diff.split('\n').count()
                };
                sb.push_str(&format!(
                    "Wrote {lines} line{} to {path}",
                    if lines == 1 { "" } else { "s" }
                ));
                continue;
            }
            if kind == Some("delete") {
                sb.push_str(&format!("Deleted {path}"));
                continue;
            }
            let (mut added, mut removed) = (0, 0);
            for line in diff.split('\n') {
                if line.starts_with('+') && !line.starts_with("+++") {
                    added += 1;
                } else if line.starts_with('-') && !line.starts_with("---") {
                    removed += 1;
                }
            }
            sb.push_str(&format!("Edited {path}  +{added} \u{2212}{removed}"));
            if !diff.is_empty() {
                sb.push('\n');
                sb.push_str(diff);
            }
        }
        sb
    }
}

/// write_file for a patch that only adds files, edit_file otherwise; "path" is the first file
/// (relative to the working directory), "paths" all of them, "content" an added file's text.
pub fn file_change_call(changes: &Value, cwd: Option<&str>) -> CallSpec {
    let mut paths_list = Vec::new();
    let mut all_adds = true;
    let mut content: Option<String> = None;
    for c in changes.as_array().into_iter().flatten() {
        if let Some(path) = str_of(c, "path").filter(|p| !p.is_empty()) {
            paths_list.push(Value::String(bridge_tools::relative(path, cwd)));
        }
        let kind = prop(c, "kind").and_then(|k| str_of(k, "type"));
        if kind != Some("add") {
            all_adds = false;
        } else if content.is_none() {
            content = str_of(c, "diff").map(str::to_string);
        }
    }
    let mut args = Map::new();
    args.insert(
        "path".into(),
        paths_list
            .first()
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    let count = paths_list.len();
    if count > 1 {
        args.insert("paths".into(), Value::Array(paths_list));
    }
    if all_adds
        && count == 1
        && let Some(content) = content
    {
        args.insert("content".into(), Value::String(content));
    }
    (
        if all_adds { "write_file" } else { "edit_file" }.into(),
        "apply_patch".into(),
        to_compact_dotnet(&Value::Object(args)),
    )
}

/// The command as the model wrote it: the shell wrapper (pwsh -Command "...", bash -lc '...')
/// removed.
pub fn display_command(item: &Value) -> String {
    if let Some(Value::Array(actions)) = prop(item, "commandActions")
        && actions.len() == 1
        && let Some(single) = str_of(&actions[0], "command").filter(|c| !c.is_empty())
    {
        return single.to_string();
    }
    str_of(item, "command").unwrap_or("").to_string()
}

fn content_text(owner: &Value, property: &str) -> String {
    let Some(Value::Array(items)) = prop(owner, property) else {
        return String::new();
    };
    let mut sb = String::new();
    for c in items {
        if let Some(text) = str_of(c, "text") {
            if !sb.is_empty() {
                sb.push('\n');
            }
            sb.push_str(text);
        }
    }
    sb
}

fn same_path(a: &str, b: Option<&str>) -> bool {
    let Some(b) = b else {
        return false;
    };
    let fa = paths::get_full_path(a);
    let fb = paths::get_full_path(b);
    paths::path_eq(
        paths::trim_ending_separator(&fa),
        paths::trim_ending_separator(&fb),
    )
}
