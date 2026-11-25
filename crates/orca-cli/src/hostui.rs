//! `--hostui` (headless): the web UI drives the native agent loop. Prints the URL and one line
//! per prompt and finished turn; Ctrl+C cancels a running turn, else stops the server.
//! Other projects opened in the browser get their own in-process session (C# starts a worker
//! process per project).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use orca_abstractions::ToolContext;
use orca_core::agent::AgentSession;
use orca_core::client::{ServedModels, client_from_config};
use orca_core::config::OrcaConfig;
use orca_core::live::{LiveEvent, LiveEventBus, types};
use orca_webui::hostui::HostUiArgs;
use orca_webui::native::AgentLiveSession;
use orca_webui::{LiveSession, SessionFactory, WebUiOptions, WebUiServer};

use crate::{ToolServices, build_registry_in};

fn session_for(config: &OrcaConfig, cwd: &Path) -> Result<(AgentSession, ToolServices)> {
    let client = client_from_config(config, Arc::new(ServedModels::new()))?;
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

pub async fn run(args: HostUiArgs, config: OrcaConfig, cwd: PathBuf) -> Result<i32> {
    if args.tui {
        eprintln!("openorca: --tui needs the interactive mode, which is not ported to Rust yet.");
        return Ok(2);
    }
    if args.bench_dir.is_some() {
        eprintln!(
            "openorca: --hostui-bench (the Benchmarks page) is not ported to Rust yet; use the C# build for benches."
        );
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
    let (mut session, mut services) = session_for(&config, &cwd)?;
    services
        .start_mcp(&config, &cwd, &mut session.registry)
        .await;
    let bus = Arc::new(LiveEventBus::new());
    let primary = AgentLiveSession::start(session, bus.clone(), "headless");

    let others: Arc<Mutex<Vec<ToolServices>>> = Arc::new(Mutex::new(Vec::new()));
    let factory_config = config.clone();
    let factory_services = others.clone();
    let factory: SessionFactory = Arc::new(move |folder: &str| -> Arc<dyn LiveSession> {
        let dir = PathBuf::from(folder);
        match session_for(&factory_config, &dir) {
            Ok((s, svc)) => {
                factory_services.lock().unwrap().push(svc);
                AgentLiveSession::start(s, Arc::new(LiveEventBus::new()), "worker")
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
    server.stop().await;
    primary.shutdown().await;
    services.shutdown().await;
    let rest: Vec<ToolServices> = std::mem::take(&mut *others.lock().unwrap());
    for mut s in rest {
        s.shutdown().await;
    }
    Ok(0)
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
