//! Live session events (port of `OpenOrca.Core.Live`): the process-wide event stream the web UI
//! renders from, and the records it carries. `ILiveSession` itself is implemented by the CLI host
//! (phase 5); its snapshot and payload records live here so both sides share them.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One thing that happened in the live session, in order. `seq` is strictly increasing so a
/// client can merge a snapshot with live events without gaps or repeats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub kind: String,
    /// UTC time, ISO 8601.
    pub at: String,
    pub data: Value,
}

/// Event type names (the web client switches on these).
pub mod types {
    pub const TURN_START: &str = "turnStart";
    pub const TURN_END: &str = "turnEnd";
    pub const USER: &str = "user";
    pub const ASSISTANT_DELTA: &str = "assistantDelta";
    pub const THINKING_DELTA: &str = "thinkingDelta";
    pub const TOOL_CALL: &str = "toolCall";
    pub const TOOL_RESULT: &str = "toolResult";
    pub const NOTICE: &str = "notice";
    pub const BUSY: &str = "busy";
    pub const AGENTS: &str = "agents";
    pub const SLOTS: &str = "slots";
    pub const PERMISSION: &str = "permission";
    pub const PERMISSION_RESOLVED: &str = "permissionResolved";
    pub const SESSION: &str = "session";
    pub const ENGINE: &str = "engine";
    pub const ENGINE_AUTH: &str = "engineAuth";
    pub const QUESTION: &str = "question";
    pub const QUESTION_RESOLVED: &str = "questionResolved";
    pub const UNCENSORED: &str = "uncensored";
    pub const AGENT_ENTRY: &str = "agentEntry";
    pub const AGENT_DELTA: &str = "agentDelta";
}

/// Why a turn ended, beyond its `outcome` ("done", "error", "cancelled"): the `reason` of a
/// `turnEnd` event, when there is one. The web client picks the mascot's mood from these
/// (client/src/lib/moods.ts), never from the message text.
pub mod reasons {
    /// The server refused the credentials (HTTP 401/403; a router that refused the key).
    pub const AUTH: &str = "auth";
    /// Rate limited (HTTP 429).
    pub const RATE_LIMITED: &str = "rateLimited";
    /// Out of quota or credit (HTTP 402).
    pub const QUOTA: &str = "quota";
    /// No model is configured.
    pub const NO_MODEL: &str = "noModel";
    /// The selected engine (Claude Code, Codex) cannot run: not installed or not signed in.
    pub const UNAVAILABLE: &str = "unavailable";
    /// The model server failed (other HTTP errors, unreachable, timed out, broken stream).
    pub const SERVER: &str = "server";
    /// The engine's turn failed.
    pub const FAILED: &str = "failed";
    /// The agent stopped at its iteration limit without finishing.
    pub const ITERATION_LIMIT: &str = "iterationLimit";
}

type Handler = Arc<dyn Fn(&LiveEvent) + Send + Sync>;

const MAX_TURN_EVENTS: usize = 4000;

#[derive(Default)]
struct BusState {
    handlers: Vec<(u64, Handler)>,
    next_handler: u64,
    turn: VecDeque<(LiveEvent, bool)>,
    open_tools: HashMap<String, VecDeque<String>>,
    seq: i64,
    tool_ids: u64,
    turn_base_message_index: i64,
    in_turn: bool,
}

/// Process-wide stream of live-session events (`LiveEventBus`). Publishing is a no-op until
/// something subscribes. Events of the current turn are kept (text deltas merged) so a client
/// that connects mid-turn sees the partial answer.
pub struct LiveEventBus {
    state: Mutex<BusState>,
    active: AtomicBool,
}

impl Default for LiveEventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Unsubscribes when dropped.
pub struct Subscription {
    bus: Weak<LiveEventBus>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(bus) = self.bus.upgrade() {
            let mut s = bus.state.lock().unwrap();
            s.handlers.retain(|(id, _)| *id != self.id);
            bus.active.store(!s.handlers.is_empty(), Ordering::SeqCst);
        }
    }
}

fn now_iso() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // `DateTime.UtcNow` as System.Text.Json writes it (round-trip "O" form, UTC).
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let mut frac = format!(".{:07}", d.subsec_nanos() / 100);
    while frac.ends_with('0') {
        frac.pop();
    }
    if frac == "." {
        frac.clear();
    }
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{frac}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

impl LiveEventBus {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BusState {
                turn_base_message_index: -1,
                ..BusState::default()
            }),
            active: AtomicBool::new(false),
        }
    }

    /// The bus the app publishes to.
    pub fn shared() -> Arc<LiveEventBus> {
        static SHARED: OnceLock<Arc<LiveEventBus>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(LiveEventBus::new())).clone()
    }

    /// True while at least one subscriber listens.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn last_seq(&self) -> i64 {
        self.state.lock().unwrap().seq
    }

    /// Conversation message count when the current turn started (-1 before any).
    pub fn turn_base_message_index(&self) -> i64 {
        self.state.lock().unwrap().turn_base_message_index
    }

    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().in_turn
    }

    pub fn subscribe(
        self: &Arc<Self>,
        handler: impl Fn(&LiveEvent) + Send + Sync + 'static,
    ) -> Subscription {
        let mut s = self.state.lock().unwrap();
        s.next_handler += 1;
        let id = s.next_handler;
        s.handlers.push((id, Arc::new(handler)));
        self.active.store(true, Ordering::SeqCst);
        Subscription {
            bus: Arc::downgrade(self),
            id,
        }
    }

    /// Publish an event to every subscriber (in order, under the lock). No-op with none.
    pub fn publish(&self, kind: &str, data: Value) {
        self.publish_inner(kind, data, false);
    }

    /// A text delta (`LiveText`): consecutive ones of the same type merge in the replay buffer.
    pub fn publish_text(&self, kind: &str, text: &str) {
        self.publish_inner(kind, json!({"text": text}), true);
    }

    fn publish_inner(&self, kind: &str, data: Value, is_text: bool) {
        if !self.is_active() {
            return;
        }
        let mut s = self.state.lock().unwrap();
        s.seq += 1;
        let ev = LiveEvent {
            seq: s.seq,
            kind: kind.to_string(),
            at: now_iso(),
            data,
        };
        remember(&mut s, &ev, is_text);
        for (_, h) in &s.handlers {
            // A broken subscriber must never break the agent loop.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&ev)));
        }
    }

    /// Re-publish an event from elsewhere (a worker process), its seq shifted by `seq_offset`.
    pub fn forward(&self, ev: &LiveEvent, seq_offset: i64) {
        let mut s = self.state.lock().unwrap();
        let mut shifted = ev.clone();
        shifted.seq += seq_offset;
        if shifted.seq <= s.seq {
            return;
        }
        s.seq = shifted.seq;
        if ev.kind == types::TURN_START {
            s.turn.clear();
            s.in_turn = true;
        } else if ev.kind == types::TURN_END {
            s.in_turn = false;
        }
        if !self.is_active() {
            return;
        }
        let is_text = matches!(
            ev.kind.as_str(),
            types::ASSISTANT_DELTA | types::THINKING_DELTA
        ) && ev.data.get("text").is_some_and(Value::is_string)
            && ev.data.as_object().is_some_and(|o| o.len() == 1);
        remember(&mut s, &shifted, is_text);
        for (_, h) in &s.handlers {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&shifted)));
        }
    }

    /// Raise the sequence number to at least `seq`.
    pub fn advance_to(&self, seq: i64) -> i64 {
        let mut s = self.state.lock().unwrap();
        if seq > s.seq {
            s.seq = seq;
        }
        s.seq
    }

    /// Mark the start of a turn: the replay buffer starts over.
    pub fn publish_turn_start(&self, base_message_index: i64, source: &str) {
        if !self.is_active() {
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            s.turn.clear();
            s.open_tools.clear();
            s.turn_base_message_index = base_message_index;
            s.in_turn = true;
        }
        self.publish(types::TURN_START, json!({"source": source}));
    }

    pub fn publish_turn_end(&self, elapsed_ms: i64, tokens: i64, outcome: &str) {
        self.publish_turn_end_with(elapsed_ms, tokens, outcome, None);
    }

    /// [`Self::publish_turn_end`] with why it ended (one of [`reasons`]), when known.
    pub fn publish_turn_end_with(
        &self,
        elapsed_ms: i64,
        tokens: i64,
        outcome: &str,
        reason: Option<&str>,
    ) {
        if !self.is_active() {
            return;
        }
        self.state.lock().unwrap().in_turn = false;
        let mut data = json!({"elapsedMs": elapsed_ms, "tokens": tokens, "outcome": outcome});
        if let Some(r) = reason {
            data["reason"] = json!(r);
        }
        self.publish(types::TURN_END, data);
    }

    /// A tool call is starting: returns the id its result will carry.
    pub fn publish_tool_call(&self, name: &str, args: &str) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        let id = {
            let mut s = self.state.lock().unwrap();
            s.tool_ids += 1;
            let id = format!("t{}", s.tool_ids);
            s.open_tools
                .entry(name.to_string())
                .or_default()
                .push_back(id.clone());
            id
        };
        self.publish(
            types::TOOL_CALL,
            json!({"id": id, "name": name, "args": truncate(args, 4000)}),
        );
        Some(id)
    }

    /// A tool call finished; paired with the oldest open call of the same tool.
    pub fn publish_tool_result(
        &self,
        name: &str,
        result: &str,
        is_error: bool,
        elapsed_ms: Option<i64>,
    ) {
        if !self.is_active() {
            return;
        }
        let id = self
            .state
            .lock()
            .unwrap()
            .open_tools
            .get_mut(name)
            .and_then(VecDeque::pop_front);
        self.publish(
            types::TOOL_RESULT,
            json!({"id": id, "name": name, "result": truncate(result, 20000), "isError": is_error, "elapsedMs": elapsed_ms}),
        );
    }

    /// The current turn's events (deltas merged), oldest first.
    pub fn current_turn_events(&self) -> Vec<LiveEvent> {
        self.state
            .lock()
            .unwrap()
            .turn
            .iter()
            .map(|(e, _)| e.clone())
            .collect()
    }

    /// The replay buffer, the seq it is complete up to, the turn's base index and whether a turn runs.
    pub fn capture_turn(&self) -> (Vec<LiveEvent>, i64, i64, bool) {
        let s = self.state.lock().unwrap();
        (
            s.turn.iter().map(|(e, _)| e.clone()).collect(),
            s.seq,
            s.turn_base_message_index,
            s.in_turn,
        )
    }
}

fn remember(s: &mut BusState, ev: &LiveEvent, is_text: bool) {
    // Bars, slots and queue state are snapshotted separately; only the transcript is replayed.
    if matches!(
        ev.kind.as_str(),
        types::AGENTS | types::SLOTS | types::BUSY | types::AGENT_ENTRY | types::AGENT_DELTA
    ) {
        return;
    }
    if is_text
        && let Some((last, true)) = s.turn.back_mut()
        && last.kind == ev.kind
    {
        let merged = format!(
            "{}{}",
            last.data["text"].as_str().unwrap_or_default(),
            ev.data["text"].as_str().unwrap_or_default()
        );
        last.seq = ev.seq;
        last.data = json!({"text": merged});
        return;
    }
    if s.turn.len() >= MAX_TURN_EVENTS {
        s.turn.pop_front();
    }
    s.turn.push_back((ev.clone(), is_text));
}

/// `LiveEventBus.Truncate`: at most `max` UTF-16 units, then "… (N more characters)" with the
/// count grouped by thousands (`N0`).
pub fn truncate(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_string();
    }
    let head = String::from_utf16_lossy(&units[..max]);
    format!(
        "{head}\n… ({} more characters)",
        group_thousands(units.len() - max)
    )
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A finished transcript entry: kind is "user", "assistant", "tool" or "system".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTranscriptItem {
    pub kind: String,
    pub text: Option<String>,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
    pub args: Option<String>,
    pub result: Option<String>,
    pub is_error: bool,
}

/// One agent bar: the Lead or a helper, with the slot and model it runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgent {
    pub id: String,
    pub letter: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub progress: Option<f64>,
    pub note: Option<String>,
    pub activity: Option<String>,
    pub slot: Option<String>,
    pub model: Option<String>,
    pub is_lead: bool,
    pub requested_model: Option<String>,
    pub served_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSlotLease {
    pub owner: String,
    pub kind: String,
    pub label: String,
    pub slot: Option<String>,
    pub copy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSlots {
    pub total: i32,
    pub leases: Vec<LiveSlotLease>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePermission {
    pub id: String,
    pub tool: String,
    pub risk: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
}

/// Log in first: run `login_command` in a terminal, then ask for the engine again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEngineAuth {
    pub engine: String,
    pub message: String,
    pub login_command: String,
}

/// What asking for an engine (or profile, mode) did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEngineResult {
    pub ok: bool,
    pub engine: String,
    pub message: Option<String>,
    pub auth: Option<LiveEngineAuth>,
}

/// Everything a newly connected client needs; live events with seq > `last_seq` follow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub working_directory: String,
    pub mode: String,
    pub model: Option<String>,
    pub busy: bool,
    pub queued: i32,
    pub last_seq: i64,
    pub history: Vec<LiveTranscriptItem>,
    pub current_turn: Vec<LiveEvent>,
    pub agents: Vec<LiveAgent>,
    pub slots: Option<LiveSlots>,
    pub pending_permissions: Vec<LivePermission>,
    pub engine: Option<String>,
    pub pending_questions: Option<Vec<LiveQuestion>>,
    pub uncensored: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collecting(bus: &Arc<LiveEventBus>) -> (Subscription, Arc<Mutex<Vec<LiveEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let sub = bus.subscribe(move |e| s2.lock().unwrap().push(e.clone()));
        (sub, seen)
    }

    #[test]
    fn publish_is_a_no_op_without_subscribers() {
        let bus = Arc::new(LiveEventBus::new());
        bus.publish(types::NOTICE, json!({}));
        assert_eq!(bus.last_seq(), 0);
        assert!(!bus.is_active());
        assert!(bus.publish_tool_call("bash", "{}").is_none());
    }

    #[test]
    fn seq_rises_and_unsubscribe_deactivates() {
        let bus = Arc::new(LiveEventBus::new());
        let (sub, seen) = collecting(&bus);
        bus.publish(types::NOTICE, json!({"level": "info", "text": "a"}));
        bus.publish(types::NOTICE, json!({"level": "info", "text": "b"}));
        let seqs: Vec<i64> = seen.lock().unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
        drop(sub);
        assert!(!bus.is_active());
    }

    #[test]
    fn text_deltas_merge_in_the_turn_buffer() {
        let bus = Arc::new(LiveEventBus::new());
        let (_sub, seen) = collecting(&bus);
        bus.publish_turn_start(3, "tui");
        bus.publish_text(types::ASSISTANT_DELTA, "Hel");
        bus.publish_text(types::ASSISTANT_DELTA, "lo");
        bus.publish_text(types::THINKING_DELTA, "hm");
        bus.publish(types::AGENTS, json!({"agents": []}));
        let turn = bus.current_turn_events();
        assert_eq!(turn.len(), 3);
        assert_eq!(turn[1].data["text"], "Hello");
        assert_eq!(turn[1].seq, 3);
        assert_eq!(seen.lock().unwrap().len(), 5, "subscribers see every delta");
        let (_, seq, base, in_turn) = bus.capture_turn();
        assert_eq!((seq, base, in_turn), (5, 3, true));
        bus.publish_turn_end(10, 5, "success");
        assert!(!bus.in_turn());
    }

    #[test]
    fn turn_end_carries_a_reason_only_when_there_is_one() {
        let bus = Arc::new(LiveEventBus::new());
        let (_sub, seen) = collecting(&bus);
        bus.publish_turn_end(1, 0, "done");
        bus.publish_turn_end_with(1, 0, "error", Some(reasons::RATE_LIMITED));
        let seen = seen.lock().unwrap();
        assert!(seen[0].data.get("reason").is_none());
        assert_eq!(seen[1].data["outcome"], "error");
        assert_eq!(seen[1].data["reason"], "rateLimited");
    }

    #[test]
    fn tool_results_pair_with_the_oldest_open_call() {
        let bus = Arc::new(LiveEventBus::new());
        let (_sub, seen) = collecting(&bus);
        let a = bus.publish_tool_call("bash", "{}").unwrap();
        let b = bus.publish_tool_call("bash", "{}").unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("t1", "t2"));
        bus.publish_tool_result("bash", "ok", false, Some(4));
        bus.publish_tool_result("read_file", "x", true, None);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2].data["id"], "t1");
        assert_eq!(seen[3].data["id"], Value::Null);
    }

    #[test]
    fn forward_keeps_order_and_skips_replays() {
        let bus = Arc::new(LiveEventBus::new());
        let (_sub, seen) = collecting(&bus);
        let ev = |seq, kind: &str| LiveEvent {
            seq,
            kind: kind.into(),
            at: String::new(),
            data: json!({}),
        };
        bus.forward(&ev(1, types::TURN_START), 10);
        bus.forward(&ev(1, types::NOTICE), 10);
        bus.forward(&ev(2, types::TURN_END), 10);
        assert_eq!(bus.last_seq(), 12);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(!bus.in_turn());
        assert_eq!(bus.advance_to(5), 12);
        assert_eq!(bus.advance_to(20), 20);
    }

    #[test]
    fn truncate_counts_utf16_and_groups_thousands() {
        assert_eq!(truncate("abc", 5), "abc");
        let long = "x".repeat(1300);
        assert_eq!(
            truncate(&long, 100),
            format!("{}\n… (1,200 more characters)", "x".repeat(100))
        );
    }
}
