//! A bounded, thread-safe record of what a sub-agent did (port of `AgentTranscript.cs`).
//!
//! Lengths count UTF-16 units, as C# `string.Length` does.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::chat::{head16, len16, n0, tail16};

/// What a transcript entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentTranscriptKind {
    /// The task the agent was given.
    Task,
    /// Text the agent wrote in one iteration (narration or its final answer).
    AssistantText,
    /// A tool call, as a short activity line ("read src/Foo.cs").
    ToolCall,
    /// A preview of a tool result.
    ToolResult,
    /// A message the user sent the agent while it worked.
    UserMessage,
    /// Lifecycle lines: progress reports, compaction, completion.
    Status,
}

impl AgentTranscriptKind {
    /// The C# enum member name.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentTranscriptKind::Task => "Task",
            AgentTranscriptKind::AssistantText => "AssistantText",
            AgentTranscriptKind::ToolCall => "ToolCall",
            AgentTranscriptKind::ToolResult => "ToolResult",
            AgentTranscriptKind::UserMessage => "UserMessage",
            AgentTranscriptKind::Status => "Status",
        }
    }
}

/// One transcript entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTranscriptEntry {
    /// When the entry was added (UTC).
    pub at: SystemTime,
    pub kind: AgentTranscriptKind,
    pub text: String,
    pub is_error: bool,
    /// Position in the whole transcript (0 = first entry ever, dropped ones count), so a viewer
    /// can merge a snapshot with live entries.
    pub n: i64,
    /// A tool call's tool name (`None` otherwise).
    pub tool: Option<String>,
    /// A tool call's arguments as JSON, truncated (`None` otherwise).
    pub args: Option<String>,
    /// Who sent a [`AgentTranscriptKind::UserMessage`]: "user" (typed to the agent) or "lead"
    /// (a follow-up from the Lead).
    pub from: Option<String>,
}

/// Everything a viewer needs at once: the entries, the text streamed since the last one, and
/// where that stream ends.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTranscriptCapture {
    pub entries: Vec<AgentTranscriptEntry>,
    pub dropped: usize,
    pub streaming: String,
    pub stream_end: i64,
}

type AppendedListener = Arc<dyn Fn(&AgentTranscriptEntry) + Send + Sync>;
type TextDeltaListener = Arc<dyn Fn(&str) + Send + Sync>;
type StreamDeltaListener = Arc<dyn Fn(&str, i64) + Send + Sync>;

#[derive(Default)]
struct State {
    entries: std::collections::VecDeque<AgentTranscriptEntry>,
    dropped: usize,
    streaming: String,
    streamed: i64,
    count: i64,
}

#[derive(Default)]
struct Listeners {
    appended: Vec<AppendedListener>,
    text_delta: Vec<TextDeltaListener>,
    stream_delta: Vec<StreamDeltaListener>,
}

/// A bounded, thread-safe record of what a sub-agent did, kept for display (the agent view)
/// after the agent's conversation has been released. Text is truncated per entry.
///
/// Listeners run on the calling thread, outside the transcript's lock; a panicking listener is
/// caught and ignored (a broken viewer must never take the agent down).
#[derive(Default)]
pub struct AgentTranscript {
    state: Mutex<State>,
    listeners: Mutex<Listeners>,
}

impl std::fmt::Debug for AgentTranscript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("AgentTranscript")
            .field("entries", &s.entries.len())
            .field("dropped", &s.dropped)
            .field("streamed", &s.streamed)
            .finish()
    }
}

fn lock_ignoring_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AgentTranscript {
    /// Most entries kept; older ones are dropped from the front.
    pub const MAX_ENTRIES: usize = 400;
    /// Longest entry text (UTF-16 units) before it is cut.
    pub const MAX_TEXT_CHARS: usize = 6000;
    /// Longest tool result preview (UTF-16 units).
    pub const MAX_TOOL_RESULT_CHARS: usize = 800;

    /// An empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock_ignoring_poison(&self.state)
    }

    /// Register a listener called after an entry is appended (on the agent's thread).
    pub fn on_appended(&self, listener: Box<dyn Fn(&AgentTranscriptEntry) + Send + Sync>) {
        lock_ignoring_poison(&self.listeners)
            .appended
            .push(Arc::from(listener));
    }

    /// Register a listener for each streamed text chunk of the current iteration.
    pub fn on_text_delta(&self, listener: Box<dyn Fn(&str) + Send + Sync>) {
        lock_ignoring_poison(&self.listeners)
            .text_delta
            .push(Arc::from(listener));
    }

    /// Like [`on_text_delta`](Self::on_text_delta), with the total number of UTF-16 units
    /// streamed so far (this chunk included): a viewer that captured up to some total skips
    /// what it already has.
    pub fn on_stream_delta(&self, listener: Box<dyn Fn(&str, i64) + Send + Sync>) {
        lock_ignoring_poison(&self.listeners)
            .stream_delta
            .push(Arc::from(listener));
    }

    /// Entries dropped from the front because the transcript hit [`Self::MAX_ENTRIES`].
    pub fn dropped_count(&self) -> usize {
        self.lock().dropped
    }

    /// Text streamed so far in the current step, not yet recorded as an entry. A viewer that
    /// opens mid-stream reads this instead of relying on deltas it never saw.
    pub fn streaming_text(&self) -> String {
        self.lock().streaming.clone()
    }

    /// The entries now.
    pub fn snapshot(&self) -> Vec<AgentTranscriptEntry> {
        self.lock().entries.iter().cloned().collect()
    }

    /// Entries, streamed text and stream position, taken together.
    pub fn capture(&self) -> AgentTranscriptCapture {
        let s = self.lock();
        AgentTranscriptCapture {
            entries: s.entries.iter().cloned().collect(),
            dropped: s.dropped,
            streaming: s.streaming.clone(),
            stream_end: s.streamed,
        }
    }

    /// Add an entry.
    pub fn add(&self, kind: AgentTranscriptKind, text: &str, is_error: bool) {
        self.add_entry(kind, text, is_error, None, None, None);
    }

    /// A tool call with its tool name and arguments (shown as a tool card by the web UI).
    pub fn add_tool_call(&self, activity: &str, tool: &str, args: &str) {
        let args = if len16(args) > Self::MAX_TEXT_CHARS {
            format!("{}…", head16(args, Self::MAX_TEXT_CHARS))
        } else {
            args.to_string()
        };
        self.add_entry(
            AgentTranscriptKind::ToolCall,
            activity,
            false,
            Some(tool.to_string()),
            Some(args),
            None,
        );
    }

    /// A message to the agent; `from` is "user" or "lead".
    pub fn add_message(&self, text: &str, from: &str) {
        self.add_entry(
            AgentTranscriptKind::UserMessage,
            text,
            false,
            None,
            None,
            Some(from.to_string()),
        );
    }

    fn add_entry(
        &self,
        kind: AgentTranscriptKind,
        text: &str,
        is_error: bool,
        tool: Option<String>,
        args: Option<String>,
        from: Option<String>,
    ) {
        let limit = if kind == AgentTranscriptKind::ToolResult {
            Self::MAX_TOOL_RESULT_CHARS
        } else {
            Self::MAX_TEXT_CHARS
        };
        let mut text = text.replace("\r\n", "\n").trim_end().to_string();
        let len = len16(&text);
        if len > limit {
            text = format!(
                "{}\n… ({} more chars)",
                head16(&text, limit),
                n0((len - limit) as i64)
            );
        }

        let entry = {
            let mut s = self.lock();
            let entry = AgentTranscriptEntry {
                at: SystemTime::now(),
                kind,
                text,
                is_error,
                n: s.count,
                tool,
                args,
                from,
            };
            s.count += 1;
            s.entries.push_back(entry.clone());
            // The finished text (or the next step) supersedes the streamed preview.
            if matches!(
                kind,
                AgentTranscriptKind::AssistantText
                    | AgentTranscriptKind::ToolCall
                    | AgentTranscriptKind::Status
            ) {
                s.streaming.clear();
            }
            if s.entries.len() > Self::MAX_ENTRIES {
                s.entries.pop_front();
                s.dropped += 1;
            }
            entry
        };

        let listeners = lock_ignoring_poison(&self.listeners).appended.clone();
        for l in listeners {
            let _ = catch_unwind(AssertUnwindSafe(|| l(&entry)));
        }
    }

    /// Record a streamed text chunk of the current step and notify the delta listeners.
    pub fn raise_delta(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let end = {
            let mut s = self.lock();
            s.streamed += len16(delta) as i64;
            s.streaming.push_str(delta);
            if len16(&s.streaming) > Self::MAX_TEXT_CHARS * 2 {
                s.streaming = tail16(&s.streaming, Self::MAX_TEXT_CHARS).to_string();
            }
            s.streamed
        };
        let (text_delta, stream_delta) = {
            let l = lock_ignoring_poison(&self.listeners);
            (l.text_delta.clone(), l.stream_delta.clone())
        };
        for l in text_delta {
            let _ = catch_unwind(AssertUnwindSafe(|| l(delta)));
        }
        for l in stream_delta {
            let _ = catch_unwind(AssertUnwindSafe(|| l(delta, end)));
        }
    }
}
