//! `abhoth router init`: set up a new standalone router in one folder (ROUTER.md 17,
//! docs/design/ROUTER-ADMIN.md 6).
//!
//! It writes a minimal `sothoth.toml` (`[control] enabled = false`, `[inventory] version = 1`,
//! optionally this machine as the first server with its GPUs and CPU as cards), an empty
//! `sothoth.json` (a folder with the old `router.*` names keeps them), the state folder, a first router admin key in a file only its owner can read
//! (printed only with `--print-key`), and optionally a systemd unit, a compose file or a
//! Windows hint. Then it prints the start command and the `/admin/` URL.
//!
//! Interactive by default; `--yes` takes the defaults and the flags without asking. It never
//! overwrites: without `--force` it refuses when any file it would write exists, and with
//! `--force` it moves each one to `<dir>/backups/` first.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::control::telemetry::{self, GpuSample};

pub const HELP: &str = "abhoth router init: set up a new standalone router in one folder

USAGE:
  abhoth router init [DIR] [--yes] [--force]
                [--listen ADDR]            (default 0.0.0.0:8010)
                [--state-dir DIR]          (default DIR/state)
                [--id ID]                  (the router's id, default abhoth)
                [--local | --no-local]     (this machine as the first server, GPUs and CPU
                                            detected; default yes)
                [--server-id ID]           (default local)
                [--key-name NAME | --no-key]   (the first admin key, default admin)
                [--key-file FILE]          (default DIR/secrets/router-admin-key, mode 600)
                [--print-key]              (also print the key; it is never printed otherwise)
                [--service systemd|compose|windows|none]   (repeatable)

  DIR defaults to ./abhoth-router. Without --yes it asks (a terminal is needed).
  It refuses to overwrite any file without --force; --force moves each to DIR/backups/ first.
";

/// What to set up.
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub dir: PathBuf,
    /// `None`: `<dir>/state`.
    pub state_dir: Option<PathBuf>,
    pub listen: String,
    pub id: String,
    pub add_local: bool,
    pub server_id: String,
    /// `None`: no admin key.
    pub key_name: Option<String>,
    /// `None`: `<dir>/secrets/router-admin-key`.
    pub key_file: Option<PathBuf>,
    pub print_key: bool,
    pub services: Vec<Service>,
    pub force: bool,
    /// The program in the start command (`/usr/local/bin/abhoth router` or `abhoth-router`).
    pub program: Vec<String>,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("abhoth-router"),
            state_dir: None,
            listen: "0.0.0.0:8010".into(),
            id: "abhoth".into(),
            add_local: true,
            server_id: "local".into(),
            key_name: Some("admin".into()),
            key_file: None,
            print_key: false,
            services: Vec::new(),
            force: false,
            program: vec!["abhoth".into(), "router".into()],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Systemd,
    Compose,
    Windows,
}

impl Service {
    fn parse(s: &str) -> Result<Option<Self>> {
        Ok(match s {
            "systemd" => Some(Self::Systemd),
            "compose" | "docker" | "docker-compose" => Some(Self::Compose),
            "windows" => Some(Self::Windows),
            "none" | "" => None,
            o => bail!("--service takes systemd, compose, windows or none (not {o})"),
        })
    }
    fn file_name(self) -> &'static str {
        match self {
            Self::Systemd => "abhoth-router.service",
            Self::Compose => "compose.router.yaml",
            Self::Windows => "windows-service.txt",
        }
    }
}

/// This machine's GPUs and CPU, as the router's own detection reports them.
#[derive(Debug, Clone, Default)]
pub struct Detected {
    pub hostname: String,
    pub gpus: Vec<GpuSample>,
    /// [`crate::control::builtins::cpu_info`]'s shape: model, threads, cores, ramGib.
    pub cpu: Option<Value>,
    /// Why no GPUs were found (no nvidia-smi, say); shown, not an error.
    pub gpu_note: Option<String>,
}

/// Detect this machine: `nvidia-smi` with the telemetry query the router polls hosts with
/// (parsed by [`telemetry::parse`]), and [`crate::control::builtins::cpu_info`].
pub fn detect_local() -> Detected {
    let q = format!("--query-gpu={}", telemetry::QUERY_FIELDS.join(","));
    let (gpus, gpu_note) = match std::process::Command::new("nvidia-smi")
        .args([q.as_str(), "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let g = telemetry::parse(&String::from_utf8_lossy(&o.stdout));
            let note = g
                .is_empty()
                .then(|| "nvidia-smi listed no GPUs".to_string());
            (g, note)
        }
        Ok(o) => (
            Vec::new(),
            Some(format!("nvidia-smi failed ({})", o.status)),
        ),
        Err(_) => (Vec::new(), Some("no nvidia-smi on this machine".into())),
    };
    Detected {
        hostname: hostname(),
        gpus,
        cpu: Some(crate::control::builtins::cpu_info()),
        gpu_note,
    }
}

fn hostname() -> String {
    for v in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(v)
            && !h.trim().is_empty()
        {
            return h.trim().to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// What [`init`] did.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub toml: PathBuf,
    pub json: PathBuf,
    pub state_dir: PathBuf,
    pub key_file: Option<PathBuf>,
    /// The key itself: only for `--print-key` (and tests). Never logged.
    pub key: Option<String>,
    pub services: Vec<PathBuf>,
    /// `(original, backup)` for each file moved aside by `--force`.
    pub backups: Vec<(PathBuf, PathBuf)>,
    pub start_command: String,
    pub admin_urls: Vec<String>,
    /// The cards written (id, label).
    pub cards: Vec<(String, String)>,
    /// Warnings from the router's own check of the new router.toml.
    pub warnings: Vec<String>,
}

fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).with_context(|| format!("{}", p.display()))
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A command-line word, quoted for a POSIX shell when it needs it.
fn sh_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:=@,+".contains(c))
    {
        s.to_string()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Card ids for GPUs in nvidia-smi index order: A, B, … Z, AA, AB …
fn card_id(n: usize) -> String {
    let mut n = n;
    let mut s = Vec::new();
    loop {
        s.push((b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s.iter().rev().collect()
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The new `router.toml`.
pub fn render_toml(
    opts: &InitOptions,
    state_dir: &Path,
    local: Option<&Detected>,
    today: &str,
) -> (String, Vec<(String, String)>) {
    let mut t = String::new();
    let mut cards = Vec::new();
    t.push_str(&format!(
        "# Sothoth's (the router's) control-plane config (ROUTER-CONFIG.md), written by\n\
         # `abhoth router init` on {today}. Edit it here or on the admin pages (/admin/).\n\
         version = 1\n\n\
         [router]\n\
         id = {}\n\
         listen = {}\n\
         state_dir = {}\n\n\
         # Off to start: the admin pages edit this file and router.json, and nothing is started,\n\
         # stopped or switched. Turn it on once servers, cards, engines and layouts are set up.\n\
         [control]\n\
         enabled = false\n\n\
         [inventory]\n\
         version = 1\n",
        toml_str(&opts.id),
        toml_str(&opts.listen),
        toml_str(&state_dir.display().to_string()),
    ));
    if let Some(d) = local {
        let title = if d.hostname.is_empty() {
            "this machine".to_string()
        } else {
            d.hostname.clone()
        };
        t.push_str(&format!(
            "\n# This machine (the router runs its commands directly).\n\
             [servers.{}]\n\
             title = {}\n\
             host = \"127.0.0.1\"\n\
             exec = \"local\"\n",
            opts.server_id,
            toml_str(&title),
        ));
        let mut gpus = d.gpus.clone();
        gpus.sort_by_key(|g| g.index);
        if !gpus.is_empty() {
            t.push_str(
                "\n# Its GPUs, by UUID. Not managed yet: set managed = true once a card's engines\n\
                 # and layouts are set up and the control plane is on.\n",
            );
        }
        for (n, g) in gpus.iter().enumerate() {
            let id = card_id(n);
            t.push_str(&format!(
                "[cards.{id}]\nserver = {}\ngpu_uuid = {}\nlabel = {}\n",
                toml_str(&opts.server_id),
                toml_str(&g.uuid),
                toml_str(&g.name),
            ));
            if let Some(m) = g.mem_total_mib {
                t.push_str(&format!("vram_gib = {:?}\n", round1(m as f64 / 1024.0)));
            }
            t.push_str("managed = false\n\n");
            cards.push((id, g.name.clone()));
        }
        if let Some(cpu) = &d.cpu {
            let label = cpu["model"].as_str().unwrap_or("CPU").to_string();
            t.push_str(&format!(
                "{}# Its CPU, for engines that run on the CPU (D153).\n\
                 [cards.cpu]\nserver = {}\ndevice = \"cpu\"\nlabel = {}\n",
                if gpus.is_empty() { "\n" } else { "" },
                toml_str(&opts.server_id),
                toml_str(&label),
            ));
            if let Some(c) = cpu["cores"].as_u64().or_else(|| cpu["threads"].as_u64())
                && c > 0
            {
                t.push_str(&format!("cores = {c}\n"));
            }
            if let Some(r) = cpu["ramGib"].as_f64()
                && r > 0.0
            {
                t.push_str(&format!("ram_gib = {:?}\n", round1(r)));
            }
            t.push_str("managed = false\n");
            cards.push(("cpu".into(), label));
        }
    }
    (t, cards)
}

/// The new `router.json`: no models yet (the Model list page adds them).
pub fn render_json() -> String {
    "{\n  \"upstreams\": {},\n  \"deployments\": []\n}\n".to_string()
}

fn start_command(opts: &InitOptions, json: &Path, toml: &Path) -> String {
    let mut w: Vec<String> = opts.program.iter().map(|s| sh_word(s)).collect();
    for (f, v) in [
        ("--config", json.display().to_string()),
        ("--control", toml.display().to_string()),
        ("--listen", opts.listen.clone()),
    ] {
        w.push(f.into());
        w.push(sh_word(&v));
    }
    w.join(" ")
}

fn admin_urls(listen: &str, host: &str) -> Vec<String> {
    let (h, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']), p),
        None => ("", listen),
    };
    let all = h.is_empty() || h == "0.0.0.0" || h == "::";
    let first = if all || h == "127.0.0.1" || h == "localhost" {
        "127.0.0.1"
    } else {
        h
    };
    let fmt = |h: &str| {
        if h.contains(':') {
            format!("http://[{h}]:{port}/admin/")
        } else {
            format!("http://{h}:{port}/admin/")
        }
    };
    let mut v = vec![fmt(first)];
    if all && !host.is_empty() {
        v.push(fmt(host));
    }
    v
}

fn service_text(s: Service, opts: &InitOptions, dir: &Path, cmd: &str) -> String {
    match s {
        Service::Systemd => format!(
            "# The router as a systemd service, written by `abhoth router init`.\n\
             # Install: sudo cp {file} /etc/systemd/system/ && sudo systemctl daemon-reload\n\
             #          && sudo systemctl enable --now abhoth-router\n\
             # A user of its own is best (User= below, owning {dir}).\n\
             [Unit]\n\
             Description=Abhoth Sothoth (router)\n\
             After=network-online.target\n\
             Wants=network-online.target\n\n\
             [Service]\n\
             ExecStart={cmd}\n\
             WorkingDirectory={dir}\n\
             #User=abhoth\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             NoNewPrivileges=true\n\n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            file = dir.join(s.file_name()).display(),
            dir = dir.display(),
        ),
        Service::Compose => {
            let d = dir.display().to_string();
            let port = opts.listen.rsplit_once(':').map_or("8010", |(_, p)| p);
            format!(
                "# Sothoth (the router) in Docker, written by `abhoth router init`. The image is Sothoth alone,\n\
                 # built from the abhoth repo: docker build -f deploy/router/Dockerfile -t abhoth-sothoth:local .\n\
                 # The folder is mounted at the same path inside, so the paths in its config work there.\n\
                 # It must be writable by uid 568 (sudo chown -R 568:568 {d}). Start:\n\
                 #   docker compose -f {file} up -d\n\
                 services:\n\
                 \x20 sothoth:\n\
                 \x20   image: abhoth-sothoth:local\n\
                 \x20   restart: unless-stopped\n\
                 \x20   user: \"568:568\"\n\
                 \x20   read_only: true\n\
                 \x20   command: [\"--config\", {json}, \"--control\", {toml}, \"--listen\", \"0.0.0.0:{port}\"]\n\
                 \x20   ports:\n\
                 \x20     - \"{port}:{port}\"\n\
                 \x20   volumes:\n\
                 \x20     - {vol}\n\
                 \x20   healthcheck:\n\
                 \x20     test: [\"CMD\", \"curl\", \"-fsS\", \"http://127.0.0.1:{port}/health/liveliness\"]\n\
                 \x20     interval: 30s\n\
                 \x20     timeout: 5s\n\
                 \x20     retries: 3\n",
                file = dir.join(s.file_name()).display(),
                json = serde_json::to_string(&json_path(dir).display().to_string())
                    .unwrap_or_default(),
                toml = serde_json::to_string(&toml_path(dir).display().to_string())
                    .unwrap_or_default(),
                vol = serde_json::to_string(&format!("{d}:{d}")).unwrap_or_default(),
            )
        }
        Service::Windows => format!(
            "The router on Windows (written by `abhoth router init`).\n\n\
             abhoth is not a native Windows service (it does not answer the service manager), so\n\
             `sc.exe create` alone will not start it. Two ways that work:\n\n\
             1. A scheduled task at start-up (an elevated PowerShell):\n\n\
             \x20  schtasks /Create /TN \"Abhoth router\" /SC ONSTART /RU SYSTEM /RL HIGHEST /TR \"{tr}\"\n\
             \x20  schtasks /Run /TN \"Abhoth router\"\n\n\
             2. A service wrapper such as NSSM or WinSW, with this command and {dir} as the\n\
             \x20  working folder:\n\n\
             \x20  {cmd}\n\n\
             Allow the port through Windows Defender Firewall if other machines should reach it.\n",
            tr = cmd.replace('"', "\\\""),
            dir = dir.display(),
        ),
    }
}

/// The control-plane file init writes in `dir`: `sothoth.toml`, or `router.toml` when the
/// folder already has the old names (docs/NAMES.md; files are never renamed).
pub fn toml_path(dir: &Path) -> PathBuf {
    dir.join(if old_names(dir) {
        "router.toml"
    } else {
        "sothoth.toml"
    })
}

/// The gateway file init writes in `dir`: `sothoth.json`, or `router.json` (see [`toml_path`]).
pub fn json_path(dir: &Path) -> PathBuf {
    dir.join(if old_names(dir) {
        "router.json"
    } else {
        "sothoth.json"
    })
}

/// Whether `dir` has a file under an old name (`router.toml` / `router.json`) and none under a
/// new one: then init keeps to the old names, so one folder never mixes them.
fn old_names(dir: &Path) -> bool {
    let any = |names: [&str; 2]| names.iter().any(|n| exists(&dir.join(n)));
    any(["router.toml", "router.json"]) && !any(["sothoth.toml", "sothoth.json"])
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Set up the router. `local` is this machine's detection (`None`: no first server). Writes
/// nothing when it refuses.
pub fn init(opts: &InitOptions, local: Option<&Detected>) -> Result<Outcome> {
    let dir = absolute(&opts.dir)?;
    let state_dir = absolute(&opts.state_dir.clone().unwrap_or_else(|| dir.join("state")))?;
    let (toml, json) = (toml_path(&dir), json_path(&dir));
    let key_file = match &opts.key_name {
        Some(_) => {
            Some(absolute(&opts.key_file.clone().unwrap_or_else(|| {
                dir.join("secrets").join("router-admin-key")
            }))?)
        }
        None => None,
    };
    if let Some(n) = &opts.key_name {
        crate::access::local_keys::check_name(n).map_err(|e| anyhow!(e))?;
    }
    if opts.add_local {
        let ok = !opts.server_id.is_empty()
            && opts
                .server_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            bail!(
                "--server-id {:?}: letters, digits, - and _ only",
                opts.server_id
            );
        }
    }
    if exists(&state_dir) && !state_dir.is_dir() {
        bail!("{} exists and is not a folder", state_dir.display());
    }
    let services: Vec<(Service, PathBuf)> = {
        let mut v: Vec<(Service, PathBuf)> = Vec::new();
        for s in &opts.services {
            if !v.iter().any(|(x, _)| x == s) {
                v.push((*s, dir.join(s.file_name())));
            }
        }
        v
    };

    // Refuse (writing nothing) unless forced.
    let mut targets: Vec<PathBuf> = vec![toml.clone(), json.clone()];
    targets.extend(key_file.iter().cloned());
    targets.extend(services.iter().map(|(_, p)| p.clone()));
    let present: Vec<&PathBuf> = targets.iter().filter(|p| exists(p)).collect();
    if !present.is_empty() && !opts.force {
        bail!(
            "refusing to overwrite (run again with --force to back them up to {} and write new ones):\n  {}",
            dir.join("backups").display(),
            present
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    let now = chrono::Utc::now();
    let today = now.format("%Y-%m-%d").to_string();
    let (toml_text, cards) =
        render_toml(opts, &state_dir, local.filter(|_| opts.add_local), &today);

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut out = Outcome {
        toml: toml.clone(),
        json: json.clone(),
        state_dir: state_dir.clone(),
        key_file: key_file.clone(),
        cards,
        ..Default::default()
    };

    // Back up what is there (forced).
    if !present.is_empty() {
        let b = dir.join("backups");
        std::fs::create_dir_all(&b).with_context(|| format!("creating {}", b.display()))?;
        let stamp = now.format("%Y%m%d-%H%M%S");
        for p in present {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let mut dest = b.join(format!("{name}.bak-{stamp}"));
            let mut n = 1;
            while exists(&dest) {
                n += 1;
                dest = b.join(format!("{name}.bak-{stamp}-{n}"));
            }
            // A rename keeps a key file's owner-only mode.
            std::fs::rename(p, &dest)
                .with_context(|| format!("backing up {} to {}", p.display(), dest.display()))?;
            out.backups.push((p.clone(), dest));
        }
    }

    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    write_new(&toml, &toml_text)?;
    write_new(&json, &render_json())?;

    // The router's own check of what was written.
    let loaded = crate::control::load::load(&toml).map_err(|issues| {
        anyhow!(
            "the new router.toml does not load (a bug in init):\n{}",
            issues
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let issues = crate::control::validate::validate_loaded(&loaded);
    if crate::control::validate::has_errors(&issues) {
        bail!(
            "the new router.toml has errors (a bug in init):\n{}",
            issues
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    out.warnings = issues.iter().map(|i| i.to_string()).collect();
    crate::config::Snapshot::load(&json).map_err(|e| anyhow!("router.json: {e}"))?;

    // The first admin key, in router.db; the file is the only copy.
    if let (Some(name), Some(kf)) = (&opts.key_name, &key_file) {
        let store = Arc::new(
            crate::control::store::Store::open(&state_dir.join("router.db"))
                .map_err(|e| anyhow!(e))?,
        );
        let keys = crate::access::LocalKeys::open(store.clone()).map_err(|e| anyhow!(e))?;
        let k = keys.create(name, "init").map_err(|e| anyhow!(e))?;
        let _ = store.audit(
            now.timestamp(),
            "cli",
            "admin-key.create",
            "",
            "",
            "",
            &k.info.name,
        );
        crate::cli::write_secret(kf, &k.key)
            .with_context(|| format!("writing {}", kf.display()))?;
        out.key = Some(k.key);
    }

    out.start_command = start_command(opts, &json, &toml);
    out.admin_urls = admin_urls(
        &opts.listen,
        local.map(|d| d.hostname.as_str()).unwrap_or(""),
    );
    for (s, p) in services {
        write_new(&p, &service_text(s, opts, &dir, &out.start_command))?;
        out.services.push(p);
    }
    Ok(out)
}

/// Write a file that must not exist yet (the backups moved any old one aside).
fn write_new(path: &Path, text: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// The summary printed after [`init`]. The key appears only with `print_key`.
pub fn report(o: &Outcome, print_key: bool, w: &mut dyn Write) -> std::io::Result<()> {
    for (from, to) in &o.backups {
        writeln!(w, "backed up {} to {}", from.display(), to.display())?;
    }
    writeln!(w, "wrote {}", o.toml.display())?;
    writeln!(w, "wrote {}", o.json.display())?;
    writeln!(w, "state folder {}", o.state_dir.display())?;
    for (id, label) in &o.cards {
        writeln!(w, "  card {id}: {label}")?;
    }
    for warn in &o.warnings {
        writeln!(w, "  {warn}")?;
    }
    if let Some(k) = &o.key_file {
        writeln!(
            w,
            "admin key written to {} (owner-only; it is the only copy)",
            k.display()
        )?;
    }
    if print_key && let Some(k) = &o.key {
        writeln!(w, "admin key: {k}")?;
    }
    for s in &o.services {
        writeln!(w, "wrote {}", s.display())?;
    }
    writeln!(w, "\nStart the router:\n  {}", o.start_command)?;
    writeln!(
        w,
        "\nThen open the admin pages and sign in with the admin key:"
    )?;
    for u in &o.admin_urls {
        writeln!(w, "  {u}")?;
    }
    if o.key_file.is_none() {
        writeln!(
            w,
            "  (no admin key yet: abhoth router admin-key create NAME --control {} --out FILE)",
            o.toml.display()
        )?;
    }
    Ok(())
}

// ── the command line ─────────────────────────────────────────────────────────────────────────

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

/// What the flags said; `None` fields are asked about (interactive) or defaulted (`--yes`).
#[derive(Debug, Default)]
struct Flags {
    dir: Option<PathBuf>,
    yes: bool,
    force: bool,
    listen: Option<String>,
    state_dir: Option<PathBuf>,
    id: Option<String>,
    local: Option<bool>,
    server_id: Option<String>,
    key_name: Option<Option<String>>,
    key_file: Option<PathBuf>,
    print_key: bool,
    services: Option<Vec<Service>>,
}

fn parse_flags(args: &[String]) -> Result<Option<Flags>> {
    let mut f = Flags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Ok(None),
            "--yes" | "-y" => f.yes = true,
            "--force" => f.force = true,
            "--listen" => f.listen = Some(take(args, &mut i, "--listen")?),
            "--state-dir" => f.state_dir = Some(take(args, &mut i, "--state-dir")?.into()),
            "--id" => f.id = Some(take(args, &mut i, "--id")?),
            "--local" => f.local = Some(true),
            "--no-local" => f.local = Some(false),
            "--server-id" => f.server_id = Some(take(args, &mut i, "--server-id")?),
            "--key-name" => f.key_name = Some(Some(take(args, &mut i, "--key-name")?)),
            "--no-key" => f.key_name = Some(None),
            "--key-file" => f.key_file = Some(take(args, &mut i, "--key-file")?.into()),
            "--print-key" => f.print_key = true,
            "--service" => {
                let v = take(args, &mut i, "--service")?;
                let s = f.services.get_or_insert_with(Vec::new);
                for part in v.split(',') {
                    if let Some(x) = Service::parse(part.trim())? {
                        s.push(x);
                    }
                }
            }
            a if !a.starts_with('-') && f.dir.is_none() => f.dir = Some(a.into()),
            a => bail!("unknown argument {a} (abhoth router init --help)"),
        }
        i += 1;
    }
    Ok(Some(f))
}

/// The start command's program: this binary, by its full path when it can be found.
fn this_program() -> Vec<String> {
    let exe = std::env::current_exe().ok();
    let stem = exe
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("abhoth")
        .to_string();
    let path = exe
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| stem.clone());
    if stem.starts_with("abhoth-router") || stem.starts_with("abhoth-sothoth") {
        vec![path]
    } else if stem.starts_with("abhoth") {
        vec![path, "router".into()]
    } else {
        // a test binary or something else embedding the router
        vec!["abhoth".into(), "router".into()]
    }
}

fn ask(input: &mut dyn BufRead, out: &mut dyn Write, q: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        write!(out, "{q}: ")?;
    } else {
        write!(out, "{q} [{default}]: ")?;
    }
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("no answer (end of input)");
    }
    let a = line.trim();
    Ok(if a.is_empty() {
        default.to_string()
    } else {
        a.to_string()
    })
}

fn ask_yes(input: &mut dyn BufRead, out: &mut dyn Write, q: &str, default: bool) -> Result<bool> {
    loop {
        write!(out, "{q} [{}]: ", if default { "Y/n" } else { "y/N" })?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("no answer (end of input)");
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(out, "  please answer y or n")?,
        }
    }
}

fn describe(d: &Detected) -> String {
    let mut gpus: Vec<&GpuSample> = d.gpus.iter().collect();
    gpus.sort_by_key(|g| g.index);
    let mut parts: Vec<String> = gpus
        .into_iter()
        .map(|g| match g.mem_total_mib {
            Some(m) => format!("{} ({:.0} GiB)", g.name, m as f64 / 1024.0),
            None => g.name.clone(),
        })
        .collect();
    if parts.is_empty() {
        parts.push(d.gpu_note.clone().unwrap_or_else(|| "no GPUs".into()));
    }
    if let Some(c) = &d.cpu {
        let model = c["model"].as_str().unwrap_or("CPU");
        parts.push(match c["threads"].as_u64() {
            Some(t) => format!("{model}, {t} threads"),
            None => model.to_string(),
        });
    }
    parts.join("; ")
}

/// `abhoth router init` with its input and output (tests drive the questions). `detect` is
/// called only when this machine is to be added.
pub fn run_with(
    args: &[String],
    interactive: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    detect: &dyn Fn() -> Detected,
) -> Result<i32> {
    let Some(f) = parse_flags(args)? else {
        write!(out, "{HELP}")?;
        return Ok(0);
    };
    let ask_them = !f.yes;
    if ask_them && !interactive {
        bail!(
            "abhoth router init asks questions and this is not a terminal: pass --yes (and flags; abhoth router init --help)"
        );
    }
    let d = InitOptions::default();
    let mut o = InitOptions {
        force: f.force,
        print_key: f.print_key,
        state_dir: f.state_dir.clone(),
        key_file: f.key_file.clone(),
        program: this_program(),
        ..InitOptions::default()
    };
    if ask_them {
        writeln!(
            out,
            "Setting up a new router. Enter takes the answer in brackets; nothing is written until the end.\n"
        )?;
    }
    o.dir = match f.dir {
        Some(p) => p,
        None if ask_them => ask(
            input,
            out,
            "Folder for sothoth.toml, sothoth.json and the state",
            &d.dir.display().to_string(),
        )?
        .into(),
        None => d.dir.clone(),
    };
    o.listen = match f.listen {
        Some(l) => l,
        None if ask_them => ask(input, out, "Listen on (address:port)", &d.listen)?,
        None => d.listen.clone(),
    };
    o.id = f.id.unwrap_or(d.id);
    o.server_id = f.server_id.unwrap_or(d.server_id);
    let mut detected = None;
    o.add_local = match f.local {
        Some(b) => b,
        None if ask_them => {
            let det = detect();
            writeln!(out, "This machine: {}", describe(&det))?;
            let yes = ask_yes(
                input,
                out,
                "Add this machine as the first server (its GPUs and CPU as cards)?",
                true,
            )?;
            detected = Some(det);
            yes
        }
        None => true,
    };
    if o.add_local && detected.is_none() {
        detected = Some(detect());
    }
    o.key_name = match f.key_name {
        Some(k) => k,
        None if ask_them => {
            if ask_yes(input, out, "Create a router admin key (for /admin/)?", true)? {
                Some(ask(input, out, "  Its name", "admin")?)
            } else {
                None
            }
        }
        None => d.key_name.clone(),
    };
    o.services = match f.services {
        Some(s) => s,
        None if ask_them => {
            let a = ask(
                input,
                out,
                "Also write a service file: none, systemd, compose or windows",
                "none",
            )?;
            let mut v = Vec::new();
            for part in a.split(',') {
                if let Some(s) = Service::parse(part.trim())? {
                    v.push(s);
                }
            }
            v
        }
        None => Vec::new(),
    };
    if ask_them {
        writeln!(out)?;
        writeln!(
            out,
            "Will write sothoth.toml and sothoth.json in {}{}{}.",
            o.dir.display(),
            if o.key_name.is_some() {
                ", and an admin key file (owner-only)"
            } else {
                ""
            },
            if o.services.is_empty() {
                ""
            } else {
                ", and the service file"
            }
        )?;
        if !ask_yes(input, out, "Go ahead?", true)? {
            writeln!(out, "nothing written")?;
            return Ok(1);
        }
    }
    let outcome = init(&o, detected.as_ref().filter(|_| o.add_local))?;
    report(&outcome, o.print_key, out)?;
    Ok(0)
}

/// `abhoth router init` on the process's terminal.
pub fn run(args: &[String]) -> Result<i32> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    run_with(args, interactive, &mut input, &mut out, &detect_local)
}

#[cfg(test)]
mod tests;
