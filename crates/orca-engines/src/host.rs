//! What a host offers, and which of a package's methods fits it (ROUTER-MEDIA-PLAN.md 10).
//!
//! The host takes the **first** method that fits: its OS and architecture, docker, the card's
//! GPU stack (or the CPU), VRAM, RAM, CPU instruction sets and the tools on PATH. Every refused
//! method gets its reasons, so the store tab can say why ("windows/amd64: no asset; needs
//! docker").

use serde::{Deserialize, Serialize};

use crate::manifest::{GpuKind, Manifest, Method, MethodType};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostFacts {
    /// `linux`, `windows`, `macos`.
    pub os: String,
    /// `amd64`, `arm64`.
    pub arch: String,
    #[serde(default)]
    pub docker: bool,
    #[serde(default)]
    pub compose: bool,
    /// Python versions found (`3.12`).
    #[serde(default)]
    pub pythons: Vec<String>,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    #[serde(default)]
    pub cpu: Cpu,
    /// Programs found on PATH (for `requires.commands` and `existing`). `None`: not known, so
    /// such checks pass and the install step finds out.
    #[serde(default)]
    pub path_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    pub index: u32,
    #[serde(default)]
    pub uuid: String,
    pub vendor: GpuKind,
    #[serde(default)]
    pub name: String,
    pub vram_gib: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    #[serde(default)]
    pub cores: u32,
    #[serde(default)]
    pub ram_gib: f64,
    #[serde(default)]
    pub simd: Vec<String>,
}

/// The card an engine is placed on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "device", rename_all = "lowercase")]
pub enum CardSel {
    Gpu {
        card: String,
        index: u32,
        #[serde(default)]
        uuid: String,
        vendor: GpuKind,
        vram_gib: f64,
    },
    Cpu {
        card: String,
        #[serde(default)]
        cores: u32,
        #[serde(default)]
        ram_gib: f64,
    },
}

impl CardSel {
    pub fn id(&self) -> &str {
        match self {
            CardSel::Gpu { card, .. } | CardSel::Cpu { card, .. } => card,
        }
    }
    pub fn stack(&self) -> GpuKind {
        match self {
            CardSel::Gpu { vendor, .. } => *vendor,
            CardSel::Cpu { .. } => GpuKind::Cpu,
        }
    }
    /// The card for a host GPU.
    pub fn for_gpu(card: &str, g: &Gpu) -> CardSel {
        CardSel::Gpu {
            card: card.into(),
            index: g.index,
            uuid: g.uuid.clone(),
            vendor: g.vendor,
            vram_gib: g.vram_gib,
        }
    }
    pub fn cpu(card: &str, h: &HostFacts) -> CardSel {
        CardSel::Cpu {
            card: card.into(),
            cores: h.cpu.cores,
            ram_gib: h.cpu.ram_gib,
        }
    }
}

/// Which methods this host may use at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MethodFilter {
    /// Host processes (pip, uv, binary, git, existing commands) are allowed. False for a router
    /// in a container that reaches its host only through hostctl.
    pub host_processes: bool,
    /// Only this method type (the engine's `method = "docker"`).
    #[serde(default)]
    pub only: Option<MethodType>,
}

impl MethodFilter {
    pub fn local() -> MethodFilter {
        MethodFilter {
            host_processes: true,
            only: None,
        }
    }
    pub fn containers_only() -> MethodFilter {
        MethodFilter {
            host_processes: false,
            only: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    pub method: usize,
    pub label: String,
    pub reasons: Vec<String>,
}

/// Does `os/arch` match a pattern (`linux/amd64`, `linux/*`, `*/arm64`, `*`)?
pub fn platform_matches(pattern: &str, os: &str, arch: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let (po, pa) = pattern.split_once('/').unwrap_or((pattern, "*"));
    (po == "*" || po.eq_ignore_ascii_case(os)) && (pa == "*" || pa.eq_ignore_ascii_case(arch))
}

fn tool_known(h: &HostFacts, name: &str) -> bool {
    match &h.path_tools {
        None => true,
        Some(t) => t.iter().any(|x| {
            x.eq_ignore_ascii_case(name) || x.eq_ignore_ascii_case(&format!("{name}.exe"))
        }),
    }
}

/// Why a method does not fit (empty: it fits).
pub fn method_reasons(
    m: &Manifest,
    i: usize,
    meth: &Method,
    h: &HostFacts,
    card: &CardSel,
    f: &MethodFilter,
) -> Vec<String> {
    let _ = i;
    let mut r = vec![];
    if let Some(only) = f.only
        && only != meth.r#type
    {
        r.push(format!("the engine asks for method {}", only.as_str()));
    }
    if !meth.platforms.is_empty()
        && !meth
            .platforms
            .iter()
            .any(|p| platform_matches(p, &h.os, &h.arch))
    {
        r.push(format!(
            "built for {}, not {}/{}",
            meth.platforms.join(", "),
            h.os,
            h.arch
        ));
    }
    if meth.is_container() {
        if !h.docker {
            r.push("needs docker".into());
        }
        if meth.r#type == MethodType::Compose && !h.compose {
            r.push("needs docker compose".into());
        }
    } else if meth.is_host_process() && !f.host_processes {
        r.push("runs a process on the host, and this host runs engines only in containers".into());
    }
    let gpus = m.method_gpus(meth);
    let stack = card.stack();
    if !gpus.contains(&stack) {
        r.push(format!(
            "uses {}, the card is {}",
            gpus.iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .join("/"),
            stack.as_str()
        ));
    }
    let need_vram = meth.requires.min_vram_gib.or(m.hardware.min_vram_gib);
    if let (CardSel::Gpu { vram_gib, .. }, Some(need)) = (card, need_vram)
        && *vram_gib + 0.01 < need
    {
        r.push(format!("needs {need} GiB VRAM, the card has {vram_gib:.1}"));
    }
    let need_ram = meth.requires.min_ram_gib.or(m.hardware.min_ram_gib);
    let have_ram = match card {
        CardSel::Cpu { ram_gib, .. } if *ram_gib > 0.0 => *ram_gib,
        _ => h.cpu.ram_gib,
    };
    if let Some(need) = need_ram
        && have_ram > 0.0
        && have_ram + 0.01 < need
    {
        r.push(format!("needs {need} GiB RAM, has {have_ram:.1}"));
    }
    if let (CardSel::Cpu { cores, .. }, Some(need)) = (card, m.hardware.min_cores)
        && *cores > 0
        && *cores < need
    {
        r.push(format!("needs {need} cores, the card has {cores}"));
    }
    for s in m.hardware.simd.iter().chain(&meth.requires.simd) {
        if !h.cpu.simd.is_empty() && !h.cpu.simd.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            r.push(format!("needs the CPU feature {s}"));
        }
    }
    for c in &meth.requires.commands {
        if !tool_known(h, c) {
            r.push(format!("needs {c} on PATH"));
        }
    }
    match meth.r#type {
        MethodType::Binary => {
            if pick_assets(m, meth, h, card).is_empty() {
                r.push(format!(
                    "no download for {}/{} ({})",
                    h.os,
                    h.arch,
                    stack.as_str()
                ));
            }
        }
        MethodType::Pip => {
            if let Some(want) = &meth.python
                && !h.pythons.is_empty()
                && !h
                    .pythons
                    .iter()
                    .any(|p| p == want || p.starts_with(&format!("{want}.")))
            {
                r.push(format!(
                    "needs Python {want} (found {})",
                    h.pythons.join(", ")
                ));
            }
        }
        MethodType::Uv => {
            if !tool_known(h, "uv") {
                r.push("needs uv on PATH".into());
            }
        }
        MethodType::Conda => r.push("conda installs are not supported yet".into()),
        MethodType::Git => {
            if !tool_known(h, "git") {
                r.push("needs git on PATH".into());
            }
        }
        MethodType::Existing => {
            if let Some(d) = &meth.detect
                && let Some(c) = &d.command
                && d.paths.is_empty()
                && !tool_known(h, c)
            {
                r.push(format!("{c} is not on PATH"));
            }
        }
        _ => {}
    }
    r
}

/// The binary assets for this host and card (a build may come in several archives, such as
/// llama.cpp's CUDA build and its CUDA runtime). Empty: no build for this host.
pub fn pick_assets<'a>(
    _m: &Manifest,
    meth: &'a Method,
    h: &HostFacts,
    card: &CardSel,
) -> Vec<&'a crate::manifest::Asset> {
    let stack = card.stack();
    meth.assets
        .iter()
        .filter(|a| platform_matches(&a.platform, &h.os, &h.arch))
        .filter(|a| a.gpu.is_empty() || a.gpu.contains(&stack))
        .collect()
}

/// The first method that fits, or every method's reasons.
pub fn choose_method<'a>(
    m: &'a Manifest,
    h: &HostFacts,
    card: &CardSel,
    f: &MethodFilter,
) -> Result<(usize, &'a Method), Vec<Rejection>> {
    let mut rej = vec![];
    for (i, meth) in m.methods.iter().enumerate() {
        let reasons = method_reasons(m, i, meth, h, card, f);
        if reasons.is_empty() {
            return Ok((i, meth));
        }
        rej.push(Rejection {
            method: i,
            label: meth.label(),
            reasons,
        });
    }
    Err(rej)
}

/// The store tab's hardware check for one card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fit {
    pub ok: bool,
    pub method: Option<usize>,
    pub method_label: Option<String>,
    pub method_type: Option<MethodType>,
    pub rejections: Vec<Rejection>,
    pub warnings: Vec<String>,
    /// What the admin must approve before installing with the chosen method.
    pub approvals: Vec<String>,
}

pub fn fit(m: &Manifest, h: &HostFacts, card: &CardSel, f: &MethodFilter) -> Fit {
    let mut warnings = vec![];
    if let CardSel::Gpu { vram_gib, .. } = card
        && let Some(need) = m.hardware.min_vram_gib
        && *vram_gib < need * 1.2
        && *vram_gib >= need
    {
        warnings.push(format!(
            "tight: needs {need} GiB of the card's {vram_gib:.1} GiB"
        ));
    }
    match choose_method(m, h, card, f) {
        Ok((i, meth)) => {
            let mut approvals = vec![];
            if meth.r#type.runs_on_host() {
                approvals.push("runs on the host, not sandboxed".to_string());
            }
            let s = &m.security;
            if s.privileged {
                approvals.push("privileged container".into());
            }
            if s.host_network {
                approvals.push("host network".into());
            }
            for hm in &s.host_mounts {
                approvals.push(format!("host mount {} ({})", hm.source, hm.why));
            }
            for d in &s.devices {
                approvals.push(format!("device {d}"));
            }
            // Earlier (preferred) methods that were refused are worth a note.
            let rejections = m
                .methods
                .iter()
                .enumerate()
                .take(i)
                .map(|(j, mm)| Rejection {
                    method: j,
                    label: mm.label(),
                    reasons: method_reasons(m, j, mm, h, card, f),
                })
                .collect();
            Fit {
                ok: true,
                method: Some(i),
                method_label: Some(meth.label()),
                method_type: Some(meth.r#type),
                rejections,
                warnings,
                approvals,
            }
        }
        Err(rejections) => Fit {
            ok: false,
            method: None,
            method_label: None,
            method_type: None,
            rejections,
            warnings,
            approvals: vec![],
        },
    }
}

/// The facts of the host this process runs on (the bridge, or a router with `exec = "local"`).
/// GPUs come from `nvidia-smi` when it is present; RAM and SIMD from the OS where cheap.
pub async fn detect_local() -> HostFacts {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        o => o,
    }
    .to_string();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    }
    .to_string();
    let tools = local_path_tools(&[
        "docker",
        "git",
        "uv",
        "conda",
        "python3",
        "python",
        "py",
        "nvidia-smi",
        "tar",
        "llama-server",
        "prlimit",
        "nice",
    ]);
    let run = |p: &str, a: &[&str]| {
        let p = p.to_string();
        let a: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        async move {
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::process::Command::new(&p)
                    .args(&a)
                    .stdin(std::process::Stdio::null())
                    .output(),
            )
            .await;
            match out {
                Ok(Ok(o)) if o.status.success() => {
                    Some(String::from_utf8_lossy(&o.stdout).to_string())
                }
                _ => None,
            }
        }
    };
    let has = |t: &str| tools.iter().any(|x| x == t);
    let docker = has("docker")
        && run("docker", &["version", "--format", "{{.Server.Version}}"])
            .await
            .is_some();
    let compose = docker && run("docker", &["compose", "version"]).await.is_some();
    let mut pythons = vec![];
    for p in ["python3", "python"] {
        if has(p)
            && let Some(v) = run(p, &["-c", "import sys;print('%d.%d'%sys.version_info[:2])"]).await
        {
            let v = v.trim().to_string();
            if !pythons.contains(&v) {
                pythons.push(v);
            }
        }
    }
    let mut gpus = vec![];
    if has("nvidia-smi")
        && let Some(csv) = run(
            "nvidia-smi",
            &[
                "--query-gpu=index,uuid,name,memory.total",
                "--format=csv,noheader,nounits",
            ],
        )
        .await
    {
        gpus = parse_nvidia_smi(&csv);
    }
    let cpu = Cpu {
        cores: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0),
        ram_gib: local_ram_gib(),
        simd: local_simd(),
    };
    HostFacts {
        os,
        arch,
        docker,
        compose,
        pythons,
        gpus,
        cpu,
        path_tools: Some(tools),
    }
}

/// `index, uuid, name, memory.total (MiB)` lines.
pub fn parse_nvidia_smi(csv: &str) -> Vec<Gpu> {
    csv.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split(',').map(str::trim).collect();
            if p.len() < 4 {
                return None;
            }
            Some(Gpu {
                index: p[0].parse().ok()?,
                uuid: p[1].to_string(),
                vendor: GpuKind::Cuda,
                name: p[2].to_string(),
                vram_gib: p[3].parse::<f64>().ok()? / 1024.0,
            })
        })
        .collect()
}

/// Which of these programs are on PATH.
pub fn local_path_tools(names: &[&str]) -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return vec![];
    };
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    names
        .iter()
        .filter(|n| find_on_path_in(&dirs, n).is_some())
        .map(|n| n.to_string())
        .collect()
}

pub fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    find_on_path_in(&dirs, name)
}

fn find_on_path_in(dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    for d in dirs {
        for cand in [
            name.to_string(),
            format!("{name}.exe"),
            format!("{name}.cmd"),
        ] {
            let p = d.join(&cand);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn local_ram_gib() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for l in s.lines() {
            if let Some(rest) = l.strip_prefix("MemTotal:") {
                let kb: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0.0);
                return kb / 1_048_576.0;
            }
        }
    }
    0.0
}

fn local_simd() -> Vec<String> {
    let mut out = vec![];
    if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(flags) = s.lines().find(|l| l.starts_with("flags"))
    {
        for f in ["avx", "avx2", "avx512f", "fma", "f16c"] {
            if flags.split_whitespace().any(|x| x == f) {
                out.push(f.to_string());
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if out.is_empty() {
        if std::arch::is_x86_feature_detected!("avx") {
            out.push("avx".into());
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            out.push("avx2".into());
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            out.push("avx512f".into());
        }
    }
    #[cfg(target_arch = "aarch64")]
    out.push("neon".into());
    out
}
