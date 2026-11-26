//! `--hostui` on the command line (`HostUiEndpoint`, `HostUiArgs`): where the web UI listens
//! and its companion flags, parsed as the C# CLI does.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUiEndpoint {
    pub host: String,
    pub port: u16,
}

impl Default for HostUiEndpoint {
    fn default() -> Self {
        Self::new(DEFAULT_HOST, DEFAULT_PORT)
    }
}

impl fmt::Display for HostUiEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

fn try_port(text: &str) -> Result<u16, String> {
    if !text.is_empty()
        && text.chars().all(|c| c.is_ascii_digit())
        && let Ok(p) = text.parse::<u32>()
        && p <= 65535
    {
        return Ok(p as u16);
    }
    Err(format!("Not a port number: {text} (1-65535)."))
}

impl HostUiEndpoint {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    pub fn is_lan(&self) -> bool {
        matches!(self.host.as_str(), "0.0.0.0" | "::" | "*")
    }

    /// "8192", "127.0.0.1:8192", "0.0.0.0", "[::1]:8200", "localhost:7001"; empty gives
    /// `fallback` (default localhost:8192). A port alone keeps the fallback's host.
    pub fn parse(text: Option<&str>, fallback: Option<&HostUiEndpoint>) -> Result<Self, String> {
        let fallback = fallback.cloned().unwrap_or_default();
        let Some(s) = text.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(fallback);
        };
        if s.chars().all(|c| c.is_ascii_digit()) {
            return Ok(Self::new(&fallback.host, try_port(s)?));
        }
        let (mut host, port_text): (String, Option<&str>) = if let Some(rest) = s.strip_prefix('[')
        {
            let Some(close) = rest.find(']') else {
                return Err("Missing ] in an IPv6 address.".into());
            };
            let host = &rest[..close];
            let after = &rest[close + 1..];
            if after.is_empty() {
                (host.into(), None)
            } else if let Some(p) = after.strip_prefix(':') {
                (host.into(), Some(p))
            } else {
                return Err(format!("Expected :port after [{host}]."));
            }
        } else if s.matches(':').count() == 1 {
            let colon = s.find(':').unwrap();
            (s[..colon].into(), Some(&s[colon + 1..]))
        } else {
            (s.into(), None) // a bare host (or a bare IPv6 address)
        };
        if host.eq_ignore_ascii_case("localhost") {
            host = DEFAULT_HOST.into();
        } else if host == "*" {
            host = "0.0.0.0".into();
        } else if host.parse::<IpAddr>().is_err() {
            return Err(format!(
                "Not an IP address: {host}. Use a port (8192), 127.0.0.1:8192 or 0.0.0.0:8192."
            ));
        }
        let port = match port_text {
            Some(p) => try_port(p)?,
            None => fallback.port,
        };
        Ok(Self::new(&host, port))
    }

    pub fn looks_like_endpoint(text: &str) -> bool {
        !text.trim().is_empty() && !text.starts_with('-') && Self::parse(Some(text), None).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostUiArgs {
    pub endpoint: HostUiEndpoint,
    pub token: Option<String>,
    pub open: bool,
    pub tui: bool,
    pub error: Option<String>,
    pub login: bool,
    pub require_totp: bool,
    pub trusted_proxies: Vec<IpAddr>,
    pub state_dir: PathBuf,
    pub bench_dir: Option<PathBuf>,
    pub bench_kit: Option<String>,
    pub bench_power: Option<String>,
    /// `--hostui-glance=<url or file>`: the Glance page's feed (SUPERVISOR.md 12), a URL that
    /// answers the full state or a recorded JSON-lines fixture. Off without it.
    pub glance: Option<String>,
    /// `--hostui-glance-public=<host:port>`: the redacted public view on its own listener.
    pub glance_public: Option<String>,
    /// `--hostui-glance-redact=<list>`: what the public view drops (default: requests, oom,
    /// containers, errors).
    pub glance_redact: Option<String>,
    /// `--hostui-glance-control=<url>`: the control plane for the card buttons (default: the
    /// profiles API).
    pub glance_control: Option<String>,
}

fn openorca_home() -> PathBuf {
    crate::projects::store::openorca_home()
}

/// `~/.openorca/webui` (or `$OPENORCA_HOME/webui`).
pub fn default_state_dir() -> PathBuf {
    openorca_home().join("webui")
}

/// `--flag` alone gives "", `--flag=value` the value, else `None`.
fn value(args: &[String], flag: &str) -> Option<String> {
    if args.iter().any(|a| a == flag) {
        return Some(String::new());
    }
    let prefix = format!("{flag}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
}

impl HostUiArgs {
    pub fn headless(&self) -> bool {
        !self.tui
    }

    /// `None` without `--hostui` (and no companion flag). Problems are in `error`.
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut enabled = false;
        let mut endpoint_text: Option<String> = None;
        let mut token: Option<String> = None;
        let (mut open, mut tui, mut login, mut require_totp) = (false, false, false, false);
        let mut proxies = Vec::new();
        let mut error: Option<String> = None;
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            if a == "--hostui" {
                enabled = true;
                if i + 1 < args.len() && HostUiEndpoint::looks_like_endpoint(&args[i + 1]) {
                    i += 1;
                    endpoint_text = Some(args[i].clone());
                }
            } else if let Some(v) = a.strip_prefix("--hostui=") {
                enabled = true;
                endpoint_text = Some(v.into());
            } else if let Some(v) = a.strip_prefix("--hostui-token=") {
                token = Some(v.into());
            } else if a == "--hostui-token" {
                if i + 1 < args.len() {
                    i += 1;
                    token = Some(args[i].clone());
                } else {
                    error = Some("--hostui-token needs a value.".into());
                }
            } else if a == "--hostui-open" {
                open = true;
            } else if a == "--tui" {
                tui = true;
            } else if a == "--hostui-login" {
                login = true;
            } else if a == "--hostui-require-2fa" {
                require_totp = true;
            } else if let Some(v) = a.strip_prefix("--hostui-trust-proxy=") {
                for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    match part.parse::<IpAddr>() {
                        Ok(ip) => proxies.push(ip),
                        Err(_) => {
                            error.get_or_insert(format!(
                                "--hostui-trust-proxy: '{part}' is not an IP address."
                            ));
                        }
                    }
                }
            }
            i += 1;
        }
        if !enabled {
            // The companion flags mean nothing alone; say so instead of silently ignoring them.
            return (token.is_some() || open).then(|| Self {
                token,
                open,
                tui,
                error: Some("--hostui-token and --hostui-open need --hostui.".into()),
                state_dir: default_state_dir(),
                ..Default::default()
            });
        }
        let endpoint = match HostUiEndpoint::parse(endpoint_text.as_deref(), None) {
            Ok(e) => e,
            Err(e) => {
                error.get_or_insert(e);
                HostUiEndpoint::default()
            }
        };
        if token.as_ref().is_some_and(|t| t.chars().count() < 8) {
            error.get_or_insert("--hostui-token must be at least 8 characters.".into());
        }
        if !login && (require_totp || !proxies.is_empty()) {
            error.get_or_insert(
                "--hostui-require-2fa and --hostui-trust-proxy need --hostui-login.".into(),
            );
        }
        let state_dir = args
            .iter()
            .find_map(|a| a.strip_prefix("--hostui-state=").map(PathBuf::from))
            .unwrap_or_else(default_state_dir);
        Some(Self {
            endpoint,
            token,
            open,
            tui,
            error,
            login,
            require_totp,
            trusted_proxies: proxies,
            state_dir,
            bench_dir: value(args, "--hostui-bench").map(|d| {
                if d.is_empty() {
                    openorca_home().join("bench")
                } else {
                    PathBuf::from(d)
                }
            }),
            bench_kit: value(args, "--hostui-bench-kit"),
            bench_power: value(args, "--hostui-bench-power"),
            glance: value(args, "--hostui-glance").filter(|v| !v.is_empty()),
            glance_public: value(args, "--hostui-glance-public").filter(|v| !v.is_empty()),
            glance_redact: value(args, "--hostui-glance-redact"),
            glance_control: value(args, "--hostui-glance-control").filter(|v| !v.is_empty()),
        })
        .map(|mut a| {
            if let Err(e) = a.glance_options() {
                a.error.get_or_insert(e);
            }
            a
        })
    }

    /// The Glance options these flags ask for; `None` when the page is off.
    pub fn glance_options(&self) -> Result<Option<crate::glance::GlanceOptions>, String> {
        let Some(src) = &self.glance else {
            if self.glance_public.is_some()
                || self.glance_redact.is_some()
                || self.glance_control.is_some()
            {
                return Err(
                    "--hostui-glance-public, -redact and -control need --hostui-glance=<feed>."
                        .into(),
                );
            }
            return Ok(None);
        };
        let mut o = crate::glance::GlanceOptions::new(crate::glance::GlanceSource::parse(src));
        o.control_url = self.glance_control.clone();
        if let Some(listen) = &self.glance_public {
            listen.parse::<std::net::SocketAddr>().map_err(|_| {
                format!("--hostui-glance-public: '{listen}' is not host:port (e.g. 0.0.0.0:8077).")
            })?;
            let mut p = crate::glance::PublicOptions::new(listen.clone());
            if let Some(r) = &self.glance_redact {
                p.redaction = crate::glance::Redaction::parse(r)
                    .map_err(|e| format!("--hostui-glance-redact: {e}"))?;
            }
            o.public = Some(p);
        } else if self.glance_redact.is_some() {
            return Err("--hostui-glance-redact needs --hostui-glance-public.".into());
        }
        Ok(Some(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Option<HostUiArgs> {
        HostUiArgs::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_flag_no_web_ui() {
        assert!(parse(&["--continue"]).is_none());
    }

    #[test]
    fn bare_flag_defaults_to_localhost_8192_headless() {
        let a = parse(&["--hostui"]).unwrap();
        assert!(a.error.is_none());
        assert_eq!(a.endpoint, HostUiEndpoint::new("127.0.0.1", 8192));
        assert!(a.headless());
        assert!(!a.open);
        assert!(a.token.is_none());
    }

    #[test]
    fn endpoint_forms() {
        for (arg, host, port) in [
            ("--hostui=9000", "127.0.0.1", 9000),
            ("--hostui=0.0.0.0:8192", "0.0.0.0", 8192),
            ("--hostui=localhost:7001", "127.0.0.1", 7001),
            ("--hostui=[::1]:8200", "::1", 8200),
            ("--hostui=0.0.0.0", "0.0.0.0", 8192),
        ] {
            let a = parse(&[arg]).unwrap();
            assert!(a.error.is_none(), "{arg}: {:?}", a.error);
            assert_eq!(
                (a.endpoint.host.as_str(), a.endpoint.port),
                (host, port),
                "{arg}"
            );
        }
    }

    #[test]
    fn space_separated_endpoint_tui_token_open() {
        let a = parse(&[
            "--hostui",
            "0.0.0.0:9100",
            "--tui",
            "--hostui-token=abcdefgh1234",
            "--hostui-open",
        ])
        .unwrap();
        assert!(a.error.is_none());
        assert!(a.endpoint.is_lan());
        assert_eq!(a.endpoint.port, 9100);
        assert!(!a.headless());
        assert_eq!(a.token.as_deref(), Some("abcdefgh1234"));
        assert!(a.open);
    }

    #[test]
    fn flag_followed_by_other_option_is_not_taken_as_endpoint() {
        let a = parse(&["--hostui", "--tui"]).unwrap();
        assert_eq!(a.endpoint.port, 8192);
        assert!(a.tui);
    }

    #[test]
    fn bad_endpoints_short_tokens_and_lone_companions_are_errors() {
        for arg in [
            "--hostui=notahost:80",
            "--hostui=70000",
            "--hostui=1.2.3.4:x",
        ] {
            assert!(parse(&[arg]).unwrap().error.is_some(), "{arg}");
        }
        assert!(
            parse(&["--hostui", "--hostui-token=abc"])
                .unwrap()
                .error
                .is_some()
        );
        assert!(parse(&["--hostui-open"]).unwrap().error.is_some());
        assert!(
            parse(&["--hostui", "--hostui-require-2fa"])
                .unwrap()
                .error
                .is_some()
        );
    }

    #[test]
    fn endpoint_fallback_keeps_host_for_port_only() {
        let lan = HostUiEndpoint::new("0.0.0.0", 8192);
        assert_eq!(
            HostUiEndpoint::parse(Some("9001"), Some(&lan)).unwrap(),
            HostUiEndpoint::new("0.0.0.0", 9001)
        );
        let lan = HostUiEndpoint::new("0.0.0.0", 8500);
        assert_eq!(
            HostUiEndpoint::parse(Some(""), Some(&lan)).unwrap().port,
            8500
        );
        assert_eq!(HostUiEndpoint::new("::1", 1).to_string(), "[::1]:1");
    }

    #[test]
    fn bench_flags() {
        let a = parse(&["--hostui", "--hostui-bench=/b", "--hostui-bench-kit=/k"]).unwrap();
        assert_eq!(a.bench_dir, Some(PathBuf::from("/b")));
        assert_eq!(a.bench_kit.as_deref(), Some("/k"));
        let a = parse(&["--hostui", "--hostui-bench"]).unwrap();
        assert!(a.bench_dir.unwrap().ends_with("bench"));
    }

    #[test]
    fn glance_flags() {
        let a = parse(&["--hostui"]).unwrap();
        assert!(a.glance_options().unwrap().is_none(), "off by default");
        let a = parse(&["--hostui", "--hostui-glance=http://h:1/api/state"]).unwrap();
        let o = a.glance_options().unwrap().unwrap();
        assert!(o.public.is_none(), "no public view unless asked");
        assert!(matches!(o.source, crate::glance::GlanceSource::Poll(_)));
        let a = parse(&[
            "--hostui",
            "--hostui-glance=/f.jsonl",
            "--hostui-glance-public=0.0.0.0:8077",
            "--hostui-glance-redact=requests,errors",
        ])
        .unwrap();
        assert!(a.error.is_none());
        let p = a.glance_options().unwrap().unwrap().public.unwrap();
        assert_eq!(p.listen, "0.0.0.0:8077");
        assert!(p.redaction.requests && p.redaction.errors && !p.redaction.containers);
        assert!(
            parse(&["--hostui", "--hostui-glance-public=0.0.0.0:8077"])
                .unwrap()
                .error
                .is_some()
        );
        assert!(
            parse(&[
                "--hostui",
                "--hostui-glance=/f",
                "--hostui-glance-public=nope"
            ])
            .unwrap()
            .error
            .is_some()
        );
        assert!(
            parse(&[
                "--hostui",
                "--hostui-glance=/f",
                "--hostui-glance-public=0.0.0.0:1",
                "--hostui-glance-redact=x"
            ])
            .unwrap()
            .error
            .is_some()
        );
    }
}
