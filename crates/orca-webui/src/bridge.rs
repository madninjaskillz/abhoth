//! Bridge engines in the web UI (the engine parts of C# `LiveSessionAdapter`): the turn's events
//! on the session's bus, the Lead bar showing the engine, the bridge CLI's progress.js bars as
//! agent bars, and the engine in the snapshot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_core::engines::session_sink::BridgeView;
use orca_core::engines::{AgentEngineEvent, AgentEngines, EngineReadiness, EngineReadinessState};
use orca_core::live::{LiveAgent, LiveEngineAuth, LiveEventBus, types};
use orca_core::progress::ProgressFileBar;
use serde_json::json;

/// What the browser sees of the engine: its id and the bridge CLI's bars.
#[derive(Debug, Clone)]
pub struct EngineView {
    pub id: &'static str,
    pub bars: Vec<ProgressFileBar>,
}

impl Default for EngineView {
    fn default() -> Self {
        Self {
            id: AgentEngines::NATIVE,
            bars: Vec::new(),
        }
    }
}

impl EngineView {
    pub fn is_bridge(&self) -> bool {
        self.id != AgentEngines::NATIVE
    }

    /// The bridge's name ("claude-code"), `None` for native.
    pub fn label(&self) -> Option<String> {
        self.is_bridge()
            .then(|| AgentEngines::display_name_of(self.id).to_string())
    }
}

pub type SharedEngineView = Arc<Mutex<EngineView>>;

/// Turns the agent host's bars into what the browser shows (the engine's Lead bar added).
pub type AgentsDecorator = Arc<dyn Fn(Vec<LiveAgent>) -> Vec<LiveAgent> + Send + Sync>;

/// [`decorate_agents`] over a live view, the Lead running while `bus` is in a turn.
pub fn decorator(view: SharedEngineView, bus: Arc<LiveEventBus>) -> AgentsDecorator {
    Arc::new(move |list| {
        let v = view.lock().unwrap().clone();
        decorate_agents(Some(list), &v, bus.in_turn())
    })
}

/// `BuildAgents` with a bridge: the Lead bar shows the engine (its id where the slot goes, its
/// name where the model goes), and the CLI's progress.js bars follow it like helpers. `list`
/// is the agent host's list (`None`: no sub-agents, a Lead bar only).
pub fn decorate_agents(
    list: Option<Vec<LiveAgent>>,
    view: &EngineView,
    in_turn: bool,
) -> Vec<LiveAgent> {
    let Some(mut list) = list.or_else(|| view.is_bridge().then(Vec::new)) else {
        return Vec::new();
    };
    if !view.is_bridge() {
        return list;
    }
    let label = view.label();
    let lead_at = match list.iter().position(|a| a.is_lead) {
        Some(i) => i,
        None => {
            list.insert(
                0,
                LiveAgent {
                    id: orca_core::orchestration::LEAD_ID.into(),
                    letter: "L".into(),
                    label: "Lead".into(),
                    kind: "main".into(),
                    status: if in_turn { "running" } else { "idle" }.into(),
                    progress: None,
                    note: None,
                    activity: None,
                    slot: None,
                    model: None,
                    is_lead: true,
                    requested_model: None,
                    served_model: None,
                },
            );
            0
        }
    };
    let lead = &mut list[lead_at];
    lead.slot = Some(view.id.to_string());
    lead.model = label.clone();
    lead.requested_model = None;
    lead.served_model = None;
    for (i, bar) in view.bars.iter().enumerate() {
        list.insert(
            lead_at + 1 + i,
            LiveAgent {
                id: format!("bar:{}", bar.key),
                letter: bar.letter().to_string(),
                label: bar.label.clone(),
                kind: "progress".into(),
                status: if bar.pct >= 100.0 {
                    "completed"
                } else {
                    "running"
                }
                .into(),
                progress: Some((bar.pct / 100.0).clamp(0.0, 1.0)),
                note: None,
                activity: None,
                slot: None,
                model: label.clone(),
                is_lead: false,
                requested_model: None,
                served_model: None,
            },
        );
    }
    list
}

/// `LiveEngineAuth` for a readiness that needs a login.
pub fn auth_of(id: &str, readiness: Option<&EngineReadiness>) -> Option<LiveEngineAuth> {
    let r = readiness.filter(|r| r.state == EngineReadinessState::NotLoggedIn)?;
    Some(LiveEngineAuth {
        engine: id.to_string(),
        message: r.message.clone(),
        login_command: r.login_command_line()?,
    })
}

/// A bridge turn on the session's bus (what C#'s renderers publish while the REPL draws it).
pub struct LiveBridgeView {
    pub bus: Arc<LiveEventBus>,
}

impl BridgeView for LiveBridgeView {
    fn event(&mut self, event: &AgentEngineEvent, elapsed: Option<Duration>, _args: Option<&str>) {
        match event {
            AgentEngineEvent::TextDelta(t) if !t.is_empty() => {
                self.bus.publish_text(types::ASSISTANT_DELTA, t);
            }
            AgentEngineEvent::ThinkingDelta(t) if !t.is_empty() => {
                self.bus.publish_text(types::THINKING_DELTA, t);
            }
            AgentEngineEvent::ToolCall {
                name, args_json, ..
            } => {
                self.bus.publish_tool_call(name, args_json);
            }
            AgentEngineEvent::ToolResult {
                name,
                result,
                is_error,
                ..
            } => {
                self.bus.publish_tool_result(
                    name,
                    result,
                    *is_error,
                    elapsed.map(|e| e.as_millis() as i64),
                );
            }
            AgentEngineEvent::Notice { level, text } => {
                self.bus
                    .publish(types::NOTICE, json!({"level": level, "text": text}));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(pct: f64) -> ProgressFileBar {
        ProgressFileBar {
            key: "build".into(),
            label: "Build the app".into(),
            pct,
            colour: "purple".into(),
            started: 1,
            updated: 2,
            source: None,
        }
    }

    #[test]
    fn native_leaves_the_list_alone() {
        assert!(decorate_agents(None, &EngineView::default(), false).is_empty());
    }

    #[test]
    fn bridge_lead_shows_the_engine_and_bars_follow_it() {
        let view = EngineView {
            id: "claude",
            bars: vec![bar(37.0), bar(100.0)],
        };
        let list = decorate_agents(None, &view, true);
        assert_eq!(list.len(), 3);
        assert!(list[0].is_lead);
        assert_eq!(list[0].status, "running");
        assert_eq!(list[0].slot.as_deref(), Some("claude"));
        assert_eq!(list[0].model.as_deref(), Some("claude-code"));
        assert_eq!(list[1].id, "bar:build");
        assert_eq!(list[1].letter, "D");
        assert_eq!(list[1].kind, "progress");
        assert_eq!(list[1].progress, Some(0.37));
        assert_eq!(list[1].status, "running");
        assert_eq!(list[2].status, "completed");
    }
}
