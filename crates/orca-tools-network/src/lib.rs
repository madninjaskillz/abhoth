//! The `network_diagnostics` tool (port of `OpenOrca.Tools.Network`): ping, DNS lookup and an
//! HTTP reachability check.
//!
//! .NET's `Ping` class sends ICMP itself; the port runs the system `ping` once per attempt
//! (raw ICMP sockets need privileges, and the workspace forbids `unsafe` FFI) and reads the
//! round-trip time and TTL from its output. DNS uses the system resolver (`getaddrinfo`),
//! like `Dns.GetHostEntryAsync`, but reports the name as given (no canonical name).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::{append_line, get_i32_lenient, io_message, required_str};
use serde_json::Value;

/// Every tool in this crate.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(NetworkDiagnosticsTool)]
}

pub struct NetworkDiagnosticsTool;

#[async_trait::async_trait]
impl Tool for NetworkDiagnosticsTool {
    fn name(&self) -> &str {
        "network_diagnostics"
    }

    fn description(&self) -> &str {
        "Network diagnostics: ping a host, perform DNS lookup, or check HTTP connectivity. Useful for debugging connection issues."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f310}",
            short_description: "Network diagnostics",
            summary_arg_keys: &["host"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        serde_json::from_str(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["ping", "dns_lookup", "check_connection"],
                "description": "Action: 'ping' a host, 'dns_lookup' to resolve a hostname, 'check_connection' to verify HTTP reachability"
            },
            "target": {
                "type": "string",
                "description": "Hostname, domain, or URL to test"
            },
            "timeout_seconds": {
                "type": "integer",
                "description": "Timeout in seconds. Defaults to 5."
            }
        },
        "required": ["action", "target"]
    }"#,
        )
        .expect("tool schema is valid JSON")
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let (action, target) = match (required_str(args, "action"), required_str(args, "target")) {
            (Ok(a), Ok(t)) => (a, t),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(e),
        };
        let mut timeout = get_i32_lenient(args.get("timeout_seconds"), 5);
        if timeout <= 0 || timeout > 30 {
            timeout = 5;
        }
        let timeout = timeout as u64;
        match action {
            "ping" => ping(target, timeout).await,
            "dns_lookup" => dns_lookup(target).await,
            "check_connection" => check_connection(target, timeout).await,
            other => ToolResult::error(format!(
                "Unknown action: {other}. Use 'ping', 'dns_lookup', or 'check_connection'."
            )),
        }
    }
}

/// `Uri.TryCreate(host, UriKind.Absolute)`: an absolute URL's host (possibly empty), else the
/// text unchanged.
fn host_of(target: &str) -> String {
    match reqwest::Url::parse(target) {
        Ok(url) => url
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        Err(_) => target.to_string(),
    }
}

async fn resolve(host: &str) -> std::io::Result<Vec<IpAddr>> {
    let mut out: Vec<IpAddr> = Vec::new();
    for addr in tokio::net::lookup_host((host, 0)).await? {
        if !out.contains(&addr.ip()) {
            out.push(addr.ip());
        }
    }
    Ok(out)
}

/// One echo request through the system `ping`: Ok(Some((time ms, ttl))) for a reply,
/// Ok(None) with a status name otherwise.
async fn ping_once(
    addr: IpAddr,
    timeout_seconds: u64,
) -> std::io::Result<Result<(u64, String), String>> {
    let mut cmd = tokio::process::Command::new("ping");
    if cfg!(windows) {
        cmd.args(["-n", "1", "-w", &(timeout_seconds * 1000).to_string()]);
    } else {
        cmd.args(["-c", "1", "-W", &timeout_seconds.to_string()]);
    }
    cmd.arg(addr.to_string());
    let out = tokio::time::timeout(Duration::from_secs(timeout_seconds + 5), cmd.output())
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    Ok(parse_ping_output(&String::from_utf8_lossy(&out.stdout)))
}

/// Reads `time=12ms` / `time<1ms` / `time=0.045 ms` and `TTL=117` / `ttl=64` from a reply line.
fn parse_ping_output(text: &str) -> Result<(u64, String), String> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let (Some(t), Some(ttl)) = (lower.find("time"), lower.find("ttl=")) else {
            continue;
        };
        let after = &lower[t + 4..];
        let after = after.trim_start_matches(['=', '<']);
        let num: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let ms = num.parse::<f64>().map(|v| v as u64).unwrap_or(0);
        let ttl: String = lower[ttl + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        return Ok((ms, ttl));
    }
    if text.to_ascii_lowercase().contains("unreachable") {
        Err("DestinationHostUnreachable".into())
    } else {
        Err("TimedOut".into())
    }
}

async fn ping(target: &str, timeout_seconds: u64) -> ToolResult {
    let host = host_of(target);
    let mut sb = String::new();
    append_line(&mut sb, &format!("Pinging {host}..."));
    let (mut successes, mut total_ms) = (0u64, 0u64);
    let resolved = resolve(&host).await;
    for _ in 0..4 {
        let addr = match &resolved {
            Ok(a) if !a.is_empty() => a[0],
            Ok(_) => {
                append_line(&mut sb, "  Error: No such host is known.");
                continue;
            }
            Err(e) => {
                append_line(&mut sb, &format!("  Error: {}", io_message(e)));
                continue;
            }
        };
        match ping_once(addr, timeout_seconds).await {
            Ok(Ok((ms, ttl))) => {
                append_line(
                    &mut sb,
                    &format!("  Reply from {addr}: time={ms}ms TTL={ttl}"),
                );
                successes += 1;
                total_ms += ms;
            }
            Ok(Err(status)) => append_line(&mut sb, &format!("  {status}")),
            Err(e) => append_line(&mut sb, &format!("  Error: {}", io_message(&e))),
        }
    }
    append_line(&mut sb, "");
    append_line(&mut sb, &format!("Result: {successes}/4 packets received"));
    if successes > 0 {
        append_line(&mut sb, &format!("Average: {}ms", total_ms / successes));
    }
    ToolResult::success(sb)
}

async fn dns_lookup(target: &str) -> ToolResult {
    let host = host_of(target);
    let addrs = match resolve(&host).await {
        Ok(a) => a,
        Err(e) => {
            return ToolResult::error(format!("DNS lookup failed for {host}: {}", io_message(&e)));
        }
    };
    let mut sb = String::new();
    append_line(&mut sb, &format!("DNS lookup for {host}:"));
    append_line(&mut sb, &format!("  Hostname: {host}"));
    if !addrs.is_empty() {
        append_line(&mut sb, "  Addresses:");
        for a in &addrs {
            let family = if a.is_ipv4() {
                "InterNetwork"
            } else {
                "InterNetworkV6"
            };
            append_line(&mut sb, &format!("    {a} ({family})"));
        }
    }
    ToolResult::success(sb)
}

async fn check_connection(target: &str, timeout_seconds: u64) -> ToolResult {
    let lower = target.to_ascii_lowercase();
    let target = if lower.starts_with("http://") || lower.starts_with("https://") {
        target.to_string()
    } else {
        format!("https://{target}")
    };
    let Ok(url) = reqwest::Url::parse(&target) else {
        return ToolResult::error(format!("Invalid URL: {target}"));
    };
    let work = async {
        orca_tools_common::http::throttle(&target).await;
        orca_tools_common::http::client().get(url).send().await
    };
    match tokio::time::timeout(Duration::from_secs(timeout_seconds), work).await {
        Err(_) => ToolResult::error(format!(
            "Connection check timed out for {target} (after {timeout_seconds}s)"
        )),
        Ok(Err(e)) if e.is_timeout() => ToolResult::error(format!(
            "Connection check timed out for {target} (after {timeout_seconds}s)"
        )),
        Ok(Err(e)) => ToolResult::error(format!("Connection check failed for {target}: {e}")),
        Ok(Ok(resp)) => {
            let header = |name: reqwest::header::HeaderName| {
                resp.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            };
            let status = resp.status();
            ToolResult::success(format!(
                "Connection check for {target}:\n  Status: {} {}\n  Server: {}\n  Content-Type: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                header(reqwest::header::SERVER),
                header(reqwest::header::CONTENT_TYPE)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_output_parsing() {
        let win = "Pinging 127.0.0.1 with 32 bytes of data:\r\nReply from 127.0.0.1: bytes=32 time<1ms TTL=128\r\n";
        assert_eq!(parse_ping_output(win), Ok((0, "128".into())));
        let win2 = "Reply from 8.8.8.8: bytes=32 time=14ms TTL=117";
        assert_eq!(parse_ping_output(win2), Ok((14, "117".into())));
        let unix = "64 bytes from 127.0.0.1: icmp_seq=1 ttl=64 time=0.045 ms";
        assert_eq!(parse_ping_output(unix), Ok((0, "64".into())));
        assert_eq!(
            parse_ping_output("Request timed out."),
            Err("TimedOut".into())
        );
        assert_eq!(
            parse_ping_output("Reply from 10.0.0.1: Destination host unreachable."),
            Err("DestinationHostUnreachable".into())
        );
    }

    #[test]
    fn hosts_from_urls() {
        assert_eq!(host_of("example.com"), "example.com");
        assert_eq!(host_of("https://example.com/x"), "example.com");
        assert_eq!(host_of("http://[::1]:8080/"), "::1");
    }
}
