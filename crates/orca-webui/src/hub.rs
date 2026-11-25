//! The session hub (port of the SignalR `SessionHub`, decision D4): JSON messages over one
//! websocket at `/hub/ws`, with the hub's method names and payloads.
//!
//! Client to server:
//! * `{"type":"invoke","id":"7","method":"SendPrompt","args":[pid,"text"]}`
//! * `{"type":"ack","n":42}`: every message up to 42 arrived (the server may forget them).
//! * `{"type":"close"}`: the client stops; the connection state is dropped at once.
//!
//! Server to client (each numbered `n`, so a reconnect can resume):
//! * `{"type":"result","id":"7","result":...}` or `{"type":"result","id":"7","error":"..."}`
//! * `{"type":"event","method":"events","args":[pid,[...]]}` (also `markers`, `files`, `projects`)
//!
//! Unnumbered: `{"type":"welcome","connectionId":"...","resumed":bool}` first, and `{"type":"ping"}`.
//!
//! Reconnect with resume (decision D66): the server keeps each connection's numbered messages
//! for a grace period after its socket drops. A client reconnecting with
//! `?resume=<connectionId>&last=<n>` gets every message after `n` replayed (results of calls
//! made before the drop included) and `resumed: true`; otherwise a fresh connection
//! (`resumed: false`), and the client reloads its snapshots as after a SignalR reconnect.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use orca_core::live::{LiveEvent, Subscription};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::audit::{AuditLog, Caller};
use crate::live::LiveSession;
use crate::projects::manager::{ProjectEvent, ProjectManager, ProjectRuntime};
use crate::util::arg_str;

pub const EVENTS_METHOD: &str = "events";
pub const MARKERS_METHOD: &str = "markers";
pub const FILES_METHOD: &str = "files";
pub const PROJECTS_METHOD: &str = "projects";

/// Most messages kept for a resume; a connection that falls further behind starts over.
const MAX_BUFFERED: usize = 20_000;
/// Maximum size of one incoming message (SignalR's MaximumReceiveMessageSize).
pub const MAX_RECEIVE_BYTES: usize = 1024 * 1024;

struct ConnOut {
    next_n: u64,
    buffer: VecDeque<(u64, String)>,
    overflowed: bool,
    sender: Option<mpsc::UnboundedSender<String>>,
    detached_at: Option<Instant>,
}

/// One client connection's state, kept across socket drops for the grace period.
pub struct Connection {
    pub id: String,
    out: Mutex<ConnOut>,
    pub caller: Caller,
    /// Signed out, disabled or its rights changed (D140): the socket closes.
    revoked: std::sync::atomic::AtomicBool,
    kick: tokio::sync::Notify,
}

impl Connection {
    fn new(caller: Caller) -> Self {
        Self {
            id: crate::util::short_id(22),
            out: Mutex::new(ConnOut {
                next_n: 1,
                buffer: VecDeque::new(),
                overflowed: false,
                sender: None,
                detached_at: None,
            }),
            caller,
            revoked: std::sync::atomic::AtomicBool::new(false),
            kick: tokio::sync::Notify::new(),
        }
    }

    fn is_revoked(&self) -> bool {
        self.revoked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number, keep and (when attached) send one message.
    pub fn send(&self, mut msg: Value) {
        let mut o = self.out.lock().unwrap();
        let n = o.next_n;
        o.next_n += 1;
        msg["n"] = json!(n);
        let text = msg.to_string();
        if o.buffer.len() >= MAX_BUFFERED {
            o.buffer.pop_front();
            o.overflowed = true;
        }
        o.buffer.push_back((n, text.clone()));
        if let Some(tx) = &o.sender
            && tx.send(text).is_err()
        {
            o.sender = None;
        }
    }

    fn ack(&self, n: u64) {
        let mut o = self.out.lock().unwrap();
        while o.buffer.front().is_some_and(|(k, _)| *k <= n) {
            o.buffer.pop_front();
        }
        // Everything up to n arrived: an overflow before it no longer matters.
        if o.buffer.front().is_none_or(|(k, _)| *k == n + 1) {
            o.overflowed = false;
        }
    }

    /// Attach a socket's sender, replaying every message after `last` first. False when
    /// messages after `last` are gone (the client must start over).
    fn attach(&self, tx: mpsc::UnboundedSender<String>, last: Option<u64>) -> bool {
        let mut o = self.out.lock().unwrap();
        if let Some(last) = last {
            let first_kept = o.buffer.front().map_or(o.next_n, |(k, _)| *k);
            if last + 1 < first_kept && (o.overflowed || last + 1 < o.next_n) {
                return false;
            }
            for (k, text) in &o.buffer {
                if *k > last {
                    let _ = tx.send(text.clone());
                }
            }
        }
        o.sender = Some(tx);
        o.detached_at = None;
        true
    }

    fn detach(&self) {
        let mut o = self.out.lock().unwrap();
        o.sender = None;
        o.detached_at = Some(Instant::now());
    }

    fn expired(&self, grace: Duration) -> bool {
        self.out
            .lock()
            .unwrap()
            .detached_at
            .is_some_and(|t| t.elapsed() > grace)
    }
}

/// Every connection, and the broadcast to all of them.
/// May this caller see this project's events (D140)?
pub type AccessFn = Arc<dyn Fn(&Caller, &str) -> bool + Send + Sync>;

pub struct Hub {
    connections: Mutex<HashMap<String, Arc<Connection>>>,
    grace: Duration,
    pumps: Mutex<HashMap<String, EventPump>>,
    access: std::sync::OnceLock<AccessFn>,
}

impl Hub {
    pub fn new(grace: Duration) -> Arc<Self> {
        let hub = Arc::new(Self {
            connections: Mutex::new(HashMap::new()),
            grace,
            pumps: Mutex::new(HashMap::new()),
            access: std::sync::OnceLock::new(),
        });
        // Forget connections whose socket stayed away longer than the grace period.
        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(hub) = weak.upgrade() else { break };
                hub.connections
                    .lock()
                    .unwrap()
                    .retain(|_, c| !c.expired(hub.grace));
            }
        });
        hub
    }

    /// `Clients.All.SendAsync(method, args...)`.
    pub fn broadcast(&self, method: &str, args: Vec<Value>) {
        let msg = json!({"type": "event", "method": method, "args": args});
        let all: Vec<_> = self.connections.lock().unwrap().values().cloned().collect();
        for c in all {
            c.send(msg.clone());
        }
    }

    /// Who sees which project's events (set once by the server).
    pub fn set_access(&self, f: AccessFn) {
        let _ = self.access.set(f);
    }

    /// `broadcast` to the connections that may see project `pid`.
    pub fn broadcast_project(&self, pid: &str, method: &str, args: Vec<Value>) {
        let msg = json!({"type": "event", "method": method, "args": args});
        let all: Vec<_> = self.connections.lock().unwrap().values().cloned().collect();
        for c in all {
            if self.access.get().is_none_or(|f| f(&c.caller, pid)) {
                c.send(msg.clone());
            }
        }
    }

    /// Close and forget the connections `pick` selects (a signed-out session, a disabled user).
    pub fn drop_where(&self, pick: impl Fn(&Caller) -> bool) {
        let gone: Vec<Arc<Connection>> = {
            let mut all = self.connections.lock().unwrap();
            let ids: Vec<String> = all
                .iter()
                .filter(|(_, c)| pick(&c.caller))
                .map(|(k, _)| k.clone())
                .collect();
            ids.iter().filter_map(|k| all.remove(k)).collect()
        };
        for c in gone {
            c.revoked.store(true, std::sync::atomic::Ordering::SeqCst);
            c.detach();
            c.kick.notify_one();
        }
    }

    pub fn connection_count(&self) -> usize {
        self.connections.lock().unwrap().len()
    }

    /// Follow the projects: one event pump per project session, plus markers, files and the list.
    pub fn attach_projects(self: &Arc<Self>, projects: &Arc<ProjectManager>) {
        let weak = Arc::downgrade(self);
        let pm = Arc::downgrade(projects);
        projects.subscribe(Arc::new(move |e| {
            let Some(hub) = weak.upgrade() else { return };
            match e {
                ProjectEvent::RuntimeCreated(rt) => hub.attach_runtime(&rt),
                ProjectEvent::Markers(pid, m) => hub.broadcast_project(
                    &pid,
                    MARKERS_METHOD,
                    vec![json!(pid), serde_json::to_value(m).unwrap_or(Value::Null)],
                ),
                ProjectEvent::Files(pid, folder, dirs) => hub.broadcast_project(
                    &pid,
                    FILES_METHOD,
                    vec![json!(pid), json!(folder), json!(dirs)],
                ),
                ProjectEvent::ListChanged => {
                    // Pumps of closed projects stop; the clients reload the list.
                    if let Some(pm) = pm.upgrade() {
                        let live: Vec<String> =
                            pm.created().iter().map(|r| r.id().to_string()).collect();
                        hub.pumps
                            .lock()
                            .unwrap()
                            .retain(|k, _| live.iter().any(|l| l == k));
                    }
                    hub.broadcast(PROJECTS_METHOD, vec![]);
                }
            }
        }));
        for rt in projects.created() {
            self.attach_runtime(&rt);
        }
    }

    fn attach_runtime(self: &Arc<Self>, rt: &Arc<ProjectRuntime>) {
        let pump = EventPump::start(
            rt.id().to_string(),
            rt.session.clone(),
            Arc::downgrade(self),
        );
        self.pumps.lock().unwrap().insert(rt.id().to_string(), pump);
    }

    pub fn stop(&self) {
        self.pumps.lock().unwrap().clear();
        let all: Vec<_> = self.connections.lock().unwrap().drain().collect();
        for (_, c) in all {
            c.detach();
        }
    }

    /// Serve one websocket: attach (fresh or resumed), then read invocations until it closes.
    pub async fn serve_socket(
        self: Arc<Self>,
        socket: WebSocket,
        state: Arc<crate::server::AppState>,
        resume: Option<String>,
        last: Option<u64>,
        caller: Caller,
    ) {
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        // Only the same user (and browser session) can resume a connection.
        let existing = resume
            .and_then(|id| self.connections.lock().unwrap().get(&id).cloned())
            .filter(|c| {
                c.caller.uid == caller.uid && c.caller.sid == caller.sid && !c.is_revoked()
            });
        let (conn, resumed) = match existing {
            Some(c) if c.attach(tx.clone(), last.or(Some(0))) => (c, true),
            _ => {
                let c = Arc::new(Connection::new(caller));
                c.attach(tx.clone(), None);
                self.connections
                    .lock()
                    .unwrap()
                    .insert(c.id.clone(), c.clone());
                (c, false)
            }
        };
        let welcome =
            json!({"type": "welcome", "connectionId": conn.id, "resumed": resumed}).to_string();
        if sink.send(Message::Text(welcome.into())).await.is_err() {
            conn.detach();
            return;
        }
        // Writer: numbered messages, plus a ping every 15 seconds.
        let writer = tokio::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(15));
            ping.tick().await;
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(text) => {
                            if sink.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    _ = ping.tick() => {
                        if sink.send(Message::Text(r#"{"type":"ping"}"#.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = sink.close().await;
        });
        let mut closed_by_client = false;
        loop {
            if conn.is_revoked() {
                break;
            }
            let msg = tokio::select! {
                m = stream.next() => match m {
                    Some(Ok(m)) => m,
                    _ => break,
                },
                _ = conn.kick.notified() => continue,
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match v["type"].as_str() {
                Some("invoke") => {
                    let id = v["id"].clone();
                    let method = v["method"].as_str().unwrap_or_default().to_string();
                    let args = v["args"].as_array().cloned().unwrap_or_default();
                    let (conn, state) = (conn.clone(), state.clone());
                    tokio::spawn(async move {
                        let reply = match dispatch(&state, &conn.caller, &method, &args).await {
                            Ok(result) => json!({"type": "result", "id": id, "result": result}),
                            Err(error) => json!({"type": "result", "id": id, "error": error}),
                        };
                        conn.send(reply);
                    });
                }
                Some("ack") => {
                    if let Some(n) = v["n"].as_u64() {
                        conn.ack(n);
                    }
                }
                Some("close") => {
                    closed_by_client = true;
                    break;
                }
                _ => {}
            }
        }
        // Only this socket's sender is dropped; a newer socket may already have taken over.
        {
            let mut o = conn.out.lock().unwrap();
            if o.sender.as_ref().is_some_and(|s| s.same_channel(&tx)) {
                o.sender = None;
                o.detached_at = Some(Instant::now());
            }
        }
        drop(tx);
        if closed_by_client {
            self.connections.lock().unwrap().remove(&conn.id);
        }
        let _ = writer.await;
    }
}

/// Forwards one project's bus events to every connected browser: the bus handler only
/// enqueues; a single task sends batches in order (a token stream becomes a few messages a frame).
pub struct EventPump {
    _subscription: Subscription,
    task: tokio::task::JoinHandle<()>,
}

impl EventPump {
    fn start(project_id: String, session: Arc<dyn LiveSession>, hub: std::sync::Weak<Hub>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<LiveEvent>();
        let sub = session.bus().subscribe(move |ev| {
            let _ = tx.send(ev.clone());
        });
        let task = tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                // A short pause lets a burst of tokens ride in one message.
                tokio::time::sleep(Duration::from_millis(15)).await;
                let mut batch = vec![first];
                while batch.len() < 500 {
                    match rx.try_recv() {
                        Ok(ev) => batch.push(ev),
                        Err(_) => break,
                    }
                }
                let Some(hub) = hub.upgrade() else { break };
                hub.broadcast_project(
                    &project_id,
                    EVENTS_METHOD,
                    vec![
                        json!(project_id),
                        serde_json::to_value(&batch).unwrap_or(Value::Null),
                    ],
                );
            }
        });
        Self {
            _subscription: sub,
            task,
        }
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn session_of(state: &crate::server::AppState, pid: &str) -> Result<Arc<ProjectRuntime>, String> {
    state
        .projects
        .get(pid)
        .ok_or_else(|| "No such project.".to_string())
}

/// The session, for driving it: not a bench run's read-only tab.
fn driven(state: &crate::server::AppState, pid: &str) -> Result<Arc<ProjectRuntime>, String> {
    let rt = session_of(state, pid)?;
    if rt.read_only {
        return Err("This is a bench run: it can be watched, not driven.".into());
    }
    Ok(rt)
}

fn to_json<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// `SessionHub`'s methods. Errors are the HubException messages.
pub async fn dispatch(
    state: &crate::server::AppState,
    caller: &Caller,
    method: &str,
    args: &[Value],
) -> Result<Value, String> {
    let audit: &AuditLog = &state.audit;
    let pid = arg_str(args, 0).unwrap_or_default();
    // Another user's project is "no such project" (D140).
    if let Some(info) = state.projects.info(&pid)
        && !caller.can_access(info.owner)
    {
        return Err("No such project.".into());
    }
    match method {
        "GetSnapshot" => {
            let rt = session_of(state, &pid)?;
            // A folder this process can't read fails with a generic error; say what's wrong.
            if let Some(problem) = crate::paths::read_problem(&rt.root()) {
                return Err(problem);
            }
            rt.session
                .get_snapshot()
                .await
                .map(|s| crate::live::snapshot_json(&s))
                .map_err(|e| format!("This project's session failed to start: {e}"))
        }
        "SendPrompt" => {
            let rt = driven(state, &pid)?;
            let text = arg_str(args, 1).unwrap_or_default();
            audit.write(
                Some(caller),
                "prompt",
                json!({"project": pid, "text": AuditLog::clip(&text, 200)}),
            );
            Ok(to_json(rt.session.submit_prompt(&text, "web").await))
        }
        "SendPromptWithAttachments" => {
            let rt = driven(state, &pid)?;
            let text = arg_str(args, 1).unwrap_or_default();
            let attachments: Vec<String> = args
                .get(2)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let root = rt.root();
            let mut lines = Vec::new();
            for a in &attachments {
                match crate::paths::resolve(&root, Some(a)) {
                    Some(full) if full.is_file() => {
                        lines.push(crate::paths::to_relative(&root, &full))
                    }
                    _ => {
                        return Ok(json!(format!(
                            "An attachment is missing or outside the project: {a}"
                        )));
                    }
                }
            }
            let prompt = compose_attachments(&text, &lines);
            audit.write(
                Some(caller),
                "prompt",
                json!({"project": pid, "text": AuditLog::clip(&text, 200), "attachments": lines.len()}),
            );
            Ok(to_json(rt.session.submit_prompt(&prompt, "web").await))
        }
        "SetEngine" => {
            let rt = driven(state, &pid)?;
            let engine = arg_str(args, 1).unwrap_or_default();
            if !matches!(engine.as_str(), "native" | "claude" | "codex") {
                return Err(format!("Unknown engine: {engine}"));
            }
            let result = rt.session.set_engine(&engine).await;
            if result.ok {
                state.projects.engines.set(&pid, &result.engine);
            }
            audit.write(
                Some(caller),
                "engine",
                json!({"project": pid, "engine": engine, "ok": result.ok, "login": result.auth.is_some()}),
            );
            Ok(to_json(result))
        }
        "AnswerQuestion" => {
            let rt = driven(state, &pid)?;
            let id = arg_str(args, 1).unwrap_or_default();
            let answer = arg_str(args, 2).unwrap_or_default();
            audit.write(
                Some(caller),
                "answer",
                json!({"project": pid, "question": id}),
            );
            Ok(json!(rt.session.answer_question(&id, &answer).await))
        }
        "GetAgentTranscript" => {
            let rt = session_of(state, &pid)?;
            let agent = arg_str(args, 1).unwrap_or_default();
            Ok(to_json(rt.session.get_agent_transcript(&agent).await))
        }
        "SendAgentMessage" => {
            let rt = driven(state, &pid)?;
            let agent = arg_str(args, 1).unwrap_or_default();
            let text = arg_str(args, 2).unwrap_or_default();
            audit.write(
                Some(caller),
                "agent.message",
                json!({"project": pid, "agent": agent, "text": AuditLog::clip(&text, 200)}),
            );
            Ok(to_json(rt.session.send_agent_message(&agent, &text).await))
        }
        "CancelAgent" => {
            let rt = driven(state, &pid)?;
            let agent = arg_str(args, 1).unwrap_or_default();
            audit.write(
                Some(caller),
                "agent.cancel",
                json!({"project": pid, "agent": agent}),
            );
            Ok(json!(rt.session.cancel_agent(&agent).await))
        }
        "Cancel" => {
            let rt = driven(state, &pid)?;
            Ok(json!(rt.session.cancel().await))
        }
        "ResolvePermission" => {
            let rt = driven(state, &pid)?;
            let id = arg_str(args, 1).unwrap_or_default();
            let decision = arg_str(args, 2).unwrap_or_default();
            Ok(json!(rt.session.resolve_permission(&id, &decision).await))
        }
        _ => Err(format!("Unknown hub method '{method}'.")),
    }
}

/// `ImageFiles.IsImage`: an image extension (case-insensitive).
pub fn is_image(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"))
}

/// `WebPromptAttachments.Compose`: the prompt text, then the attached files to open.
pub fn compose_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let note = if paths.iter().any(|p| is_image(p)) {
        " (images: view or inspect them with your tools)"
    } else {
        ""
    };
    let list: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
    let head = if text.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", text.trim_end())
    };
    format!("{head}Attached files{note}:\n{}", list.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PromptAttachmentsTests.ListsTheAttachedPathsAfterTheText`.
    #[test]
    fn lists_the_attached_paths_after_the_text() {
        assert_eq!(compose_attachments("hello", &[]), "hello");
        assert_eq!(
            compose_attachments(
                "look  ",
                &[".orca/attachments/a.png".into(), "notes.txt".into()]
            ),
            "look\n\nAttached files (images: view or inspect them with your tools):\n- .orca/attachments/a.png\n- notes.txt"
        );
        assert_eq!(
            compose_attachments("", &["notes.txt".into()]),
            "Attached files:\n- notes.txt"
        );
    }
}
