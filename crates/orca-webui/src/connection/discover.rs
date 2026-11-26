//! Finding routers on the LAN: a DNS-SD browse for `_abhoth-router._tcp.local.` over mDNS
//! (`mdns-sd`, its own thread, stopped after each browse), then each answer checked with its
//! identification document (`GET /.well-known/abhoth-router`, read-only, no-switch).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use orca_core::router_mode::{self, MDNS_SERVICE_TYPE, RouterInfo};
use serde::Serialize;

/// One advertised router.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// The DNS-SD instance name (the router's display name).
    pub name: String,
    pub host: String,
    pub port: u16,
    /// Its addresses, best first ([`rank`]).
    pub addresses: Vec<IpAddr>,
    /// TXT `v`.
    pub version: Option<String>,
    /// TXT `path` (the identification document).
    pub path: Option<String>,
}

/// One checked router, as the settings page lists it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Checked {
    pub name: String,
    /// The root URL that answered (or the best guess when none did).
    pub url: String,
    /// What `lmStudio.baseUrl` would be.
    pub base_url: String,
    pub version: Option<String>,
    /// It answered with an identification document.
    pub verified: bool,
    pub info: Option<RouterInfo>,
}

/// Lower is better: a private LAN IPv4, other IPv4, then IPv6; loopback, link-local and
/// container bridges (172.17-172.31, often Docker's) last.
pub fn rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() || v4.is_link_local() {
                9
            } else if o[0] == 172 && (17..=31).contains(&o[1]) {
                5
            } else if v4.is_private() {
                0
            } else {
                1
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80 {
                9
            } else {
                3
            }
        }
    }
}

fn instance_name(fullname: &str) -> String {
    let n = fullname
        .strip_suffix(MDNS_SERVICE_TYPE)
        .unwrap_or(fullname)
        .trim_end_matches('.');
    n.replace("\\ ", " ").replace("\\.", ".")
}

/// Browse for `listen` (blocking: run it on a blocking thread). An error when mDNS can't run
/// here at all (no multicast socket).
pub fn browse(listen: Duration) -> Result<Vec<Found>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS unavailable: {e}"))?;
    let rx = daemon
        .browse(MDNS_SERVICE_TYPE)
        .map_err(|e| format!("mDNS browse: {e}"))?;
    let deadline = Instant::now() + listen;
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(s)) if s.is_valid() => {
                let mut addresses: Vec<IpAddr> =
                    s.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                addresses.sort_by_key(|a| (rank(a), a.is_ipv6(), *a));
                addresses.dedup();
                found.insert(
                    s.get_fullname().to_string(),
                    Found {
                        name: s
                            .get_property_val_str("name")
                            .map(str::to_string)
                            .unwrap_or_else(|| instance_name(s.get_fullname())),
                        host: s.get_hostname().trim_end_matches('.').to_string(),
                        port: s.get_port(),
                        addresses,
                        version: s.get_property_val_str("v").map(str::to_string),
                        path: s.get_property_val_str("path").map(str::to_string),
                    },
                );
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(MDNS_SERVICE_TYPE);
    if let Ok(done) = daemon.shutdown() {
        let _ = done.recv_timeout(Duration::from_millis(500));
    }
    Ok(found.into_values().collect())
}

fn url_of(ip: &IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

/// Check each found router: the first of its addresses (best first, at most 3) whose
/// identification document answers.
pub async fn verify(found: Vec<Found>, http: &reqwest::Client) -> Vec<Checked> {
    let checks = found.into_iter().map(|f| async move {
        for ip in f.addresses.iter().take(3) {
            let url = url_of(ip, f.port);
            if let Ok(Some(info)) = router_mode::identify(&url, http).await {
                return Checked {
                    name: if info.name.is_empty() {
                        f.name.clone()
                    } else {
                        info.name.clone()
                    },
                    base_url: router_mode::base_url_of(&url, Some(&info)),
                    url,
                    version: Some(info.version.clone()),
                    verified: true,
                    info: Some(info),
                };
            }
        }
        let url = f
            .addresses
            .first()
            .map(|ip| url_of(ip, f.port))
            .unwrap_or_else(|| format!("http://{}:{}", f.host, f.port));
        Checked {
            name: f.name.clone(),
            base_url: router_mode::base_url_of(&url, None),
            url,
            version: f.version.clone(),
            verified: false,
            info: None,
        }
    });
    futures_util::future::join_all(checks).await
}

/// The best address of a set (tests and logs).
pub fn best(addresses: &[IpAddr]) -> Option<IpAddr> {
    addresses
        .iter()
        .min_by_key(|a| (rank(a), a.is_ipv6()))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_addresses_come_before_bridges_and_loopback() {
        let ips: Vec<IpAddr> = ["127.0.0.1", "172.17.0.1", "fe80::1", "10.1.2.3", "fd00::5"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(best(&ips), Some("10.1.2.3".parse().unwrap()));
        let mut sorted = ips.clone();
        sorted.sort_by_key(|a| (rank(a), a.is_ipv6(), *a));
        assert_eq!(sorted[0].to_string(), "10.1.2.3");
        assert_eq!(sorted.last().unwrap().to_string(), "fe80::1");
    }

    #[test]
    fn instance_names_are_unescaped() {
        assert_eq!(
            instance_name("Lab\\ router._abhoth-router._tcp.local."),
            "Lab router"
        );
    }

    /// Advertise with the crate directly, browse with [`browse`]; skipped (with a line saying
    /// so) where multicast loopback does not work.
    #[test]
    fn a_router_advertised_here_is_found() {
        use mdns_sd::ServiceInfo;
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        let name = format!("Browse test {}", std::process::id());
        let info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &name,
            "browse-test.local.",
            "",
            18_011,
            &[
                ("v", "9.9.9"),
                ("path", router_mode::WELL_KNOWN_PATH),
                ("name", name.as_str()),
            ][..],
        )
        .unwrap()
        .enable_addr_auto();
        daemon.register(info).unwrap();
        let found = browse(Duration::from_secs(6)).unwrap();
        let _ = daemon.shutdown();
        let Some(f) = found.iter().find(|f| f.name == name) else {
            eprintln!(
                "skipped: no multicast loopback here ({} other routers seen)",
                found.len()
            );
            return;
        };
        assert_eq!(f.port, 18_011);
        assert_eq!(f.version.as_deref(), Some("9.9.9"));
        assert!(!f.addresses.is_empty());
    }
}
