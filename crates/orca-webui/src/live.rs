//! `ILiveSession`: the running session as the web UI sees it (its events, a snapshot for a
//! client that just connected, and the things a browser may do to it), plus the sub-agent
//! records of the agent tabs.
//!
//! Sub-agent features (agent tabs, transcripts, messaging) go through [`AgentHost`], which
//! the orchestration port implements; until it is wired, sessions have no sub-agents and
//! answer as the C# default interface methods do.

use std::sync::Arc;

use async_trait::async_trait;
use orca_core::live::{LiveAgent, LiveEngineResult, LiveEventBus, LiveSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript entry of a sub-agent (`LiveAgentEntry`). Kind: "task", "assistant", "tool",
/// "toolResult", "message", "status". `n` rises per agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgentEntry {
    pub agent_id: String,
    pub n: i64,
    pub at: String,
    pub kind: String,
    pub text: String,
    pub is_error: bool,
    pub tool: Option<String>,
    pub args: Option<String>,
    pub from: Option<String>,
}

/// A sub-agent's transcript so far, for its tab (`LiveAgentTranscript`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgentTranscript {
    pub agent_id: String,
    pub letter: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub task: String,
    pub entries: Vec<LiveAgentEntry>,
    pub dropped: i32,
    pub streaming: String,
    pub stream_end: i64,
    pub can_message: bool,
    pub can_cancel: bool,
    pub result: Option<String>,
}

/// What a message to a sub-agent did: "queued", "resumed", "notAccepting" or "notFound".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgentMessageResult {
    pub ok: bool,
    pub outcome: String,
    pub message: Option<String>,
}

impl LiveAgentMessageResult {
    pub fn new(ok: bool, outcome: &str, message: Option<&str>) -> Self {
        Self {
            ok,
            outcome: outcome.into(),
            message: message.map(str::to_string),
        }
    }
}

/// `SettingChange`: what changing one setting did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingChange {
    pub ok: bool,
    pub key: String,
    pub error: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub live: bool,
}

impl SettingChange {
    pub fn failed(key: &str, error: &str) -> Self {
        Self {
            ok: false,
            key: key.into(),
            error: Some(error.into()),
            old: None,
            new: None,
            live: true,
        }
    }
}

pub fn engine_result(ok: bool, engine: &str, message: Option<&str>) -> LiveEngineResult {
    LiveEngineResult {
        ok,
        engine: engine.into(),
        message: message.map(str::to_string),
        auth: None,
    }
}

/// The sub-agents of a session (the orchestrator), as the agent tabs need them. Implemented by
/// the orchestration port; a session without one has no sub-agents.
#[async_trait]
pub trait AgentHost: Send + Sync {
    /// The agent bars (Lead and helpers) for the snapshot.
    fn agents(&self) -> Vec<LiveAgent>;
    async fn transcript(&self, agent_id: &str) -> Option<LiveAgentTranscript>;
    async fn send_message(&self, agent_id: &str, text: &str) -> LiveAgentMessageResult;
    async fn cancel(&self, agent_id: &str) -> bool;
}

/// `ILiveSession`. Every method with a C# default has the same default here.
#[async_trait]
pub trait LiveSession: Send + Sync {
    fn bus(&self) -> Arc<LiveEventBus>;

    /// The session's working folder (the terminal and git panels run there).
    fn working_directory(&self) -> String;

    /// "tui", "headless" or "worker".
    fn mode(&self) -> String;

    /// The snapshot; an error is the message the hub reports.
    async fn get_snapshot(&self) -> Result<LiveSnapshot, String>;

    /// Queue a prompt; it runs after anything already queued or running. Returns an error
    /// message, or `None` when queued.
    async fn submit_prompt(&self, text: &str, source: &str) -> Option<String>;

    /// Cancel the running generation. False when nothing was running.
    async fn cancel(&self) -> bool;

    /// Answer a pending permission request ("yes", "all" or "no").
    async fn resolve_permission(&self, id: &str, decision: &str) -> bool;

    async fn set_engine(&self, _engine: &str) -> LiveEngineResult {
        engine_result(false, "native", Some("This session can't change engines."))
    }

    async fn answer_question(&self, _id: &str, _answer: &str) -> bool {
        false
    }

    fn set_preferred_engine(&self, _engine: Option<&str>) {}

    /// The running session's settings (`ConfigSchema.Describe`, secrets masked).
    async fn get_settings(&self) -> Option<Value> {
        None
    }

    async fn set_setting(&self, key: &str, _value: Option<Value>, _reset: bool) -> SettingChange {
        SettingChange::failed(key, "This session can't change settings.")
    }

    /// The running session's model server settings (with the key: the connection panel probes
    /// the router with it, server side; never sent to the page). `None`: not known here.
    async fn lm_studio(&self) -> Option<orca_core::config::LmStudioConfig> {
        None
    }

    async fn set_profile(&self, _profile: Option<&str>) -> LiveEngineResult {
        engine_result(
            false,
            "",
            Some(
                "This session's profile is set where it was started (--profile, or /profile in its terminal).",
            ),
        )
    }

    fn set_preferred_profile(&self, _profile: Option<&str>) {}

    async fn set_mode(&self, _mode: Option<&str>) -> LiveEngineResult {
        engine_result(
            false,
            "",
            Some("This session's mode is set where it was started (--mode)."),
        )
    }

    fn set_preferred_mode(&self, _mode: Option<&str>) {}

    async fn get_agent_transcript(&self, _agent_id: &str) -> Option<LiveAgentTranscript> {
        None
    }

    async fn send_agent_message(&self, _agent_id: &str, _text: &str) -> LiveAgentMessageResult {
        LiveAgentMessageResult::new(false, "notFound", Some("This session has no sub-agents."))
    }

    async fn cancel_agent(&self, _agent_id: &str) -> bool {
        false
    }

    /// Stop the session (a closed project's worker). The primary session belongs to the CLI.
    async fn shutdown(&self) {}
}

/// Makes the session for a project folder other than the primary one.
pub type SessionFactory = Arc<dyn Fn(&str) -> Arc<dyn LiveSession> + Send + Sync>;

/// The snapshot as the client reads it: C# wraps the agent bars in `{ "agents": [...] }`.
pub fn snapshot_json(s: &LiveSnapshot) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        let agents = obj.remove("agents").unwrap_or(Value::Array(vec![]));
        // Keep C#'s property order: ... currentTurn, agents, slots, pendingPermissions, ...
        let mut out = serde_json::Map::new();
        for (k, val) in std::mem::take(obj) {
            if k == "slots" {
                out.insert("agents".into(), serde_json::json!({ "agents": agents }));
            }
            out.insert(k, val);
        }
        if !out.contains_key("agents") {
            out.insert("agents".into(), serde_json::json!({ "agents": agents }));
        }
        *obj = out;
    }
    v
}
