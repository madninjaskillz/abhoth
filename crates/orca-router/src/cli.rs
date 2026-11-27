//! `abhoth router ...` (and the image's `abhoth-router`):
//!
//! ```text
//! abhoth router --config router.json [--port 8010 | --listen 0.0.0.0:8010] [--lb-port 8092] [--slots-port 8099]
//! abhoth router import-litellm --litellm config.yaml --lb nginx.conf [--gateway gateway.json]
//!               [--key-file /run/secrets/ninfer-key] [--base router.json] --out router.json [--watch SECONDS]
//! abhoth router check --config router.json
//! abhoth router import --from DIR --out router.toml [--report report.md] [--seed seed.json]
//!               [--key-delivery file_flag|wrapper|argv] [--key-file-build ENGINE=BUILD]... [--enable-on-demand] [--server ID]
//! abhoth router config-check --config router.toml
//! abhoth router init [DIR] [--yes] [--force] ...   (a new standalone router; see init.rs)
//! abhoth router calendar check|import-cron|import-ics --file calendar.toml ...
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{RouterConfig, Snapshot};
use crate::import::{self, ImportOptions};
use crate::server::{self, Mode};
use crate::state::AppState;

pub const HELP: &str = "abhoth router: the OpenAI-compatible gateway (replaces LiteLLM, qfn-lb, pig-slots)

USAGE:
  abhoth router --config router.json [--port P | --listen ADDR] [--host H] [--lb-port P] [--slots-port P]
                [--control router.toml]   (the control plane, when its control.enabled is on)
                [--profiles-port N]       (the profiles API shim, :8098; needs --control)
                [--name NAME] [--key-url URL] [--mdns auto|on|off]
                [--wyoming stt=ADDR] [--wyoming tts=ADDR]   (raw Wyoming ports for Home Assistant:
                                          to the stt / tts engines with media_api = wyoming;
                                          needs --control; repeatable)
                                          (how clients see it: /.well-known/abhoth-router and
                                           mDNS _abhoth-router._tcp; auto = on unless loopback)
  abhoth router observe-summary --dir DIR [--date YYYY-MM-DD]   (the decision-only shadow's day)
  abhoth router import-litellm --litellm config.yaml --lb nginx.conf [--gateway gateway.json]
                [--key-file PATH] [--base router.json] --out router.json [--watch SECONDS]
  abhoth router check --config router.json
  abhoth router admin-demo --listen 127.0.0.1:8010 --link-token-file FILE [--accounts-url URL] [--loopback-admin] [--state-dir DIR]
                (the admin API on an in-memory demo host, for the web UI's admin pages)
  abhoth router import --from DIR --out router.toml [--report report.md] [--seed seed.json]
                [--key-delivery file_flag|wrapper|argv] [--key-file-build ENGINE=BUILD]... [--enable-on-demand] [--server ID]
                (router.toml for the control plane from a copy of the GPU host's layout files)
  abhoth router config-check --config router.toml
  abhoth router init [DIR] [--yes] [--force] [--listen ADDR] [--no-local] [--no-key] [--print-key]
                [--service systemd|compose|windows]   (a new standalone router: router.toml,
                 router.json, the state folder, an admin key file; see init --help)
  abhoth router admin-key create NAME --control router.toml [--out FILE]
  abhoth router admin-key list|revoke ID|NAME --control router.toml
                (the router's own admin keys, for its admin pages at /admin/; --out writes the
                 new key to FILE (mode 600) instead of printing it)
  abhoth router sso link --webui URL --control router.toml --public-url URL... [--name NAME]
  abhoth router sso status|unlink --control router.toml
                (Sign in with Abhoth on the admin pages: asks the Abhoth web UI at URL to
                 register this router; an Abhoth admin approves the code it prints. A router
                 linked with accounts_url registers itself and needs none of this)
  abhoth router bridge-token create ID --control router.toml --out FILE [--rotate]
  abhoth router bridge-token list|revoke ID --control router.toml
                (tokens for bridges that dial /bridge/ws; the new token goes to FILE (mode 600),
                 never to the terminal; the router keeps only its hash)
  abhoth router calendar check|import-cron|import-ics --file calendar.toml ...   (see calendar --help)
  abhoth router hostctl serve --allowlist FILE        (the SSH forced command on a GPU host)
  abhoth router hostctl exec-root --allowlist FILE    (its single sudo entry)
  abhoth router hostctl export --config router.toml --server ID [--out FILE]
";

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

/// Entry point; `args` start after the word `router`. Returns the exit code.
pub async fn run(args: Vec<String>) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            print!("{HELP}");
            Ok(0)
        }
        Some("import-litellm") => import_cmd(&args[1..]).await,
        Some("import") => control_import_cmd(&args[1..]),
        Some("hostctl") => hostctl_cmd(&args[1..]).await,
        Some("admin-key") => admin_key_cmd(&args[1..]),
        Some("sso") => sso_cmd(&args[1..]).await,
        Some("init") => crate::init::run(&args[1..]),
        Some("bridge-token") => bridge_token_cmd(&args[1..]),
        Some("config-check") => {
            let mut config = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--config" => config = Some(take(&args, &mut i, "--config")?),
                    a => bail!("unknown argument {a}"),
                }
                i += 1;
            }
            let path = PathBuf::from(config.ok_or_else(|| anyhow!("--config is required"))?);
            control_check(&path)
        }
        Some("observe-summary") => {
            let (mut dir, mut date) = (None, None);
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--dir" => dir = Some(take(&args, &mut i, "--dir")?),
                    "--date" => date = Some(take(&args, &mut i, "--date")?),
                    a => bail!("unknown argument {a}"),
                }
                i += 1;
            }
            let dir = dir.ok_or_else(|| anyhow!("--dir is required"))?;
            print!("{}", crate::observe::summary_cli(&dir, date));
            Ok(0)
        }
        Some("check") => {
            let mut config = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--config" => config = Some(take(&args, &mut i, "--config")?),
                    a => bail!("unknown argument {a}"),
                }
                i += 1;
            }
            let path = PathBuf::from(config.ok_or_else(|| anyhow!("--config is required"))?);
            let s = Snapshot::load(&path).map_err(|e| anyhow!(e))?;
            println!(
                "ok: rev {}, {} upstreams, {} deployments",
                s.config.rev,
                s.config.upstreams.len(),
                s.config.deployments.len()
            );
            Ok(0)
        }
        Some("admin-demo") => admin_demo(&args[1..]).await,
        Some("calendar") => crate::calendar::cli::run(&args[1..]).map_err(|e| anyhow!(e)),
        _ => serve_cmd(&args).await,
    }
}

/// The admin API over [`crate::control_api::demo::DemoControl`]: no engines, no commands.
async fn admin_demo(args: &[String]) -> Result<i32> {
    let (mut listen, mut token_file, mut accounts, mut calendar) =
        ("127.0.0.1:8010".to_string(), None, None, None::<String>);
    let mut loopback_admin = false;
    let mut state_dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => listen = take(args, &mut i, "--listen")?,
            "--link-token-file" => token_file = Some(take(args, &mut i, "--link-token-file")?),
            "--accounts-url" => accounts = Some(take(args, &mut i, "--accounts-url")?),
            // A calendar.toml for the calendar page (in-memory store, the demo's cards A-D).
            "--calendar" => calendar = Some(take(args, &mut i, "--calendar")?),
            // Screenshots: requests from this machine act as an admin (no sign-in).
            "--loopback-admin" => loopback_admin = true,
            // The inventory and engine store keep their files here (default: a temp folder).
            "--state-dir" => state_dir = Some(take(args, &mut i, "--state-dir")?),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let token = match &token_file {
        Some(f) => Some(std::fs::read(f).with_context(|| format!("reading {f}"))?),
        None => None,
    };
    if let Some(f) = &calendar {
        let file = PathBuf::from(f);
        let dir = file
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let store = Arc::new(crate::control::store::Store::memory().map_err(|e| anyhow!("{e}"))?);
        let svc = crate::calendar::CalendarService::open(
            "deepthought",
            "Europe/London",
            &file,
            &dir,
            store,
            Default::default(),
        )
        .map_err(|e| anyhow!(e))?;
        eprintln!("calendar: {} entries from {f}", svc.entries().len());
        crate::calendar::install(svc).map_err(|e| anyhow!(e))?;
    }
    let snap = Snapshot::build(RouterConfig::default(), None).map_err(|e| anyhow!(e))?;
    let st = AppState::new(snap, None);
    let demo = crate::control_api::demo::DemoControl::new();
    demo.spawn_ticker();
    {
        // The inventory, downloads and engine store against the demo's config; nothing runs.
        use crate::control_api::ControlBackend;
        let dir = state_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("abhoth-admin-demo"));
        let _ = std::fs::create_dir_all(&dir);
        let main = PathBuf::from(demo.config().path);
        let no_exec: crate::control::downloads::ExecFor =
            Arc::new(|_c, s| Err(format!("admin-demo: nothing runs on `{s}`")));
        crate::control::downloads::install_with(&main, &dir, demo.clone(), no_exec);
        // Bridges against the demo's config: a bridge with a token minted into this state dir
        // (`bridge-token create`) can connect, so the Bridges page shows a real one.
        use crate::control::schema::ControlConfig;
        if let Ok(c) = toml::from_str::<ControlConfig>(&demo.config().text) {
            let hub = crate::bridges::Hub::new(&dir, Arc::new(c));
            crate::bridges::install(hub.clone());
            let _ = st.bridges.set(hub);
        }
    }
    let settings = crate::access::AccessSettings {
        accounts_token: accounts
            .as_ref()
            .map(|_| "secret:accounts_token".to_string()),
        accounts_url: accounts,
        loopback_admin,
        ..Default::default()
    };
    let _ = st.control.set(Arc::new(crate::control_api::ControlPlane {
        access: crate::access::Access::new(settings, token),
        backend: demo,
    }));
    let l = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    eprintln!("abhoth router admin-demo on {listen}");
    server::serve(l, st, Mode::Gateway, shutdown_signal()).await?;
    Ok(0)
}

async fn serve_cmd(args: &[String]) -> Result<i32> {
    let mut config = None;
    let mut host = "0.0.0.0".to_string();
    let mut listen: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut lb_port: Option<u16> = None;
    let mut slots_port: Option<u16> = None;
    let mut profiles_port: Option<u16> = None;
    let mut control: Option<String> = None;
    let mut discovery = crate::discovery::Discovery::default();
    let mut mdns = crate::discovery::mdns::MdnsMode::Auto;
    let mut wyoming: Vec<(crate::control::schema::ModelKind, String)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => config = Some(take(args, &mut i, "--config")?),
            "--port" => port = Some(take(args, &mut i, "--port")?.parse().context("--port")?),
            "--host" => host = take(args, &mut i, "--host")?,
            "--control" => control = Some(take(args, &mut i, "--control")?),
            "--name" => discovery.name = take(args, &mut i, "--name")?,
            "--key-url" => discovery.key_url = Some(take(args, &mut i, "--key-url")?),
            "--mdns" => {
                let v = take(args, &mut i, "--mdns")?;
                mdns = crate::discovery::mdns::MdnsMode::parse(&v)
                    .ok_or_else(|| anyhow!("--mdns takes auto, on or off (not {v})"))?;
            }
            "--listen" => listen = Some(take(args, &mut i, "--listen")?),
            "--wyoming" => {
                let v = take(args, &mut i, "--wyoming")?;
                wyoming
                    .push(crate::media::wyoming_listen::parse_flag(&v).ok_or_else(|| {
                        anyhow!("--wyoming takes stt=ADDR or tts=ADDR (not {v})")
                    })?);
            }
            "--lb-port" => {
                lb_port = Some(
                    take(args, &mut i, "--lb-port")?
                        .parse()
                        .context("--lb-port")?,
                )
            }
            "--slots-port" => {
                slots_port = Some(
                    take(args, &mut i, "--slots-port")?
                        .parse()
                        .context("--slots-port")?,
                )
            }
            "--profiles-port" => {
                profiles_port = Some(
                    take(args, &mut i, "--profiles-port")?
                        .parse()
                        .context("--profiles-port")?,
                )
            }
            a => bail!("unknown argument {a} (abhoth router --help)"),
        }
        i += 1;
    }
    let path = PathBuf::from(
        config.ok_or_else(|| anyhow!("--config is required (abhoth router --help)"))?,
    );
    let snap = Snapshot::load(&path).map_err(|e| anyhow!(e))?;
    let addr = listen.unwrap_or_else(|| format!("{host}:{}", port.unwrap_or(8010)));
    let st = AppState::new(snap, Some(path));
    let mut shim = None;
    if let Some(c) = &control {
        let started = crate::control::startup::start(&st, std::path::Path::new(c))
            .await
            .map_err(|e| anyhow!("--control {c}: {e}"))?;
        eprintln!("abhoth router: {}", started.message);
        shim = started.shim;
    }
    let mut tasks = Vec::new();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let mut bind = |addr: String, mode: Mode| -> Result<()> {
        let std_l =
            std::net::TcpListener::bind(&addr).with_context(|| format!("binding {addr}"))?;
        std_l.set_nonblocking(true)?;
        let l = tokio::net::TcpListener::from_std(std_l)?;
        eprintln!("abhoth router: {mode:?} on {addr}");
        let st = st.clone();
        let mut rx = rx.clone();
        tasks.push(tokio::spawn(async move {
            let stop = async move {
                let _ = rx.wait_for(|v| *v).await;
            };
            if let Err(e) = server::serve(l, st, mode, stop).await {
                eprintln!("abhoth router: {addr}: {e}");
            }
        }));
        Ok(())
    };
    bind(addr.clone(), Mode::Gateway)?;
    // D149: advertise on the LAN (mDNS) unless the gateway is loopback-only or --mdns off.
    let (gw_host, gw_port) = split_host_port(&addr);
    let mut _advert = None;
    if mdns.wanted(&gw_host) {
        let name = crate::discovery::display_name(Some(&discovery));
        let ip = gw_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .ok()
            .filter(|ip| !ip.is_unspecified());
        match crate::discovery::mdns::advertise(&name, gw_port, ip) {
            Ok(a) => {
                eprintln!("abhoth router: mDNS {}", a.fullname());
                discovery.mdns = true;
                _advert = Some(a);
            }
            Err(e) => eprintln!("abhoth router: not advertised on the LAN: {e}"),
        }
    }
    let _ = st.discovery.set(discovery);
    if let Some(p) = lb_port {
        bind(format!("{host}:{p}"), Mode::Lb)?;
    }
    if let Some(p) = slots_port {
        bind(format!("{host}:{p}"), Mode::Slots)?;
    }
    if !wyoming.is_empty() && control.is_none() {
        bail!("--wyoming needs --control (its engines come from the control plane)");
    }
    for (kind, addr) in wyoming {
        // not awaited on shutdown (a held Wyoming connection would only delay it); a port still
        // taken (the old app during a handover) is retried, and never keeps the gateway down
        let st = st.clone();
        tokio::spawn(async move {
            let mut said = false;
            let l = loop {
                match tokio::net::TcpListener::bind(&addr).await {
                    Ok(l) => break l,
                    Err(e) => {
                        if !said {
                            eprintln!(
                                "abhoth router: Wyoming {} on {addr} not open yet ({e}); retrying",
                                kind.as_str()
                            );
                            said = true;
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            };
            eprintln!("abhoth router: Wyoming {} on {addr}", kind.as_str());
            crate::media::wyoming_listen::serve(l, st, kind).await;
        });
    }
    if let Some(p) = profiles_port {
        let Some(shim) = shim.clone() else {
            bail!("--profiles-port needs --control with control.enabled = true");
        };
        let addr = format!("{host}:{p}");
        let std_l =
            std::net::TcpListener::bind(&addr).with_context(|| format!("binding {addr}"))?;
        std_l.set_nonblocking(true)?;
        let l = tokio::net::TcpListener::from_std(std_l)?;
        eprintln!("abhoth router: profiles shim on {addr}");
        tasks.push(tokio::spawn(async move {
            if let Err(e) = crate::switch::shim_backend::serve(l, shim).await {
                eprintln!("abhoth router: profiles shim: {e}");
            }
        }));
    }
    tokio::spawn(crate::reload::watch(st.clone(), Duration::from_secs(2)));
    tokio::spawn(crate::ondemand::background(st.clone()));
    tokio::spawn(crate::observe::run(st.clone()));
    tokio::spawn(crate::observe::canary(st.clone()));
    #[cfg(unix)]
    {
        let st = st.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut hup) = signal(SignalKind::hangup()) {
                while hup.recv().await.is_some() {
                    let _ = crate::reload::reload_now(&st);
                }
            }
        });
    }
    shutdown_signal().await;
    eprintln!("abhoth router: stopping");
    let _ = tx.send(true);
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
    }
    Ok(0)
}

/// `host:port` (or `[v6]:port`) -> (host, port); no port = 8010.
fn split_host_port(addr: &str) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.ends_with(':') => {
            (h.to_string(), p.parse().unwrap_or(8010))
        }
        _ => (addr.to_string(), 8010),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The import-litellm arguments.
#[derive(Debug, Clone)]
pub struct ImportArgs {
    pub litellm: PathBuf,
    pub lb: PathBuf,
    pub gateway: Option<PathBuf>,
    pub key_file: Option<String>,
    pub base: Option<PathBuf>,
    pub out: PathBuf,
}

/// One import; writes `out` atomically, only when the text changed. Ok(true) = written.
pub fn import_once(a: &ImportArgs) -> Result<bool> {
    let litellm =
        std::fs::read_to_string(&a.litellm).with_context(|| a.litellm.display().to_string())?;
    let lb = std::fs::read_to_string(&a.lb).with_context(|| a.lb.display().to_string())?;
    let gateway = match &a.gateway {
        Some(p) => Some(
            serde_json::from_str(
                &std::fs::read_to_string(p).with_context(|| p.display().to_string())?,
            )
            .with_context(|| p.display().to_string())?,
        ),
        None => None,
    };
    let base: Option<RouterConfig> = match &a.base {
        Some(p) if p.exists() => Some(
            serde_json::from_str(&std::fs::read_to_string(p)?)
                .with_context(|| p.display().to_string())?,
        ),
        _ => None,
    };
    let cfg = import::from_litellm(
        &litellm,
        &lb,
        &ImportOptions {
            key_file: a.key_file.clone(),
            gateway,
            base,
        },
    )
    .map_err(|e| anyhow!(e))?;
    let text = serde_json::to_string_pretty(&cfg)? + "\n";
    if std::fs::read_to_string(&a.out).ok().as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    let tmp = a.out.with_extension("json.tmp");
    std::fs::write(&tmp, &text).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, &a.out).with_context(|| a.out.display().to_string())?;
    Ok(true)
}

async fn import_cmd(args: &[String]) -> Result<i32> {
    let (mut litellm, mut lb, mut gateway, mut key_file, mut base, mut out, mut watch) =
        (None, None, None, None, None, None, None::<u64>);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--litellm" => litellm = Some(PathBuf::from(take(args, &mut i, "--litellm")?)),
            "--lb" => lb = Some(PathBuf::from(take(args, &mut i, "--lb")?)),
            "--gateway" => gateway = Some(PathBuf::from(take(args, &mut i, "--gateway")?)),
            "--key-file" => key_file = Some(take(args, &mut i, "--key-file")?),
            "--base" => base = Some(PathBuf::from(take(args, &mut i, "--base")?)),
            "--out" => out = Some(PathBuf::from(take(args, &mut i, "--out")?)),
            "--watch" => watch = Some(take(args, &mut i, "--watch")?.parse().context("--watch")?),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let a = ImportArgs {
        litellm: litellm.ok_or_else(|| anyhow!("--litellm is required"))?,
        lb: lb.ok_or_else(|| anyhow!("--lb is required"))?,
        gateway,
        key_file,
        base,
        out: out.ok_or_else(|| anyhow!("--out is required"))?,
    };
    let Some(every) = watch else {
        let wrote = import_once(&a)?;
        eprintln!(
            "{}: {}",
            a.out.display(),
            if wrote { "written" } else { "unchanged" }
        );
        return Ok(0);
    };
    eprintln!(
        "abhoth router import-litellm: every {every} s -> {}",
        a.out.display()
    );
    let mut last_err = String::new();
    loop {
        match import_once(&a) {
            Ok(true) => {
                eprintln!("{}: written", a.out.display());
                last_err.clear();
            }
            Ok(false) => last_err.clear(),
            Err(e) => {
                let s = format!("{e:#}");
                if s != last_err {
                    eprintln!("import failed (keeping the old router.json): {s}");
                    last_err = s;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(every.max(1))) => {}
            _ = shutdown_signal() => return Ok(0),
        }
    }
}

/// For callers that hold an `Arc<AppState>` already (tests).
pub fn state_from(cfg: RouterConfig) -> Result<Arc<AppState>, String> {
    Ok(AppState::new(Snapshot::build(cfg, None)?, None))
}

fn control_import_cmd(args: &[String]) -> Result<i32> {
    use crate::control::import::{self as ci, KeyChoice, Options, Sources};
    let (mut from, mut out, mut report, mut seed) = (None, None, None, None);
    let mut opt = Options::default();
    let mut key: Option<KeyChoice> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => from = Some(PathBuf::from(take(args, &mut i, "--from")?)),
            "--out" => out = Some(PathBuf::from(take(args, &mut i, "--out")?)),
            "--report" => report = Some(PathBuf::from(take(args, &mut i, "--report")?)),
            "--seed" => seed = Some(PathBuf::from(take(args, &mut i, "--seed")?)),
            "--server" => opt.server = take(args, &mut i, "--server")?,
            "--enable-on-demand" => opt.enable_on_demand = true,
            "--key-file-build" => {
                let v = take(args, &mut i, "--key-file-build")?;
                let (engine, build) = v
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--key-file-build {v}: ENGINE=BUILD"))?;
                opt.key_file_builds
                    .entry(engine.to_string())
                    .or_default()
                    .push(build.to_string());
            }
            "--key-delivery" => {
                key = Some(match take(args, &mut i, "--key-delivery")?.as_str() {
                    "file_flag" => KeyChoice::FileFlag,
                    "wrapper" => KeyChoice::Wrapper,
                    "argv" => KeyChoice::Argv,
                    k => bail!("--key-delivery {k}: file_flag, wrapper or argv"),
                })
            }
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let from = from.ok_or_else(|| anyhow!("--from is required"))?;
    let out = out.ok_or_else(|| anyhow!("--out is required"))?;
    // Default: llama-server reads --api-key-file; ninfer only from our 2026-11-26 fork build,
    // so its key is read inside the container by a wrapper except on builds named with
    // --key-file-build ninfer=<build>.
    opt.keys
        .insert("ninfer".into(), key.unwrap_or(KeyChoice::Wrapper));
    opt.keys
        .insert("llamacpp".into(), key.unwrap_or(KeyChoice::FileFlag));
    let src = Sources::from_dir(&from).map_err(|e| anyhow!(e))?;
    let im = ci::import(&src, &opt).map_err(|e| anyhow!(e))?;
    std::fs::write(&out, &im.toml).with_context(|| format!("writing {}", out.display()))?;
    if let Some(r) = &report {
        std::fs::write(r, ci::report_markdown(&im))
            .with_context(|| format!("writing {}", r.display()))?;
    }
    if let Some(s) = &seed {
        std::fs::write(s, serde_json::to_string_pretty(&im.seed)?)
            .with_context(|| format!("writing {}", s.display()))?;
    }
    let todo = im.report.iter().filter(|r| r.kind == "todo").count();
    println!(
        "wrote {}: {} layouts, {} engines, {} routes; {todo} TODOs in the report",
        out.display(),
        im.config.layouts.len(),
        im.config.engines.len(),
        im.config.routes.len()
    );
    control_check(&out)
}

/// Load and validate a control-plane `router.toml`; exit 1 on errors.
/// `abhoth router admin-key create|list|revoke` (D151): the router's own admin keys, in the
/// `router.db` of the `router.toml`'s `state_dir`. Works while the router runs (SQLite, WAL).
/// `abhoth router sso link|status|unlink` (D154).
async fn sso_cmd(args: &[String]) -> Result<i32> {
    use orca_core::sso::{
        ClientRegistration, LINK_POLL_PATH, LINK_REQUESTS_PATH, LinkStatus, LinkTicket,
        callback_for, redirect_uri_ok,
    };
    let sub = args.first().map(String::as_str).unwrap_or("");
    let (mut control, mut webui, mut name) = (None, None, None);
    let mut public: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--control" | "--config" => control = Some(take(args, &mut i, "--control")?),
            "--webui" => webui = Some(take(args, &mut i, "--webui")?),
            "--name" => name = Some(take(args, &mut i, "--name")?),
            "--public-url" => public.push(take(args, &mut i, "--public-url")?),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let control = PathBuf::from(control.ok_or_else(|| {
        anyhow!("--control router.toml is required (its state_dir keeps the credentials)")
    })?);
    let loaded = crate::control::load::load(&control).map_err(|issues| {
        anyhow!(
            "{}",
            issues
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(
                    "
"
                )
        )
    })?;
    let state_dir = PathBuf::from(&loaded.config.router.state_dir);
    let file = state_dir.join(crate::access::sso::CREDENTIALS_FILE);
    let client = crate::access::sso::SsoClient::open(&state_dir);
    match sub {
        "status" => {
            let a = &loaded.config.access;
            match client.credentials() {
                Some(c) => {
                    println!("registered as {} for:", c.client_id);
                    for u in &c.redirect_uris {
                        println!("  {u}");
                    }
                }
                None if a.accounts_url.is_some() => {
                    println!(
                        "not registered yet; linked, so it registers itself at the first sign-in"
                    )
                }
                None => println!("not registered (abhoth router sso link --webui URL ...)"),
            }
            println!(
                "sign-in at: {}",
                a.sso_url
                    .as_deref()
                    .unwrap_or("(access.sso_url unset: the origin of --key-url when linked)")
            );
            Ok(0)
        }
        "unlink" => {
            match std::fs::remove_file(&file) {
                Ok(()) => println!(
                    "credentials removed; remove the router in Abhoth (Settings > Service keys > Router sign-in) too"
                ),
                Err(_) => println!("no credentials here"),
            }
            Ok(0)
        }
        "link" => {
            let webui = webui
                .ok_or_else(|| {
                    anyhow!("--webui URL (the Abhoth web UI, e.g. https://abhoth.example.org)")
                })?
                .trim_end_matches('/')
                .to_string();
            if public.is_empty() {
                bail!(
                    "--public-url URL: the address you open this router's admin pages at (e.g. http://router.lan:8010); repeat it for several"
                );
            }
            let redirect_uris: Vec<String> = public.iter().map(|u| callback_for(u)).collect();
            for u in &redirect_uris {
                redirect_uri_ok(u).map_err(|e| anyhow!("{u}: {e}"))?;
            }
            let name = name.unwrap_or_else(|| crate::discovery::display_name(None));
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?;
            let r = http
                .post(format!("{webui}{LINK_REQUESTS_PATH}"))
                .json(&ClientRegistration {
                    name: name.clone(),
                    redirect_uris,
                })
                .send()
                .await
                .context("asking the Abhoth web UI")?;
            if !r.status().is_success() {
                let st = r.status();
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                bail!(
                    "the Abhoth web UI refused ({st}): {}",
                    v["error"].as_str().unwrap_or("?")
                );
            }
            let t: LinkTicket = r.json().await.context("the web UI's answer")?;
            println!(
                "Asked {webui} to register router \"{name}\".
An Abhoth admin approves it in Settings > Service keys > Router sign-in; the code there must be:

    {}
",
                t.code
            );
            let until = std::time::Instant::now() + Duration::from_secs(t.expires_in.max(1) as u64);
            while std::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let Ok(r) = http
                    .post(format!("{webui}{LINK_POLL_PATH}"))
                    .json(&serde_json::json!({"id": t.id, "poll": t.poll}))
                    .send()
                    .await
                else {
                    continue;
                };
                let Ok(s) = r.json::<LinkStatus>().await else {
                    continue;
                };
                match (s.status.as_str(), s.credentials) {
                    ("pending", _) => continue,
                    ("approved", Some(c)) => {
                        client.save(&c).map_err(|e| anyhow!(e))?;
                        println!(
                            "Approved: registered as {} (credentials in {}, mode 600).
Set in router.toml:

    [access]
    sso_url = \"{webui}\"
    public_urls = {:?}

then restart the router; its admin pages offer \"Sign in with Abhoth\".",
                            c.client_id,
                            file.display(),
                            public
                        );
                        return Ok(0);
                    }
                    ("denied", _) => bail!("an Abhoth admin denied the request"),
                    (other, _) => bail!("the request is {other}; run link again"),
                }
            }
            bail!("nobody approved the request in time; run link again")
        }
        _ => bail!("abhoth router sso link|status|unlink --control router.toml (see --help)"),
    }
}

fn admin_key_cmd(args: &[String]) -> Result<i32> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let (mut control, mut out, mut name) = (None, None, None);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--control" | "--config" => control = Some(take(args, &mut i, "--control")?),
            "--out" => out = Some(take(args, &mut i, "--out")?),
            a if !a.starts_with("--") && name.is_none() => name = Some(a.to_string()),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let control = PathBuf::from(control.ok_or_else(|| {
        anyhow!("--control router.toml is required (its state_dir holds router.db)")
    })?);
    let loaded = crate::control::load::load(&control).map_err(|issues| {
        anyhow!(
            "{}",
            issues
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let state_dir = PathBuf::from(&loaded.config.router.state_dir);
    let store = Arc::new(
        crate::control::store::Store::open(&state_dir.join("router.db")).map_err(|e| anyhow!(e))?,
    );
    let keys = crate::access::LocalKeys::open(store.clone()).map_err(|e| anyhow!(e))?;
    let now = chrono::Utc::now().timestamp();
    match sub {
        "create" => {
            let name = name.ok_or_else(|| anyhow!("admin-key create NAME"))?;
            let k = keys.create(&name, "cli").map_err(|e| anyhow!(e))?;
            let _ = store.audit(now, "cli", "admin-key.create", "", "", "", &k.info.name);
            match out {
                Some(f) => {
                    write_secret(std::path::Path::new(&f), &k.key)
                        .with_context(|| format!("writing {f}"))?;
                    println!(
                        "admin key {} ({}, id {}) written to {f}; sign in at /admin/ with it",
                        k.info.name, k.info.prefix, k.info.id
                    );
                }
                None => {
                    println!("{}", k.key);
                    eprintln!(
                        "admin key {} (id {}): shown once; sign in at /admin/ with it and keep it somewhere safe",
                        k.info.name, k.info.id
                    );
                }
            }
            Ok(0)
        }
        "list" => {
            for k in keys.list().map_err(|e| anyhow!(e))? {
                println!(
                    "{:>4}  {:<24} {}  {}{}",
                    k.id,
                    k.name,
                    k.prefix,
                    chrono::DateTime::from_timestamp(k.created, 0)
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_default(),
                    if k.revoked.is_some() { "  revoked" } else { "" }
                );
            }
            Ok(0)
        }
        "revoke" => {
            let which = name.ok_or_else(|| anyhow!("admin-key revoke ID|NAME"))?;
            let n = keys.revoke(&which).map_err(|e| anyhow!(e))?;
            let _ = store.audit(now, "cli", "admin-key.revoke", "", "", "", &n);
            println!("revoked {n} (its sessions end on their next request)");
            Ok(0)
        }
        _ => bail!("admin-key create NAME | list | revoke ID|NAME, with --control router.toml"),
    }
}

/// `abhoth router bridge-token create|list|revoke`: bridge tokens (ROUTER-MEDIA-PLAN.md 13.1).
fn bridge_token_cmd(args: &[String]) -> Result<i32> {
    use crate::bridges::tokens;
    let sub = args.first().map(String::as_str).unwrap_or("");
    let (mut control, mut out, mut id, mut rotate) = (None, None, None, false);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--control" | "--config" => control = Some(take(args, &mut i, "--control")?),
            "--out" => out = Some(take(args, &mut i, "--out")?),
            "--rotate" => rotate = true,
            a if !a.starts_with("--") && id.is_none() => id = Some(a.to_string()),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    let control = PathBuf::from(control.ok_or_else(|| {
        anyhow!("--control router.toml is required (its state_dir holds the token hashes)")
    })?);
    let loaded = crate::control::load::load(&control).map_err(|issues| {
        anyhow!(
            "{}",
            issues
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(
                    "
"
                )
        )
    })?;
    let state_dir = PathBuf::from(&loaded.config.router.state_dir);
    match sub {
        "create" => {
            let id = id.ok_or_else(|| anyhow!("bridge-token create ID --out FILE"))?;
            let out = out.ok_or_else(|| {
                anyhow!("--out FILE is required: a bridge token is never printed")
            })?;
            let path = std::path::Path::new(&out);
            if path.exists() {
                bail!("{out} exists; give a new file");
            }
            let token = tokens::mint(&state_dir, &id, rotate).map_err(|e| anyhow!(e))?;
            write_secret(path, &token).with_context(|| format!("writing {out}"))?;
            println!(
                "bridge {id}: token written to {out} (mode 600). Copy it to the bridge's token_file; the router keeps only its hash."
            );
            Ok(0)
        }
        "list" => {
            for t in tokens::list(&state_dir) {
                let when = chrono::DateTime::from_timestamp(t.created, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default();
                println!("{:<32} token made {when}", t.id);
            }
            Ok(0)
        }
        "revoke" => {
            let id = id.ok_or_else(|| anyhow!("bridge-token revoke ID"))?;
            tokens::revoke(&state_dir, &id).map_err(|e| anyhow!(e))?;
            println!("bridge {id}: token revoked (a running router drops it at its next connect)");
            Ok(0)
        }
        _ => bail!("bridge-token create|list|revoke (see --help)"),
    }
}

/// Write a secret to a new file readable by its owner only.
pub(crate) fn write_secret(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    let mut f = o.open(path)?;
    f.write_all(secret.as_bytes())?;
    f.write_all(b"\n")
}

fn control_check(path: &std::path::Path) -> Result<i32> {
    use crate::control::{load, validate};
    let loaded = match load::load(path) {
        Ok(l) => l,
        Err(issues) => {
            for i in issues {
                eprintln!("{i}");
            }
            return Ok(1);
        }
    };
    let issues = validate::validate_loaded(&loaded);
    for i in &issues {
        eprintln!("{i}");
    }
    if validate::has_errors(&issues) {
        return Ok(1);
    }
    println!("ok: rev {}, {} warnings", loaded.rev, issues.len());
    Ok(0)
}

async fn hostctl_cmd(args: &[String]) -> Result<i32> {
    use crate::control::hostctl;
    let sub = args.first().map(String::as_str).unwrap_or("");
    let (mut allowlist, mut config, mut server, mut out) = (None, None, None, None);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--allowlist" => allowlist = Some(PathBuf::from(take(args, &mut i, "--allowlist")?)),
            "--config" => config = Some(PathBuf::from(take(args, &mut i, "--config")?)),
            "--server" => server = Some(take(args, &mut i, "--server")?),
            "--out" => out = Some(PathBuf::from(take(args, &mut i, "--out")?)),
            a => bail!("unknown argument {a}"),
        }
        i += 1;
    }
    match sub {
        "serve" | "exec-root" => {
            let al = allowlist.ok_or_else(|| anyhow!("--allowlist is required"))?;
            Ok(hostctl::serve_stdio(&al, sub == "exec-root").await)
        }
        "export" => {
            let config = config.ok_or_else(|| anyhow!("--config is required"))?;
            let server = server.ok_or_else(|| anyhow!("--server is required"))?;
            let text =
                hostctl::export_cli(&config, &server, out.as_ref()).map_err(|e| anyhow!(e))?;
            if out.is_none() {
                print!("{text}");
            }
            Ok(0)
        }
        other => bail!("hostctl {other}: serve, exec-root or export"),
    }
}
