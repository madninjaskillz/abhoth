//! The `sniff` wake source: a classic BPF filter for a cooked AF_PACKET socket (`SOCK_DGRAM`,
//! so packets start at the IP header) that passes TCP segments with PSH set to the watched
//! ports, and a classifier that wakes on HTTP `POST`/`PUT` (gpu-power parity, ~7 ms).
//!
//! The socket itself lives in hostd (it needs `CAP_NET_RAW`); this module is pure.

/// One `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// At most this many ports go into one filter (jump offsets are 8 bits).
pub const MAX_PORTS: usize = 100;

/// (code, jt, jf, k, label k jumps to (ja), own label)
type Ins = (u16, J, J, u32, Option<&'static str>, Option<&'static str>);

#[derive(Clone, Copy)]
enum J {
    N(u8),
    L(&'static str),
}

/// Builds the filter. An empty port list drops everything.
pub fn bpf_program(ports: &[u16]) -> Vec<SockFilter> {
    let mut ports: Vec<u16> = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    ports.truncate(MAX_PORTS);
    if ports.is_empty() {
        return vec![SockFilter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0,
        }];
    }
    // (code, jt, jf, k, k-label for ja, own label)
    let mut ins: Vec<Ins> = Vec::new();
    let mut add = |code, jt, jf, k, kl, label| ins.push((code, jt, jf, k, kl, label));
    use J::{L, N};
    add(0x30, N(0), N(0), 0, None, None); //          ldb [0]
    add(0x54, N(0), N(0), 0xf0, None, None); //       and #0xf0
    add(0x15, L("v4"), N(0), 0x40, None, None); //    jeq #0x40
    add(0x15, L("v6"), L("drop"), 0x60, None, None);
    add(0x30, N(0), N(0), 9, None, Some("v4")); //    ldb [9]  protocol
    add(0x15, N(0), L("drop"), 6, None, None); //     tcp?
    add(0x28, N(0), N(0), 6, None, None); //          ldh [6]  fragment
    add(0x45, L("drop"), N(0), 0x1fff, None, None);
    add(0xb1, N(0), N(0), 0, None, None); //          ldxb 4*([0]&0xf)
    add(0x50, N(0), N(0), 13, None, None); //         ldb [x+13] tcp flags
    add(0x45, N(0), L("drop"), 0x08, None, None); //  PSH
    add(0x48, N(0), N(0), 2, None, None); //          ldh [x+2] dst port
    add(0x05, N(0), N(0), 0, Some("ports"), None); // ja ports
    add(0x30, N(0), N(0), 6, None, Some("v6")); //    ldb [6] next header
    add(0x15, N(0), L("drop"), 6, None, None);
    add(0x30, N(0), N(0), 53, None, None); //         ldb [40+13]
    add(0x45, N(0), L("drop"), 0x08, None, None);
    add(0x28, N(0), N(0), 42, None, None); //         ldh [40+2]
    for (i, p) in ports.iter().enumerate() {
        add(
            0x15,
            L("accept"),
            N(0),
            u32::from(*p),
            None,
            (i == 0).then_some("ports"),
        );
    }
    add(0x06, N(0), N(0), 0, None, Some("drop"));
    add(0x06, N(0), N(0), 160, None, Some("accept"));
    let pos = |l: &str| ins.iter().position(|i| i.5 == Some(l)).expect("label");
    let rel = |n: usize, j: J| -> u8 {
        match j {
            N(x) => x,
            L(l) => u8::try_from(pos(l) - n - 1).expect("jump fits"),
        }
    };
    ins.iter()
        .enumerate()
        .map(|(n, &(code, jt, jf, k, kl, _))| SockFilter {
            code,
            jt: rel(n, jt),
            jf: rel(n, jf),
            k: kl.map_or(k, |l| (pos(l) - n - 1) as u32),
        })
        .collect()
}

/// A packet the sniffer accepted: destination address bytes (4 or 16), port, and the request
/// line start for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub dst: Vec<u8>,
    pub dport: u16,
    pub request: String,
}

/// Classifies a cooked IP packet: `Some` when it carries an HTTP request starting with one of
/// `methods` (e.g. `["POST", "PUT"]`).
pub fn classify(pkt: &[u8], methods: &[&str]) -> Option<Hit> {
    if pkt.len() < 40 {
        return None;
    }
    let (ihl, dst) = match pkt[0] >> 4 {
        4 => (usize::from(pkt[0] & 15) * 4, pkt[16..20].to_vec()),
        6 => (40, pkt[24..40].to_vec()),
        _ => return None,
    };
    if pkt.len() < ihl + 20 {
        return None;
    }
    let dport = u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]);
    let pay = pkt.get(ihl + usize::from(pkt[ihl + 12] >> 4) * 4..)?;
    let m = methods
        .iter()
        .find(|m| pay.starts_with(m.as_bytes()) && pay.get(m.len()) == Some(&b' '))?;
    let line = pay.split(|b| *b == b'\r').next().unwrap_or_default();
    let request = String::from_utf8_lossy(&line[..line.len().min(60)])
        .trim()
        .to_string();
    debug_assert!(request.starts_with(m));
    Some(Hit {
        dst,
        dport,
        request,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny interpreter for the opcodes the filter uses.
    fn run(prog: &[SockFilter], pkt: &[u8]) -> u32 {
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        let b = |i: usize| u32::from(*pkt.get(i).unwrap_or(&0));
        let h = |i: usize| (b(i) << 8) | b(i + 1);
        loop {
            let f = prog[pc];
            pc += 1;
            match f.code {
                0x30 => a = b(f.k as usize),
                0x28 => a = h(f.k as usize),
                0x50 => a = b((x + f.k) as usize),
                0x48 => a = h((x + f.k) as usize),
                0xb1 => x = 4 * (b(f.k as usize) & 0xf),
                0x54 => a &= f.k,
                0x05 => pc += f.k as usize,
                0x15 => pc += usize::from(if a == f.k { f.jt } else { f.jf }),
                0x45 => pc += usize::from(if a & f.k != 0 { f.jt } else { f.jf }),
                0x06 => return f.k,
                c => panic!("opcode {c:#x}"),
            }
        }
    }

    fn v4(dport: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![
            0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 6, 0, 0, 10, 0, 0, 1, 172, 16, 0, 2,
        ];
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&40000u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&dport.to_be_bytes());
        tcp[12] = 5 << 4;
        tcp[13] = flags;
        p.extend(tcp);
        p.extend_from_slice(payload);
        p
    }

    fn v6(dport: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x60, 0, 0, 0, 0, 0, 6, 64];
        p.extend([0u8; 16]);
        p.extend([0xfd; 16]);
        let mut tcp = vec![0u8; 20];
        tcp[2..4].copy_from_slice(&dport.to_be_bytes());
        tcp[12] = 5 << 4;
        tcp[13] = 0x18;
        p.extend(tcp);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn filter_accepts_push_to_watched_ports_only() {
        let prog = bpf_program(&[8188, 8096]);
        let post = b"POST /v1/chat/completions HTTP/1.1\r\n";
        assert_eq!(run(&prog, &v4(8096, 0x18, post)), 160);
        assert_eq!(run(&prog, &v4(8188, 0x18, post)), 160);
        assert_eq!(run(&prog, &v4(9999, 0x18, post)), 0, "unwatched port");
        assert_eq!(run(&prog, &v4(8096, 0x10, b"")), 0, "no PSH");
        assert_eq!(run(&prog, &v6(8188, post)), 160);
        assert_eq!(run(&prog, &v6(22, post)), 0);
        assert_eq!(run(&bpf_program(&[]), &v4(8096, 0x18, post)), 0);
    }

    #[test]
    fn classify_http_methods() {
        let m = ["POST", "PUT"];
        let h = classify(
            &v4(
                8096,
                0x18,
                b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n",
            ),
            &m,
        )
        .unwrap();
        assert_eq!(h.dport, 8096);
        assert_eq!(h.dst, vec![172, 16, 0, 2]);
        assert_eq!(h.request, "POST /v1/chat/completions HTTP/1.1");
        assert!(classify(&v4(8096, 0x18, b"GET /health HTTP/1.1\r\n"), &m).is_none());
        assert!(classify(&v4(8096, 0x18, b"POSTMAN"), &m).is_none());
        assert!(classify(&v6(8188, b"PUT /x HTTP/1.1\r\n"), &m).is_some());
        assert!(classify(&[0u8; 10], &m).is_none());
    }

    #[test]
    fn many_ports_fit() {
        let ports: Vec<u16> = (8000..8200).collect();
        let prog = bpf_program(&ports);
        assert_eq!(
            prog.iter()
                .filter(|f| f.code == 0x15 && f.k >= 8000)
                .count(),
            MAX_PORTS
        );
        assert_eq!(run(&prog, &v4(8099, 0x18, b"POST / HTTP/1.1\r\n")), 160);
    }
}
