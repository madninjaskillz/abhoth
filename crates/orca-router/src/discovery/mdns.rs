//! Advertising the router on the LAN: DNS-SD over mDNS (RFC 6762/6763), service type
//! `_abhoth-router._tcp.local.`, one instance per router, TXT `v` (version), `api`, `path` (the
//! identification document) and `name`. The daemon (`mdns-sd`) runs its own thread and answers
//! queries on every interface; dropping the [`Advert`] withdraws it (a goodbye packet).
//!
//! `--mdns auto` (the default) advertises when the gateway listens beyond loopback.

use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use orca_core::router_mode::{API_VERSION, MDNS_SERVICE_TYPE, WELL_KNOWN_PATH};

/// `--mdns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MdnsMode {
    /// On when the listen address is not loopback.
    #[default]
    Auto,
    On,
    Off,
}

impl MdnsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" | "true" | "yes" | "1" => Some(Self::On),
            "off" | "false" | "no" | "0" => Some(Self::Off),
            _ => None,
        }
    }

    /// Whether to advertise for a gateway bound to `host`.
    pub fn wanted(self, host: &str) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => !is_loopback_host(host),
        }
    }
}

/// `127.0.0.1`, `::1`, `localhost`.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A DNS label from a display name: letters, digits and '-', at most 63 bytes.
pub fn host_label(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    let s = if s.is_empty() { "abhoth-router" } else { s };
    s.chars().take(63).collect()
}

/// A running advertisement; dropping it withdraws the service.
pub struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advert {
    /// The instance's full DNS-SD name.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advert {
    fn drop(&mut self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}

/// Advertise `name` on `port`. `ip`: the one address to announce (the gateway's own bind
/// address), or `None` for every interface's.
pub fn advertise(name: &str, port: u16, ip: Option<IpAddr>) -> Result<Advert, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS: {e}"))?;
    let host = format!("{}.local.", host_label(&super::host_name_or(name)));
    let version = env!("CARGO_PKG_VERSION");
    let api = API_VERSION.to_string();
    let props = [
        ("v", version),
        ("api", api.as_str()),
        ("path", WELL_KNOWN_PATH),
        ("name", name),
    ];
    // Instance names are at most 63 bytes.
    let instance: String = name.chars().take(60).collect();
    let info = match ip {
        Some(ip) => ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &host, ip, port, &props[..]),
        None => ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &host, "", port, &props[..])
            .map(ServiceInfo::enable_addr_auto),
    }
    .map_err(|e| format!("mDNS: {e}"))?;
    let fullname = info.get_fullname().to_string();
    daemon
        .register(info)
        .map_err(|e| format!("mDNS register: {e}"))?;
    Ok(Advert { daemon, fullname })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_advertises_only_beyond_loopback() {
        assert!(MdnsMode::Auto.wanted("0.0.0.0"));
        assert!(MdnsMode::Auto.wanted("10.1.2.3"));
        assert!(!MdnsMode::Auto.wanted("127.0.0.1"));
        assert!(!MdnsMode::Auto.wanted("localhost"));
        assert!(!MdnsMode::Auto.wanted("[::1]"));
        assert!(MdnsMode::On.wanted("127.0.0.1"));
        assert!(!MdnsMode::Off.wanted("0.0.0.0"));
        assert_eq!(MdnsMode::parse("OFF"), Some(MdnsMode::Off));
        assert_eq!(MdnsMode::parse("maybe"), None);
    }

    #[test]
    fn host_labels_are_dns_safe() {
        assert_eq!(
            host_label("Abhoth router on gpu-host"),
            "Abhoth-router-on-gpu-host"
        );
        assert_eq!(host_label("  ..  "), "abhoth-router");
        assert_eq!(host_label(&"x".repeat(100)).len(), 63);
    }
}
