//! The Claude engine's sessions: one `claude -p` per request, stream-json both ways.
//!
//! - **Pool:** at most `maxSessions` CLIs at once, the rest queue up to `queueSeconds` (then
//!   429 + Retry-After). Processes are spawned on demand and end with their request; none is
//!   kept idle, so a large limit costs nothing until it is used.
//! - **Plain model:** `--tools ""` (Claude Code's own tools off), `--permission-mode dontAsk`,
//!   `--safe-mode` (none of the operator's CLAUDE.md, hooks, plugins or MCP servers), the
//!   caller's system prompt from a file in place of Claude Code's, no session persistence, an
//!   empty working directory. `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` are removed from the
//!   CLI's environment so it always uses the subscription login (or `oauthTokenFile`).
//! - **Client tools:** offered to Claude as an in-process MCP server over the CLI's control
//!   protocol (`--mcp-config` with `"type": "sdk"`): Claude sees the real JSON schemas and
//!   answers with real tool_use blocks, which become OpenAI tool_calls. The CLI is stopped at
//!   the end of that message, before it would call the tool, so the client runs it.
//! - **Cancellation:** the CLI is killed when the client goes away (the answer's receiver is
//!   dropped), at the timeout, and when the answer is complete.
//! - **Rate limits:** a rejected request marks the subscription limited until its reset;
//!   requests then wait up to `rateLimitWaitSeconds` for it, else get 429 + Retry-After.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Notify, mpsc};

use super::convert::{self, Collector, Out, Prepared, TOOL_SERVER, Translator};
use super::{ClaudeConfig, Outcome, error_response, failed};
use crate::config::Snapshot;
use crate::state::AppState;

/// Engine state kept across config reloads (in `Extensions`).
#[derive(Default)]
pub struct Engine {
    gate: Gate,
    limited_until: Mutex<Option<Instant>>,
    seq: AtomicU64,
    started: AtomicU64,
    rate_limited: AtomicU64,
    failed: AtomicU64,
    queue_full: AtomicU64,
}

/// A counting gate whose size can change with the config.
#[derive(Default)]
struct Gate {
    state: Mutex<(u32, u32)>, // (active, waiting)
    notify: Notify,
}

/// A session slot, held by the session task until its CLI has stopped.
struct OwnedPermit(Arc<AppState>);

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        let g = &self.0.ext.claude.gate;
        g.state.lock().unwrap().0 -= 1;
        g.notify.notify_one();
    }
}

impl Gate {
    /// Take a slot (true), or give up at `deadline` (false). The caller wraps a taken slot
    /// in an [`OwnedPermit`].
    async fn acquire(&self, max: u32, deadline: Instant) -> bool {
        let mut waiting = false;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = self.state.lock().unwrap();
                if s.0 < max {
                    s.0 += 1;
                    if waiting {
                        s.1 -= 1;
                    }
                    return true;
                }
                if !waiting {
                    s.1 += 1;
                    waiting = true;
                }
            }
            if tokio::time::timeout_at(deadline.into(), notified)
                .await
                .is_err()
            {
                self.state.lock().unwrap().1 -= 1;
                // pass a wake-up we may have taken on to the next waiter
                self.notify.notify_one();
                return false;
            }
        }
    }
}

impl Engine {
    /// `(active, queued)`.
    pub fn load(&self) -> (u32, u32) {
        *self.gate.state.lock().unwrap()
    }

    fn limited_for(&self) -> Option<Duration> {
        let g = self.limited_until.lock().unwrap();
        g.and_then(|t| t.checked_duration_since(Instant::now()))
    }

    fn limit(&self, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs.max(1));
        let mut g = self.limited_until.lock().unwrap();
        if g.is_none_or(|t| t < until) {
            *g = Some(until);
        }
    }

    pub fn metrics_text(&self) -> String {
        let (active, queued) = self.load();
        let limited = self.limited_for().map_or(0, |d| d.as_secs());
        format!(
            "# TYPE abhoth_claude_sessions gauge\nabhoth_claude_sessions {active}\n\
             # TYPE abhoth_claude_queued gauge\nabhoth_claude_queued {queued}\n\
             # TYPE abhoth_claude_rate_limited_seconds gauge\nabhoth_claude_rate_limited_seconds {limited}\n\
             # TYPE abhoth_claude_sessions_started_total counter\nabhoth_claude_sessions_started_total {}\n\
             # TYPE abhoth_claude_rate_limited_total counter\nabhoth_claude_rate_limited_total {}\n\
             # TYPE abhoth_claude_failed_total counter\nabhoth_claude_failed_total {}\n\
             # TYPE abhoth_claude_queue_full_total counter\nabhoth_claude_queue_full_total {}\n",
            self.started.load(Ordering::Relaxed),
            self.rate_limited.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.queue_full.load(Ordering::Relaxed),
        )
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rate_limited(st: &AppState, secs: Option<u64>, message: &str) -> Outcome {
    let secs = secs.unwrap_or(60).max(1);
    st.ext.claude.limit(secs);
    st.ext.claude.rate_limited.fetch_add(1, Ordering::Relaxed);
    let msg = format!("the Claude subscription is rate limited; retry in {secs} s ({message})");
    failed(
        "claude: rate limited",
        Some(error_response(429, &msg, "rate_limit_error", Some(secs))),
    )
}

/// Run one chat (see [`super::send`]).
pub async fn send(
    st: &Arc<AppState>,
    snap: &Snapshot,
    _card: &str,
    body: &Map<String, Value>,
) -> Outcome {
    let cfg = &snap.config.claude;
    let eng = &st.ext.claude;
    let p = match convert::prepare(body) {
        Ok(p) => p,
        Err(e) => return Ok(error_response(400, &e, "invalid_request_error", None)),
    };
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_seconds.max(1));

    // backpressure: wait out a short rate limit, refuse a long one
    if let Some(left) = eng.limited_for() {
        if left.as_secs() > cfg.rate_limit_wait_seconds {
            return rate_limited(st, Some(left.as_secs() + 1), "limit not reset yet");
        }
        tokio::time::sleep(left).await;
    }

    let queue_deadline = Instant::now() + Duration::from_secs(cfg.queue_seconds);
    if !eng
        .gate
        .acquire(cfg.max_sessions.max(1), queue_deadline)
        .await
    {
        eng.queue_full.fetch_add(1, Ordering::Relaxed);
        let msg = format!(
            "all {} Claude sessions are busy; retry shortly",
            cfg.max_sessions
        );
        return failed(
            "claude: all sessions busy",
            Some(error_response(429, &msg, "rate_limit_error", Some(5))),
        );
    }
    // the session task owns the slot from here (released when its CLI has stopped)
    let owned = OwnedPermit(st.clone());

    let id = format!(
        "chatcmpl-{:x}{:x}",
        now_unix(),
        eng.seq.fetch_add(1, Ordering::Relaxed)
    );
    let (tx, mut rx) = mpsc::channel::<Out>(256);
    let model_slot = Arc::new(Mutex::new(None::<String>));
    let session = match Session::spawn(cfg, &p, &id) {
        Ok(s) => s,
        Err(e) => {
            eng.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("claude: could not start `{}`: {e}", cfg.command);
            let msg = format!("could not start the Claude CLI ({}): {e}", cfg.command);
            return failed(
                format!("claude: {e}"),
                Some(error_response(503, &msg, "no_upstream", None)),
            );
        }
    };
    eng.started.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(session.run(tx, deadline, owned, model_slot.clone()));

    // the first event decides the status: an error before any content is an HTTP error
    let mut first: Vec<Out> = Vec::new();
    loop {
        let Some(o) = rx.recv().await else {
            eng.failed.fetch_add(1, Ordering::Relaxed);
            return Ok(error_response(
                502,
                "the Claude CLI ended without an answer",
                "upstream_error",
                None,
            ));
        };
        match o {
            Out::RateLimited {
                retry_after,
                message,
            } => return rate_limited(st, retry_after, &message),
            Out::Error {
                status,
                kind,
                message,
            } => {
                eng.failed.fetch_add(1, Ordering::Relaxed);
                let r = error_response(status, &message, &kind, None);
                return if matches!(status, 502 | 503) {
                    failed(format!("claude: {message}"), Some(r))
                } else {
                    Ok(r)
                };
            }
            o => {
                let end = o.is_end();
                first.push(o);
                if end || p.stream {
                    break;
                }
            }
        }
    }
    let model = model_slot
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| p.model.clone());
    let created = now_unix() as i64;

    if !p.stream {
        let mut c = Collector::default();
        for o in &first {
            c.push(o);
        }
        while c.finish.is_none() {
            match rx.recv().await {
                Some(Out::RateLimited {
                    retry_after,
                    message,
                }) => return rate_limited(st, retry_after, &message),
                Some(Out::Error {
                    status,
                    kind,
                    message,
                }) => {
                    eng.failed.fetch_add(1, Ordering::Relaxed);
                    return Ok(error_response(status, &message, &kind, None));
                }
                Some(o) => c.push(&o),
                None => {
                    eng.failed.fetch_add(1, Ordering::Relaxed);
                    return Ok(error_response(
                        502,
                        "the Claude CLI ended mid-answer",
                        "upstream_error",
                        None,
                    ));
                }
            }
        }
        let v = c.completion(&id, created, &model);
        let r = axum::http::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(reqwest::Body::from(v.to_string()))
            .expect("static response parts");
        return Ok(reqwest::Response::from(r));
    }

    // streaming: the role chunk, what came already, then the rest as it arrives
    let include_usage = p.include_usage;
    let head = convert::sse_role(&id, created, &model);
    let mut pre = head;
    let mut ended = false;
    for o in &first {
        pre.push_str(&tail_for(o, &id, created, &model, include_usage));
        ended |= o.is_end();
    }
    if ended {
        pre.push_str("data: [DONE]\n\n");
    }
    let st2 = st.clone();
    let rest =
        futures_util::stream::unfold((rx, ended, id, model), move |(mut rx, ended, id, model)| {
            let st2 = st2.clone();
            async move {
                if ended {
                    return None;
                }
                let o = rx.recv().await;
                let (chunk, end) = match &o {
                    Some(o) => {
                        if let Out::RateLimited { retry_after, .. } = o {
                            st2.ext.claude.limit(retry_after.unwrap_or(60));
                            st2.ext.claude.rate_limited.fetch_add(1, Ordering::Relaxed);
                        }
                        if matches!(o, Out::Error { .. }) {
                            st2.ext.claude.failed.fetch_add(1, Ordering::Relaxed);
                        }
                        let mut s = tail_for(o, &id, created, &model, include_usage);
                        if o.is_end() {
                            s.push_str("data: [DONE]\n\n");
                        }
                        (s, o.is_end())
                    }
                    None => (
                        format!(
                            "{}data: [DONE]\n\n",
                            convert::sse_chunk(
                                &Out::Error {
                                    status: 502,
                                    kind: "upstream_error".into(),
                                    message: "the Claude CLI ended mid-answer".into(),
                                },
                                &id,
                                created,
                                &model
                            )
                            .unwrap_or_default()
                        ),
                        true,
                    ),
                };
                Some((Bytes::from(chunk), (rx, end, id, model)))
            }
        });
    let body = futures_util::stream::once(async move { Bytes::from(pre) })
        .chain(rest)
        .map(Ok::<Bytes, std::io::Error>);
    let r = axum::http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(reqwest::Body::wrap_stream(body))
        .expect("static response parts");
    Ok(reqwest::Response::from(r))
}

/// The SSE text for one event (plus the include_usage chunk after the finish).
fn tail_for(o: &Out, id: &str, created: i64, model: &str, include_usage: bool) -> String {
    let mut s = convert::sse_chunk(o, id, created, model).unwrap_or_default();
    if let (Out::Finish { usage, .. }, true) = (o, include_usage) {
        s.push_str(&convert::sse_usage(*usage, id, created, model));
    }
    s
}

/// The engine's directory: `work/` (the CLI's empty cwd) and `prompts/` (system prompt files).
fn base_dir(cfg: &ClaudeConfig) -> PathBuf {
    cfg.work_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("abhoth-claude"))
}

/// Deletes the request's system prompt file.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    o.open(path)?.write_all(text.as_bytes())
}

/// The CLI's arguments for a request (no secrets: the token goes in the environment).
pub fn cli_args(cfg: &ClaudeConfig, p: &Prepared, prompt_file: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
        "--tools",
        "",
        "--disable-slash-commands",
        "--strict-mcp-config",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if cfg.safe_mode {
        a.push("--safe-mode".into());
    }
    a.push("--model".into());
    a.push(p.model.clone());
    a.push("--system-prompt-file".into());
    a.push(prompt_file.to_string_lossy().into_owned());
    if let Some(e) = &p.effort {
        a.push("--effort".into());
        a.push(e.clone());
    }
    a.push("--mcp-config".into());
    if p.tools.is_empty() {
        a.push(json!({"mcpServers": {}}).to_string());
    } else {
        a.push(
            json!({"mcpServers": {TOOL_SERVER: {"type": "sdk", "name": TOOL_SERVER}}}).to_string(),
        );
        a.push("--allowedTools".into());
        a.push(format!("mcp__{TOOL_SERVER}"));
    }
    a.extend(cfg.extra_args.iter().cloned());
    a
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    tools: Vec<Value>,
    stderr: Arc<Mutex<String>>,
    token: Option<String>,
    /// Control handshake and the user turn, written when the session runs.
    pending: Option<String>,
    _prompt: TempFile,
}

impl Session {
    fn spawn(cfg: &ClaudeConfig, p: &Prepared, id: &str) -> std::io::Result<Session> {
        let base = base_dir(cfg);
        let work = base.join("work");
        let prompts = base.join("prompts");
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&prompts)?;
        let prompt_path = prompts.join(format!("{id}.txt"));
        let system = p
            .system
            .clone()
            .unwrap_or_else(|| cfg.default_system_prompt.clone());
        write_private(&prompt_path, &system)?;
        let prompt = TempFile(prompt_path);

        let token = match cfg.oauth_token_file.as_deref() {
            Some(f) => {
                let t = std::fs::read_to_string(f).map_err(|e| {
                    std::io::Error::new(e.kind(), format!("oauthTokenFile {f}: {e}"))
                })?;
                Some(t.trim().to_string()).filter(|t| !t.is_empty())
            }
            None => None,
        };

        let mut cmd = Command::new(&cfg.command);
        cmd.args(cli_args(cfg, p, &prompt.0))
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // the subscription, never an API key that happens to be in the router's env
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            // not a nested Claude Code session (when the router runs under one)
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        if let Some(n) = p.max_tokens {
            cmd.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", n.to_string());
        }
        if let Some(t) = &token {
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", t);
        }
        if let Some(h) = cfg.home.as_deref().filter(|h| !h.is_empty()) {
            std::fs::create_dir_all(h)
                .map_err(|e| std::io::Error::new(e.kind(), format!("home {h}: {e}")))?;
            // HOME alone decides where the CLI keeps its files
            cmd.env("HOME", h).env_remove("CLAUDE_CONFIG_DIR");
        }
        #[cfg(windows)]
        {
            // no console window per request
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let buf = stderr.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 2048];
                while let Ok(n) = err.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock().unwrap();
                    b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if b.len() > 4096 {
                        let cut = b.len() - 4096;
                        let cut = (cut..b.len()).find(|i| b.is_char_boundary(*i)).unwrap_or(0);
                        b.drain(..cut);
                    }
                }
            });
        }
        let mut s = Session {
            child,
            stdin,
            tools: p.tools.clone(),
            stderr,
            token,
            pending: None,
            _prompt: prompt,
        };
        // what goes in first: the control handshake (only with client tools: their MCP server
        // lives on this side of the control protocol), then the one user turn
        let mut input = String::new();
        if !s.tools.is_empty() {
            input.push_str(
                &json!({"type": "control_request", "request_id": "init",
                        "request": {"subtype": "initialize", "hooks": null}})
                .to_string(),
            );
            input.push('\n');
        }
        input.push_str(
            &json!({"type": "user", "session_id": "",
                    "message": {"role": "user", "content": p.content},
                    "parent_tool_use_id": null})
            .to_string(),
        );
        input.push('\n');
        s.pending = Some(input);
        Ok(s)
    }

    /// Drive the CLI to the end of the answer, passing events to `tx`; then stop it.
    async fn run(
        mut self,
        tx: mpsc::Sender<Out>,
        deadline: Instant,
        _permit: OwnedPermit,
        model: Arc<Mutex<Option<String>>>,
    ) {
        let Some(stdout) = self.child.stdout.take() else {
            let _ = self.child.start_kill();
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        if let Some(mut w) = self.stdin.take() {
            let input = self.pending.take().unwrap_or_default();
            if w.write_all(input.as_bytes()).await.is_ok() && w.flush().await.is_ok() {
                // with client tools stdin stays open for the MCP replies; without, EOF now
                // tells the CLI there is nothing more to come
                if !self.tools.is_empty() {
                    self.stdin = Some(w);
                }
            }
        }
        let mut tr = Translator::default();
        'read: loop {
            tokio::select! {
                _ = tx.closed() => break 'read, // the client went away
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let _ = tx.send(Out::Error {
                        status: 504,
                        kind: "timeout".into(),
                        message: "the Claude CLI did not finish in time".into(),
                    }).await;
                    break 'read;
                }
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                        if v.get("type").and_then(Value::as_str) == Some("control_request") {
                            self.control(&v).await;
                            continue;
                        }
                        let outs = tr.push(&v, now_unix());
                        if let Some(m) = &tr.model {
                            *model.lock().unwrap() = Some(m.clone());
                        }
                        for o in outs {
                            if tx.send(o).await.is_err() {
                                break 'read;
                            }
                        }
                        if tr.done() {
                            break 'read;
                        }
                    }
                    _ => {
                        let status =
                            tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
                        let code = match status {
                            Ok(Ok(s)) => s.to_string(),
                            _ => "still running".into(),
                        };
                        let err = self.stderr_tail();
                        let msg = if err.is_empty() {
                            format!("the Claude CLI ended ({code}) without an answer")
                        } else {
                            format!("the Claude CLI ended ({code}) without an answer: {err}")
                        };
                        let _ = tx.send(tr.exit_error(&msg, now_unix())).await;
                        break 'read;
                    }
                }
            }
        }
        drop(self.stdin.take());
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    /// The CLI's stderr tail, one line, with the token (if any) removed.
    fn stderr_tail(&self) -> String {
        let mut s = self
            .stderr
            .lock()
            .unwrap()
            .trim()
            .replace(['\r', '\n'], " ");
        if let Some(t) = &self.token
            && t.len() >= 8
        {
            s = s.replace(t.as_str(), "[token]");
        }
        if s.len() > 600 {
            let cut = (s.len() - 600..s.len())
                .find(|i| s.is_char_boundary(*i))
                .unwrap_or(0);
            s.drain(..cut);
        }
        s
    }

    /// Answer a control request from the CLI (the client tools' MCP server; nothing else).
    async fn control(&mut self, v: &Value) {
        let rid = v.get("request_id").cloned().unwrap_or(Value::Null);
        let req = v.get("request").unwrap_or(&Value::Null);
        let resp = match req.get("subtype").and_then(Value::as_str).unwrap_or("") {
            "mcp_message" => {
                let reply = mcp_reply(&self.tools, req.get("message").unwrap_or(&Value::Null));
                json!({"subtype": "success", "request_id": rid,
                       "response": {"mcp_response": reply}})
            }
            // never expected (the tools are allowed up front); the client runs tools, not us
            "can_use_tool" => json!({"subtype": "success", "request_id": rid,
                "response": {"behavior": "deny", "message": "the client runs this tool"}}),
            other => json!({"subtype": "error", "request_id": rid,
                "error": format!("not supported here: {other}")}),
        };
        if let Some(w) = self.stdin.as_mut() {
            let line = format!(
                "{}\n",
                json!({"type": "control_response", "response": resp})
            );
            let _ = w.write_all(line.as_bytes()).await;
            let _ = w.flush().await;
        }
    }
}

/// The client tools' MCP server (JSON-RPC in, JSON-RPC out). A `tools/call` is answered with a
/// placeholder: the session is stopped at the end of the message that asked for it, so Claude
/// never reads it; the client runs the tool and sends the result in its next request.
pub fn mcp_reply(tools: &[Value], msg: &Value) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": msg
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "abhoth-client-tools", "version": "1.0.0"},
        }),
        m if m.starts_with("notifications/") => json!({}),
        "tools/list" => json!({"tools": tools}),
        "tools/call" => json!({"content": [{"type": "text",
            "text": "This tool runs on the client; its result arrives with the next request."}]}),
        _ => {
            return json!({"jsonrpc": "2.0", "id": id,
                          "error": {"code": -32601, "message": format!("method not found: {method}")}});
        }
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
