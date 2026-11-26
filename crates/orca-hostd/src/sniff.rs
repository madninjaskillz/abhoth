//! The sniff wake source (gpu-power parity): an AF_PACKET socket with the classic BPF filter
//! from `orca_gpuctl::sniff` sees an HTTP `POST`/`PUT` to a port served by a card's engine and
//! wakes the card ~7 ms after the request, before the engine has started on it.
//!
//! Ports are discovered every 10 s: the card's compute processes (from nvidia-smi) and their
//! near ancestors' listening sockets in `/proc`. Engines in another network namespace (Docker)
//! are matched by (their addresses, port); host-namespace engines by port alone. `extra`
//! ports from the policy (`idle.sniff_ports`) are added per card.
//!
//! Needs root (CAP_NET_RAW); when the socket cannot be opened hostd logs it once and the cards
//! still wake on utilisation and on the router's `wake`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

/// Ports to watch, and which cards each one wakes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortMap {
    /// (destination address bytes, port) → cards; for engines in their own netns.
    pub ip: BTreeMap<(Vec<u8>, u16), BTreeSet<String>>,
    /// port → cards; for host-netns engines and configured ports.
    pub any: BTreeMap<u16, BTreeSet<String>>,
}

impl PortMap {
    pub fn ports(&self) -> Vec<u16> {
        let s: BTreeSet<u16> = self
            .ip
            .keys()
            .map(|(_, p)| *p)
            .chain(self.any.keys().copied())
            .collect();
        s.into_iter().collect()
    }

    pub fn targets(&self, dst: &[u8], port: u16) -> Option<&BTreeSet<String>> {
        self.ip
            .get(&(dst.to_vec(), port))
            .or_else(|| self.any.get(&port))
    }
}

/// Ports hostd itself and common infrastructure listen on; never watched.
const IGNORE_PORTS: [u16; 3] = [22, 53, 111];

fn listen_ports(proc_: &Path, pid: u32, inodes: Option<&BTreeSet<String>>) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for f in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(proc_.join(pid.to_string()).join("net").join(f))
        else {
            continue;
        };
        for line in text.lines().skip(1) {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() > 9
                && p[3] == "0A"
                && inodes.is_none_or(|i| i.contains(p[9]))
                && let Some(port) = p[1]
                    .rsplit(':')
                    .next()
                    .and_then(|h| u16::from_str_radix(h, 16).ok())
            {
                out.insert(port);
            }
        }
    }
    out.retain(|p| !IGNORE_PORTS.contains(p));
    out
}

fn stat_parent(proc_: &Path, pid: u32) -> Option<(String, u32)> {
    let st = std::fs::read_to_string(proc_.join(pid.to_string()).join("stat")).ok()?;
    let comm = st.get(st.find('(')? + 1..st.rfind(')')?)?.to_string();
    let ppid = st
        .get(st.rfind(')')? + 2..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((comm, ppid))
}

/// Socket inodes held by `pid` and its near ancestors (the HTTP server is often the parent of
/// the GPU process). Stops at container shims, systemd and dockerd.
fn sock_inodes(proc_: &Path, mut pid: u32) -> BTreeSet<String> {
    let mut ino = BTreeSet::new();
    for _ in 0..4 {
        if let Ok(rd) = std::fs::read_dir(proc_.join(pid.to_string()).join("fd")) {
            for e in rd.flatten() {
                if let Ok(t) = std::fs::read_link(e.path()) {
                    let t = t.to_string_lossy();
                    if let Some(i) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                        ino.insert(i.to_string());
                    }
                }
            }
        }
        let Some((comm, ppid)) = stat_parent(proc_, pid) else {
            break;
        };
        if ppid <= 1 || comm.starts_with("containerd-shim") {
            break;
        }
        pid = ppid;
        match std::fs::read_to_string(proc_.join(pid.to_string()).join("comm")) {
            Ok(c)
                if c.starts_with("containerd-shim")
                    || c.starts_with("systemd")
                    || c.starts_with("dockerd") =>
            {
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    ino
}

/// Local addresses of a network namespace (excluding loopback), as raw bytes.
fn netns_ips(proc_: &Path, pid: u32) -> BTreeSet<Vec<u8>> {
    let mut ips = BTreeSet::new();
    let base = proc_.join(pid.to_string()).join("net");
    if let Ok(t) = std::fs::read_to_string(base.join("fib_trie")) {
        let mut last: Option<String> = None;
        for line in t.lines() {
            let s = line.trim();
            if let Some(a) = s.strip_prefix("|--") {
                last = Some(a.trim().to_string());
            } else if s.contains("host LOCAL")
                && let Some(a) = &last
                && !a.starts_with("127.")
                && let Ok(ip) = a.parse::<Ipv4Addr>()
            {
                ips.insert(ip.octets().to_vec());
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(base.join("if_inet6")) {
        for line in t.lines() {
            if let Some(hx) = line.split_whitespace().next()
                && hx.len() == 32
                && let Ok(v) = u128::from_str_radix(hx, 16)
                && Ipv6Addr::from(v) != Ipv6Addr::LOCALHOST
            {
                ips.insert(Ipv6Addr::from(v).octets().to_vec());
            }
        }
    }
    ips
}

fn netns(proc_: &Path, pid: &str) -> Option<std::path::PathBuf> {
    std::fs::read_link(proc_.join(pid).join("ns").join("net")).ok()
}

/// Builds the port map from the cards' compute PIDs and the configured extra ports.
pub fn discover(
    proc_: &Path,
    pids: &BTreeMap<String, Vec<u32>>,
    extra: &BTreeMap<String, Vec<u16>>,
) -> PortMap {
    let mut m = PortMap::default();
    for (card, ports) in extra {
        for p in ports {
            m.any.entry(*p).or_default().insert(card.clone());
        }
    }
    let host_ns = netns(proc_, "1");
    for (card, ps) in pids {
        for &pid in ps {
            let Some(ns) = netns(proc_, &pid.to_string()) else {
                continue;
            };
            if host_ns.is_none() || Some(&ns) == host_ns.as_ref() {
                for p in listen_ports(proc_, pid, Some(&sock_inodes(proc_, pid))) {
                    m.any.entry(p).or_default().insert(card.clone());
                }
            } else {
                let ips = netns_ips(proc_, pid);
                for p in listen_ports(proc_, pid, None) {
                    if ips.is_empty() {
                        m.any.entry(p).or_default().insert(card.clone());
                    }
                    for ip in &ips {
                        m.ip.entry((ip.clone(), p))
                            .or_default()
                            .insert(card.clone());
                    }
                }
            }
        }
    }
    m
}

/// `nvidia-smi --query-compute-apps=pid,gpu_uuid` → uuid → pids.
pub fn parse_compute_apps(out: &str) -> BTreeMap<String, Vec<u32>> {
    let mut m: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for line in out.lines() {
        let mut it = line.split(',').map(str::trim);
        if let (Some(pid), Some(uuid)) = (it.next(), it.next())
            && let Ok(pid) = pid.parse()
        {
            m.entry(uuid.to_string()).or_default().push(pid);
        }
    }
    m
}

#[cfg(target_os = "linux")]
pub use linux::spawn;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::clocks::ClockMsg;
    use orca_gpuctl::WakeSource;
    use socket2::{Domain, Protocol, SockFilter, Socket, Type};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn open(ports: &[u16]) -> std::io::Result<Socket> {
        // ETH_P_ALL in network byte order.
        let s = Socket::new(
            Domain::PACKET,
            Type::DGRAM,
            Some(Protocol::from(i32::from(3u16.to_be()))),
        )?;
        attach(&s, ports)?;
        s.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(s)
    }

    fn attach(s: &Socket, ports: &[u16]) -> std::io::Result<()> {
        let prog: Vec<SockFilter> = orca_gpuctl::sniff::bpf_program(ports)
            .into_iter()
            .map(|f| SockFilter::new(f.code, f.jt, f.jf, f.k))
            .collect();
        s.attach_filter(&prog)
    }

    /// Runs the sniffer on its own thread. `map` is updated by discovery; a change re-attaches
    /// the filter. Wakes go to the clocks thread (at most one per card per second).
    pub fn spawn(map: Arc<Mutex<PortMap>>, clocks: std::sync::mpsc::Sender<ClockMsg>) {
        let _ = std::thread::Builder::new().name("hostd-sniff".into()).spawn(move || {
            let mut warned = false;
            loop {
                let mut cur = map.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let s = match open(&cur.ports()) {
                    Ok(s) => s,
                    Err(e) => {
                        if !warned {
                            eprintln!("hostd: sniff wake off: {e} (wakes on utilisation and router calls only)");
                            warned = true;
                        }
                        std::thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                };
                eprintln!("hostd: sniffer up (HTTP POST/PUT to engine ports {:?})", cur.ports());
                let mut last: BTreeMap<String, Instant> = BTreeMap::new();
                let mut buf = [0u8; 200];
                loop {
                    let now_map = map.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if now_map != cur {
                        if now_map.ports() != cur.ports() && attach(&s, &now_map.ports()).is_err() {
                            break;
                        }
                        cur = now_map;
                        eprintln!("hostd: sniff ports {:?}", cur.ports());
                    }
                    let n = match (&s).read(&mut buf) {
                        Ok(n) => n,
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => continue,
                        Err(e) => {
                            eprintln!("hostd: sniff socket: {e}; reopening");
                            break;
                        }
                    };
                    let Some(hit) = orca_gpuctl::sniff::classify(&buf[..n], &["POST", "PUT"]) else { continue };
                    let Some(cards) = cur.targets(&hit.dst, hit.dport) else { continue };
                    for c in cards {
                        if last.get(c).is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                            continue;
                        }
                        last.insert(c.clone(), Instant::now());
                        let _ = clocks.send(ClockMsg::Wake {
                            card: c.clone(),
                            source: WakeSource::Sniff,
                            detail: format!("{} :{}", hit.request, hit.dport),
                        });
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_host_and_container_ports() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let mk = |pid: &str, ns: &str, tcp: &str, stat: &str| {
            let b = p.join(pid);
            std::fs::create_dir_all(b.join("net")).unwrap();
            std::fs::create_dir_all(b.join("ns")).unwrap();
            std::fs::create_dir_all(b.join("fd")).unwrap();
            std::fs::write(b.join("net/tcp"), format!("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n{tcp}")).unwrap();
            std::fs::write(b.join("stat"), stat).unwrap();
            std::fs::write(b.join("comm"), "x\n").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(format!("net:[{ns}]"), b.join("ns/net")).unwrap();
            #[cfg(not(unix))]
            let _ = ns;
        };
        mk("1", "1", "", "1 (init) S 0");
        // A container engine on port 8000 (0x1F40), listening; its netns has 172.17.0.2.
        mk(
            "50",
            "2",
            "   0: 00000000:1F40 00000000:0000 0A 0 0 0 0 0 0 0 0 777 1\n   1: 00000000:0016 00000000:0000 0A 0 0 0 0 0 0 0 0 778 1\n",
            "50 (python) S 1",
        );
        std::fs::write(p.join("50/net/fib_trie"), "Main:\n  +-- 0.0.0.0/0\n     |-- 172.17.0.2\n        /32 host LOCAL\n     |-- 127.0.0.1\n        /32 host LOCAL\n").unwrap();
        let pids = BTreeMap::from([("A".to_string(), vec![50]), ("B".to_string(), vec![99])]);
        let extra = BTreeMap::from([("B".to_string(), vec![8188])]);
        let m = discover(p, &pids, &extra);
        assert_eq!(
            m.any.get(&8188).unwrap().iter().collect::<Vec<_>>(),
            vec!["B"]
        );
        #[cfg(unix)]
        {
            assert!(m.targets(&[172, 17, 0, 2], 8000).unwrap().contains("A"));
            assert_eq!(m.ports(), vec![8000, 8188], "ssh (22) is never watched");
        }
    }

    #[test]
    fn compute_apps() {
        let m = parse_compute_apps("1234, GPU-a\n77, GPU-a\n9, GPU-b\nbad\n");
        assert_eq!(m["GPU-a"], vec![1234, 77]);
        assert_eq!(m["GPU-b"], vec![9]);
    }
}
