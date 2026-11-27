//! What the bridge's host has: GPUs (through `nvidia-smi`), the CPU and its RAM (within the
//! container's limits, when it runs in one), and the model files under `model_dir`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::config::BridgeConfig;
use crate::proto::{CardDesc, DescLimits, Descriptor, EngineDesc, PROTOCOL, ServerDesc};

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub vram_gib: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cpu {
    pub cores: u32,
    pub ram_gib: f64,
    pub simd: Vec<String>,
}

/// `nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv,noheader,nounits` lines.
pub fn parse_gpus(csv: &str) -> Vec<Gpu> {
    csv.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 4 {
                return None;
            }
            Some(Gpu {
                index: f[0].parse().ok()?,
                uuid: f[1].to_string(),
                name: f[2].to_string(),
                vram_gib: (f[3].parse::<f64>().ok()? / 1024.0 * 10.0).round() / 10.0,
            })
        })
        .collect()
}

fn quiet(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        // No console window flashes up from the desktop app (CREATE_NO_WINDOW).
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdin(Stdio::null()).stderr(Stdio::null())
}

/// GPUs from `nvidia-smi` (none when it is missing or fails).
pub async fn gpus() -> Vec<Gpu> {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=index,uuid,name,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    quiet(&mut cmd);
    match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => parse_gpus(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// `cpu.max` (`400000 100000` = 4 CPUs, `max 100000` = no limit).
pub fn parse_cpu_max(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let quota: f64 = it.next()?.parse().ok()?;
    let period: f64 = it.next()?.parse().ok()?;
    (period > 0.0).then(|| (quota / period).ceil().max(1.0) as u32)
}

/// `MemTotal:  32768000 kB` from /proc/meminfo, in GiB.
pub fn parse_meminfo(s: &str) -> Option<f64> {
    let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0 / 1024.0)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The CPU card's facts: cores and RAM within the container's limits.
pub async fn cpu() -> Cpu {
    let mut cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let mut ram = None;
    if cfg!(target_os = "linux") {
        if let Some(n) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
            .ok()
            .as_deref()
            .and_then(parse_cpu_max)
        {
            cores = cores.min(n);
        }
        ram = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(parse_meminfo);
        if let Some(limit) = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            let gib = limit / 1024.0 / 1024.0 / 1024.0;
            ram = Some(ram.map_or(gib, |r: f64| r.min(gib)));
        }
    } else if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ]);
        quiet(&mut cmd);
        if let Ok(Ok(o)) = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
            ram = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
                .map(|b| b / 1024.0 / 1024.0 / 1024.0);
        }
    }
    Cpu {
        cores,
        ram_gib: round1(ram.unwrap_or(0.0)),
        simd: simd(),
    }
}

#[cfg(target_arch = "x86_64")]
fn simd() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if std::arch::is_x86_feature_detected!("avx2") {
        v.push("avx2".into());
    }
    if std::arch::is_x86_feature_detected!("fma") {
        v.push("fma".into());
    }
    if std::arch::is_x86_feature_detected!("avx512f") {
        v.push("avx512f".into());
    }
    v
}

#[cfg(target_arch = "aarch64")]
fn simd() -> Vec<String> {
    vec!["neon".into()]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn simd() -> Vec<String> {
    Vec::new()
}

pub fn os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    }
}

pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    }
}

/// Model files per folder under `model_dir` (relative paths, two levels deep, at most 2000).
pub fn files(model_dir: &Path, folders: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let tops: Vec<String> = if folders.is_empty() {
        match std::fs::read_dir(model_dir) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect(),
            Err(_) => return out,
        }
    } else {
        folders.to_vec()
    };
    let mut budget = 2000usize;
    for top in tops {
        if top.contains("..") {
            continue;
        }
        let dir = model_dir.join(&top);
        let mut list = Vec::new();
        walk(&dir, "", 2, &mut list, &mut budget);
        list.sort();
        if !list.is_empty() {
            out.insert(top, list);
        }
    }
    out
}

fn walk(dir: &Path, prefix: &str, depth: u32, out: &mut Vec<String>, budget: &mut usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if *budget == 0 {
            return;
        }
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(".part") {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match e.file_type() {
            Ok(t) if t.is_dir() && depth > 1 => walk(&e.path(), &rel, depth - 1, out, budget),
            Ok(t) if t.is_file() => {
                out.push(rel);
                *budget -= 1;
            }
            _ => {}
        }
    }
}

/// Build the descriptor: detected cards with the config's overrides, configured engines, files.
pub fn descriptor(c: &BridgeConfig, gpus: &[Gpu], cpu: &Cpu) -> Descriptor {
    let mut cards = Vec::new();
    if c.detect.nvidia {
        for g in gpus {
            if !c.detect.gpus.is_empty() && !c.detect.gpus.contains(&g.uuid) {
                continue;
            }
            cards.push(CardDesc {
                id: format!("GPU{}", g.index),
                device: "gpu".into(),
                gpu_uuid: Some(g.uuid.clone()),
                label: g.name.clone(),
                vram_gib: Some(g.vram_gib),
                ..CardDesc::default()
            });
        }
    }
    if c.detect.cpu {
        cards.push(CardDesc {
            id: "CPU".into(),
            device: "cpu".into(),
            label: "CPU".into(),
            cores: Some(cpu.cores),
            ram_gib: Some(cpu.ram_gib),
            simd: cpu.simd.clone(),
            ..CardDesc::default()
        });
    }
    // Overrides: change detected cards, add a configured CPU card, drop disabled ones.
    for (id, o) in &c.cards {
        if !cards.iter().any(|x| &x.id == id) && id.eq_ignore_ascii_case("cpu") {
            cards.push(CardDesc {
                id: id.clone(),
                device: "cpu".into(),
                label: "CPU".into(),
                simd: cpu.simd.clone(),
                ..CardDesc::default()
            });
        }
        if let Some(card) = cards.iter_mut().find(|x| &x.id == id) {
            if let Some(v) = &o.label {
                card.label = v.clone();
            }
            if o.vram_gib.is_some() {
                card.vram_gib = o.vram_gib;
            }
            if o.cores.is_some() {
                card.cores = o.cores;
            }
            if o.ram_gib.is_some() {
                card.ram_gib = o.ram_gib;
            }
            if o.reserve_gib.is_some() {
                card.reserve_gib = o.reserve_gib;
            }
            if let Some(s) = o.shared {
                card.shared = s;
            }
            if !o.engines.is_empty() {
                card.engines = o.engines.clone();
            }
        }
    }
    cards.retain(|x| !c.cards.get(&x.id).is_some_and(|o| o.disable));
    // Cards with no engines listed take every configured engine that names them, per kind.
    for card in &mut cards {
        if card.engines.is_empty() {
            for (eid, e) in &c.engines {
                if e.cards.contains(&card.id) {
                    for k in &e.kinds {
                        card.engines.entry(k.clone()).or_default().push(eid.clone());
                    }
                }
            }
        }
    }
    let present: Vec<String> = cards.iter().map(|x| x.id.clone()).collect();
    let engines = c
        .engines
        .iter()
        .map(|(id, e)| EngineDesc {
            id: id.clone(),
            package: e.package.clone(),
            method: Some(e.method().into()),
            kinds: e.kinds.clone(),
            caps: e.caps.clone(),
            api: e.api.clone(),
            probe: None,
            ports: e
                .cards
                .iter()
                .filter(|card| present.contains(card))
                .filter_map(|card| e.port_for(card).map(|p| (card.clone(), p)))
                .collect(),
            lifecycle: e.lifecycle.clone(),
            idle_stop_seconds: (e.lifecycle == "on_demand").then_some(e.idle_stop_seconds),
            mem_gib: e.mem_gib,
        })
        .collect();
    let files = c
        .bridge
        .model_dir
        .as_deref()
        .map(|d| files(Path::new(d), &c.bridge.file_folders))
        .unwrap_or_default();
    Descriptor {
        v: PROTOCOL,
        server: ServerDesc {
            id: c.bridge.id.clone(),
            title: if c.bridge.title.is_empty() {
                c.bridge.id.clone()
            } else {
                c.bridge.title.clone()
            },
            os: os().into(),
            arch: arch().into(),
            model_dir: c.bridge.model_dir.clone(),
            comfy_root: c.bridge.comfy_root.clone(),
        },
        cards,
        engines,
        files,
        limits: DescLimits {
            max_concurrent: c.limits.max_concurrent,
        },
        remote_config: false,
        config_rev: None,
        models: Vec::new(),
    }
}

/// Leave out what is not shared: engines `keep` refuses (from `engines` and from every card's
/// list), and files under `drop_files` prefixes (an unshared model's folder).
pub fn share_only(d: &mut Descriptor, keep: &dyn Fn(&str) -> bool, drop_files: &[String]) {
    d.engines.retain(|e| keep(&e.id));
    for c in &mut d.cards {
        for list in c.engines.values_mut() {
            list.retain(|e| keep(e));
        }
        c.engines.retain(|_, l| !l.is_empty());
    }
    if !drop_files.is_empty() {
        for list in d.files.values_mut() {
            list.retain(|f| {
                let f = f.replace('\\', "/");
                !drop_files
                    .iter()
                    .any(|p| f == *p || f.starts_with(&format!("{p}/")))
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_lines_parse() {
        let g = parse_gpus(
            "0, GPU-aaa, Quadro M4000, 8192\n1, GPU-bbb, NVIDIA GeForce RTX 3070 Ti, 8192\nbad\n",
        );
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].uuid, "GPU-aaa");
        assert_eq!(g[1].index, 1);
        assert!((g[0].vram_gib - 8.0).abs() < 0.01);
    }

    #[test]
    fn cgroup_and_meminfo_parse() {
        assert_eq!(parse_cpu_max("400000 100000"), Some(4));
        assert_eq!(parse_cpu_max("max 100000"), None);
        let gib = parse_meminfo("MemTotal:       32768000 kB\nMemFree: 1 kB\n").unwrap();
        assert!((gib - 31.25).abs() < 0.01);
    }

    #[test]
    fn the_descriptor_merges_detection_with_overrides() {
        let c: BridgeConfig = toml::from_str(
            r#"
            [bridge]
            id = "nas"
            title = "nas.local"
            router = "http://r:8010"
            token_file = "/t"
            [detect]
            gpus = ["GPU-aaa"]
            [cards.GPU0]
            shared = true
            vram_gib = 7.5
            [cards.CPU]
            cores = 4
            ram_gib = 6
            [engines.kokoro]
            kinds = ["tts"]
            cards = ["GPU0", "CPU"]
            port = 18880
            [engines.whisper]
            kinds = ["stt"]
            cards = ["GPU0", "GPU1"]
            ports = { GPU0 = 18881, GPU1 = 18882 }
            "#,
        )
        .unwrap();
        let gpus = parse_gpus("0, GPU-aaa, Quadro M4000, 8192\n1, GPU-bbb, Other, 8192\n");
        let cpu = Cpu {
            cores: 16,
            ram_gib: 31.0,
            simd: vec!["avx2".into()],
        };
        let d = descriptor(&c, &gpus, &cpu);
        assert!(d.problems().is_empty(), "{:?}", d.problems());
        let ids: Vec<&str> = d.cards.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["GPU0", "CPU"]);
        assert!(d.cards[0].shared);
        assert_eq!(d.cards[0].vram_gib, Some(7.5));
        assert_eq!(d.cards[1].cores, Some(4));
        assert_eq!(d.cards[1].ram_gib, Some(6.0));
        assert_eq!(d.cards[0].engines["tts"], ["kokoro"]);
        assert_eq!(d.cards[0].engines["stt"], ["whisper"]);
        // whisper's GPU1 port is dropped: that card was filtered out
        let w = d.engines.iter().find(|e| e.id == "whisper").unwrap();
        assert_eq!(w.ports.len(), 1);
        assert_eq!(d.server.title, "nas.local");
        assert_eq!(d.engines[0].idle_stop_seconds, Some(600));
    }
}
