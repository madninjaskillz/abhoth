//! `--hostui` (headless): the web UI drives the agent loop (native or a bridge engine). Prints the URL and one line
//! per prompt and finished turn; Ctrl+C cancels a running turn, else stops the server.
//! Other projects opened in the browser get their own in-process session (C# starts a worker
//! process per project).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// `println!` that ignores a closed stdout: the host keeps serving when whoever started it stops
/// reading (a test harness, a closed terminal, a pipe), where `println!` would panic (D135).
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// `eprintln!` that ignores a closed stderr (see `say!`).
macro_rules! say_err {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

use anyhow::Result;
use orca_abstractions::ToolContext;
use orca_core::agent::AgentSession;
use orca_core::config::OrcaConfig;
use orca_core::config::schema::ProfileOverlay;
use orca_core::live::{LiveEvent, LiveEventBus, types};
use orca_webui::hostui::HostUiArgs;
use orca_webui::native::{AgentLiveSession, NativeOptions, QuestionBroker, Rebuilt};
use orca_webui::{LiveSession, SessionFactory, WebUiOptions, WebUiServer};

use crate::{ToolServices, build_registry_in};

/// `ask_user` answered in the browser (`LiveQuestionBroker`).
struct BrowserAsker(Arc<QuestionBroker>);

#[async_trait::async_trait]
impl orca_tools_utility::UserPrompter for BrowserAsker {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        self.0.ask(question, options).await
    }
}

/// A web UI session for `cwd`: its questions go to `questions`' browser. It starts without a
/// model too (a fresh install): turns then ask for one to be set in the settings (D116).
fn browser_session(
    config: &OrcaConfig,
    cwd: &Path,
    questions: &Arc<QuestionBroker>,
) -> Result<(AgentSession, ToolServices)> {
    let client = crate::wire_host_client(config, orca_core::logging::process_sink());
    let (registry, services) =
        crate::build_registry_with(config, cwd, Arc::new(BrowserAsker(questions.clone())));
    let mut session = AgentSession::new(config.clone(), client, registry, ToolContext::new(cwd));
    session.startup_probes = true;
    Ok((session, services))
}

/// The config a project session starts from with `profile` and `mode` (C# starts its worker
/// with `--profile` / `--mode`): config.json, the profile, the mode; this process's runtime
/// flags (--sandbox, --allow-dir, --demo) carry over.
async fn config_for(
    base: &OrcaConfig,
    profile: Option<&str>,
    mode: Option<&str>,
) -> Result<(OrcaConfig, Option<ProfileOverlay>), String> {
    let dir = orca_core::config::default_config_dir();
    let mut manager = orca_core::config::ConfigManager::new(Some(&dir));
    manager.load().map_err(|e| e.to_string())?;
    if let Some(name) = profile.filter(|p| !p.is_empty()) {
        let mut notices = Vec::new();
        if let Some(e) =
            orca_core::profiles::apply_at_startup(&mut manager, &dir, name, &mut notices).await
        {
            return Err(e);
        }
    }
    if let Some(m) = mode.filter(|m| !m.is_empty()) {
        manager.apply_mode(m)?;
    }
    let overlay = ProfileOverlay::of(&manager);
    let mut config = manager.config.clone();
    config.demo_mode = base.demo_mode;
    config.sandbox_mode = base.sandbox_mode;
    config.allowed_directory = base.allowed_directory.clone();
    Ok((config, overlay))
}

/// [`session_for`] with its usage lines going to `log` (a bench run: its own home's log).
fn session_logged(
    config: &OrcaConfig,
    cwd: &Path,
    log: Option<Arc<dyn orca_core::logging::LogSink>>,
) -> Result<(AgentSession, ToolServices)> {
    let (client, _) = crate::wire_client(config, log, false)?;
    let (registry, services) = build_registry_in(config, cwd);
    let mut session = AgentSession::new(config.clone(), client, registry, ToolContext::new(cwd));
    session.startup_probes = true;
    Ok((session, services))
}

/// The client a session started without a model gets once the settings name one (D116).
fn client_factory() -> orca_webui::native::ClientFactory {
    Arc::new(|config: &OrcaConfig| {
        crate::wire_host_client(config, orca_core::logging::process_sink())
    })
}

/// Start the live session over `session` with its sub-agents wired (`ToolWiring`): the agent
/// tools, and the agent tabs and bars on `bus`.
fn start_live(
    mut session: AgentSession,
    services: &ToolServices,
    bus: Arc<LiveEventBus>,
    mode: &str,
    mut options: NativeOptions,
) -> Arc<AgentLiveSession> {
    let config_dir = orca_core::config::manager::default_config_dir();
    let mut web_host = None;
    let agents: Option<Arc<dyn orca_webui::live::AgentHost>> = match crate::wire_agents(
        &mut session,
        &services.agent_tools,
        Some(&config_dir),
        true,
        None,
    ) {
        Ok(manager) => {
            let b = bus.clone();
            let host = Arc::new(crate::agent_host::WebAgentHost::new(
                manager.orchestrator().clone(),
                session.config.clone(),
                Arc::new(move || b.in_turn()),
            ));
            web_host = Some(host.clone());
            Some(host)
        }
        Err(e) => {
            tracing::warn!("web UI session: no sub-agents: {e}");
            None
        }
    };
    options.agents = agents;
    if options.client_factory.is_none() {
        options.client_factory = Some(client_factory());
    }
    let live = AgentLiveSession::start_with(session, bus.clone(), mode, options);
    if let Some(host) = web_host {
        host.attach(bus, Some(live.agents_decorator()));
    }
    live
}

fn clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Local time is not worth a dependency: UTC, as hh:mm:ss.
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

fn one_line(text: &str, max: usize) -> String {
    let s = text.replace("\r\n", " ").replace(['\n', '\r'], " ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The activity line for an event, as HostUiStartup prints it.
pub fn activity_line(ev: &LiveEvent) -> Option<String> {
    let d = &ev.data;
    let at = clock();
    match ev.kind.as_str() {
        types::USER => Some(format!(
            "[{at}] prompt ({}): {}",
            d["source"].as_str().unwrap_or(""),
            one_line(d["text"].as_str().unwrap_or(""), 80)
        )),
        types::TURN_END => Some(format!(
            "[{at}] turn {} in {:.1}s · {} tokens",
            d["outcome"].as_str().unwrap_or(""),
            d["elapsedMs"].as_f64().unwrap_or(0.0) / 1000.0,
            orca_webui::util::group_thousands(d["tokens"].as_u64().unwrap_or(0) as usize)
        )),
        types::PERMISSION => Some(format!(
            "[{at}] waiting for approval of {} in the browser",
            d["tool"].as_str().unwrap_or("")
        )),
        types::NOTICE if d["level"] == "error" => Some(format!(
            "[{at}] error: {}",
            one_line(d["text"].as_str().unwrap_or(""), 160)
        )),
        _ => None,
    }
}

/// The desktop window's side of the host (`abhoth --desktop`, D110): where the URL goes once the
/// server listens, the signal to stop, and the window's extras.
pub struct DesktopLink {
    /// The URL with the token (or why the server did not start).
    pub ready: std::sync::mpsc::Sender<Result<String, String>>,
    /// Quit from the window or the tray.
    pub stop: tokio::sync::oneshot::Receiver<()>,
    pub hooks: orca_webui::server::DesktopHooks,
}

static DESKTOP_LINK: Mutex<Option<DesktopLink>> = Mutex::new(None);

/// Set by the desktop app before it runs the host: the next [`run`] serves its window.
pub fn set_desktop_link(link: DesktopLink) {
    *DESKTOP_LINK.lock().unwrap() = Some(link);
}

pub async fn run(
    args: HostUiArgs,
    config: OrcaConfig,
    cwd: PathBuf,
    overlay: Option<ProfileOverlay>,
) -> Result<i32> {
    let desktop = DESKTOP_LINK.lock().unwrap().take();
    run_with(args, config, cwd, overlay, desktop).await
}

/// [`run`], or the host of the desktop window when `desktop` is set.
pub async fn run_with(
    args: HostUiArgs,
    config: OrcaConfig,
    cwd: PathBuf,
    overlay: Option<ProfileOverlay>,
    desktop: Option<DesktopLink>,
) -> Result<i32> {
    let (ready, mut stop, hooks) = match desktop {
        Some(d) => (Some(d.ready), Some(d.stop), Some(d.hooks)),
        None => (None, None, None),
    };
    let fail = |text: String| {
        if let Some(r) = &ready {
            let _ = r.send(Err(text));
        }
    };
    if args.tui {
        say_err!("abhoth: --tui needs the interactive mode, which is not ported to Rust yet.");
        return Ok(2);
    }
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let (mut session, mut services) = browser_session(&config, &cwd, &questions)?;
    services
        .start_mcp(&config, &cwd, &mut session.registry)
        .await;
    // The primary session's profile and mode are set where it was started, as in C#.
    let primary = start_live(
        session,
        &services,
        bus.clone(),
        "headless",
        NativeOptions {
            overlay,
            questions: Some(questions.clone()),
            ..Default::default()
        },
    );
    // A server that answers the /uncensored field with a 400 is told in the browser too.
    let notice_bus = bus.clone();
    orca_core::client::uncensored_gate::set_on_field_refused(Some(Arc::new(move |text: &str| {
        notice_bus.publish(
            orca_core::live::types::NOTICE,
            serde_json::json!({"level": "warning", "text": text}),
        );
    })));

    let others: Arc<Mutex<Vec<ToolServices>>> = Arc::new(Mutex::new(Vec::new()));
    let factory_config = config.clone();
    let factory_services = others.clone();
    let factory: SessionFactory = Arc::new(move |folder: &str| -> Arc<dyn LiveSession> {
        let dir = PathBuf::from(folder);
        let bus = Arc::new(LiveEventBus::new());
        let questions = Arc::new(QuestionBroker::new(bus.clone()));
        match browser_session(&factory_config, &dir, &questions) {
            Ok((s, svc)) => {
                // The project's profile and mode: the session is made again with them before
                // its next turn (C# restarts the worker with --profile / --mode).
                let (base, keep, q) = (
                    factory_config.clone(),
                    factory_services.clone(),
                    questions.clone(),
                );
                let rebuild: orca_webui::native::RebuildFn =
                    Arc::new(move |profile: Option<String>, mode: Option<String>| {
                        let (base, keep, q, dir) =
                            (base.clone(), keep.clone(), q.clone(), dir.clone());
                        Box::pin(async move {
                            let (config, overlay) =
                                config_for(&base, profile.as_deref(), mode.as_deref()).await?;
                            let (mut session, svc) =
                                browser_session(&config, &dir, &q).map_err(|e| e.to_string())?;
                            // The agent tools again (the tabs keep the first session's agents).
                            let home = orca_core::config::manager::default_config_dir();
                            if let Err(e) = crate::wire_agents(
                                &mut session,
                                &svc.agent_tools,
                                Some(&home),
                                true,
                                None,
                            ) {
                                tracing::warn!("rebuilt web UI session: no sub-agents: {e}");
                            }
                            keep.lock().unwrap().push(svc);
                            Ok(Rebuilt { session, overlay })
                        })
                    });
                let live = start_live(
                    s,
                    &svc,
                    bus,
                    "worker",
                    NativeOptions {
                        questions: Some(questions),
                        rebuild: Some(rebuild),
                        ..Default::default()
                    },
                );
                factory_services.lock().unwrap().push(svc);
                live
            }
            Err(e) => Arc::new(FailedSession {
                bus: Arc::new(LiveEventBus::new()),
                folder: folder.to_string(),
                error: e.to_string(),
            }),
        }
    });

    let token = args
        .token
        .clone()
        .unwrap_or_else(orca_webui::util::new_token);
    let mut options = WebUiOptions::new(token);
    options.host = args.endpoint.host.clone();
    if args.login {
        let mut login = orca_webui::login::LoginOptions::new(args.state_dir.clone());
        login.trusted_proxies = args.trusted_proxies.clone();
        login.require_totp = args.require_totp;
        options.login = Some(login);
    }
    options.port = args.endpoint.port;
    options.session_factory = Some(factory);
    options.profiles_url = orca_core::profiles::resolve_server_url(&config);
    options.profiles_cache_file =
        Some(orca_core::config::default_config_dir().join("profiles-cache.json"));
    options.config_home = Some(orca_core::config::default_config_dir());
    // Generated images (imageGen.outputDirectory), read on each request so a settings change
    // applies.
    #[cfg(feature = "image")]
    {
        let live = primary.clone();
        options.image_directory = Some(Arc::new(move || {
            Some(orca_tools_image::generate::resolve_output_directory(
                &live.config().image_gen,
            ))
        }));
    }
    if let Some(dir) = &args.bench_dir {
        options.bench = Some(bench_options(dir, &args, &config, others.clone()));
    }
    options.desktop = hooks;
    let mut server = match WebUiServer::start(options, primary.clone()).await {
        Ok(s) => s,
        Err(e) => {
            fail(e.to_string());
            say_err!("abhoth: could not start the web UI: {e}");
            primary.shutdown().await;
            return Ok(1);
        }
    };

    say!(
        "{} {} — web UI (headless)",
        orca_core::brand::PRODUCT_NAME,
        crate::version()
    );
    // Sign-in mode: no token in the URL.
    let url = if args.login {
        server.base_url().to_string()
    } else {
        server.url_with_token()
    };
    if ready.is_some() {
        // The desktop window: the token never leaves this process.
        say!("● Desktop window on {}", server.base_url());
    } else {
        say!("● Web UI {url}");
    }
    if args.login {
        if args.trusted_proxies.is_empty() {
            say!("  Sign-in mode; plain HTTP only from this machine.");
        } else {
            let proxies: Vec<String> = args.trusted_proxies.iter().map(|p| p.to_string()).collect();
            say!(
                "  Sign-in mode; HTTPS through {} (plain HTTP only from this machine).",
                proxies.join(", ")
            );
        }
    } else if args.endpoint.is_lan() {
        say!("  Open to the network: anyone with the token can drive this session.");
    }
    say!("Working folder: {}", cwd.display());
    say!("Ctrl+C stops it.");
    let _log = bus.subscribe(|ev| {
        if let Some(line) = activity_line(ev) {
            say!("{line}");
        }
    });
    if let Some(r) = &ready {
        // The desktop window opens the URL itself (the token stays in this process).
        let _ = r.send(Ok(server.url_with_token()));
    } else if args.open && !open_browser(&url) {
        say_err!("abhoth: could not open a browser; open the URL yourself.");
    }

    // Ctrl+C: cancel a running turn, else stop. The desktop window's Quit stops at once.
    loop {
        let ctrl_c = tokio::signal::ctrl_c();
        match stop.as_mut() {
            Some(quit) => {
                tokio::select! {
                    _ = quit => {
                        primary.cancel().await;
                        break;
                    }
                    r = ctrl_c => {
                        if r.is_err() || !primary.cancel().await {
                            break;
                        }
                    }
                }
            }
            None => {
                if ctrl_c.await.is_err() || !primary.cancel().await {
                    break;
                }
            }
        }
    }
    primary.permissions().deny_all();
    questions.cancel_all();
    server.stop().await;
    primary.shutdown().await;
    services.shutdown().await;
    let rest: Vec<ToolServices> = std::mem::take(&mut *others.lock().unwrap());
    for mut s in rest {
        s.shutdown().await;
    }
    Ok(0)
}

/// `HostUiSetup.BenchOptionsFor`: the bench runner of `--hostui-bench`. Each run gets an
/// in-process native session in its workspace, configured from the run's own home
/// (OPENORCA_HOME/config.json: the arm's lead mode and settings); the base config is this
/// process's config.json. C# starts a worker process per run instead (decision D74).
fn bench_options(
    dir: &Path,
    args: &HostUiArgs,
    config: &OrcaConfig,
    others: Arc<Mutex<Vec<ToolServices>>>,
) -> orca_webui::bench::BenchOptions {
    let full = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let home = orca_core::config::default_config_dir();
    let script = orca_core::brand::env_var("BENCHKIT")
        .ok_or(std::env::VarError::NotPresent)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(Path::to_path_buf))
                .unwrap_or_default()
                .join("bench")
                .join("benchkit.py")
        });
    let kit = args
        .bench_kit
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("kit"));
    let factory: orca_webui::bench::BenchSessionFactory = Arc::new(
        move |folder: &str,
              env: &std::collections::BTreeMap<String, String>|
              -> Arc<dyn LiveSession> {
            let failed = |error: String| -> Arc<dyn LiveSession> {
                Arc::new(FailedSession {
                    bus: Arc::new(LiveEventBus::new()),
                    folder: folder.to_string(),
                    error,
                })
            };
            let run_home = env.get("OPENORCA_HOME").map(PathBuf::from);
            let config = match crate::load_config(run_home.as_deref()) {
                Ok(c) => c,
                Err(e) => return failed(format!("{e:#}")),
            };
            // The run's "Slot usage" lines go to its own home's log, where the bench reads
            // them (C#: the worker process logs to its OPENORCA_HOME).
            let log = run_home.as_deref().map(|h| {
                orca_core::logging::FileLog::open(h) as Arc<dyn orca_core::logging::LogSink>
            });
            match session_logged(&config, Path::new(folder), log) {
                Ok((s, svc)) => {
                    let live = start_live(
                        s,
                        &svc,
                        Arc::new(LiveEventBus::new()),
                        "worker",
                        NativeOptions::default(),
                    );
                    others.lock().unwrap().push(svc);
                    live
                }
                Err(e) => failed(e.to_string()),
            }
        },
    );
    let mut o = orca_webui::bench::BenchOptions::new(
        full(dir),
        full(&kit),
        script,
        home.join("config.json"),
        factory,
    );
    o.registry_url = config
        .slots
        .as_ref()
        .filter(|s| s.enabled)
        .and_then(|s| s.registry_url.clone());
    o.power_env_file = args.bench_power.as_deref().map(PathBuf::from);
    if let Ok(repo) = std::env::var("REALTASKS_REPO")
        && !repo.is_empty()
    {
        o.environment.insert("REALTASKS_REPO".into(), repo);
    }
    o
}

/// A project whose session could not be built: its tab shows why.
struct FailedSession {
    bus: Arc<LiveEventBus>,
    folder: String,
    error: String,
}

#[async_trait::async_trait]
impl LiveSession for FailedSession {
    fn bus(&self) -> Arc<LiveEventBus> {
        self.bus.clone()
    }

    fn working_directory(&self) -> String {
        self.folder.clone()
    }

    fn mode(&self) -> String {
        "worker".into()
    }

    async fn get_snapshot(&self) -> Result<orca_core::live::LiveSnapshot, String> {
        Err(self.error.clone())
    }

    async fn submit_prompt(&self, _text: &str, _source: &str) -> Option<String> {
        Some(format!(
            "This project's session failed to start: {}",
            self.error
        ))
    }

    async fn cancel(&self) -> bool {
        false
    }

    async fn resolve_permission(&self, _id: &str, _decision: &str) -> bool {
        false
    }
}

pub(crate) fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, data: serde_json::Value) -> LiveEvent {
        LiveEvent {
            seq: 1,
            kind: kind.into(),
            at: String::new(),
            data,
        }
    }

    #[test]
    fn activity_lines() {
        let l = activity_line(&ev(
            types::USER,
            json!({"text": "fix\nit", "source": "web"}),
        ))
        .unwrap();
        assert!(l.ends_with("prompt (web): fix it"), "{l}");
        let l = activity_line(&ev(
            types::TURN_END,
            json!({"elapsedMs": 1500, "tokens": 1234, "outcome": "done"}),
        ))
        .unwrap();
        assert!(l.ends_with("turn done in 1.5s · 1,234 tokens"), "{l}");
        assert!(
            activity_line(&ev(types::NOTICE, json!({"level": "warning", "text": "x"}))).is_none()
        );
        assert!(activity_line(&ev(types::ASSISTANT_DELTA, json!({"text": "x"}))).is_none());
    }
}
