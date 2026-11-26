//! `abhoth hostd …` subcommands (also the standalone `abhoth-hostd` binary):
//!
//! - `run`: the daemon (systemd, root);
//! - `boot`: PREINIT; persistence, app clocks, power limits and `-rgc` from the last-known-good
//!   policy, then exit (replaces `boot.sh`'s clock part);
//! - `failsafe`: cron, every minute; `-rgc` on every managed card if the heartbeat is older than
//!   2 minutes (a dead daemon never leaves a card at 135 MHz);
//! - `collect`: read-only telemetry to stdout as JSON lines (no socket, no control; safe to
//!   run as an unprivileged user for checks);
//! - `limits-check`, `policy-show`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orca_gpuctl::nvml::NvidiaSmi;
use orca_gpuctl::plans::{boot_plan, failsafe_plan, run_plan};
use orca_gpuctl::{LimitedControl, Mode};

use crate::files::{self, Paths};
use crate::sample::{Collector, Sources, Want, batch_readings};

const USAGE: &str = "\
usage: abhoth hostd <command> [options]

commands:
  run         the host daemon (root, under systemd)
  boot        PREINIT: apply persistence, app clocks, power limits, reset clocks; then exit
  failsafe    cron: reset clocks on every managed card if the heartbeat is stale
  collect     print telemetry as JSON lines (read-only)
  limits-check  parse and check the limits file
  policy-show   print the last-known-good policy
  policy-put    --policy FILE: build a policy from TOML, validate it against the limits and
                send it to the running hostd (or, if none runs, save it as last-known-good)

options:
  --apps DIR          the apps pool root; defaults the paths below to DIR/abhoth/...
  --limits FILE       limits file           (default $APPS/abhoth/etc/hostd-limits.toml)
  --state DIR         state directory       (default $APPS/abhoth/state/hostd)
  --run DIR           runtime directory     (default /run/abhoth)
  --clocks MODE       run: on | dry-run | off (default dry-run until the cutover)
  --nvidia-smi CMD    nvidia-smi command, e.g. \"sudo -n nvidia-smi\" (default nvidia-smi)
  --ipmitool CMD      ipmitool command (default ipmitool)
  --no-bmc            do not read the BMC
  --no-fan-mode       do not read the Supermicro fan mode
  --proc-root DIR     root for /proc (default /)
  --hwmon-dir DIR     hwmon class dir (default /sys/class/hwmon)
  --count N           collect: batches to print (default 1)
  --interval-ms N     collect: gap between batches (default 1000)
  --readings          collect: print flat sensor readings instead of batches
  --policy FILE       policy-put: the policy TOML
  --dev               run: allow a non-root-owned limits file (development only)
";

#[derive(Debug, Clone)]
struct Opts {
    cmd: String,
    paths: Paths,
    clocks: Mode,
    sources: Sources,
    count: u32,
    interval_ms: u64,
    readings: bool,
    dev: bool,
    policy: Option<PathBuf>,
}

fn split_cmd(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut it = args.iter();
    let cmd = it.next().cloned().ok_or("missing command")?;
    let mut apps: Option<PathBuf> = std::env::var_os("APPS").map(PathBuf::from);
    let (mut limits, mut state, mut run) = (None, None, None);
    let mut o = Opts {
        cmd,
        paths: Paths { limits: PathBuf::new(), state_dir: PathBuf::new(), run_dir: PathBuf::new() },
        clocks: Mode::DryRun,
        sources: Sources::default(),
        count: 1,
        interval_ms: 1000,
        readings: false,
        dev: false,
        policy: None,
    };
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--apps" => apps = Some(val()?.into()),
            "--limits" => limits = Some(PathBuf::from(val()?)),
            "--state" => state = Some(PathBuf::from(val()?)),
            "--run" => run = Some(PathBuf::from(val()?)),
            "--clocks" => {
                o.clocks = match val()?.as_str() {
                    "on" => Mode::On,
                    "dry-run" => Mode::DryRun,
                    "off" => Mode::Off,
                    m => return Err(format!("--clocks: unknown mode {m}")),
                }
            }
            "--nvidia-smi" => o.sources.nvidia_smi = split_cmd(&val()?),
            "--ipmitool" => o.sources.ipmitool = split_cmd(&val()?),
            "--no-bmc" => o.sources.ipmitool.clear(),
            "--no-fan-mode" => o.sources.bmc_fan_mode = false,
            "--proc-root" => o.sources.proc_root = val()?.into(),
            "--hwmon-dir" => o.sources.hwmon_dir = val()?.into(),
            "--count" => o.count = val()?.parse().map_err(|_| "--count: a number")?,
            "--interval-ms" => o.interval_ms = val()?.parse().map_err(|_| "--interval-ms: a number")?,
            "--readings" => o.readings = true,
            "--dev" => o.dev = true,
            "--policy" => o.policy = Some(val()?.into()),
            "-h" | "--help" => return Err(String::new()),
            x => return Err(format!("unknown option {x}")),
        }
    }
    let base = apps.map(|a| a.join("abhoth"));
    let need = |p: Option<PathBuf>, rel: &str, what: &str| -> Result<PathBuf, String> {
        p.or_else(|| base.as_ref().map(|b| b.join(rel))).ok_or(format!("{what}: pass --{what} or --apps"))
    };
    let needs_paths = matches!(o.cmd.as_str(), "run" | "boot" | "failsafe" | "limits-check" | "policy-show" | "policy-put");
    if needs_paths {
        o.paths.limits = need(limits, "etc/hostd-limits.toml", "limits")?;
        o.paths.state_dir = need(state, "state/hostd", "state")?;
    }
    o.paths.run_dir = run.unwrap_or_else(|| PathBuf::from("/run/abhoth"));
    Ok(o)
}

/// Runs a subcommand; returns the process exit code.
pub async fn main(args: &[String]) -> i32 {
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("abhoth hostd: {e}");
            }
            eprint!("{USAGE}");
            return 2;
        }
    };
    let r = match o.cmd.as_str() {
        "run" => run(&o).await,
        "boot" => boot(&o),
        "failsafe" => failsafe(&o),
        "collect" => collect(&o).await,
        "limits-check" => crate::limits::load(&o.paths.limits, false).map(|l| println!("ok, sha256 {}", l.hash)).map_err(|e| e.to_string()),
        "policy-show" => files::load_policy(&o.paths).map(|p| match p {
            Some(p) => println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default()),
            None => println!("no policy on disk"),
        }),
        "policy-put" => policy_put(&o).await,
        c => Err(format!("unknown command {c}\n{USAGE}")),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("abhoth hostd {}: {e}", o.cmd);
            1
        }
    }
}

async fn policy_put(o: &Opts) -> Result<(), String> {
    let file = o.policy.as_ref().ok_or("--policy FILE is required")?;
    let limits = crate::limits::load(&o.paths.limits, root_required(o.dev)).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let cur = files::load_policy(&o.paths)?.map(|p| p.rev);
    let doc = crate::policy::from_toml(&text, &limits.limits, cur.map_or(1, |r| r + 1))?;
    #[cfg(unix)]
    if let Ok(s) = tokio::net::UnixStream::connect(o.paths.socket()).await {
        use crate::protocol::{Envelope, FromHostd, ToHostd, read_frame, write_frame};
        let (r, mut w) = s.into_split();
        let mut r = tokio::io::BufReader::new(r);
        write_frame(&mut w, &Envelope::new(Some(1), ToHostd::PolicyPut(doc))).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        loop {
            match read_frame::<_, FromHostd>(&mut r, &mut buf).await.map_err(|e| e.to_string())? {
                None => return Err("hostd closed the connection".into()),
                Some(env) if env.id == Some(1) => {
                    return match env.msg {
                        FromHostd::PolicyApplied { rev } => {
                            println!("policy rev {rev} applied by the running hostd");
                            Ok(())
                        }
                        FromHostd::PolicyRefused { reasons, .. } => Err(format!("refused: {}", reasons.join("; "))),
                        m => Err(format!("unexpected reply {m:?}")),
                    };
                }
                Some(_) => {}
            }
        }
    }
    let v = crate::policy::validate(doc, &limits.limits, cur).map_err(|e| format!("refused: {}", e.join("; ")))?;
    files::save_policy(&o.paths, &v.doc).map_err(|e| e.to_string())?;
    println!("hostd is not running: policy rev {} saved as last-known-good ({})", v.doc.rev, o.paths.policy().display());
    for n in v.notes {
        println!("note: {n}");
    }
    Ok(())
}

fn root_required(dev: bool) -> bool {
    !dev
}

fn smi(o: &Opts) -> NvidiaSmi {
    NvidiaSmi { program: o.sources.nvidia_smi.last().cloned().unwrap_or_else(|| "nvidia-smi".into()) }
}

/// The `boot` plan over the last-known-good policy's power configs.
fn boot(o: &Opts) -> Result<(), String> {
    let limits = crate::limits::load(&o.paths.limits, root_required(o.dev)).map_err(|e| e.to_string())?;
    let Some(p) = files::load_policy(&o.paths)? else {
        println!("no policy on disk yet: nothing to do");
        return Ok(());
    };
    let cards: Vec<(String, orca_gpuctl::CardPowerConfig)> =
        p.power.iter().filter_map(|(label, c)| p.cards.get(label).map(|b| (b.uuid.clone(), c.clone()))).collect();
    let plan = boot_plan(cards.iter().map(|(u, c)| (u.as_str(), c)));
    let mut g = LimitedControl { limits: &limits.limits.gpu, inner: smi(o) };
    let fails = run_plan(&mut g, &plan);
    for (u, c) in &plan {
        println!("{u}: {}", c.describe());
    }
    if fails.is_empty() { Ok(()) } else { Err(fails.join("; ")) }
}

/// The dead-man: if the heartbeat is stale, reset clocks on every card the limits list.
fn failsafe(o: &Opts) -> Result<(), String> {
    let limits = crate::limits::load(&o.paths.limits, root_required(o.dev)).map_err(|e| e.to_string())?;
    let age = files::heartbeat_age(&o.paths, orca_sensors::now_ms());
    let managed: Vec<String> = limits.limits.gpu.gpus.keys().cloned().collect();
    let plan = failsafe_plan(age, &managed);
    if plan.is_empty() {
        return Ok(());
    }
    eprintln!("abhoth hostd failsafe: heartbeat {age:?} is stale; resetting clocks on {} cards", plan.len());
    let mut g = LimitedControl { limits: &limits.limits.gpu, inner: smi(o) };
    let fails = run_plan(&mut g, &plan);
    let _ = files::write_lowered(&o.paths, &[]);
    if fails.is_empty() { Ok(()) } else { Err(fails.join("; ")) }
}

async fn collect(o: &Opts) -> Result<(), String> {
    let mut c = Collector::new(o.sources.clone());
    for i in 0..o.count.max(1) {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(o.interval_ms)).await;
        }
        let b = c.collect(Want { bmc: !o.sources.ipmitool.is_empty(), ..Want::ALL }).await;
        if o.readings {
            for (id, r) in batch_readings(&b) {
                println!("{}", serde_json::json!({"id": id, "v": r.value, "u": r.unit, "ts": r.ts}));
            }
        } else {
            println!("{}", serde_json::to_string(&b).map_err(|e| e.to_string())?);
        }
        for (src, e) in &c.errors {
            eprintln!("{src}: {e}");
        }
    }
    Ok(())
}

async fn run(o: &Opts) -> Result<(), String> {
    let limits = crate::limits::load(&o.paths.limits, root_required(o.dev)).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&o.paths.run_dir).map_err(|e| format!("{}: {e}", o.paths.run_dir.display()))?;
    std::fs::create_dir_all(&o.paths.state_dir).map_err(|e| format!("{}: {e}", o.paths.state_dir.display()))?;
    let (etx, erx) = tokio::sync::mpsc::unbounded_channel();
    let (ctx, clocks_thread) = crate::clocks::spawn(Box::new(smi(o)), limits.limits.gpu.clone(), o.clocks, Some(o.paths.clone()), etx);
    let shared = crate::daemon::Shared::new(env!("CARGO_PKG_VERSION"), limits, Some(o.paths.clone()), ctx.clone());
    tokio::spawn(crate::daemon::clock_events(shared.clone(), erx));
    // Last-known-good: hostd restarts straight into the policy it had.
    match files::load_policy(&o.paths) {
        Ok(Some(p)) => {
            if let Err(e) = shared.apply_policy(p, false).await {
                eprintln!("hostd: the policy on disk no longer passes the limits ({}); running without one", e.join("; "));
            }
        }
        Ok(None) => eprintln!("hostd: no policy yet; telemetry only until the supervisor sends one"),
        Err(e) => eprintln!("hostd: {e}; running without a policy"),
    }
    let sd = crate::daemon::SdNotify::from_env();
    tokio::spawn(crate::daemon::telemetry_loop(
        shared.clone(),
        Collector::new(o.sources.clone()),
        crate::daemon::Intervals::default(),
        sd.clone(),
    ));
    let ports = Arc::new(std::sync::Mutex::new(crate::sniff::PortMap::default()));
    tokio::spawn(crate::daemon::discovery_loop(shared.clone(), o.sources.nvidia_smi.clone(), o.sources.proc_root.clone(), ports.clone()));
    #[cfg(target_os = "linux")]
    crate::sniff::spawn(ports, ctx.clone());
    let server = serve_socket(shared.clone(), &o.paths)?;
    if let Some(s) = &sd {
        s.notify("READY=1");
    }
    eprintln!("hostd: running (clocks {:?}), socket {}", o.clocks, o.paths.socket().display());
    wait_for_signal().await;
    eprintln!("hostd: stopping: every card back to serving");
    if let Some(s) = &sd {
        s.notify("STOPPING=1");
    }
    server.abort();
    crate::daemon::stop_fans(&shared).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = ctx.send(crate::clocks::ClockMsg::Shutdown { reply: tx });
    let _ = tokio::time::timeout(Duration::from_secs(20), rx).await;
    drop(ctx);
    let _ = tokio::task::spawn_blocking(move || clocks_thread.join()).await;
    let _ = std::fs::remove_file(o.paths.socket());
    Ok(())
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Listens on `run_dir/hostd.sock` (mode 0660; the run dir's group, set setgid by the install
/// script, gives `abhoth-exec` access).
#[cfg(unix)]
fn serve_socket(shared: Arc<crate::daemon::Shared>, paths: &Paths) -> Result<tokio::task::JoinHandle<()>, String> {
    use std::os::unix::fs::PermissionsExt;
    let sock = paths.socket();
    let _ = std::fs::remove_file(&sock);
    let l = tokio::net::UnixListener::bind(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660)).map_err(|e| format!("{}: {e}", sock.display()))?;
    Ok(tokio::spawn(async move {
        loop {
            match l.accept().await {
                Ok((s, _)) => {
                    let (r, w) = s.into_split();
                    tokio::spawn(crate::daemon::serve_conn(shared.clone(), tokio::io::BufReader::new(r), w));
                }
                Err(e) => {
                    eprintln!("hostd: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }))
}

#[cfg(not(unix))]
fn serve_socket(_shared: Arc<crate::daemon::Shared>, _paths: &Paths) -> Result<tokio::task::JoinHandle<()>, String> {
    Err("hostd's socket server needs a Unix host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_options() {
        let o = parse(&a("run --apps /pool --clocks on --nvidia-smi sudo")).unwrap();
        assert_eq!(o.paths.limits, PathBuf::from("/pool/abhoth/etc/hostd-limits.toml"));
        assert_eq!(o.paths.state_dir, PathBuf::from("/pool/abhoth/state/hostd"));
        assert_eq!(o.paths.run_dir, PathBuf::from("/run/abhoth"));
        assert_eq!(o.clocks, Mode::On);
        let o = parse(&a("collect --no-bmc --count 3")).unwrap();
        assert!(o.sources.ipmitool.is_empty());
        assert_eq!(o.count, 3);
        assert!(parse(&a("run")).is_err() || std::env::var_os("APPS").is_some());
        assert!(parse(&a("run --clocks maybe --apps x")).is_err());
        assert_eq!(parse(&a("run --apps x")).unwrap().clocks, Mode::DryRun, "dry-run until the cutover");
    }

    #[test]
    fn failsafe_does_nothing_with_a_fresh_heartbeat() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("l.toml"), "[gpus.\"GPU-a\"]\nclock_mhz = [135, 1597]\n").unwrap();
        let mut o = parse(&a(&format!("failsafe --limits {} --state {} --run {} --dev --nvidia-smi /nonexistent-nvidia-smi",
            d.path().join("l.toml").display(), d.path().join("s").display(), d.path().join("r").display()))).unwrap();
        files::write_heartbeat(&o.paths, orca_sensors::now_ms()).unwrap();
        assert!(failsafe(&o).is_ok(), "fresh heartbeat: no commands");
        // Stale (missing) heartbeat: it tries -rgc, and the fake binary fails loudly.
        o.paths.run_dir = d.path().join("other");
        let e = failsafe(&o).unwrap_err();
        assert!(e.contains("GPU-a"), "{e}");
    }
}
