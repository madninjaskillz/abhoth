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
  abhoth router observe-summary --dir DIR [--date YYYY-MM-DD]   (the decision-only shadow's day)
  abhoth router import-litellm --litellm config.yaml --lb nginx.conf [--gateway gateway.json]
                [--key-file PATH] [--base router.json] --out router.json [--watch SECONDS]
  abhoth router check --config router.json
  abhoth router admin-demo --listen 127.0.0.1:8010 --link-token-file FILE [--accounts-url URL]
                (the admin API on an in-memory demo host, for the web UI's admin pages)
  abhoth router import --from DIR --out router.toml [--report report.md] [--seed seed.json]
                [--key-delivery file_flag|wrapper|argv] [--key-file-build ENGINE=BUILD]... [--enable-on-demand] [--server ID]
                (router.toml for the control plane from a copy of the GPU host's layout files)
  abhoth router config-check --config router.toml
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
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => listen = take(args, &mut i, "--listen")?,
            "--link-token-file" => token_file = Some(take(args, &mut i, "--link-token-file")?),
            "--accounts-url" => accounts = Some(take(args, &mut i, "--accounts-url")?),
            // A calendar.toml for the calendar page (in-memory store, the demo's cards A-D).
            "--calendar" => calendar = Some(take(args, &mut i, "--calendar")?),
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
    let settings = crate::access::AccessSettings {
        accounts_token: accounts
            .as_ref()
            .map(|_| "secret:accounts_token".to_string()),
        accounts_url: accounts,
        loopback_admin: false,
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
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => config = Some(take(args, &mut i, "--config")?),
            "--port" => port = Some(take(args, &mut i, "--port")?.parse().context("--port")?),
            "--host" => host = take(args, &mut i, "--host")?,
            "--control" => control = Some(take(args, &mut i, "--control")?),
            "--listen" => listen = Some(take(args, &mut i, "--listen")?),
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
    bind(addr, Mode::Gateway)?;
    if let Some(p) = lb_port {
        bind(format!("{host}:{p}"), Mode::Lb)?;
    }
    if let Some(p) = slots_port {
        bind(format!("{host}:{p}"), Mode::Slots)?;
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
