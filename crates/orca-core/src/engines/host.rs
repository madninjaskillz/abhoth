//! Runs a session's turns on the selected bridge engine and switches engines mid-session
//! (the front-end-neutral part of C# `EngineHost`). The REPL and the web UI each wrap it with
//! their own event sink (rendering / browser events); the native loop stays with the caller.
//!
//! A bridge engine is created on its first turn and kept for the session (one CLI process);
//! its progress.js bars (the `openorca-<session>` bucket) are followed while it lives.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use super::claude::{ClaudeBridgeEngine, ClaudeBridgeOptions};
use super::codex::{CodexBridgeEngine, CodexBridgeOptions};
use super::transport::BridgeTransportFactory;
use super::{
    AgentEngine, AgentEngines, AgentEventSink, AgentTurn, AgentTurnMode, CancelToken,
    EngineAuthRequired, EngineError, EngineReadiness, EngineReadinessState, HostToolSpec,
    readiness,
};
use crate::chat::Conversation;
use crate::client::Role;
use crate::config::OrcaConfig;
use crate::progress::{self, ProgressBucketWatcher, ProgressFileBar};

/// Result of [`EngineHost::switch`]: switched, or why not (`EngineSwitchResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSwitchResult {
    pub switched: bool,
    pub error: Option<String>,
    pub readiness: Option<EngineReadiness>,
}

/// What a new bridge is started with (the C# `BridgeFactory` arguments plus what
/// `CreateBridge` reads).
#[derive(Debug, Clone)]
pub struct BridgeStart {
    pub id: &'static str,
    /// The CLI session to continue (the conversation came from the same engine).
    pub resume: Option<String>,
    /// PROGRESS_SESSION for the CLI.
    pub progress_session: String,
    pub cwd: String,
    pub host_tools: Vec<HostToolSpec>,
}

/// Builds bridge engines (tests swap in fakes).
pub type BridgeFactory = Box<dyn FnMut(&BridgeStart) -> Box<dyn AgentEngine> + Send>;

/// Checks a bridge CLI before it is enabled (tests swap in fakes).
pub type ReadinessCheck = Box<
    dyn Fn(&str, &OrcaConfig) -> BoxFuture<'static, Result<EngineReadiness, EngineError>>
        + Send
        + Sync,
>;

/// Called with the bridge's bars when they change.
pub type ProgressListener = Arc<dyn Fn(&[ProgressFileBar]) + Send + Sync>;
type Listener = ProgressListener;
type AuthListener = Arc<dyn Fn(&EngineAuthRequired) + Send + Sync>;

/// One bridge turn (`EngineHost.RunTurnAsync` with a bridge selected).
pub struct BridgeTurnRequest<'a> {
    pub config: &'a OrcaConfig,
    /// The working directory the CLI runs in.
    pub cwd: &'a Path,
    /// The session's conversation; the user's message is its last user message.
    pub conversation: &'a mut Conversation,
    /// The front end's session id (the progress bucket); `None`: a random 8-hex id.
    pub session_id: Option<&'a str>,
    /// Host tools offered to the CLI (ask_user).
    pub host_tools: Vec<HostToolSpec>,
    pub sink: &'a mut dyn AgentEventSink,
    pub mode: AgentTurnMode,
}

/// Runs bridge turns and switches engines (`EngineHost`). The selected engine is
/// `config.agent.engine`; the native engine runs in the caller.
#[derive(Default)]
pub struct EngineHost {
    bridge: Option<Box<dyn AgentEngine>>,
    progress: Option<ProgressBucketWatcher>,
    progress_listeners: Arc<Mutex<Vec<Listener>>>,
    auth_listeners: Vec<AuthListener>,
    /// Builds bridges (tests).
    pub bridge_factory: Option<BridgeFactory>,
    /// Checks readiness (tests).
    pub readiness_check: Option<ReadinessCheck>,
    /// Where progress.js buckets live (`None` = the default roots; tests).
    pub progress_roots: Option<Vec<PathBuf>>,
}

impl EngineHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// The selected engine's id ("native", "claude", "codex").
    pub fn engine_id(config: &OrcaConfig) -> &'static str {
        AgentEngines::normalize(Some(&config.agent.engine)).unwrap_or(AgentEngines::NATIVE)
    }

    pub fn is_bridge(config: &OrcaConfig) -> bool {
        Self::engine_id(config) != AgentEngines::NATIVE
    }

    /// The footer's label in place of slot and model ("claude-code"), `None` for native.
    pub fn footer_label(config: &OrcaConfig) -> Option<&'static str> {
        Self::is_bridge(config).then(|| AgentEngines::display_name_of(Self::engine_id(config)))
    }

    /// Bars the bridged CLI drew with progress.js, oldest first.
    pub fn progress_bars(&self) -> Vec<ProgressFileBar> {
        self.progress
            .as_ref()
            .map(ProgressBucketWatcher::bars)
            .unwrap_or_default()
    }

    /// The bucket the bridged CLI's progress.js writes to (`None` before a bridge starts).
    pub fn progress_session(&self) -> Option<String> {
        self.progress.as_ref().map(|p| p.session().to_string())
    }

    /// Called (on any thread) with the new bars when [`progress_bars`](Self::progress_bars)
    /// change (an empty list when the bridge ends).
    pub fn on_progress_changed(&self, listener: ProgressListener) {
        lock(&self.progress_listeners).push(listener);
    }

    /// `AuthRequired`: a bridge was asked for but its CLI isn't logged in.
    pub fn on_auth_required(&mut self, listener: Arc<dyn Fn(&EngineAuthRequired) + Send + Sync>) {
        self.auth_listeners.push(listener);
    }

    fn raise_auth(&self, readiness: &EngineReadiness, id: &str) {
        if readiness.state != EngineReadinessState::NotLoggedIn {
            return;
        }
        let Some(login) = readiness.login_command.clone() else {
            return;
        };
        let auth = EngineAuthRequired {
            engine_id: id.to_string(),
            message: readiness.message.clone(),
            login_command: login,
        };
        for l in &self.auth_listeners {
            l(&auth);
        }
    }

    /// Is `engine_id` usable now (installed, new enough, logged in)? A check that fails
    /// outright reads as an `Error` readiness with its message.
    pub async fn check(&self, engine_id: &str, config: &OrcaConfig) -> EngineReadiness {
        let result = match &self.readiness_check {
            Some(check) => check(engine_id, config).await,
            None => readiness::check(engine_id, config, &CancelToken::new()).await,
        };
        result.unwrap_or_else(|e| EngineReadiness {
            engine_id: engine_id.to_string(),
            state: EngineReadinessState::Error,
            message: e.to_string(),
            executable: None,
            version: None,
            login_command: None,
        })
    }

    /// Switch engines mid-session (`SwitchAsync`). A bridge is checked first; until that passes
    /// the current engine stays. A switch ends the current bridge, and the caller forgets the
    /// conversation's engine session (the next turn starts a fresh one). Not logged in raises
    /// the auth listeners.
    pub async fn switch(&mut self, name: &str, config: &mut OrcaConfig) -> EngineSwitchResult {
        let Some(id) = AgentEngines::normalize(Some(name)) else {
            return EngineSwitchResult {
                switched: false,
                error: Some(format!(
                    "Unknown engine '{name}'. Engines: {}.",
                    AgentEngines::ALL.join(", ")
                )),
                readiness: None,
            };
        };
        let mut ready = None;
        if id != AgentEngines::NATIVE {
            let r = self.check(id, config).await;
            if !r.is_ready() {
                self.raise_auth(&r, id);
                return EngineSwitchResult {
                    switched: false,
                    error: Some(r.message.clone()),
                    readiness: Some(r),
                };
            }
            ready = Some(r);
        }
        self.drop_bridge().await;
        config.agent.engine = id.to_string();
        EngineSwitchResult {
            switched: true,
            error: None,
            readiness: ready,
        }
    }

    /// At startup with a bridge selected: check it; when it isn't usable, fall back to native
    /// (`VerifySelectedAsync`). `None` when native is selected.
    pub async fn verify_selected(&mut self, config: &mut OrcaConfig) -> Option<EngineReadiness> {
        if !Self::is_bridge(config) {
            return None;
        }
        let wanted = Self::engine_id(config);
        let r = self.check(wanted, config).await;
        if !r.is_ready() {
            config.agent.engine = AgentEngines::NATIVE.to_string();
            self.raise_auth(&r, wanted);
        }
        Some(r)
    }

    /// Forget the bridge session (/clear): the next turn starts a new CLI session.
    pub async fn reset(&mut self) {
        self.drop_bridge().await;
    }

    /// End the CLI process (`DisposeAsync`).
    pub async fn dispose(&mut self) {
        self.drop_bridge().await;
    }

    /// Run one turn on the selected bridge. `Err(Unavailable)`: the CLI can't run (the bridge
    /// is dropped; the caller shows `✗ message` and the turn ends normally, as in C#).
    /// `Err(Cancelled)` after `cancel` fired; `Err(Failed)` for other failures.
    pub async fn run_bridge_turn(
        &mut self,
        req: BridgeTurnRequest<'_>,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        let BridgeTurnRequest {
            config,
            cwd,
            conversation,
            session_id,
            host_tools,
            sink,
            mode,
        } = req;
        let id = Self::engine_id(config);
        if id == AgentEngines::NATIVE {
            return Err(EngineError::Failed(
                "The native engine runs in the host.".into(),
            ));
        }
        let user_text = last_user_text(conversation);
        if self.bridge.as_ref().is_none_or(|b| b.id() != id) {
            // Continue the CLI's session when this conversation came from the same engine.
            let resume = (conversation.engine_id.as_deref() == Some(id))
                .then(|| conversation.engine_session_id.clone())
                .flatten();
            let bucket = session_id.map_or_else(random_bucket_id, str::to_string);
            let start = BridgeStart {
                id,
                resume,
                progress_session: progress::bridge_session(&bucket),
                cwd: cwd.to_string_lossy().into_owned(),
                host_tools,
            };
            let engine = match self.bridge_factory.as_mut() {
                Some(f) => f(&start),
                None => create_bridge(config, &start, None),
            };
            self.bridge = Some(engine);
            self.start_progress(&start.progress_session, config);
        }
        let Some(engine) = self.bridge.as_mut() else {
            return Err(EngineError::Failed("No bridge.".into()));
        };
        let turn = AgentTurn::new(conversation, user_text, sink).with_mode(mode);
        let result = engine.run_turn(turn, cancel).await;
        if matches!(result, Err(EngineError::Unavailable(_))) {
            self.drop_bridge().await;
        }
        result
    }

    fn start_progress(&mut self, session: &str, config: &OrcaConfig) {
        let cli = if Self::engine_id(config) == AgentEngines::CLAUDE {
            &config.bridges.claude
        } else {
            &config.bridges.codex
        };
        if !cli.progress_bars {
            return;
        }
        self.stop_progress();
        let watcher = ProgressBucketWatcher::new(session, self.progress_roots.clone(), None);
        let listeners = self.progress_listeners.clone();
        watcher.on_changed(Box::new(move |bars| notify(&listeners, bars)));
        self.progress = Some(watcher);
    }

    fn stop_progress(&mut self) {
        let Some(w) = self.progress.take() else {
            return;
        };
        let session = w.session().to_string();
        drop(w);
        progress::clear(&session, self.progress_roots.as_deref());
        notify(&self.progress_listeners, &[]);
    }

    async fn drop_bridge(&mut self) {
        let old = self.bridge.take();
        self.stop_progress();
        if let Some(mut old) = old {
            old.dispose().await;
        }
    }
}

fn notify(listeners: &Mutex<Vec<Listener>>, bars: &[ProgressFileBar]) {
    let list = lock(listeners).clone();
    for l in list {
        l(bars);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `CreateBridge`: a Claude Code or Codex bridge from `bridges.*` (a transport factory for tests).
pub fn create_bridge(
    config: &OrcaConfig,
    start: &BridgeStart,
    test_factory: Option<BridgeTransportFactory>,
) -> Box<dyn AgentEngine> {
    let skip_version_check = test_factory.is_some();
    if start.id == AgentEngines::CLAUDE {
        let mut o = ClaudeBridgeOptions::from_config(
            &config.bridges.claude,
            &start.cwd,
            start.resume.as_deref(),
            Some(&start.progress_session),
        );
        o.host_tools = start.host_tools.clone();
        o.skip_version_check = skip_version_check;
        Box::new(ClaudeBridgeEngine::new(o, test_factory))
    } else {
        let mut o = CodexBridgeOptions::from_config(
            &config.bridges.codex,
            &start.cwd,
            start.resume.as_deref(),
            Some(&start.progress_session),
        );
        o.host_tools = start.host_tools.clone();
        o.skip_version_check = skip_version_check;
        Box::new(CodexBridgeEngine::new(o, test_factory))
    }
}

/// The text of the conversation's last user message.
pub fn last_user_text(conversation: &Conversation) -> String {
    conversation
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// C#: the first 8 hex digits of a new GUID.
pub fn random_bucket_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as u64 ^ u64::from(std::process::id())).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", mixed >> 32)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
