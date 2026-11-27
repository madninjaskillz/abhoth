//! abhoth bridge (ROUTER-MEDIA-PLAN.md 11 and 13): a machine the router manages without opening a
//! port on it. The bridge dials the router's `/bridge/ws` with a token, describes its server,
//! cards and engines, and carries the router's commands and proxied requests to local engines.
//!
//! - [`proto`]: the frames and the descriptor (both ends);
//! - [`mux`]: commands and streams over one connection, with flow control (both ends);
//! - [`config`], [`detect`], [`runner`], [`client`]: the bridge itself.

pub mod cli;
pub mod client;
pub mod config;
pub mod detect;
pub mod mux;
pub mod package;
pub mod proto;
pub mod runner;

use std::sync::Arc;

/// Where the bridge writes its log lines (stdout headless; the app's log in the desktop).
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// Log lines on stdout with a UTC time stamp.
pub fn stdout_log() -> Log {
    Arc::new(|line: &str| {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        println!("{} abhoth-bridge: {line}", utc(t));
    })
}

/// `2026-11-27T01:02:03Z` from Unix seconds (no date library needed).
pub fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(super::utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(super::utc(1_795_740_000), "2026-11-27T00:40:00Z");
    }
}
