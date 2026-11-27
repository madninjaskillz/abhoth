//! `abhoth bridge …` (and the image's `abhoth-bridge`):
//!
//! ```text
//! abhoth bridge [--config bridge.toml]        run headless until SIGTERM / Ctrl+C (logs to stdout)
//! abhoth bridge check --config bridge.toml    the descriptor it would send (no token in it)
//! abhoth bridge detect                        the GPUs and CPU it finds
//! abhoth bridge init --id ID --router URL --out bridge.toml [--token-file FILE] [--token-stdin]
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::client::{self, Timing};
use crate::config::BridgeConfig;
use crate::detect;

pub const HELP: &str =
    "abhoth bridge: make this machine a server the router manages (no inbound port)

USAGE:
  abhoth bridge [--config bridge.toml]
        Dial the router, describe this host's cards and engines, start and stop engines for it,
        and carry its requests to them. Logs to stdout; SIGTERM or Ctrl+C stops it (and the
        engines it started). Without --config, the ABHOTH_BRIDGE_* variables configure it.
  abhoth bridge check --config bridge.toml
        Print the descriptor the bridge would send (detected cards, engines, files).
  abhoth bridge detect
        Print the GPUs (nvidia-smi) and the CPU it finds.
  abhoth bridge init --id ID --router URL --out bridge.toml [--token-file FILE] [--token-stdin]
        Write a starter bridge.toml for this host. --token-stdin reads the token a router admin
        minted from stdin into FILE (mode 600; default: next to bridge.toml). It is never printed.

ENVIRONMENT:
  ABHOTH_BRIDGE_ID, ABHOTH_BRIDGE_ROUTER, ABHOTH_BRIDGE_TOKEN_FILE, ABHOTH_BRIDGE_STATE_DIR,
  ABHOTH_BRIDGE_MODEL_DIR, ABHOTH_BRIDGE_TITLE override bridge.toml's [bridge] keys.
";

fn val(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Entry point; `args` start after the word `bridge`. Returns the exit code.
pub async fn main(args: &[String], version: &str) -> i32 {
    match run(args, version).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("abhoth bridge: {e}");
            2
        }
    }
}

async fn run(args: &[String], version: &str) -> Result<i32, String> {
    let sub = args.first().map(String::as_str);
    let rest = match sub {
        Some("check" | "detect" | "init" | "run") => &args[1..],
        _ => args,
    };
    let mut config: Option<PathBuf> = None;
    let (mut id, mut router, mut out, mut token_file, mut token_stdin) =
        (None, None, None, None, false);
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(0);
            }
            "--config" | "-c" => config = Some(PathBuf::from(val(rest, &mut i, "--config")?)),
            "--id" => id = Some(val(rest, &mut i, "--id")?),
            "--router" => router = Some(val(rest, &mut i, "--router")?),
            "--out" => out = Some(PathBuf::from(val(rest, &mut i, "--out")?)),
            "--token-file" => token_file = Some(val(rest, &mut i, "--token-file")?),
            "--token-stdin" => token_stdin = true,
            a => return Err(format!("unknown argument {a} (see --help)")),
        }
        i += 1;
    }
    let load = |c: &Option<PathBuf>| match c {
        Some(p) => BridgeConfig::load(p),
        None => BridgeConfig::from_env(),
    };
    match sub {
        Some("detect") => {
            let gpus: Vec<serde_json::Value> = detect::gpus()
                .await
                .iter()
                .map(|g| {
                    serde_json::json!({"index": g.index, "uuid": g.uuid, "name": g.name, "vram_gib": g.vram_gib})
                })
                .collect();
            let c = detect::cpu().await;
            let v = serde_json::json!({
                "gpus": gpus,
                "cpu": {"cores": c.cores, "ram_gib": c.ram_gib, "simd": c.simd},
                "os": detect::os(),
                "arch": detect::arch(),
            });
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(0)
        }
        Some("check") => {
            let c = load(&config)?;
            let gpus = if c.detect.nvidia {
                detect::gpus().await
            } else {
                Vec::new()
            };
            let d = detect::descriptor(&c, &gpus, &detect::cpu().await);
            println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            let p = d.problems();
            for x in &p {
                eprintln!("problem: {x}");
            }
            match c.token() {
                Ok(_) => eprintln!("token: {} holds a bridge token", c.bridge.token_file),
                Err(e) => eprintln!("token: {e}"),
            }
            Ok(if p.is_empty() { 0 } else { 1 })
        }
        Some("init") => {
            let id = id.ok_or("init needs --id")?;
            let router = router.ok_or("init needs --router")?;
            let out = out.ok_or("init needs --out bridge.toml")?;
            init(&id, &router, &out, token_file, token_stdin).await
        }
        _ => {
            let c = load(&config)?;
            let log = crate::stdout_log();
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                shutdown_signal().await;
                let _ = tx.send(true);
            });
            client::run(c, version, log, Timing::default(), rx).await;
            Ok(0)
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Write a file only its owner can read (mode 600 on Unix).
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    let mut f = o.open(path)?;
    f.write_all(text.as_bytes())
}

async fn init(
    id: &str,
    router: &str,
    out: &Path,
    token_file: Option<String>,
    token_stdin: bool,
) -> Result<i32, String> {
    if out.exists() {
        return Err(format!("{} exists; give a new file", out.display()));
    }
    let dir = out
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tf = token_file.unwrap_or_else(|| dir.join("bridge.token").display().to_string());
    if token_stdin {
        let mut t = String::new();
        std::io::stdin()
            .read_to_string(&mut t)
            .map_err(|e| format!("reading the token: {e}"))?;
        let t = t.trim();
        if !t.starts_with(crate::proto::TOKEN_PREFIX) {
            return Err("that is not a bridge token (they start with abb_)".into());
        }
        write_private(Path::new(&tf), &format!("{t}\n")).map_err(|e| format!("{tf}: {e}"))?;
    }
    let gpus = detect::gpus().await;
    let mut cards = String::new();
    for g in &gpus {
        cards.push_str(&format!(
            "# [cards.GPU{}]          # {} ({:.0} GiB, {})\n# engines = {{ image = [\"comfyui\"] }}\n# shared = true          # something else also uses this GPU\n\n",
            g.index, g.name, g.vram_gib, g.uuid
        ));
    }
    let text = format!(
        r#"# abhoth bridge for this host (ROUTER-MEDIA-PLAN.md 11). `abhoth bridge check --config {name}`
# shows what it would tell the router.
[bridge]
id = "{id}"
router = "{router}"
token_file = "{tf}"
state_dir = "{state}"
# model_dir = "/models"

[detect]
nvidia = true
cpu = true

{cards}# An engine the router may start here (on demand, stopped after 10 minutes idle):
# [engines.kokoro]
# kinds = ["tts"]
# cards = ["GPU0"]
# port = 18880
# health = "/health"
# [engines.kokoro.docker]
# image = "ghcr.io/remsky/kokoro-fastapi-gpu@sha256:…"
# container_port = 8880
"#,
        name = out.display(),
        tf = tf.replace('\\', "/"),
        state = dir
            .join("bridge-state")
            .display()
            .to_string()
            .replace('\\', "/"),
    );
    std::fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "wrote {} ({} GPU(s) found){}",
        out.display(),
        gpus.len(),
        if token_stdin {
            format!("; token saved to {tf}")
        } else {
            format!("; put the token a router admin made for `{id}` in {tf}")
        }
    );
    Ok(0)
}
