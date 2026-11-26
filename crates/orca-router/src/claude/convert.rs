//! OpenAI chat <-> the Claude Code CLI's stream-json: the request side ([`prepare`]: system
//! prompt, user content, client tools, effort) and the answer side ([`Translator`]: CLI events
//! to [`Out`] events, [`Collector`] and [`sse_chunk`] for the OpenAI shapes).
//!
//! The CLI takes user turns only, so a conversation with history (earlier assistant turns, tool
//! calls and results) is sent as one user turn holding a transcript of it; a single user
//! message is sent as it is. Client tools reach Claude as real tool definitions through an
//! in-process MCP server (engine.rs); Claude's tool_use blocks come back as OpenAI tool_calls.

use serde_json::{Map, Value, json};

/// The MCP server name the client's tools are offered under (`mcp__c__<name>` to Claude).
pub const TOOL_SERVER: &str = "c";
const TOOL_PREFIX: &str = "mcp__c__";
/// Anthropic tool names are at most 64 characters, the prefix included.
const MAX_TOOL_NAME: usize = 64 - TOOL_PREFIX.len();

/// A chat request made ready for the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    /// The CLI's `--model` (the deployment's served id).
    pub model: String,
    /// The caller's system and developer messages; None = the engine's default.
    pub system: Option<String>,
    /// The one user turn: Anthropic content blocks.
    pub content: Vec<Value>,
    /// MCP tool definitions (`name`, `description`, `inputSchema`).
    pub tools: Vec<Value>,
    /// `--effort` (low, medium, high, xhigh, max).
    pub effort: Option<String>,
    pub max_tokens: Option<u64>,
    pub stream: bool,
    pub include_usage: bool,
}

/// Check and convert an OpenAI chat body. `Err` = a 400 for the caller.
pub fn prepare(body: &Map<String, Value>) -> Result<Prepared, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(n) = body.get("n").and_then(Value::as_u64)
        && n > 1
    {
        return Err("n > 1 is not supported on this route".into());
    }
    let msgs = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
        .ok_or("messages: a non-empty array is required")?;

    let mut system: Vec<String> = Vec::new();
    let mut turns: Vec<&Map<String, Value>> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let m = m
            .as_object()
            .ok_or_else(|| format!("messages[{i}]: not an object"))?;
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(m.get("content"));
                if !t.trim().is_empty() {
                    system.push(t);
                }
            }
            "user" | "assistant" | "tool" | "function" => turns.push(m),
            r => return Err(format!("messages[{i}]: unknown role `{r}`")),
        }
    }
    if turns.is_empty() {
        return Err("messages: no user message".into());
    }

    let content = if turns.len() == 1 && role(turns[0]) == "user" {
        user_blocks(turns[0].get("content"))?
    } else {
        transcript(&turns)?
    };

    let tool_choice_none = body.get("tool_choice").and_then(Value::as_str) == Some("none");
    let mut tools = Vec::new();
    if !tool_choice_none && let Some(list) = body.get("tools").and_then(Value::as_array) {
        for (i, t) in list.iter().enumerate() {
            tools.push(mcp_tool(t).map_err(|e| format!("tools[{i}]: {e}"))?);
        }
    }

    let effort = match body.get("reasoning_effort").and_then(Value::as_str) {
        None | Some("none") | Some("") => None,
        Some("minimal") => Some("low".to_string()),
        Some(e @ ("low" | "medium" | "high" | "xhigh" | "max")) => Some(e.to_string()),
        Some(e) => {
            return Err(format!(
                "reasoning_effort `{e}` is not supported on this route"
            ));
        }
    };
    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = body
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Prepared {
        model,
        system: (!system.is_empty()).then(|| system.join("\n\n")),
        content,
        tools,
        effort,
        max_tokens,
        stream,
        include_usage,
    })
}

fn role(m: &Map<String, Value>) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

/// Text of a string or an array of parts (non-text parts left out).
fn text_of(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn text_block(t: impl Into<String>) -> Value {
    json!({"type": "text", "text": t.into()})
}

/// A user message's content as Anthropic blocks (text and images).
fn user_blocks(c: Option<&Value>) -> Result<Vec<Value>, String> {
    match c {
        Some(Value::String(s)) => Ok(vec![text_block(s.clone())]),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str).unwrap_or("text") {
                    "text" | "input_text" => out.push(text_block(
                        p.get("text").and_then(Value::as_str).unwrap_or(""),
                    )),
                    "image_url" | "input_image" => {
                        let url = p
                            .pointer("/image_url/url")
                            .or_else(|| p.get("image_url"))
                            .and_then(Value::as_str)
                            .ok_or("an image_url part without a url")?;
                        out.push(image_block(url)?);
                    }
                    t => {
                        return Err(format!(
                            "content part type `{t}` is not supported on this route"
                        ));
                    }
                }
            }
            if out.is_empty() {
                out.push(text_block(""));
            }
            Ok(out)
        }
        None | Some(Value::Null) => Ok(vec![text_block("")]),
        Some(_) => Err("content must be a string or an array of parts".into()),
    }
}

fn image_block(url: &str) -> Result<Value, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or("a data: url without data")?;
        let media = meta
            .strip_suffix(";base64")
            .ok_or("data: images must be base64")?;
        return Ok(json!({"type": "image",
                         "source": {"type": "base64", "media_type": media, "data": data}}));
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(json!({"type": "image", "source": {"type": "url", "url": url}}));
    }
    Err("image urls must be data: or http(s)".into())
}

/// A conversation with history as one user turn: a transcript, then what to do with it.
fn transcript(turns: &[&Map<String, Value>]) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = vec![text_block(
        "The conversation so far is below as a transcript (the user's turns, your earlier \
         replies, the tool calls you made and their results). Continue it: write your next \
         reply as the assistant, without repeating the transcript's tags.\n\n<transcript>\n",
    )];
    for m in turns {
        match role(m) {
            "user" => {
                out.push(text_block("<user>\n"));
                out.extend(user_blocks(m.get("content"))?);
                out.push(text_block("\n</user>\n"));
            }
            "assistant" => {
                let mut t = String::from("<assistant>\n");
                t.push_str(&text_of(m.get("content")));
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let id = c.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = c
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let args = c
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        t.push_str(&format!(
                            "\n<tool_call id=\"{id}\" name=\"{name}\">{args}</tool_call>"
                        ));
                    }
                }
                t.push_str("\n</assistant>\n");
                out.push(text_block(t));
            }
            _ => {
                // tool / function results
                let id = m
                    .get("tool_call_id")
                    .or_else(|| m.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                out.push(text_block(format!(
                    "<tool_result id=\"{id}\">\n{}\n</tool_result>\n",
                    text_of(m.get("content"))
                )));
            }
        }
    }
    out.push(text_block("</transcript>"));
    Ok(merge_text(out))
}

/// Adjacent text blocks joined (images stay where they are).
fn merge_text(blocks: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for b in blocks {
        let is_text = b.get("type").and_then(Value::as_str) == Some("text");
        if is_text
            && let Some(last) = out.last_mut()
            && last.get("type").and_then(Value::as_str) == Some("text")
        {
            let add = b
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(Value::String(t)) = last.get_mut("text") {
                t.push_str(&add);
            }
            continue;
        }
        out.push(b);
    }
    out
}

/// An OpenAI tool as an MCP tool definition.
fn mcp_tool(t: &Value) -> Result<Value, String> {
    if let Some(k) = t.get("type").and_then(Value::as_str)
        && k != "function"
    {
        return Err(format!("tool type `{k}` is not supported on this route"));
    }
    let f = t.get("function").ok_or("no function")?;
    let name = f.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty()
        || name.len() > MAX_TOOL_NAME
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "tool name `{name}`: 1-{MAX_TOOL_NAME} characters of A-Z a-z 0-9 _ - on this route"
        ));
    }
    let schema = match f.get("parameters") {
        Some(p @ Value::Object(_)) => p.clone(),
        _ => json!({"type": "object", "properties": {}}),
    };
    Ok(json!({
        "name": name,
        "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
        "inputSchema": schema,
    }))
}

/// Token counts, Anthropic's split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Usage {
    fn from_json(u: &Value) -> Self {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input: n("input_tokens"),
            cache_creation: n("cache_creation_input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            output: n("output_tokens"),
        }
    }

    /// OpenAI's usage object (prompt tokens include cached ones).
    pub fn to_openai(self) -> Value {
        let prompt = self.input + self.cache_creation + self.cache_read;
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": self.output,
            "total_tokens": prompt + self.output,
            "prompt_tokens_details": {"cached_tokens": self.cache_read},
        })
    }
}

/// What the engine passes on, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Text(String),
    Reasoning(String),
    /// A tool call starts (OpenAI index, id, the client's tool name).
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolArgs {
        index: usize,
        json: String,
    },
    /// The answer is complete: `stop`, `length` or `tool_calls`.
    Finish {
        reason: String,
        usage: Usage,
    },
    /// The subscription's limit was hit (seconds until it resets, when known).
    RateLimited {
        retry_after: Option<u64>,
        message: String,
    },
    /// Anything else that went wrong: an HTTP status for the caller and a message.
    Error {
        status: u16,
        kind: String,
        message: String,
    },
}

impl Out {
    /// Ends the answer.
    pub fn is_end(&self) -> bool {
        matches!(
            self,
            Out::Finish { .. } | Out::RateLimited { .. } | Out::Error { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Block {
    Text,
    Thinking,
    Tool(usize),
    Other,
}

/// CLI stream-json lines in, [`Out`] events out.
#[derive(Debug, Default)]
pub struct Translator {
    /// The model the CLI resolved (`system/init`, `message_start`).
    pub model: Option<String>,
    blocks: Vec<(u64, Block)>,
    tools: usize,
    partial: bool,
    stop_reason: Option<String>,
    msg_usage: Usage,
    total: Usage,
    resets_at: Option<u64>,
    done: bool,
}

impl Translator {
    pub fn done(&self) -> bool {
        self.done
    }

    /// One parsed line. `now` = Unix seconds (for Retry-After).
    pub fn push(&mut self, v: &Value, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        // a subagent's traffic (never expected with tools off)
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return out;
        }
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" => {
                if let Some(m) = v.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
            }
            "stream_event" => self.stream_event(v.get("event").unwrap_or(&Value::Null), &mut out),
            "assistant" => self.assistant(v, now, &mut out),
            "rate_limit_event" => {
                let info = v.get("rate_limit_info").unwrap_or(&Value::Null);
                if info.get("status").and_then(Value::as_str) == Some("rejected") {
                    self.resets_at = info
                        .get("resetsAt")
                        .or_else(|| info.get("resets_at"))
                        .and_then(Value::as_u64);
                }
            }
            "result" => self.result(v, now, &mut out),
            _ => {}
        }
        if out.iter().any(Out::is_end) {
            self.done = true;
        }
        out
    }

    fn stream_event(&mut self, e: &Value, out: &mut Vec<Out>) {
        self.partial = true;
        match e.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                self.blocks.clear();
                self.stop_reason = None;
                if let Some(m) = e.pointer("/message/model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
                self.msg_usage = e
                    .pointer("/message/usage")
                    .map(Usage::from_json)
                    .unwrap_or_default();
            }
            "content_block_start" => {
                let idx = e.get("index").and_then(Value::as_u64).unwrap_or(0);
                let cb = e.get("content_block").unwrap_or(&Value::Null);
                let kind = match cb.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(t) = cb.get("text").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push(Out::Text(t.to_string()));
                        }
                        Block::Text
                    }
                    "thinking" => Block::Thinking,
                    "tool_use" => {
                        let index = self.tools;
                        self.tools += 1;
                        out.push(Out::ToolStart {
                            index,
                            id: cb
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            name: client_tool_name(
                                cb.get("name").and_then(Value::as_str).unwrap_or(""),
                            ),
                        });
                        Block::Tool(index)
                    }
                    _ => Block::Other,
                };
                self.blocks.push((idx, kind));
            }
            "content_block_delta" => {
                let idx = e.get("index").and_then(Value::as_u64).unwrap_or(0);
                let kind = self
                    .blocks
                    .iter()
                    .rev()
                    .find(|(i, _)| *i == idx)
                    .map_or(Block::Other, |(_, k)| *k);
                let d = e.get("delta").unwrap_or(&Value::Null);
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                match (d.get("type").and_then(Value::as_str).unwrap_or(""), kind) {
                    ("text_delta", _) => out.push(Out::Text(s("text"))),
                    ("thinking_delta", _) => out.push(Out::Reasoning(s("thinking"))),
                    ("input_json_delta", Block::Tool(index)) => out.push(Out::ToolArgs {
                        index,
                        json: s("partial_json"),
                    }),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = e.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_string());
                }
                if let Some(n) = e.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.msg_usage.output = n;
                }
            }
            "message_stop" => {
                self.add_message_usage();
                // Claude wants a tool: the turn ends here, the client runs it (the CLI is
                // stopped before it would call the tool itself).
                if self.stop_reason.as_deref() == Some("tool_use") && self.tools > 0 {
                    out.push(Out::Finish {
                        reason: "tool_calls".into(),
                        usage: self.total,
                    });
                }
            }
            _ => {}
        }
    }

    fn add_message_usage(&mut self) {
        let m = std::mem::take(&mut self.msg_usage);
        self.total.input += m.input;
        self.total.cache_creation += m.cache_creation;
        self.total.cache_read += m.cache_read;
        self.total.output += m.output;
    }

    fn assistant(&mut self, v: &Value, now: u64, out: &mut Vec<Out>) {
        let msg = v.get("message").unwrap_or(&Value::Null);
        if let Some(code) = v.get("error").and_then(Value::as_str) {
            let text = text_of(msg.get("content"));
            out.push(self.failure(code, &text, now));
            return;
        }
        if self.partial {
            return; // already passed on from the stream events
        }
        // no partial messages (an older CLI): the whole message at once
        if let Some(m) = msg.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        for b in msg
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => out.push(Out::Text(
                    b.get("text").and_then(Value::as_str).unwrap_or("").into(),
                )),
                "thinking" => out.push(Out::Reasoning(
                    b.get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                )),
                "tool_use" => {
                    let index = self.tools;
                    self.tools += 1;
                    out.push(Out::ToolStart {
                        index,
                        id: b.get("id").and_then(Value::as_str).unwrap_or("").into(),
                        name: client_tool_name(b.get("name").and_then(Value::as_str).unwrap_or("")),
                    });
                    out.push(Out::ToolArgs {
                        index,
                        json: b.get("input").map_or("{}".into(), Value::to_string),
                    });
                }
                _ => {}
            }
        }
        if let Some(u) = msg.get("usage") {
            let u = Usage::from_json(u);
            self.total.input += u.input;
            self.total.cache_creation += u.cache_creation;
            self.total.cache_read += u.cache_read;
            self.total.output += u.output;
        }
        if self.tools > 0 {
            out.push(Out::Finish {
                reason: "tool_calls".into(),
                usage: self.total,
            });
        }
    }

    fn result(&mut self, v: &Value, now: u64, out: &mut Vec<Out>) {
        let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
            || subtype.starts_with("error");
        if is_error {
            let mut text = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(errs) = v.get("errors").and_then(Value::as_array) {
                for e in errs.iter().filter_map(Value::as_str) {
                    text.push_str(if text.is_empty() { "" } else { "; " });
                    text.push_str(e);
                }
            }
            if text.is_empty() {
                text = format!("claude ended with {subtype}");
            }
            out.push(self.failure("", &text, now));
            return;
        }
        let usage = match v.get("usage") {
            Some(u) if u.is_object() => Usage::from_json(u),
            _ => self.total,
        };
        let reason = match v
            .get("stop_reason")
            .and_then(Value::as_str)
            .or(self.stop_reason.as_deref())
        {
            Some("max_tokens") => "length",
            Some("tool_use") if self.tools > 0 => "tool_calls",
            _ => "stop",
        };
        out.push(Out::Finish {
            reason: reason.into(),
            usage,
        });
    }

    /// The CLI ended without a result: its message (stderr) classified like an API error.
    pub fn exit_error(&self, text: &str, now: u64) -> Out {
        self.failure("", text, now)
    }

    /// An error from the CLI as [`Out`]: rate limits apart, then auth/overload (503: another
    /// card may take it) and the rest (502).
    fn failure(&self, code: &str, text: &str, now: u64) -> Out {
        let lower = text.to_lowercase();
        let rate = code == "rate_limit"
            || self.resets_at.is_some()
            || [
                "usage limit",
                "rate limit",
                "rate_limit",
                "hit your limit",
                "limit reached",
                "limit will reset",
                "too many requests",
                " 429",
            ]
            .iter()
            .any(|p| lower.contains(p));
        if rate {
            let at = self.resets_at.or_else(|| legacy_reset(text));
            return Out::RateLimited {
                retry_after: at.map(|t| t.saturating_sub(now).max(1)),
                message: text.to_string(),
            };
        }
        let auth = code == "authentication_failed"
            || code == "billing_error"
            || [
                "/login",
                "oauth",
                "invalid api key",
                "authentication",
                "401",
                "403",
            ]
            .iter()
            .any(|p| lower.contains(p));
        if auth {
            return Out::Error {
                status: 503,
                kind: "upstream_auth_error".into(),
                message: format!(
                    "the Claude CLI is not logged in or its token was refused: {text}"
                ),
            };
        }
        let busy = code == "server_error"
            || ["overloaded", "529", " 500", " 502", " 503", "api error"]
                .iter()
                .any(|p| lower.contains(p));
        Out::Error {
            status: if busy { 503 } else { 502 },
            kind: "upstream_error".into(),
            message: text.to_string(),
        }
    }
}

/// `Claude AI usage limit reached|1752345600` (older CLIs): the reset as Unix seconds.
fn legacy_reset(text: &str) -> Option<u64> {
    let (_, t) = text.rsplit_once('|')?;
    t.trim().parse().ok()
}

/// `mcp__c__get_weather` -> `get_weather`.
pub fn client_tool_name(n: &str) -> String {
    n.strip_prefix(TOOL_PREFIX).unwrap_or(n).to_string()
}

/// A tool call being assembled.
#[derive(Debug, Clone, Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

/// The non-streaming answer, built from [`Out`] events.
#[derive(Debug, Default)]
pub struct Collector {
    text: String,
    reasoning: String,
    calls: Vec<Call>,
    pub finish: Option<(String, Usage)>,
}

impl Collector {
    pub fn push(&mut self, o: &Out) {
        match o {
            Out::Text(t) => self.text.push_str(t),
            Out::Reasoning(t) => self.reasoning.push_str(t),
            Out::ToolStart { index, id, name } => {
                if self.calls.len() <= *index {
                    self.calls.resize(index + 1, Call::default());
                }
                self.calls[*index].id = id.clone();
                self.calls[*index].name = name.clone();
            }
            Out::ToolArgs { index, json } => {
                if let Some(c) = self.calls.get_mut(*index) {
                    c.args.push_str(json);
                }
            }
            Out::Finish { reason, usage } => self.finish = Some((reason.clone(), *usage)),
            _ => {}
        }
    }

    /// `chat.completion`.
    pub fn completion(&self, id: &str, created: i64, model: &str) -> Value {
        let (reason, usage) = self
            .finish
            .clone()
            .unwrap_or(("stop".into(), Usage::default()));
        let mut msg = Map::new();
        msg.insert("role".into(), json!("assistant"));
        msg.insert(
            "content".into(),
            if self.text.is_empty() && !self.calls.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !self.reasoning.is_empty() {
            msg.insert("reasoning_content".into(), json!(self.reasoning));
        }
        if !self.calls.is_empty() {
            let calls: Vec<Value> = self
                .calls
                .iter()
                .map(|c| {
                    json!({"id": c.id, "type": "function",
                           "function": {"name": c.name,
                                        "arguments": if c.args.is_empty() { "{}" } else { &c.args }}})
                })
                .collect();
            msg.insert("tool_calls".into(), json!(calls));
        }
        json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "message": Value::Object(msg), "finish_reason": reason}],
            "usage": usage.to_openai(),
        })
    }
}

/// One `chat.completion.chunk` SSE event for an [`Out`] (None = nothing to send).
/// `Finish` carries the usage too, so logs and clients that ignore `stream_options` see it.
pub fn sse_chunk(o: &Out, id: &str, created: i64, model: &str) -> Option<String> {
    let (delta, finish, usage) = match o {
        Out::Text(t) => (json!({"content": t}), Value::Null, None),
        Out::Reasoning(t) => (json!({"reasoning_content": t}), Value::Null, None),
        Out::ToolStart { index, id, name } => (
            json!({"tool_calls": [{"index": index, "id": id, "type": "function",
                                    "function": {"name": name, "arguments": ""}}]}),
            Value::Null,
            None,
        ),
        Out::ToolArgs { index, json } => (
            json!({"tool_calls": [{"index": index, "function": {"arguments": json}}]}),
            Value::Null,
            None,
        ),
        Out::Finish { reason, usage } => (json!({}), json!(reason), Some(usage.to_openai())),
        Out::RateLimited { message, .. } => {
            return Some(sse_error(429, "rate_limit_error", message));
        }
        Out::Error {
            status,
            kind,
            message,
        } => return Some(sse_error(*status, kind, message)),
    };
    let mut c = json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(u) = usage {
        c["usage"] = u;
    }
    Some(format!("data: {c}\n\n"))
}

/// The opening chunk (`role: assistant`).
pub fn sse_role(id: &str, created: i64, model: &str) -> String {
    let c = json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
    });
    format!("data: {c}\n\n")
}

/// `stream_options.include_usage`: the extra chunk with empty choices.
pub fn sse_usage(usage: Usage, id: &str, created: i64, model: &str) -> String {
    let c = json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [], "usage": usage.to_openai(),
    });
    format!("data: {c}\n\n")
}

fn sse_error(status: u16, kind: &str, message: &str) -> String {
    let e = json!({"error": {"message": message, "type": kind, "param": null,
                             "code": status.to_string()}});
    format!("data: {e}\n\n")
}
