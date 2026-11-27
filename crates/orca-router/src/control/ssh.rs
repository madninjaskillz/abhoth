//! The SSH executor (ROUTER-CONFIG.md 6.1) and the per-server executor set.
//!
//! **Deviation from the design** (which names an in-process SSH library): this uses the
//! system OpenSSH client, already in the image and audited, with `BatchMode`, a pinned host
//! key in a router-owned `known_hosts` (`StrictHostKeyChecking=yes`, no global files),
//! `IdentitiesOnly` and a per-server key file. The server side is a forced command
//! (`hostctl serve`), so the session can only carry one framed request ([`super::hostctl`]):
//! commands, builtins and host-daemon (`hostd`) calls alike. Connections are reused through
//! OpenSSH connection sharing on unix.
//!
//! A pinned `SHA256:` fingerprint is checked against `ssh-keyscan` output before the key is
//! written to `known_hosts` (retried once when the scan finds no match; a line already
//! written for the same pin is reused, so new executors do not rescan); a full
//! `ssh-ed25519 AAAA…` line is written as is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::exec::{Cancel, ExecError, ExecOutput, ExecRequest, Executor, LocalExecutor, Scrubber};
use super::hostctl::{Frame, PROTOCOL, parse_reply};
use super::schema::{ControlConfig, ExecKind};

/// One SSH target.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub server: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Private key file (0600).
    pub key_file: PathBuf,
    /// `SHA256:…` or `ssh-ed25519 AAAA…`.
    pub host_key: String,
    pub connect_timeout: u64,
    pub keepalive: u64,
}

pub struct SshExecutor {
    pub target: SshTarget,
    /// Router-owned directory: `known_hosts-<server>`, control sockets.
    pub state_dir: PathBuf,
    pub scrubber: Scrubber,
    /// `ssh` binary (tests point it at a fake).
    pub ssh_bin: String,
    pub keyscan_bin: String,
    known: Mutex<bool>,
}

impl SshExecutor {
    pub fn new(target: SshTarget, state_dir: PathBuf, scrubber: Scrubber) -> Self {
        Self {
            target,
            state_dir,
            scrubber,
            ssh_bin: "ssh".into(),
            keyscan_bin: "ssh-keyscan".into(),
            known: Mutex::new(false),
        }
    }

    fn known_hosts(&self) -> PathBuf {
        self.state_dir
            .join(format!("known_hosts-{}", self.target.server))
    }

    fn host_pattern(&self) -> String {
        if self.target.port == 22 {
            self.target.host.clone()
        } else {
            format!("[{}]:{}", self.target.host, self.target.port)
        }
    }

    /// Write the pinned host key to the router's known_hosts (once per process).
    async fn ensure_known_host(&self) -> Result<(), ExecError> {
        let mut done = self.known.lock().await;
        if *done {
            return Ok(());
        }
        std::fs::create_dir_all(&self.state_dir).map_err(|e| ExecError::Io(e.to_string()))?;
        let hk = self.target.host_key.trim();
        let line = if hk.starts_with("ssh-") || hk.starts_with("ecdsa-") {
            format!("{} {hk}\n", self.host_pattern())
        } else if let Some(want) = hk.strip_prefix("SHA256:") {
            let want = want.trim_end_matches('=');
            let pattern = self.host_pattern();
            // An earlier executor for this server already checked the pin and wrote the line:
            // reuse it rather than scanning again (every new executor would rescan, and one
            // empty answer failed a download).
            if let Ok(t) = std::fs::read_to_string(self.known_hosts())
                && pinned_line(&t, Some(&pattern), want, &pattern).is_some()
            {
                *done = true;
                return Ok(());
            }
            let mut found = None;
            for attempt in 0..2 {
                if attempt > 0 {
                    tokio::time::sleep(KEYSCAN_RETRY).await;
                }
                let out = tokio::process::Command::new(&self.keyscan_bin)
                    .args([
                        "-T",
                        &self.target.connect_timeout.to_string(),
                        "-p",
                        &self.target.port.to_string(),
                        &self.target.host,
                    ])
                    .output()
                    .await
                    .map_err(|e| ExecError::Unreachable(format!("ssh-keyscan: {e}")))?;
                found = pinned_line(&String::from_utf8_lossy(&out.stdout), None, want, &pattern);
                if found.is_some() {
                    break;
                }
            }
            found.ok_or_else(|| {
                ExecError::Unreachable(format!(
                    "{}: no host key matches the pinned fingerprint (refusing to connect)",
                    self.target.server
                ))
            })?
        } else {
            return Err(ExecError::Unreachable(
                "host_key is neither a SHA256 fingerprint nor a key line".into(),
            ));
        };
        super::builtins::write_atomic(&self.known_hosts(), line.as_bytes(), Some(0o600))
            .map_err(ExecError::Io)?;
        *done = true;
        Ok(())
    }

    /// The `ssh` argv (without the program).
    pub fn args(&self) -> Vec<String> {
        let t = &self.target;
        let mut a: Vec<String> = vec![
            "-T".into(),
            "-i".into(),
            t.key_file.to_string_lossy().into_owned(),
            "-p".into(),
            t.port.to_string(),
        ];
        let mut o = |k: &str, v: String| {
            a.push("-o".into());
            a.push(format!("{k}={v}"));
        };
        o("BatchMode", "yes".into());
        o("IdentitiesOnly", "yes".into());
        o("StrictHostKeyChecking", "yes".into());
        o(
            "UserKnownHostsFile",
            self.known_hosts().to_string_lossy().into_owned(),
        );
        o("GlobalKnownHostsFile", "/dev/null".into());
        o("ConnectTimeout", t.connect_timeout.to_string());
        o("ServerAliveInterval", t.keepalive.to_string());
        o("ServerAliveCountMax", "3".into());
        o("ForwardAgent", "no".into());
        o("ClearAllForwardings", "yes".into());
        if cfg!(unix) {
            o("ControlMaster", "auto".into());
            o(
                "ControlPath",
                self.state_dir
                    .join(format!("cm-{}", t.server))
                    .to_string_lossy()
                    .into_owned(),
            );
            o("ControlPersist", "60".into());
        }
        a.push(format!("{}@{}", t.user, t.host));
        // The forced command ignores what is asked; say what it is for the host's logs.
        a.push("hostctl".into());
        a
    }
}

#[async_trait]
impl Executor for SshExecutor {
    async fn run(&self, req: ExecRequest, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
        if cancel.is_cancelled() {
            return Err(ExecError::Cancelled);
        }
        self.ensure_known_host().await?;
        let t0 = Instant::now();
        let mut child = tokio::process::Command::new(&self.ssh_bin)
            .args(self.args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ExecError::Io(format!("ssh: {e}")))?;
        let frame = serde_json::to_string(&Frame {
            v: PROTOCOL,
            req: req.clone(),
        })
        .map_err(|e| ExecError::Io(e.to_string()))?;
        if let Some(mut si) = child.stdin.take() {
            // ssh that fails early (auth, host key) can exit without reading its stdin: a broken
            // pipe here is not the error; its exit status and stderr below are.
            let mut wrote = si.write_all(frame.as_bytes()).await;
            if wrote.is_ok() {
                wrote = si.write_all(b"\n").await;
            }
            if let Err(e) = wrote
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(ExecError::Io(e.to_string()));
            }
        }
        // The host enforces the request's own timeout; this one also covers connect and reply.
        let limit = req.timeout + Duration::from_secs(self.target.connect_timeout + 15);
        let out = tokio::select! {
            r = tokio::time::timeout(limit, child.wait_with_output()) => match r {
                Ok(r) => r.map_err(|e| ExecError::Io(e.to_string()))?,
                Err(_) => {
                    return Ok(ExecOutput { exit_code: None, timed_out: true, duration_ms: t0.elapsed().as_millis() as u64,
                                           stderr: "no reply from the host in time".into(), ..Default::default() });
                }
            },
            _ = cancel.cancelled() => return Err(ExecError::Cancelled),
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        match parse_reply(&stdout) {
            Some(Ok(o)) => Ok(self.scrubber.scrub_output(o)),
            Some(Err(e)) => Err(e),
            None => {
                let err = self
                    .scrubber
                    .scrub(String::from_utf8_lossy(&out.stderr).trim());
                let tail: String = err
                    .lines()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ");
                Err(ExecError::Unreachable(format!(
                    "{} (ssh exit {}): {tail}",
                    self.target.server,
                    out.status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                )))
            }
        }
    }
}

/// `SHA256:` fingerprint (unpadded base64 of the SHA-256 of the key blob) of a base64 key.
/// How long to wait before the one retry of an `ssh-keyscan` that found no matching key.
const KEYSCAN_RETRY: Duration = Duration::from_millis(500);

/// The `known_hosts` line (`<pattern> <kind> <blob>`) for the first key in `text` (keyscan
/// output or a `known_hosts` file) whose fingerprint is `want` (`SHA256:` stripped, no
/// padding). `host`: only lines for this host pattern count (a `known_hosts` file).
fn pinned_line(text: &str, host: Option<&str>, want: &str, pattern: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (h, kind, blob) = (f.next()?, f.next()?, f.next()?);
        if host.is_some_and(|x| x != h) {
            return None;
        }
        (fingerprint(blob)? == want).then(|| {
            format!(
                "{pattern} {kind} {blob}
"
            )
        })
    })
}

pub fn fingerprint(blob_b64: &str) -> Option<String> {
    let blob = b64_decode(blob_b64)?;
    Some(
        b64_encode(&Sha256::digest(&blob))
            .trim_end_matches('=')
            .to_string(),
    )
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(b: &[u8]) -> String {
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for ch in s.trim().bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|c| *c == ch)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits & 0xff) as u8);
        }
    }
    Some(out)
}

// ── per-server routing ───────────────────────────────────────────────────────────────────────

/// Routes each request to its server's executor.
#[derive(Clone, Default)]
pub struct Executors {
    pub by_server: BTreeMap<String, Arc<dyn Executor>>,
}

impl Executors {
    /// From the config: `exec = "local"` -> in-process, `exec = "ssh"` -> [`SshExecutor`].
    /// `secret_file` maps a secret id to its file (the SSH key must be a file secret).
    pub fn from_config(
        c: &ControlConfig,
        state_dir: &Path,
        scrubber: Scrubber,
        secret_file: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<Self, String> {
        let mut by_server: BTreeMap<String, Arc<dyn Executor>> = BTreeMap::new();
        for (id, s) in &c.servers {
            let ex: Arc<dyn Executor> = match s.exec {
                ExecKind::Local => Arc::new(LocalExecutor {
                    scrubber: scrubber.clone(),
                    ..Default::default()
                }),
                ExecKind::Ssh => {
                    let ssh = s
                        .ssh
                        .as_ref()
                        .ok_or_else(|| format!("servers.{id}: exec = \"ssh\" without [ssh]"))?;
                    let key_id = ssh.key.strip_prefix("secret:").unwrap_or(&ssh.key);
                    let key_file = secret_file(key_id).ok_or_else(|| {
                        format!("servers.{id}.ssh.key: secret {key_id} must be a file")
                    })?;
                    Arc::new(SshExecutor::new(
                        SshTarget {
                            server: id.clone(),
                            host: s.host.clone(),
                            port: ssh.port,
                            user: ssh.user.clone(),
                            key_file,
                            host_key: ssh.host_key.clone(),
                            connect_timeout: ssh.connect_timeout,
                            keepalive: ssh.keepalive_seconds,
                        },
                        state_dir.join("ssh"),
                        scrubber.clone(),
                    ))
                }
            };
            by_server.insert(id.clone(), ex);
        }
        Ok(Self { by_server })
    }
}

#[async_trait]
impl Executor for Executors {
    async fn run(&self, req: ExecRequest, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
        let ex = self
            .by_server
            .get(&req.server)
            .ok_or_else(|| ExecError::Refused(format!("unknown server {}", req.server)))?;
        ex.run(req, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_fingerprints() {
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode("aGVsbG8").unwrap(), b"hello");
        assert!(b64_decode("a*b").is_none());
        // sha256("hello") = 2cf24dba…; its base64 without padding:
        let fp = b64_encode(&Sha256::digest(b"hello"));
        assert_eq!(fingerprint("aGVsbG8=").unwrap(), fp.trim_end_matches('='));
    }

    #[test]
    fn pinned_lines_from_keyscan_and_known_hosts() {
        let fp = fingerprint("aGVsbG8=").unwrap();
        let scan = "# h:22 SSH-2.0
h ssh-rsa d29ybGQ=
h ssh-ed25519 aGVsbG8=
";
        assert_eq!(
            pinned_line(scan, None, &fp, "[h]:2222").as_deref(),
            Some(
                "[h]:2222 ssh-ed25519 aGVsbG8=
"
            )
        );
        assert!(
            pinned_line("", None, &fp, "h").is_none(),
            "an empty scan finds nothing"
        );
        assert!(pinned_line(scan, None, "other", "h").is_none());
        // a known_hosts file counts only for its own host pattern
        let known = "h ssh-ed25519 aGVsbG8=
";
        assert!(pinned_line(known, Some("h"), &fp, "h").is_some());
        assert!(pinned_line(known, Some("[h]:2222"), &fp, "[h]:2222").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keyscan_is_retried_once_and_its_answer_reused() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("abhoth-keyscan-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // a keyscan that answers nothing the first time, then the key
        let scan = d.join("fake-keyscan");
        let flag = d.join("scanned-once");
        std::fs::write(
            &scan,
            format!(
                "#!/bin/sh
if [ -f {f} ]; then echo 'h ssh-ed25519 aGVsbG8='; else touch {f}; fi
",
                f = flag.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&scan, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = SshTarget {
            server: "gpu".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            key_file: "/k".into(),
            host_key: format!("SHA256:{}", fingerprint("aGVsbG8=").unwrap()),
            connect_timeout: 5,
            keepalive: 15,
        };
        let mut ex = SshExecutor::new(target.clone(), d.clone(), Scrubber::default());
        ex.keyscan_bin = scan.to_string_lossy().into_owned();
        ex.ensure_known_host().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("known_hosts-gpu")).unwrap(),
            "h ssh-ed25519 aGVsbG8=
"
        );
        // a new executor reuses the checked line: its keyscan never runs
        let mut ex2 = SshExecutor::new(target, d.clone(), Scrubber::default());
        ex2.keyscan_bin = "/nonexistent/ssh-keyscan".into();
        ex2.ensure_known_host().await.unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ssh_argv_is_locked_down() {
        let ex = SshExecutor::new(
            SshTarget {
                server: "gpu".into(),
                host: "gpu-host".into(),
                port: 2222,
                user: "abhoth-exec".into(),
                key_file: "/run/secrets/ssh-gpu".into(),
                host_key: "ssh-ed25519 AAAA".into(),
                connect_timeout: 5,
                keepalive: 15,
            },
            "/state/ssh".into(),
            Scrubber::default(),
        );
        let a = ex.args().join(" ");
        for want in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "IdentitiesOnly=yes",
            "GlobalKnownHostsFile=/dev/null",
            "-p 2222",
            "abhoth-exec@gpu-host",
        ] {
            assert!(a.contains(want), "{want} in {a}");
        }
        assert_eq!(ex.host_pattern(), "[gpu-host]:2222");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn talks_to_a_fake_host() {
        // A fake `ssh` that answers like `hostctl serve` would.
        let d = std::env::temp_dir().join(format!("abhoth-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let fake = d.join("fake-ssh");
        let reply = serde_json::to_string(&super::super::hostctl::Reply::Ok(ExecOutput::ok_with(
            "pong secret-value-123",
        )))
        .unwrap();
        std::fs::write(
            &fake,
            format!("#!/bin/sh\ncat >/dev/null\necho '{reply}'\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut ex = SshExecutor::new(
            SshTarget {
                server: "gpu".into(),
                host: "h".into(),
                port: 22,
                user: "u".into(),
                key_file: "/k".into(),
                host_key: "ssh-ed25519 AAAA".into(),
                connect_timeout: 5,
                keepalive: 15,
            },
            d.clone(),
            Scrubber::new(["secret-value-123".to_string()]),
        );
        ex.ssh_bin = fake.to_string_lossy().into_owned();
        let o = ex
            .run(
                ExecRequest::command("gpu", "x", "echo", Duration::from_secs(5)),
                &Cancel::new(),
            )
            .await
            .unwrap();
        assert_eq!(o.stdout, "pong ***");
        assert!(
            std::fs::read_to_string(d.join("known_hosts-gpu"))
                .unwrap()
                .starts_with("h ssh-ed25519")
        );
        // A host that answers nothing is unreachable.
        std::fs::write(&fake, "#!/bin/sh\necho 'Permission denied' >&2\nexit 255\n").unwrap();
        let e = ex
            .run(
                ExecRequest::command("gpu", "x", "echo", Duration::from_secs(5)),
                &Cancel::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(e, ExecError::Unreachable(m) if m.contains("Permission denied")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
