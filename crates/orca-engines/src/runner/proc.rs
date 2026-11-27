//! Host processes by exact PID: the OS start time guards against PID reuse, and every kill
//! checks it first. No unsafe code: `/proc` on Linux, `ps` on other Unixes, PowerShell on
//! Windows.

use std::time::Duration;

/// The process's start time as the OS reports it (an opaque, stable number), or None when no
/// such process runs.
pub async fn start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // field 22 (starttime), counted after the ")" that ends the command name
        let after = &s[s.rfind(')')? + 1..];
        let f: Vec<&str> = after.split_whitespace().collect();
        // after ")" the fields start at 3 (state), so starttime (22) is index 19
        return f.get(19)?.parse().ok();
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = tokio::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if t.is_empty() {
            return None;
        }
        return Some(fnv(&t));
    }
    #[cfg(windows)]
    {
        let out = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!(
                        "$p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($p) {{ $p.StartTime.ToFileTimeUtc() }}"
                    ),
                ])
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Is this exact process (PID and start time) still running?
pub async fn alive(pid: u32, started: u64) -> bool {
    start_time(pid).await == Some(started)
}

/// Ask the process to stop (SIGTERM; on Windows a polite taskkill of its tree).
pub async fn terminate(pid: u32) {
    #[cfg(unix)]
    let _ = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await;
    #[cfg(windows)]
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .output()
        .await;
}

/// Kill it (SIGKILL; taskkill /F of its tree).
pub async fn kill(pid: u32) {
    #[cfg(unix)]
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output()
        .await;
    #[cfg(windows)]
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .await;
}

/// Stop an adopted process (one this runner did not spawn): checks the start time before
/// each signal so a reused PID is never touched.
pub async fn stop_exact(pid: u32, started: u64, grace: Duration) -> Result<(), String> {
    if !alive(pid, started).await {
        return Ok(());
    }
    terminate(pid).await;
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !alive(pid, started).await {
            return Ok(());
        }
    }
    if alive(pid, started).await {
        kill(pid).await;
    }
    Ok(())
}
