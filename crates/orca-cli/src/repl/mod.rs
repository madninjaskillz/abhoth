//! The interactive loop (`ReplLoop`): the prompt, slash commands, `!` shortcuts and agent
//! turns with streaming output, Ctrl+C cancel, plan approval and session auto-save.

pub mod agent_footer;
pub mod command_parser;
pub mod engine;
pub mod line_editor;
pub mod observer;
pub mod panel;
pub mod permission;
pub mod shell;
pub mod state;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_core::agent::AgentSession;
use orca_core::client::{ChatMessage, ChatOptions, Role};
use orca_core::session::{CheckpointManager, SessionManager};

use crate::commands::{
    ASK_TOGGLE_SENTINEL, AgentHost, CLEAR_SENTINEL, CommandContext, CommandRegistry, CommandResult,
    PLAN_TOGGLE_SENTINEL,
};
use crate::custom_commands::CustomCommandLoader;
use crate::ui::Console;
use crate::ui::markup::escape;
use command_parser::{CommandParser, ParsedCommand, SlashCommand};
use line_editor::{Completion, LineEditor, ReadOutcome};
use panel::{StatusInfo, TerminalPanel};
use state::{InputMode, ReplState};

/// How the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplExit {
    /// /exit, end of input, or Ctrl+C twice.
    Quit,
    /// `/profile switch`: start again with this profile, resuming this session.
    Restart {
        profile: String,
        session_id: Option<String>,
    },
}

/// What one turn did.
enum TurnOutcome {
    Done,
    Cancelled,
    Failed,
}

pub struct Repl {
    pub console: Console,
    pub panel: TerminalPanel,
    pub state: ReplState,
    pub session: AgentSession,
    pub sessions: SessionManager,
    pub checkpoints: CheckpointManager,
    pub config_dir: PathBuf,
    pub registry: Arc<CommandRegistry>,
    pub custom: CustomCommandLoader,
    pub parser: CommandParser,
    pub agents: Option<Box<dyn AgentHost>>,
    /// The engine turns run on (`EngineHost`): native here, or a bridge CLI.
    pub engines: orca_core::engines::EngineHost,
    /// The inference slots (the Lead bar shows where its requests go).
    pub slots: Option<Arc<orca_core::slots::SlotRuntime>>,
    /// The footer's bars and the engine name on its Lead bar.
    footer: Option<agent_footer::FooterRenderer>,
    footer_engine: Arc<std::sync::Mutex<Option<String>>>,
    /// agent.planner: writes the plan before the first turn (`ReplLoop.Planner`).
    pub planner: Option<orca_core::orchestration::planner_phase::PlannerPhase>,
    /// Lines printed after the banner (profile notices, resume notices).
    pub startup_notices: Vec<String>,
    /// `config.ActiveProfile`: the inference profile in use.
    pub active_profile: Option<String>,
    editor: LineEditor,
    /// The kept text across a mode / verbosity cycle.
    saved_input: String,
    last_interrupt: Option<Instant>,
    /// `/retry`: the position in `lmStudio.models` requests go to.
    retry_index: usize,
    /// Pasted images and files waiting as chips in the prompt (`InputHandler.Attachments`).
    tray: orca_core::attachments::AttachmentTray,
    images_as_paths_notice_shown: bool,
    interactive: bool,
}

impl Repl {
    pub fn new(
        console: Console,
        session: AgentSession,
        config_dir: PathBuf,
        current_session_id: Option<String>,
    ) -> Self {
        let cwd = session.ctx.cwd.clone();
        let registry = Arc::new(crate::commands::registry());
        let custom = CustomCommandLoader::new(config_dir.clone(), cwd.clone());
        let mut parser = CommandParser::new();
        parser.set_command_registry(registry.clone());
        parser.set_custom_command_names(custom.names());
        let state = ReplState {
            current_session_id,
            ..Default::default()
        };
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        Self {
            panel: TerminalPanel::new(console.clone()),
            editor: LineEditor::new(console.clone()),
            console,
            state,
            session,
            sessions: SessionManager::new(&config_dir),
            checkpoints: CheckpointManager::new(config_dir.join("checkpoints")),
            config_dir,
            registry,
            custom,
            parser,
            agents: None,
            engines: orca_core::engines::EngineHost::new(),
            slots: None,
            footer: None,
            footer_engine: Arc::new(std::sync::Mutex::new(None)),
            planner: None,
            startup_notices: Vec::new(),
            active_profile: None,
            saved_input: String::new(),
            last_interrupt: None,
            retry_index: 0,
            tray: orca_core::attachments::AttachmentTray::new(
                orca_core::attachments::AttachmentStore::new(
                    orca_core::config::project_instructions::find_project_root(&cwd)
                        .unwrap_or_else(|| cwd.clone()),
                ),
            ),
            images_as_paths_notice_shown: false,
            interactive,
        }
    }

    /// `StatusInfoProvider.Get`.
    pub fn status_info(&self) -> StatusInfo {
        let config = &self.session.config;
        let cwd = &self.session.ctx.cwd;
        let mut profile = (!config.tool_profile.eq_ignore_ascii_case("openorca"))
            .then(|| config.tool_profile.clone());
        if let Some(inference) = self.active_profile.as_deref().filter(|p| !p.is_empty()) {
            profile = Some(match profile {
                None => inference.to_string(),
                Some(p) => format!("{p} · {inference}"),
            });
        }
        // A bridge engine's name stands where C# shows it on the Lead bar (D101).
        if let Some(engine) = orca_core::engines::EngineHost::footer_label(config) {
            profile = Some(match profile {
                None => engine.to_string(),
                Some(p) => format!("{p} · {engine}"),
            });
        }
        let window = config.context.context_window_size;
        let context_percent = (window > 0).then(|| {
            let used = self
                .session
                .conversation()
                .estimate_context_tokens(config.context.chars_per_token);
            ((100.0 * used as f64 / window as f64).round() as i64).clamp(0, 999) as i32
        });
        let directory = cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| cwd.to_string_lossy().to_string());
        StatusInfo {
            profile,
            context_percent,
            directory: Some(directory),
            branch: panel::read_git_branch(cwd),
            uncensored: config.agent.uncensored.is_on(),
        }
    }

    fn refresh_system_prompt(&mut self) {
        self.session.plan_mode = self.state.plan_mode();
        self.session.native_tools = self.session.resolve_native_tools();
        let prompt = self.session.default_system_prompt();
        self.session.set_system_prompt(prompt);
    }

    /// `RunAsync`.
    pub async fn run(&mut self) -> ReplExit {
        if self.interactive {
            self.console.clear();
            let status = self.status_info();
            self.panel.setup(&self.state, &status);
        }
        self.show_welcome_banner().await;
        for notice in std::mem::take(&mut self.startup_notices) {
            self.console
                .markup_line(&format!("[yellow]{}[/]", escape(&notice)));
        }
        if let Some(p) = &self.active_profile {
            self.console.markup_line(&format!(
                "[grey]Profile:[/] [cyan]{}[/] [dim]— /profile to list or switch[/]",
                escape(p)
            ));
        }
        // Foreground sub-agents ask here too (background ones are refused).
        self.session
            .set_prompter(Box::new(permission::ConsolePermissionPrompter::new(
                self.console.clone(),
            )));
        self.start_footer();
        self.refresh_system_prompt();
        // BrowserUncensored asks each chat server what it is (in the background) at start.
        if !self.session.config.demo_mode {
            let identity = orca_core::client::ServerIdentity::shared();
            for (_, url, key) in crate::commands::uncensored::endpoints(&self.session.config) {
                identity.warm(&url, key.as_deref());
            }
        }
        crate::render::links::configure(&self.session.config.ui.hyperlinks, self.console.is_ansi());
        let notice_console = self.console.clone();
        orca_core::client::tool_images::set_on_vision_refused(Some(Arc::new(move |text: &str| {
            notice_console.markup_line(&format!("[yellow]{}[/]", escape(text)));
        })));
        // A server that answers the /uncensored field with a 400 (speed_projection_not_supported)
        // likewise.
        let notice_console = self.console.clone();
        orca_core::client::uncensored_gate::set_on_field_refused(Some(Arc::new(
            move |text: &str| {
                notice_console.markup_line(&format!("[yellow]{}[/]", escape(text)));
            },
        )));
        let exit = self.input_loop().await;
        if exit == ReplExit::Quit {
            self.save_auto_memory().await;
        }
        self.footer = None;
        self.panel.teardown();
        // The bridge CLI ends with the session.
        self.engines.dispose().await;
        exit
    }

    async fn show_welcome_banner(&self) {
        crate::render::welcome::show_banner(&self.console, &self.session.config).await;
    }

    /// Read one input line: the line editor at a terminal, else stdin lines.
    async fn read_input(&mut self) -> Option<String> {
        if !self.interactive {
            return tokio::task::block_in_place(line_editor::read_non_interactive);
        }
        loop {
            let status = self.status_info();
            self.sync_footer_engine();
            self.panel.enter_input(&self.state, &status);
            let completion = Completion {
                command_names: self.parser.all_command_names(false),
                cwd: self.session.ctx.cwd.clone(),
            };
            let prompt = line_editor::prompt_markup(self.state.mode, false);
            let initial = std::mem::take(&mut self.saved_input);
            let ghost = self.state.suggested_next_prompt.take();
            let editor = &mut self.editor;
            let outcome = tokio::task::block_in_place(|| {
                editor.read_line(prompt, &initial, ghost.as_deref(), &completion)
            });
            self.panel.exit_input();
            match outcome {
                ReadOutcome::Line(line) => {
                    let trimmed = line.trim();
                    // Backslash continuation: keep reading on a new line.
                    if let Some(head) = trimmed.strip_suffix('\\') {
                        self.saved_input = format!("{head}\n");
                        continue;
                    }
                    self.editor.add_history(trimmed);
                    self.last_interrupt = None;
                    return Some(trimmed.to_string());
                }
                ReadOutcome::Interrupted => {
                    if self
                        .last_interrupt
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
                    {
                        return None;
                    }
                    self.last_interrupt = Some(Instant::now());
                    self.console
                        .markup_line("\n[yellow]Press Ctrl+C again to exit.[/]");
                }
                ReadOutcome::Eof => return None,
                ReadOutcome::ModeCycled(text) => {
                    self.state.cycle_mode();
                    self.session.plan_mode = self.state.plan_mode();
                    self.refresh_system_prompt();
                    self.saved_input = text;
                }
                ReadOutcome::VerbosityCycled(text) => {
                    self.state.cycle_verbosity();
                    self.saved_input = text;
                }
                ReadOutcome::PasteRequested(text) => {
                    let reader = orca_core::attachments::clipboard::create_default();
                    let cwd = self.session.ctx.cwd.to_string_lossy().to_string();
                    let pasted = self.tray.paste_from(reader.as_ref(), &cwd).await;
                    if let Some(n) = &pasted.notice {
                        self.console.markup_line(&format!("[dim]{}[/]", escape(n)));
                    }
                    self.saved_input = format!("{text}{}", pasted.insert);
                }
            }
        }
    }

    /// `RunInputLoopAsync`.
    async fn input_loop(&mut self) -> ReplExit {
        loop {
            let Some(input) = self.read_input().await else {
                return ReplExit::Quit;
            };
            if input.trim().is_empty() {
                continue;
            }
            if let Some(cmd) = input.strip_prefix('!').filter(|c| !c.is_empty()) {
                shell::run_shortcut(&self.console, cmd.trim(), &self.session.ctx.cwd).await;
                continue;
            }
            let command = self.parser.try_parse(&input);
            if command.is_none() && CommandParser::looks_like_command(&input) {
                self.console.markup_line(&format_user_echo(&input));
                let name = input.split(' ').next().unwrap_or(&input);
                self.console
                    .markup_line(&match self.parser.suggest_command(name) {
                        None => format!(
                            "[yellow]Unknown command {}[/] [dim]— /help lists commands[/]",
                            escape(name)
                        ),
                        Some(s) => format!(
                            "[yellow]Unknown command {}[/] [dim]— did you mean[/] [cyan]/{}[/][dim]?[/]",
                            escape(name),
                            escape(&s)
                        ),
                    });
                self.console.blank();
                continue;
            }
            if let Some(command) = command {
                let ask_with_question =
                    command.plugin_name.as_deref() == Some("ask") && !command.args.is_empty();
                if !ask_with_question {
                    self.console.markup_line(&format_user_echo(&input));
                }
                if ask_with_question {
                    let question = command.args.join(" ");
                    self.run_no_tools_turn(&question).await;
                    continue;
                }
                match self.handle_command(&command).await {
                    Handled::Exit => return ReplExit::Quit,
                    Handled::Restart(exit) => return exit,
                    Handled::Continue => continue,
                    Handled::RunTurn(prompt) => {
                        self.session.messages.push(ChatMessage::user(prompt));
                    }
                }
            } else {
                if self.state.mode == InputMode::Ask {
                    self.run_no_tools_turn(&input).await;
                    continue;
                }
                self.console.markup_line(&format_user_echo(&input));
                let base = self.session.ctx.cwd.clone();
                let (expanded, attached) =
                    crate::input_preprocessor::expand_file_references(&input, &base);
                if !attached.is_empty() {
                    self.console.markup_line(&format!(
                        "  [dim]⎿  Attached {}[/]",
                        escape(&attached.join(", "))
                    ));
                }
                let mut expanded = expanded;
                // Results of background agents (/agent) that finished since the last turn.
                if let Some(notes) = self
                    .agents
                    .as_ref()
                    .and_then(|h| h.manager())
                    .and_then(|m| m.drain_finished_user_agent_notes())
                    .filter(|n| !n.is_empty())
                {
                    self.console.markup_line(&attached_notes_line(&notes));
                    expanded = format!("{notes}\n\n[User message]\n{expanded}");
                }
                self.console.blank();
                if let Some(planner) = self
                    .planner
                    .as_mut()
                    .filter(|_| !orca_core::engines::EngineHost::is_bridge(&self.session.config))
                {
                    self.session.seed_sub_agents();
                    expanded = planner
                        .prepare_first_message(
                            &expanded,
                            &orca_core::orchestration::CancelToken::new(),
                        )
                        .await;
                    self.session.take_sub_agent_work();
                }
                let attachments = self.tray.take(&expanded);
                let message = if attachments.is_empty() {
                    ChatMessage::user(expanded)
                } else {
                    self.build_attachment_message(&expanded, &attachments).await
                };
                self.session.messages.push(message);
            }
            self.run_agent_turn().await;
            if self.state.plan_mode() {
                self.handle_plan_approval().await;
            }
            self.auto_save();
            self.console.blank();
        }
    }

    /// Run a turn for the user message already at the end of the conversation.
    async fn run_agent_turn(&mut self) -> TurnOutcome {
        self.run_agent_turn_as(None).await
    }

    /// [`Self::run_agent_turn`] in a turn mode (`None`: Plan in plan mode, else Normal; Ask for
    /// /ask and Ask mode). A bridge engine runs the turn when one is selected.
    async fn run_agent_turn_as(
        &mut self,
        mode: Option<orca_core::engines::AgentTurnMode>,
    ) -> TurnOutcome {
        if orca_core::engines::EngineHost::is_bridge(&self.session.config) {
            use orca_core::engines::AgentTurnMode;
            let mode = mode.unwrap_or(if self.state.plan_mode() {
                AgentTurnMode::Plan
            } else {
                AgentTurnMode::Normal
            });
            return self.run_bridge_turn(mode).await;
        }
        // run_turn adds the user message itself: take it back off.
        let Some(last) = self.session.messages.pop() else {
            return TurnOutcome::Failed;
        };
        let started = Instant::now();
        let tokens_before = self.session.total_output_tokens;
        let mut observer = observer::TurnObserver::new(
            self.console.clone(),
            self.state.clone(),
            self.session.registry.all(),
        );
        // Ctrl+C cancels the turn: a key in raw mode at a terminal (the watcher), or the
        // console signal.
        let watcher = self.interactive.then(KeyWatcher::start);
        let result = {
            let turn = self.session.run_turn_with_message(last, &mut observer);
            let key = async {
                match &watcher {
                    Some(w) => w.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                r = turn => Some(r),
                _ = tokio::signal::ctrl_c() => None,
                _ = key => None,
            }
        };
        drop(watcher);
        let suggestion = if result.is_some() {
            observer.finish()
        } else {
            observer.cancel();
            None
        };
        let outcome = match result {
            Some(Ok(_)) => {
                self.state.total_turns += 1;
                self.console.markup_line(&format_turn_footer(
                    started.elapsed(),
                    self.session.total_output_tokens - tokens_before,
                ));
                if self.state.mode == InputMode::Normal {
                    self.state.suggested_next_prompt = suggestion;
                }
                TurnOutcome::Done
            }
            None => {
                // AgentLoopRunner catches the cancel itself: the turn then ends normally.
                self.session.close_dangling_tool_calls();
                self.console
                    .markup_line("\n[yellow]Generation cancelled.[/]");
                self.state.total_turns += 1;
                self.console.markup_line(&format_turn_footer(
                    started.elapsed(),
                    self.session.total_output_tokens - tokens_before,
                ));
                TurnOutcome::Cancelled
            }
            Some(Err(e)) => {
                self.render_loop_error(&e);
                TurnOutcome::Failed
            }
        };
        let status = self.status_info();
        self.panel.redraw(&self.state, &status);
        outcome
    }

    /// `RunNoToolsTurnAsync`: /ask and ask mode, a turn without tools.
    async fn run_no_tools_turn(&mut self, question: &str) {
        self.console.markup_line(&format_user_echo(question));
        self.console.blank();
        self.session.messages.push(ChatMessage::user(question));
        let saved = std::mem::take(&mut self.session.registry);
        let bridge = orca_core::engines::EngineHost::is_bridge(&self.session.config);
        if bridge {
            // The bridge offers ask_user from the registry; Ask mode keeps the CLI tool-less.
            self.session.registry = saved;
            self.run_agent_turn_as(Some(orca_core::engines::AgentTurnMode::Ask))
                .await;
        } else {
            self.run_agent_turn().await;
            self.session.registry = saved;
        }
        self.console.blank();
    }

    /// `BuildAttachmentMessageAsync`.
    async fn build_attachment_message(
        &mut self,
        text: &str,
        attachments: &[orca_core::attachments::PromptAttachment],
    ) -> ChatMessage {
        use orca_core::attachments::AttachmentKind;
        let vision = if attachments.iter().any(|a| a.kind == AttachmentKind::Image) {
            orca_core::client::tool_images::for_main_model(&self.session.config).await
        } else {
            false
        };
        let cwd = self.session.ctx.cwd.to_string_lossy().to_string();
        let built = orca_core::attachments::message_builder::build(text, attachments, vision, &cwd);
        if built.images_as_paths > 0 && !vision && !self.images_as_paths_notice_shown {
            self.images_as_paths_notice_shown = true;
            self.console.markup_line(
                "[dim]  (This model doesn't take images, so attached images go as file paths for its tools. If it does, set [cyan]lmStudio.vision[/] to true.)[/]",
            );
            self.console.blank();
        }
        built.message
    }

    fn render_loop_error(&self, error: &orca_core::agent::AgentError) {
        render_loop_error(
            &self.console,
            error,
            &self.session.config.lm_studio.base_url,
        );
    }

    /// `HandlePlanApprovalAsync`.
    async fn handle_plan_approval(&mut self) {
        loop {
            if !looks_like_plan(self.session.last_assistant_response.as_deref()) {
                return;
            }
            self.console.blank();
            let choices = [
                "Approve & Execute".to_string(),
                "Modify plan".to_string(),
                "Discard".to_string(),
            ];
            let console = self.console.clone();
            let choice = tokio::task::block_in_place(|| {
                crate::ui::prompt::select(
                    &console,
                    "[cyan]Plan complete. What would you like to do?[/]",
                    &choices,
                )
            });
            match choice {
                Some(0) => {
                    self.state.set_plan_mode(false);
                    self.console
                        .markup_line("[grey]Plan mode disabled[/] — back to normal mode.");
                    let plan = self
                        .session
                        .last_assistant_response
                        .clone()
                        .unwrap_or_default();
                    let artifact = self.save_plan_artifact(&plan);
                    self.refresh_system_prompt();
                    let mut msg = "The plan is approved. Please execute it now. Make all the changes described in the plan above.".to_string();
                    if let Some(p) = artifact {
                        msg.push_str(&format!(" The plan is also saved at {p} for reference."));
                    }
                    self.session.messages.push(ChatMessage::user(msg));
                    self.run_agent_turn().await;
                    return;
                }
                Some(1) => {
                    let console = self.console.clone();
                    let note = tokio::task::block_in_place(|| {
                        crate::ui::prompt::text(
                            &console,
                            "[yellow]What should change in the plan?[/]",
                        )
                    })
                    .unwrap_or_default();
                    if note.trim().is_empty() {
                        return;
                    }
                    self.session.messages.push(ChatMessage::user(format!(
                        "Revise the plan with this feedback (stay in planning — do not execute yet): {note}"
                    )));
                    self.run_agent_turn().await;
                }
                _ => {
                    self.console.markup_line("[grey]Plan discarded.[/]");
                    return;
                }
            }
        }
    }

    fn save_plan_artifact(&self, plan: &str) -> Option<String> {
        let dir = self.session.ctx.cwd.join(".orca").join("plans");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!(
            "plan-{}.md",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ));
        std::fs::write(&path, plan).ok()?;
        let shown = path.to_string_lossy().to_string();
        self.console
            .markup_line(&format!("[grey]Plan saved: {}[/]", escape(&shown)));
        Some(shown)
    }

    /// `AutoSaveSessionAsync`.
    fn auto_save(&mut self) {
        if !self.session.config.session.auto_save {
            return;
        }
        match self.sessions.save_with_engine(
            &self.session.messages,
            None,
            self.state.current_session_id.as_deref(),
            &self.state.engine,
        ) {
            Ok(id) => {
                self.session.tool_hooks.session_id = Some(id.clone());
                self.state.current_session_id = Some(id);
            }
            Err(e) => tracing::warn!("Failed to auto-save session: {e}"),
        }
    }

    /// `SaveAutoMemoryAsync`: after two or more turns, ask the model for learnings worth keeping.
    async fn save_auto_memory(&mut self) {
        let config = &self.session.config;
        if !config.memory.auto_memory_enabled
            || orca_core::engines::selected_bridge(config).is_some()
            || self.state.total_turns < 2
        {
            return;
        }
        let mut messages = vec![ChatMessage::system(
            "Based on this session, what project-specific patterns, conventions, or learnings should be remembered? Write concise bullet points. Only include things that would be useful in future sessions. If there's nothing noteworthy to remember, respond with just 'NONE'.",
        )];
        let relevant: Vec<&ChatMessage> = self
            .session
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User | Role::Assistant))
            .collect();
        for m in relevant.iter().skip(relevant.len().saturating_sub(10)) {
            let text = m.content.clone().unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            let units: Vec<u16> = text.encode_utf16().collect();
            let text = if units.len() > 2000 {
                format!("{}...", String::from_utf16_lossy(&units[..2000]))
            } else {
                text
            };
            let mut msg = ChatMessage::user(text);
            msg.role = m.role;
            messages.push(msg);
        }
        let options = ChatOptions {
            temperature: Some(0.3),
            max_output_tokens: Some(500),
            model: config.lm_studio.primary_model().map(String::from),
            ..Default::default()
        };
        let request = self.session.client.complete("Lead", &messages, &options);
        let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(30), request).await else {
            return;
        };
        let learnings = response.text.trim().to_string();
        if !learnings.is_empty() && !learnings.to_uppercase().starts_with("NONE") {
            let memory = orca_core::config::memory_manager::MemoryManager::new(config)
                .with_cwd(self.session.ctx.cwd.clone());
            if memory.save_learnings(&learnings).is_ok() {
                self.console
                    .markup_line("[dim]Saved session learnings to memory.[/]");
            }
        }
    }

    /// `CommandHandler.HandleCommandAsync` plus the commands the loop handles itself.
    async fn handle_command(&mut self, command: &ParsedCommand) -> Handled {
        match command.command {
            SlashCommand::PluginCommand => {
                let name = command.plugin_name.clone().unwrap_or_default();
                let Some(cmd) = self.registry.resolve(&name) else {
                    return Handled::Continue;
                };
                let result = self.execute(cmd.as_ref(), &command.args).await;
                self.handle_command_result(result).await
            }
            SlashCommand::CustomCommand => {
                // Decision D60: the rendered template is the prompt (C# drops it).
                let Some(cmd) = self.registry.resolve("customcommand") else {
                    return Handled::Continue;
                };
                match self.execute(cmd.as_ref(), &command.args).await {
                    CommandResult::RunAgent(prompt) => Handled::RunTurn(prompt),
                    _ => Handled::Continue,
                }
            }
            SlashCommand::Retry => {
                let next = self.retry_index + 1;
                let Some(model) = self.session.config.lm_studio.models.get(next).cloned() else {
                    self.console.markup_line(
                        "[red]No backup model configured. Set one in [cyan]/config[/] → LLM Server Settings.[/]",
                    );
                    return Handled::Continue;
                };
                let mut conversation = self.session.conversation();
                if conversation.remove_last_turns(1) == 0 {
                    self.console
                        .markup_line("[yellow]Nothing to retry — no previous turn found.[/]");
                    return Handled::Continue;
                }
                // The last user message comes back for the retried turn.
                let prompt = self
                    .session
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .and_then(|m| m.content.clone())
                    .unwrap_or_default();
                self.session.set_conversation(conversation);
                self.retry_index = next;
                self.session.client.settings_mut().default_model = Some(model.clone());
                // The request names the backup model (C# keeps sending lmStudio.models[0];
                // D127).
                self.session.primary_model_override = Some(model.clone());
                self.console.markup_line(&format!(
                    "[yellow]Retrying last turn with backup model: {}[/]",
                    escape(&model)
                ));
                Handled::RunTurn(prompt)
            }
            SlashCommand::Config => {
                let mut ctx = self.context();
                if ctx.session.config.demo_mode {
                    let (config, dir) = (ctx.session.config.clone(), ctx.config_dir.to_path_buf());
                    crate::commands::config_editor::render_config_table(ctx.console, &config, &dir);
                } else {
                    let dir = ctx.config_dir.to_path_buf();
                    crate::commands::config_editor::show_config(&mut ctx, &dir).await;
                }
                Handled::Continue
            }
            SlashCommand::Benchmark => {
                let mut ctx = self.context();
                crate::commands::benchmark_cmd::handle(&mut ctx, &command.args).await;
                Handled::Continue
            }
            SlashCommand::Profile => {
                let restart = {
                    let mut ctx = self.context();
                    crate::commands::profile_cmd::handle(&mut ctx, &command.args).await
                };
                let Some(restart) = restart else {
                    return Handled::Continue;
                };
                let session_id = if self.session.messages.iter().any(|m| m.role == Role::User) {
                    self.sessions
                        .save(
                            &self.session.messages,
                            None,
                            self.state.current_session_id.as_deref(),
                        )
                        .ok()
                } else {
                    None
                };
                Handled::Restart(ReplExit::Restart {
                    profile: restart.profile,
                    session_id,
                })
            }
            SlashCommand::Engine => {
                self.handle_engine_command(&command.args).await;
                Handled::Continue
            }
        }
    }

    fn context(&mut self) -> CommandContext<'_> {
        CommandContext {
            console: &self.console,
            session: &mut self.session,
            state: &mut self.state,
            sessions: &self.sessions,
            checkpoints: &self.checkpoints,
            config_dir: &self.config_dir,
            registry: &self.registry,
            custom: &self.custom,
            agents: self.agents.as_deref(),
        }
    }

    async fn execute(
        &mut self,
        cmd: &dyn crate::commands::Command,
        args: &[String],
    ) -> CommandResult {
        self.session.seed_sub_agents();
        let result = {
            let mut ctx = self.context();
            cmd.execute(&mut ctx, args).await
        };
        // Sub-agents a command ran (`/review`, `/workflow`, `/agent`) record their calls.
        self.session.take_sub_agent_work();
        result
    }

    /// `HandleCommandResultAsync`.
    async fn handle_command_result(&mut self, result: CommandResult) -> Handled {
        match result {
            CommandResult::Ok => Handled::Continue,
            CommandResult::Exit => Handled::Exit,
            CommandResult::RunAgent(prompt) => match prompt.as_str() {
                CLEAR_SENTINEL => {
                    self.session.messages.clear();
                    self.engines.reset().await;
                    self.state.engine = Default::default();
                    self.refresh_system_prompt();
                    self.panel.teardown();
                    self.console.clear();
                    let status = self.status_info();
                    if self.interactive {
                        self.panel.setup(&self.state, &status);
                    }
                    crate::render::welcome::show_minimal(&self.console);
                    Handled::Continue
                }
                ASK_TOGGLE_SENTINEL => {
                    self.state.mode = if self.state.mode == InputMode::Ask {
                        InputMode::Normal
                    } else {
                        InputMode::Ask
                    };
                    if self.state.mode == InputMode::Ask {
                        self.console.markup_line(
                            "[magenta]Ask mode enabled[/] — responses will not use tools.",
                        );
                    } else {
                        self.console
                            .markup_line("[grey]Ask mode disabled[/] — back to normal mode.");
                    }
                    Handled::Continue
                }
                PLAN_TOGGLE_SENTINEL => {
                    self.refresh_system_prompt();
                    Handled::Continue
                }
                _ => {
                    // C# runs this turn without the footer, plan approval or auto-save.
                    self.session.messages.push(ChatMessage::user(prompt));
                    self.run_agent_turn().await;
                    Handled::Continue
                }
            },
        }
    }
}

/// Watches the keyboard during a turn (raw mode) for Ctrl+C; stands back while a prompt
/// (permission, plan approval) reads keys.
struct KeyWatcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    cancel: Arc<tokio::sync::Notify>,
    fired: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyWatcher {
    fn start() -> Self {
        use std::sync::atomic::Ordering;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = Arc::new(tokio::sync::Notify::new());
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, c, f) = (stop.clone(), cancel.clone(), fired.clone());
        let thread = std::thread::spawn(move || {
            use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
            let _raw = crate::ui::prompt::RawGuard::enable_for_watcher();
            let tick = Duration::from_millis(30);
            while !s.load(Ordering::SeqCst) {
                if crate::ui::prompt::prompt_reading() {
                    std::thread::sleep(tick);
                    continue;
                }
                if !crossterm::event::poll(tick).unwrap_or(false)
                    || crate::ui::prompt::prompt_reading()
                {
                    continue;
                }
                if let Ok(Event::Key(k)) = crossterm::event::read()
                    && k.kind != KeyEventKind::Release
                    && k.code == KeyCode::Char('c')
                    && k.modifiers.contains(KeyModifiers::CONTROL)
                {
                    f.store(true, Ordering::SeqCst);
                    c.notify_one();
                    break;
                }
            }
        });
        Self {
            stop,
            cancel,
            fired,
            thread: Some(thread),
        }
    }

    async fn cancelled(&self) {
        if self.fired.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.cancel.notified().await;
    }
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Handled {
    Continue,
    Exit,
    Restart(ReplExit),
    /// Run an agent turn with this user message.
    RunTurn(String),
}

/// How a failed turn is shown (`AgentLoopRunner`'s error rendering).
pub(crate) fn render_loop_error(
    console: &Console,
    error: &orca_core::agent::AgentError,
    base_url: &str,
) {
    let (message, hint) = crate::render::friendly_errors::describe(error, base_url);
    console.markup_line(&format!("\n[red]✗ {}[/]", escape(&message)));
    if let Some(h) = hint {
        console.markup_line(&format!("  [dim]{}[/]", escape(&h)));
    }
}

/// `FormatUserEcho`: the submitted prompt, bold after a blue chevron.
pub fn format_user_echo(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.split('\n');
    let mut out = format!(
        "[blue bold]❯[/] [bold]{}[/]",
        escape(lines.next().unwrap_or(""))
    );
    for l in lines {
        out.push_str(&format!("\n  [bold]{}[/]", escape(l)));
    }
    out
}

/// `FormatTurnFooter`: "✻ 12.4s · 1,234 tokens".
pub fn format_turn_footer(elapsed: Duration, output_tokens: i64) -> String {
    let secs = elapsed.as_secs_f64();
    let time = if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m {:02}s", (secs / 60.0) as u64, elapsed.as_secs() % 60)
    };
    let tokens = if output_tokens > 0 {
        format!(
            " · {} token{}",
            crate::commands::n0(output_tokens),
            if output_tokens == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    format!("[dim]✻ {time}{tokens}[/]")
}

/// `LooksLikePlan`: at least 200 characters, more than one line, not ending in a question.
pub fn looks_like_plan(text: Option<&str>) -> bool {
    let Some(text) = text else {
        return false;
    };
    if text.trim().is_empty() || text.encode_utf16().count() < 200 {
        return false;
    }
    let trimmed = text.trim_end();
    !trimmed.ends_with('?') && trimmed.contains('\n')
}

/// `AgentBadge.AttachedNotesLine`: which finished background agents' results go with the
/// message.
pub fn attached_notes_line(notes: &str) -> String {
    let mut tags: Vec<String> = Vec::new();
    for line in notes.lines() {
        if let Some(rest) = line.strip_prefix("[Agent ")
            && let Some(letter) = rest.chars().next()
            && letter.is_ascii_uppercase()
            && letter != 'L'
            && rest[letter.len_utf8()..].starts_with(" \u{2014}")
        {
            let tag = format!("Agent {letter}");
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    if tags.is_empty() {
        return "[dim]  (attached results of finished background agents)[/]".into();
    }
    format!(
        "[dim]  (attached results of finished background agents:[/] {}[dim])[/]",
        crate::render::agent_badge::decorate_markup(&tags.join(", "), Some("dim"), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_echo_marks_each_line() {
        assert_eq!(
            crate::ui::markup::remove(&format_user_echo("fix [this]\nand that")),
            "❯ fix [this]\n  and that"
        );
    }

    #[test]
    fn turn_footer_formats_time_and_tokens() {
        assert_eq!(
            format_turn_footer(Duration::from_millis(12_400), 1234),
            "[dim]✻ 12.4s · 1,234 tokens[/]"
        );
        assert_eq!(
            format_turn_footer(Duration::from_secs(75), 1),
            "[dim]✻ 1m 15s · 1 token[/]"
        );
        assert_eq!(
            format_turn_footer(Duration::from_secs(2), 0),
            "[dim]✻ 2.0s[/]"
        );
    }

    #[test]
    fn looks_like_plan_needs_length_lines_and_no_question() {
        let long = format!("Step 1\n{}", "x".repeat(200));
        assert!(looks_like_plan(Some(&long)));
        assert!(!looks_like_plan(Some(&format!("{long}?"))));
        assert!(!looks_like_plan(Some(&"x".repeat(300))));
        assert!(!looks_like_plan(Some("short\nplan")));
        assert!(!looks_like_plan(None));
    }
}
