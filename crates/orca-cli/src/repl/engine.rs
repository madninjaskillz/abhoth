//! Bridge engines in the REPL: the turn renderer (`EngineEventRenderer`), the `/engine`
//! command, the login offer and the startup check (`ReplLoop.HandleEngineCommandAsync`,
//! `OfferLoginAsync`, `PrepareEngineAsync`). Turns run on [`EngineHost`]; the session state
//! and approvals are [`SessionBridgeSink`]'s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_abstractions::Tool;
use orca_core::engines::session_sink::{BridgeView, SessionBridgeSink};
use orca_core::engines::{
    AgentEngineEvent, AgentEngines, AgentTurnMode, BridgeTurnRequest, CancelToken, EngineError,
    EngineHost, EngineReadiness, EngineReadinessState,
};
use orca_core::progress::ProgressFileBar;

use crate::render::markdown::MarkdownStreamRenderer;
use crate::render::tool_call::ToolCallRenderer;
use crate::ui::Console;
use crate::ui::markup::escape;

use super::state::ReplState;
use super::{KeyWatcher, Repl, TurnOutcome, format_turn_footer};

/// `EngineEventRenderer`'s screen side: shows a bridge turn like a native one (streamed
/// markdown, thinking, tool cards with Abhoth's tool names, notices, denials).
pub struct ReplBridgeView {
    console: Console,
    state: ReplState,
    tools: ToolCallRenderer,
    markdown: MarkdownStreamRenderer,
    registry: HashMap<String, Arc<dyn Tool>>,
    in_thinking: bool,
    batch_open: bool,
    open_calls: usize,
}

impl ReplBridgeView {
    pub fn new(console: Console, state: ReplState, tools: &[Arc<dyn Tool>]) -> Self {
        Self {
            tools: ToolCallRenderer::new(console.clone()),
            markdown: MarkdownStreamRenderer::new(console.clone()),
            console,
            state,
            registry: tools
                .iter()
                .map(|t| (t.name().to_string(), t.clone()))
                .collect(),
            in_thinking: false,
            batch_open: false,
            open_calls: 0,
        }
    }

    /// `EndTurn`: close whatever is still open.
    pub fn end_turn(&mut self) {
        self.end_text();
        self.open_calls = 0;
    }

    fn end_thinking_text(&mut self) {
        if self.in_thinking {
            self.console.raw("\x1b[0m\n");
            self.in_thinking = false;
        }
    }

    fn end_text(&mut self) {
        self.end_thinking_text();
        if self.markdown.active() {
            self.markdown.finish();
            self.console.blank();
        }
    }
}

impl BridgeView for ReplBridgeView {
    fn event(&mut self, event: &AgentEngineEvent, elapsed: Option<Duration>, args: Option<&str>) {
        match event {
            AgentEngineEvent::ThinkingDelta(text) => {
                if !self.state.show_thinking() {
                    return;
                }
                if self.markdown.active() {
                    self.end_text();
                }
                if !self.in_thinking {
                    self.console.raw("\x1b[36m");
                    self.in_thinking = true;
                }
                self.console.raw(text);
            }
            AgentEngineEvent::TextDelta(text) => {
                self.batch_open = false;
                self.end_thinking_text();
                self.markdown.append(text);
            }
            AgentEngineEvent::ToolCall {
                name, args_json, ..
            } => {
                self.end_text();
                if !self.batch_open {
                    self.tools.begin_batch();
                    self.batch_open = true;
                }
                self.open_calls += 1;
                let tool = self.registry.get(name).cloned();
                self.tools
                    .render_tool_call(name, args_json, tool.as_deref(), &self.state);
            }
            AgentEngineEvent::ToolResult {
                name,
                result,
                is_error,
                ..
            } => {
                self.end_text();
                let tool = self.registry.get(name).cloned();
                self.tools.render_tool_result(
                    name,
                    result,
                    *is_error,
                    elapsed,
                    args,
                    tool.as_deref(),
                    &self.state,
                );
                self.open_calls = self.open_calls.saturating_sub(1);
                if self.open_calls == 0 {
                    self.batch_open = false;
                }
            }
            AgentEngineEvent::Notice { level, text } => {
                self.end_text();
                let colour = match level.as_str() {
                    "error" => "red",
                    "warning" => "yellow",
                    _ => "grey",
                };
                self.console
                    .markup_line(&format!("[{colour}]{}[/]", escape(text)));
            }
            _ => {}
        }
    }

    fn before_prompt(&mut self) {
        self.end_text();
    }

    fn denied(&mut self, original_name: &str) {
        self.console.markup_line(&format!(
            "  [yellow]\u{2298} {} denied[/]",
            escape(original_name)
        ));
    }
}

/// The bridge's progress.js bars as one line for the info row: `[A] Label 37%` per bar.
pub fn format_external_bars(bars: &[ProgressFileBar]) -> String {
    bars.iter()
        .map(|b| {
            let colour =
                orca_core::orchestration::palette::XTERM[(b.color_slot().rem_euclid(8)) as usize];
            format!(
                "\x1b[48;5;{colour}m\x1b[30m {} \x1b[0m {} \x1b[2m{:.0}%\x1b[0m",
                b.letter(),
                crate::render::text::sanitize(&b.label),
                b.pct.clamp(0.0, 100.0)
            )
        })
        .collect::<Vec<_>>()
        .join("  ")
}

impl Repl {
    /// A bridge turn (`EngineHost.RunTurnAsync` with a bridge, plus `RunAgentTurnAsync`'s
    /// footer): the user's message is already the last message of the session.
    pub(super) async fn run_bridge_turn(&mut self, mode: AgentTurnMode) -> TurnOutcome {
        let started = Instant::now();
        let tokens_before = self.session.total_output_tokens;
        let mut conversation = self.session.conversation();
        conversation.engine_id = self.state.engine.engine.clone();
        conversation.engine_session_id = self.state.engine.engine_session_id.clone();
        let host_tools = orca_core::engines::print::host_tools(Some(&self.session.registry));
        let cwd = self.session.ctx.cwd.clone();
        let config = self.session.config.clone();
        let session_id = self.state.current_session_id.clone();
        let cancel = CancelToken::new();
        let watcher = self.interactive.then(KeyWatcher::start);
        let mut view = ReplBridgeView::new(
            self.console.clone(),
            self.state.clone(),
            self.session.registry.all(),
        );
        let result = {
            let mut sink = SessionBridgeSink::new(&mut self.session, &mut view);
            let mut turn = Box::pin(self.engines.run_bridge_turn(
                BridgeTurnRequest {
                    config: &config,
                    cwd: &cwd,
                    conversation: &mut conversation,
                    session_id: session_id.as_deref(),
                    host_tools,
                    sink: &mut sink,
                    mode,
                },
                &cancel,
            ));
            let key = async {
                match &watcher {
                    Some(w) => w.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            // Ctrl+C cancels the turn; the bridge then interrupts its CLI and winds down.
            let r = tokio::select! {
                r = &mut turn => r,
                _ = tokio::signal::ctrl_c() => { cancel.cancel(); (&mut turn).await }
                _ = key => { cancel.cancel(); (&mut turn).await }
            };
            drop(turn);
            r
        };
        view.end_turn();
        drop(watcher);
        self.state.engine = orca_core::session::EngineRef {
            engine: conversation.engine_id.clone(),
            engine_session_id: conversation.engine_session_id.clone(),
        };
        self.session.set_conversation(conversation);
        let outcome = match result {
            Ok(()) | Err(EngineError::Unavailable(_)) => {
                if let Err(EngineError::Unavailable(m)) = &result {
                    self.console
                        .markup_line(&format!("[red]\u{2717} {}[/]", escape(m)));
                }
                self.state.total_turns += 1;
                self.console.markup_line(&format_turn_footer(
                    started.elapsed(),
                    self.session.total_output_tokens - tokens_before,
                ));
                TurnOutcome::Done
            }
            Err(EngineError::Cancelled) => {
                self.console.markup_line("\n[yellow]Cancelled.[/]");
                TurnOutcome::Cancelled
            }
            Err(EngineError::Failed(m)) => {
                tracing::error!("Error in agent loop: {m}");
                let (message, hint) = crate::render::friendly_errors::describe_message(
                    &m,
                    &self.session.config.lm_studio.base_url,
                );
                self.console
                    .markup_line(&format!("\n[red]\u{2717} {}[/]", escape(&message)));
                if let Some(h) = hint {
                    self.console
                        .markup_line(&format!("  [dim]{}[/]", escape(&h)));
                }
                TurnOutcome::Failed
            }
        };
        let status = self.status_info();
        self.panel.redraw(&self.state, &status);
        outcome
    }

    /// `/engine [native|claude|codex]`: show or switch the engine; a switch starts a fresh
    /// engine session (`HandleEngineCommandAsync`).
    pub(super) async fn handle_engine_command(&mut self, args: &[String]) {
        if args.is_empty() {
            let label = AgentEngines::display_name_of(EngineHost::engine_id(&self.session.config));
            self.console.markup_line(&format!(
                "[grey]Engine:[/] [cyan]{}[/] [dim]— /engine native | claude | codex[/]",
                escape(label)
            ));
            return;
        }
        let before = EngineHost::engine_id(&self.session.config);
        let mut result = self.switch_engine(&args[0]).await;
        if !result.switched
            && let Some(r) = result
                .readiness
                .clone()
                .filter(|r| r.state == EngineReadinessState::NotLoggedIn)
            && self.offer_login(&r).await
        {
            result = self.switch_engine(&args[0]).await;
        }
        let now = AgentEngines::display_name_of(EngineHost::engine_id(&self.session.config));
        if !result.switched {
            self.console.markup_line(&format!(
                "[yellow]{}[/] [dim](still {})[/]",
                escape(result.error.as_deref().unwrap_or("Engine not switched.")),
                escape(now)
            ));
            return;
        }
        self.console
            .markup_line(&if before == EngineHost::engine_id(&self.session.config) {
                format!(
                    "[grey]Engine {}: fresh session; it doesn't see the earlier turns.[/]",
                    escape(now)
                )
            } else {
                format!(
                    "[grey]Engine switched to[/] [cyan]{}[/][grey]: a fresh session that doesn't see the earlier turns (they stay in this transcript).[/]",
                    escape(now)
                )
            });
        let status = self.status_info();
        self.panel.redraw(&self.state, &status);
    }

    /// `SwitchAsync` plus forgetting the conversation's engine session on a switch.
    async fn switch_engine(&mut self, name: &str) -> orca_core::engines::EngineSwitchResult {
        let result = self.engines.switch(name, &mut self.session.config).await;
        if result.switched {
            self.state.engine = Default::default();
        }
        result
    }

    /// `OfferLoginAsync`: a bridge CLI isn't logged in: offer to run its login command here,
    /// attached to this terminal, and report whether it is logged in afterwards.
    pub(super) async fn offer_login(&mut self, readiness: &EngineReadiness) -> bool {
        self.console
            .markup_line(&format!("[yellow]{}[/]", escape(&readiness.message)));
        let Some(login) = readiness.login_command.clone().filter(|l| !l.is_empty()) else {
            return false;
        };
        if !self.interactive {
            return false;
        }
        let line = readiness.login_command_line().unwrap_or_default();
        let console = self.console.clone();
        let run = tokio::task::block_in_place(|| {
            crate::ui::prompt::confirm(
                &console,
                &format!("Run [cyan]{}[/] here now?", escape(&line)),
                true,
            )
        });
        if !run {
            return false;
        }
        let panel_was_active = self.panel.is_active();
        if panel_was_active {
            self.panel.teardown();
        }
        let mut env = orca_core::engines::transport::current_environment();
        orca_core::engines::claude::apply_environment(&mut env, false, None);
        let status = tokio::process::Command::new(&login[0])
            .args(&login[1..])
            .env_clear()
            .envs(env)
            .status()
            .await;
        if let Err(e) = status {
            self.console.markup_line(&format!(
                "[red]Could not run the login: {}[/]",
                escape(&e.to_string())
            ));
        }
        if panel_was_active {
            let status = self.status_info();
            self.panel.setup(&self.state, &status);
        }
        let again = self
            .engines
            .check(&readiness.engine_id, &self.session.config)
            .await;
        self.console.markup_line(&if again.is_ready() {
            format!("[green]\u{2713} {}: logged in.[/]", escape(&again.message))
        } else {
            format!("[yellow]{}[/]", escape(&again.message))
        });
        again.is_ready()
    }

    /// `PrepareEngineAsync(interactive: true)`: startup with a bridge selected (--engine,
    /// agent.engine, a resumed bridge session): check it; not logged in: offer the login;
    /// still unusable: native for now.
    pub async fn prepare_engine(&mut self) {
        if !EngineHost::is_bridge(&self.session.config) {
            return;
        }
        let wanted = EngineHost::engine_id(&self.session.config);
        let Some(readiness) = self.engines.verify_selected(&mut self.session.config).await else {
            return;
        };
        if readiness.is_ready() {
            return;
        }
        if readiness.state == EngineReadinessState::NotLoggedIn {
            if self.offer_login(&readiness).await && self.switch_engine(wanted).await.switched {
                return;
            }
        } else {
            self.console
                .markup_line(&format!("[yellow]{}[/]", escape(&readiness.message)));
        }
        self.console.markup_line(&format!(
            "[grey]Using the native engine for now; /engine {} to try again.[/]",
            escape(wanted)
        ));
    }

    /// The bridge's progress.js bars on the info row while they change.
    pub(super) fn show_bridge_progress(&self) {
        let Some(write) = self.panel.info_line_writer() else {
            return;
        };
        let last = Arc::new(std::sync::Mutex::new(String::new()));
        self.engines.on_progress_changed(Arc::new(move |bars| {
            let line = format_external_bars(bars);
            let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
            if *l != line {
                write(&line);
                *l = line;
            }
        }));
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
