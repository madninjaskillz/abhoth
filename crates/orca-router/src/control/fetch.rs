//! The `download` and `download-status` builtins (D152): a Hugging Face file fetched on the
//! target server into its model folder, resuming a `.part` file with HTTP Range. The bearer token
//! comes on stdin (never argv, never logged); hostctl allows only `https://huggingface.co/` URLs.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::exec::{Cancel, ExecOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadArgs {
    pub url: String,
    pub dest: String,
    /// Expected size in bytes (skips a finished file, checks the end).
    #[serde(default)]
    pub size: Option<u64>,
    /// Expected SHA-256 (hex); checked before the file is put in place.
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStatusArgs {
    pub dest: String,
}

/// The `.part` file a download writes into before it is renamed into place.
pub fn part_path(dest: &str) -> String {
    format!("{dest}.part")
}

/// `{"bytes": n, "done": bool}`: how far a download has got.
pub fn status(dest: &str) -> Value {
    if let Ok(m) = std::fs::metadata(dest) {
        return json!({"bytes": m.len(), "done": true});
    }
    let n = std::fs::metadata(part_path(dest))
        .map(|m| m.len())
        .unwrap_or(0);
    json!({"bytes": n, "done": false})
}

fn line(bytes: u64, done: bool) -> String {
    json!({"bytes": bytes, "done": done}).to_string()
}

fn stopped(bytes: u64, timed_out: bool) -> ExecOutput {
    ExecOutput {
        stdout: line(bytes, false),
        timed_out,
        cancelled: !timed_out,
        ..Default::default()
    }
}

fn partial(bytes: u64, why: String) -> ExecOutput {
    ExecOutput {
        exit_code: Some(5),
        stdout: line(bytes, false),
        stderr: why,
        ..Default::default()
    }
}

/// Fetch `url` to `dest`. Stops at `timeout` or a cancel with the part kept, so the next call
/// carries on. stdout: `{"bytes", "done"}`.
pub async fn download(
    a: &DownloadArgs,
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> ExecOutput {
    if let Ok(m) = std::fs::metadata(&a.dest)
        && a.size.is_none_or(|s| s == m.len())
    {
        return ExecOutput::ok_with(&line(m.len(), true));
    }
    let dest = Path::new(&a.dest);
    if let Some(d) = dest.parent()
        && let Err(e) = std::fs::create_dir_all(d)
    {
        return ExecOutput::fail(1, &format!("{}: {e}", d.display()));
    }
    let part = part_path(&a.dest);
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let token = stdin
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .filter(|t| !t.is_empty());
    let deadline = Instant::now() + timeout;
    if a.size.is_none_or(|s| have < s) {
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .user_agent("abhoth-router")
            .build()
        {
            Ok(c) => c,
            Err(e) => return ExecOutput::fail(1, &format!("http client: {e}")),
        };
        let mut req = client.get(&a.url);
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let resp = tokio::select! {
            r = req.send() => r,
            _ = cancel.cancelled() => return stopped(have, false),
        };
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => return partial(have, format!("download: {e} (resume carries on)")),
        };
        let code = resp.status().as_u16();
        match code {
            206 => {}
            200 => have = 0,
            401 | 403 => {
                return ExecOutput::fail(
                    3,
                    "Hugging Face refused the download (a gated model: accept its licence on huggingface.co and set a token)",
                );
            }
            404 => return ExecOutput::fail(4, "not found on Hugging Face"),
            _ => return partial(have, format!("download: HTTP {code}")),
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(have > 0)
            .truncate(have == 0)
            .open(&part);
        let mut f = match f {
            Ok(f) => f,
            Err(e) => return ExecOutput::fail(1, &format!("{part}: {e}")),
        };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = tokio::select! {
                c = resp.chunk() => c,
                _ = cancel.cancelled() => {
                    let _ = f.flush();
                    return stopped(have, false);
                }
                _ = tokio::time::sleep(left) => {
                    let _ = f.flush();
                    return stopped(have, true);
                }
            };
            match chunk {
                Ok(Some(b)) => {
                    if let Err(e) = f.write_all(&b) {
                        return ExecOutput::fail(1, &format!("{part}: {e}"));
                    }
                    have += b.len() as u64;
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = f.flush();
                    return partial(
                        have,
                        format!("download interrupted: {e} (resume carries on)"),
                    );
                }
            }
        }
        if let Err(e) = f.flush() {
            return ExecOutput::fail(1, &format!("{part}: {e}"));
        }
    }
    if let Some(s) = a.size
        && s != have
    {
        if have > s {
            let _ = std::fs::remove_file(&part);
        }
        return partial(have, format!("got {have} of {s} bytes (resume carries on)"));
    }
    if let Some(want) = &a.sha256 {
        match sha256_file(Path::new(&part)) {
            Ok(got) if got.eq_ignore_ascii_case(want) => {}
            Ok(_) => {
                let _ = std::fs::remove_file(&part);
                return ExecOutput::fail(
                    6,
                    "the file's SHA-256 does not match Hugging Face's: removed, download it again",
                );
            }
            Err(e) => return ExecOutput::fail(1, &e),
        }
    }
    if let Err(e) = std::fs::rename(&part, dest) {
        return ExecOutput::fail(1, &format!("{}: {e}", dest.display()));
    }
    ExecOutput::ok_with(&line(have, true))
}

fn sha256_file(p: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let mut f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
