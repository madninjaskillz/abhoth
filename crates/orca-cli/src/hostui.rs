//! `--hostui` (headless): the web UI drives the native agent loop. Prints the URL and one line
//! per prompt and finished turn; Ctrl+C cancels a running turn, else stops the server.
//! Other projects opened in the browser get their own in-process session (C# starts a worker
//! process per project).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// A web UI session for `cwd`: its questions go to `questions`' browser.
fn browser_session(
    config: &OrcaConfig,
    cwd: &Path,
    questions: &Arc<QuestionBroker>,
) -> Result<(AgentSession, ToolServices)> {
    let (client, _) = crate::wire_client(config, orca_core::logging::process_sink(), false)?;
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

pub async fn run(
    args: HostUiArgs,
    config: OrcaConfig,
    cwd: PathBuf,
    overlay: Option<ProfileOverlay>,
) -> Result<i32> {
    if args.tui {
        eprintln!("openorca: --tui needs the interactive mode, which is not ported to Rust yet.");
        return Ok(2);
    }
    if args.login {
        eprintln!(
            "openorca: --hostui-login (sign-in mode) is not ported to Rust yet; use the token."
        );
        return Ok(2);
    }
    if orca_core::engines::selected_bridge(&config).is_some() {
        eprintln!(
            "openorca: the Rust web UI runs the native engine only (agent.engine is {}).",
            config.agent.engine
        );
        return Ok(2);
    }
    let bus = Arc::new(LiveEventBus::new());
    let questions = Arc::new(QuestionBroker::new(bus.clone()));
    let (mut session, mut services) = browser_session(&config, &cwd, &questions)?;
    services
        .start_mcp(&config, &cwd, &mut session.registry)
        .await;
    // The primary session's profile and mode are set where it was started, as in C#.
    let primary = AgentLiveSession::start_with(
        session,
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
                factory_services.lock().unwrap().push(svc);
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
                            let (session, svc) =
                                browser_session(&config, &dir, &q).map_err(|e| e.to_string())?;
                            keep.lock().unwrap().push(svc);
                            Ok(Rebuilt { session, overlay })
                        })
                    });
                AgentLiveSession::start_with(
                    s,
                    bus,
                    "worker",
                    NativeOptions {
                        questions: Some(questions),
                        rebuild: Some(rebuild),
                        ..Default::default()
                    },
                )
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
    let mut server = match WebUiServer::start(options, primary.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("openorca: could not start the web UI: {e}");
            primary.shutdown().await;
            return Ok(1);
        }
    };

    println!("OpenOrca {} — web UI (headless)", crate::version());
    println!("● Web UI {}", server.url_with_token());
    if args.endpoint.is_lan() {
        println!("  Open to the network: anyone with the token can drive this session.");
    }
    println!("Working folder: {}", cwd.display());
    println!("Ctrl+C stops it.");
    let _log = bus.subscribe(|ev| {
        if let Some(line) = activity_line(ev) {
            println!("{line}");
        }
    });
    if args.open && !open_browser(&server.url_with_token()) {
        eprintln!("openorca: could not open a browser; open the URL yourself.");
    }

    // Ctrl+C: cancel a running turn, else stop.
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            break;
        }
        if !primary.cancel().await {
            break;
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
    let script = std::env::var("OPENORCA_BENCHKIT")
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
            if orca_core::engines::selected_bridge(&config).is_some() {
                return failed(format!(
                    "the Rust bench runs the native engine only (agent.engine is {})",
                    config.agent.engine
                ));
            }
            // The run's "Slot usage" lines go to its own home's log, where the bench reads
            // them (C#: the worker process logs to its OPENORCA_HOME).
            let log = run_home.as_deref().map(|h| {
                orca_core::logging::FileLog::open(h) as Arc<dyn orca_core::logging::LogSink>
            });
            match session_logged(&config, Path::new(folder), log) {
                Ok((s, svc)) => {
                    others.lock().unwrap().push(svc);
                    AgentLiveSession::start(s, Arc::new(LiveEventBus::new()), "worker")
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

fn open_browser(url: &str) -> bool {
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
