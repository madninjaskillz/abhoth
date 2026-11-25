//! `/profile [list | show <name> | switch <name>]` (`ProfileCommand`). A profile = a server
//! layout on the inference box + client settings. Switching to a profile whose layout is live
//! only restarts this session's wiring; otherwise the server switch is confirmed first (it
//! restarts the model servers for everyone), followed live, and then the client follows. The
//! startup side of `--profile` is [`orca_core::profiles::apply_at_startup`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use orca_core::config::{ConfigManager, OrcaConfig};
use orca_core::profiles::catalog::get_ci;
use orca_core::profiles::{ProfileCatalog, ProfileService};

use super::CommandContext;
use crate::ui::markup::escape;
use crate::ui::widgets::Table;
use crate::ui::{Console, prompt};

/// A request to restart this session's engine wiring in-process with another profile,
/// resuming the conversation (`ProfileRestart`). The REPL fills `session_id` when the
/// conversation has a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRestart {
    pub profile: String,
    pub session_id: Option<String>,
}

impl ProfileRestart {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            session_id: None,
        }
    }
}

type ServiceFactory = Box<dyn Fn(&OrcaConfig) -> ProfileService + Send + Sync>;
type Blocked = Box<dyn Fn() -> Option<String> + Send + Sync>;
type Persist = Box<dyn Fn(&OrcaConfig) -> Result<(), String> + Send + Sync>;
type Confirm = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// `ProfileCommand`: the handler with its replaceable parts (tests swap the service, the
/// confirmation and the save).
pub struct ProfileCommand {
    service_factory: ServiceFactory,
    /// Why this session can't switch profiles right now (`None` = it can).
    pub switch_blocked: Option<Blocked>,
    /// Saves profiles.active to config.json (tests replace it: they must never write the real
    /// config). Default: load config.json from the config dir, set profiles.active, save.
    pub persist: Option<Persist>,
    /// Confirms a server switch (tests replace it). Default: a yes/no prompt, no when input is
    /// not a terminal.
    pub confirm: Option<Confirm>,
    /// How often a switch job is polled (default: the service's 2 s).
    pub job_poll: Option<Duration>,
    config_dir: PathBuf,
}

impl ProfileCommand {
    /// The command with the real profiles API (`ProfileService.FromConfig`) and config.json in
    /// `config_dir`.
    pub fn new(config_dir: &Path) -> Self {
        let dir = config_dir.to_path_buf();
        Self {
            service_factory: Box::new(move |config| ProfileService::from_config(config, &dir)),
            switch_blocked: None,
            persist: None,
            confirm: None,
            job_poll: None,
            config_dir: config_dir.to_path_buf(),
        }
    }

    /// The command over another service (tests).
    pub fn with_service(
        config_dir: &Path,
        factory: impl Fn(&OrcaConfig) -> ProfileService + Send + Sync + 'static,
    ) -> Self {
        Self {
            service_factory: Box::new(factory),
            ..Self::new(config_dir)
        }
    }

    /// `HandleAsync`: handle /profile on the session's `config`; returns a restart request
    /// when the session should switch over.
    pub async fn handle(
        &self,
        console: &Console,
        config: &mut OrcaConfig,
        args: &[String],
    ) -> Option<ProfileRestart> {
        let mut sub = args
            .first()
            .map_or_else(|| "list".to_string(), |a| a.to_lowercase());
        let mut name = args.get(1).cloned();
        if !matches!(sub.as_str(), "list" | "ls" | "show" | "switch" | "use") && args.len() == 1 {
            name = Some(args[0].clone());
            sub = "switch".into();
        }
        let service = (self.service_factory)(config);
        let (catalog, error) = service.get_catalog().await;
        let Some(catalog) = catalog else {
            console.markup_line(&format!(
                "[yellow]No profiles: {}[/]",
                escape(error.as_deref().unwrap_or("unknown error"))
            ));
            return None;
        };
        if let Some(error) = &error {
            console.markup_line(&format!(
                "[yellow]Offline:[/] [dim]{} — showing the copy from {}.[/]",
                escape(error),
                fetched_at(&catalog)
            ));
        }
        match sub.as_str() {
            "list" | "ls" => {
                list(console, config, &catalog);
                None
            }
            "show" => {
                let name = name.or_else(|| config.active_profile.clone());
                show(console, &catalog, name.as_deref());
                None
            }
            "switch" | "use" => {
                let Some(name) = name else {
                    console.markup_line("[yellow]Usage: /profile switch <name>[/]");
                    return None;
                };
                if let Some(blocked) = self.switch_blocked.as_ref().and_then(|f| f()) {
                    console
                        .markup_line(&format!("[yellow]Not switching: {}.[/]", escape(&blocked)));
                    return None;
                }
                if catalog.from_cache {
                    console.markup_line("[yellow]The profiles API is unreachable, so the live layout is unknown: not switching.[/]");
                    return None;
                }
                self.switch(console, config, &service, &catalog, &name)
                    .await
            }
            _ => {
                console.markup_line(
                    "[yellow]Usage: /profile [[list | show <name> | switch <name>]][/]",
                );
                None
            }
        }
    }

    async fn switch(
        &self,
        console: &Console,
        config: &mut OrcaConfig,
        service: &ProfileService,
        catalog: &ProfileCatalog,
        name: &str,
    ) -> Option<ProfileRestart> {
        if catalog.find(name).is_none() {
            console.markup_line(&format!(
                "[yellow]Unknown profile {}.[/] [dim]/profile list[/]",
                escape(name)
            ));
            return None;
        }
        let plan = match service.plan(name).await {
            Ok(p) => p,
            Err(e) => {
                console.markup_line(&format!(
                    "[red]Could not plan the switch: {}[/]",
                    escape(&e.to_string())
                ));
                return None;
            }
        };
        let layout = plan.layout.clone().unwrap_or_default();
        if !plan.fits {
            if !plan.refusals.is_empty() {
                console.markup_line(&format!(
                    "[yellow]The server can't switch to {} now:[/] {}",
                    escape(&layout),
                    escape(&plan.refusals.join("; "))
                ));
                return None;
            }
            let minutes = ((plan.estimate_seconds as f64 / 60.0).ceil() as i32).max(1);
            let question = format!(
                "Switch the server to layout [cyan]{}[/] (live: {})? This [yellow]{}[/], about {minutes} min.",
                escape(&layout),
                escape(plan.live.as_deref().unwrap_or("unknown")),
                escape(plan.effect.as_deref().unwrap_or("restarts model servers"))
            );
            let yes = match &self.confirm {
                Some(f) => f(&question),
                None => default_confirm(console, &question),
            };
            if !yes {
                console.markup_line("[grey]Not switched.[/]");
                return None;
            }
            let requester = format!(
                "{}@{} ({} TUI)",
                user_name(),
                machine_name(),
                orca_core::brand::BINARY_NAME
            );
            let mut job = match service
                .request_switch(name, &requester, Some("/profile switch"))
                .await
            {
                Ok(j) => j,
                Err(e) => {
                    console.markup_line(&format!("[red]{}[/]", escape(&e.to_string())));
                    return None;
                }
            };
            if job.state.as_deref() == Some("refused") {
                console.markup_line(&format!(
                    "[yellow]Refused: {}[/]",
                    escape(job.error.as_deref().unwrap_or(""))
                ));
                return None;
            }
            if !job.already {
                console.markup_line(&format!(
                    "[grey]Switch queued ({}); following it:[/]",
                    escape(job.id.as_deref().unwrap_or(""))
                ));
                let limit = Duration::from_secs(900.max(plan.estimate_seconds as u64 * 3));
                job = service
                    .wait_for_job(
                        job,
                        |s| {
                            console.markup_line(&format!(
                                "  [dim]{:>6}s[/] {}",
                                format!("{:.0}", s.t),
                                escape(&s.msg)
                            ))
                        },
                        limit,
                        self.job_poll,
                    )
                    .await;
                if !job.succeeded() {
                    console.markup_line(&format!(
                        "[red]Switch {}:[/] {}{}",
                        escape(job.state.as_deref().unwrap_or("unfinished")),
                        escape(job.error.as_deref().unwrap_or("")),
                        job.live
                            .as_deref()
                            .map(|l| format!(" [dim](live layout: {})[/]", escape(l)))
                            .unwrap_or_default()
                    ));
                    return None;
                }
                console.markup_line(&format!(
                    "[green]Server on {} after {:.0}s.[/]",
                    escape(&layout),
                    job.seconds.unwrap_or(0.0)
                ));
            }
        }
        // The client side: remembered as the default for new sessions, applied by restarting
        // this session's wiring.
        config.profiles.active = Some(name.to_string());
        let saved = match &self.persist {
            Some(f) => f(config),
            None => persist_active(&self.config_dir, name),
        };
        if let Err(e) = saved {
            console.markup_line(&format!("[red]{}[/]", escape(&e)));
        }
        console.markup_line(&format!(
            "[grey]Switching this session to profile[/] [cyan]{}[/][grey]; the conversation carries on.[/]",
            escape(name)
        ));
        Some(ProfileRestart::new(name))
    }
}

/// `/profile` with the real profiles API on the session's config. The REPL handles the
/// returned restart (it ends the loop and restarts the session with that profile).
pub async fn handle(ctx: &mut CommandContext<'_>, args: &[String]) -> Option<ProfileRestart> {
    ProfileCommand::new(ctx.config_dir)
        .handle(ctx.console, &mut ctx.session.config, args)
        .await
}

/// Write profiles.active to config.json in `dir`, leaving the rest of the file as it is.
fn persist_active(dir: &Path, name: &str) -> Result<(), String> {
    let mut manager = ConfigManager::new(Some(dir));
    manager.load().map_err(|e| e.to_string())?;
    manager.config.profiles.active = Some(name.to_string());
    manager.save().map_err(|e| e.to_string())
}

fn default_confirm(console: &Console, question: &str) -> bool {
    if !prompt::interactive() {
        return false;
    }
    prompt::confirm(console, question, false)
}

fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// `catalog.FetchedAt.ToLocalTime():g` (en-US short date and time).
fn fetched_at(catalog: &ProfileCatalog) -> String {
    let t: chrono::DateTime<chrono::Local> =
        catalog.fetched_at.unwrap_or(std::time::UNIX_EPOCH).into();
    t.format("%-m/%-d/%Y %-I:%M %p").to_string()
}

fn list(console: &Console, config: &OrcaConfig, catalog: &ProfileCatalog) {
    let live = catalog.live.as_ref();
    let mut line = format!(
        "[grey]Live server layout:[/] [cyan]{}[/]",
        escape(live.and_then(|l| l.layout.as_deref()).unwrap_or("unknown"))
    );
    if let Some(t) = live.and_then(|l| l.title.as_deref()) {
        line.push_str(&format!(" [dim]({})[/]", escape(t)));
    }
    if live.is_some_and(|l| !l.healthy && l.layout.is_some()) {
        line.push_str(" [red]unhealthy[/]");
    }
    if let Some(job) = live.and_then(|l| l.job.as_ref()) {
        line.push_str(&format!(
            " [yellow]switch {} → {}[/]",
            escape(job.state.as_deref().unwrap_or("")),
            escape(job.layout.as_deref().unwrap_or(""))
        ));
    }
    console.markup_line(&line);
    let mut table = Table::new()
        .column("")
        .column("Profile")
        .column("Layout")
        .column("Fits live")
        .column("About");
    let mut profiles: Vec<_> = catalog.profiles.iter().filter(|(_, p)| !p.hidden).collect();
    profiles.sort_by_key(|(n, _)| n.to_lowercase());
    for (name, p) in profiles {
        let active = config
            .active_profile
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(name));
        let fits = if catalog.fits(p) {
            "[green]yes[/]".to_string()
        } else if let Some(r) = catalog.restore_note(p) {
            format!("[yellow]switch, {}[/]", escape(&r))
        } else {
            "[yellow]switch[/]".to_string()
        };
        table.row(&[
            if active { "[cyan]●[/]" } else { "" }.to_string(),
            escape(name),
            escape(p.layout.as_deref().unwrap_or("(none)")),
            fits,
            escape(p.title.as_deref().unwrap_or("")),
        ]);
    }
    table.write(console);
    console.markup_line("[dim]/profile show <name> · /profile switch <name>[/]");
}

fn show(console: &Console, catalog: &ProfileCatalog, name: Option<&str>) {
    let Some(name) = name else {
        console.markup_line("[grey]No profile is active in this session.[/] [dim]/profile list[/]");
        return;
    };
    let Some(p) = catalog.find(name) else {
        console.markup_line(&format!("[yellow]Unknown profile {}.[/]", escape(name)));
        return;
    };
    console.markup_line(&format!(
        "[cyan]{}[/] — {}",
        escape(name),
        escape(p.title.as_deref().unwrap_or(""))
    ));
    if let Some(d) = &p.description {
        console.markup_line(&format!("[dim]{}[/]", escape(d)));
    }
    if let Some(l) = &p.layout {
        let summary = get_ci(&catalog.layouts, l)
            .map(|lay| lay.summary())
            .unwrap_or_default();
        let live = if catalog.fits(p) {
            " [green](live)[/]".to_string()
        } else {
            format!(
                " [yellow](live: {})[/]",
                escape(
                    catalog
                        .live
                        .as_ref()
                        .and_then(|l| l.layout.as_deref())
                        .unwrap_or("unknown")
                )
            )
        };
        let restore = catalog
            .restore_note(p)
            .map(|r| format!(" [yellow]{}[/]", escape(&r)))
            .unwrap_or_default();
        console.markup_line(&format!(
            "[grey]Server layout:[/] {} [dim]{}[/]{live}{restore}",
            escape(l),
            escape(&summary)
        ));
    } else {
        console.markup_line("[grey]Server layout:[/] none needed");
    }
    console.markup_line("[grey]Client settings:[/]");
    for (path, value) in &p.client {
        console.markup_line(&format!(
            "  {} = [dim]{}[/]",
            escape(path),
            escape(&serde_json::to_string(value).unwrap_or_else(|_| "null".into()))
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CATALOG: &str = r#"
        {
          "version": 1,
          "layouts": { "flash2": { "title": "Flash x2", "cards": { "A": { "engine": "strata", "slots": 3 }, "B": { "engine": "strata", "slots": 2 } } },
                       "dense2": { "title": "Dense x2", "cards": { "A": { "engine": "ninfer", "slots": 2, "model": "m" }, "B": { "engine": "ninfer", "slots": 2, "model": "m" } } } },
          "profiles": {
            "flash2": { "title": "Flash", "layout": "flash2", "client": { "agent.maxConcurrent": 4 } },
            "dense2": { "title": "Dense", "layout": "dense2", "client": { "agent.peerMode": true } },
            "claude": { "title": "Claude", "layout": null, "client": { "agent.engine": "claude" } }
          },
          "live": { "layout": "flash2", "healthy": true, "cards": {}, "leases": [] }
        }
        "#;

    #[derive(Default)]
    struct Server {
        switches: AtomicUsize,
        job_polls: AtomicUsize,
        lease_held: bool,
    }

    impl Server {
        fn respond(&self, path: &str, query: &str) -> (u16, String) {
            match path {
                "/profiles" => (200, CATALOG.to_string()),
                "/plan" if query.contains("dense2") => (
                    200,
                    format!(
                        r#"{{"profile":"dense2","layout":"dense2","live":"flash2","fits":false,"estimateSeconds":150,"effect":"restarts model servers, affects everything using the models","refusals":[{}]}}"#,
                        if self.lease_held {
                            "\"pig-slots leases held: B1\""
                        } else {
                            ""
                        }
                    ),
                ),
                "/plan" => (
                    200,
                    r#"{"layout":"flash2","live":"flash2","fits":true,"estimateSeconds":0,"refusals":[]}"#.into(),
                ),
                "/switch" => {
                    let n = self.switches.fetch_add(1, Ordering::SeqCst) + 1;
                    (
                        202,
                        format!(r#"{{"id":"j{n}","state":"queued","estimateSeconds":150}}"#),
                    )
                }
                "/jobs/j1" => {
                    let n = self.job_polls.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = if n < 2 {
                        r#"{"id":"j1","state":"running","steps":[{"t":1,"msg":"card A: stopping qfn-strata"}]}"#
                    } else {
                        r#"{"id":"j1","state":"done","seconds":140,"steps":[{"t":1,"msg":"card A: stopping qfn-strata"},{"t":140,"msg":"done: live layout dense2"}]}"#
                    };
                    (200, body.into())
                }
                _ => (404, "{}".into()),
            }
        }
    }

    /// A minimal HTTP/1.1 server (one request per connection) answering from `server`.
    async fn serve(server: Arc<Server>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let server = server.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let (status, body) = server.respond(path, query);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    struct Made {
        cmd: ProfileCommand,
        console: Console,
        out: Arc<Mutex<Vec<u8>>>,
        config: OrcaConfig,
        asked: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    async fn make(server: Arc<Server>, answer: bool) -> Made {
        let url = serve(server).await;
        let dir = tempfile::tempdir().unwrap();
        let mut manager = ConfigManager::new(Some(dir.path()));
        manager
            .load_from_text(r#"{ "slots": { "registryUrl": "http://box:8099", "copies": [] } }"#)
            .unwrap();
        let (console, out) = Console::buffer();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let a = asked.clone();
        let mut cmd = ProfileCommand::with_service(dir.path(), move |_| {
            ProfileService::new(Some(&url), None, Duration::from_secs(5))
        });
        cmd.confirm = Some(Box::new(move |q| {
            a.lock().unwrap().push(q.to_string());
            answer
        }));
        cmd.persist = Some(Box::new(|_| Ok(())));
        cmd.job_poll = Some(Duration::from_millis(5));
        Made {
            cmd,
            console,
            out,
            config: manager.config,
            asked,
            _dir: dir,
        }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn list_shows_live_layout_and_which_profiles_fit() {
        let mut m = make(Arc::new(Server::default()), false).await;
        assert_eq!(m.cmd.handle(&m.console, &mut m.config, &[]).await, None);
        let text = Console::buffer_text(&m.out);
        assert!(text.contains("Live server layout: flash2"), "{text}");
        assert!(text.contains("dense2"));
        assert!(text.contains("switch"));
        assert!(text.contains("claude"));
    }

    #[tokio::test]
    async fn show_prints_the_client_settings() {
        let mut m = make(Arc::new(Server::default()), false).await;
        m.cmd
            .handle(&m.console, &mut m.config, &args(&["show", "dense2"]))
            .await;
        let text = Console::buffer_text(&m.out);
        assert!(text.contains("agent.peerMode = true"), "{text}");
        assert!(text.contains("ninfer×2"), "{text}");
    }

    #[tokio::test]
    async fn switch_to_a_fitting_profile_restarts_without_asking_or_touching_the_server() {
        let server = Arc::new(Server::default());
        let mut m = make(server.clone(), false).await;
        let restart = m
            .cmd
            .handle(&m.console, &mut m.config, &args(&["switch", "claude"]))
            .await;
        assert_eq!(restart.unwrap().profile, "claude");
        assert!(m.asked.lock().unwrap().is_empty());
        assert_eq!(server.switches.load(Ordering::SeqCst), 0);
        assert_eq!(m.config.profiles.active.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn switch_needing_the_server_asks_naming_the_effect_and_stops_on_no() {
        let server = Arc::new(Server::default());
        let mut m = make(server.clone(), false).await;
        assert_eq!(
            m.cmd
                .handle(&m.console, &mut m.config, &args(&["switch", "dense2"]))
                .await,
            None
        );
        let asked = m.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let q = &asked[0];
        assert!(q.contains("restarts model servers"));
        assert!(q.contains("affects everything using the models"));
        assert!(q.contains("about 3 min"));
        assert_eq!(server.switches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn switch_needing_the_server_on_yes_follows_the_job_then_restarts() {
        let server = Arc::new(Server::default());
        let mut m = make(server.clone(), true).await;
        let restart = m
            .cmd
            .handle(&m.console, &mut m.config, &args(&["dense2"]))
            .await;
        assert_eq!(restart.unwrap().profile, "dense2");
        assert_eq!(server.switches.load(Ordering::SeqCst), 1);
        let text = Console::buffer_text(&m.out);
        assert!(text.contains("stopping qfn-strata"), "{text}");
        assert!(text.contains("Server on dense2"), "{text}");
    }

    #[tokio::test]
    async fn switch_while_leases_are_held_is_refused_without_asking() {
        let server = Arc::new(Server {
            lease_held: true,
            ..Server::default()
        });
        let mut m = make(server.clone(), true).await;
        assert_eq!(
            m.cmd
                .handle(&m.console, &mut m.config, &args(&["switch", "dense2"]))
                .await,
            None
        );
        assert!(m.asked.lock().unwrap().is_empty());
        assert!(Console::buffer_text(&m.out).contains("leases held"));
        assert_eq!(server.switches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn parser_knows_profile_command_and_flag() {
        use crate::args::CliArgs;
        use crate::repl::command_parser::{CommandParser, SlashCommand};
        let parsed = CommandParser::new()
            .try_parse("/profile switch dense2")
            .unwrap();
        assert_eq!(parsed.command, SlashCommand::Profile);
        assert_eq!(parsed.args, vec!["switch", "dense2"]);
        let cwd = Path::new(".");
        assert_eq!(
            CliArgs::parse(&args(&["--profile", "dense2"]), cwd)
                .profile
                .as_deref(),
            Some("dense2")
        );
        assert_eq!(
            CliArgs::parse(&args(&["--profile=flash2"]), cwd)
                .profile
                .as_deref(),
            Some("flash2")
        );
        assert_eq!(CliArgs::parse(&[], cwd).profile, None);
    }
}
