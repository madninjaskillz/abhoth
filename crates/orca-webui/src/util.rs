//! Small helpers: .NET-style UTC timestamps, random tokens, JSON argument access.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::Value;

/// `DateTime.UtcNow` as System.Text.Json writes it: `2026-11-25T10:00:00.1234567Z`
/// (trailing zeros of the fraction dropped, no fraction when whole).
pub fn utc_now_iso() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_unix(d.as_secs() as i64, d.subsec_nanos())
}

/// A Unix time as .NET's round-trip ("O"-like) UTC text.
pub fn iso_from_unix(secs: i64, nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let ticks = nanos / 100;
    let mut frac = if ticks == 0 {
        String::new()
    } else {
        format!(".{ticks:07}")
    };
    while frac.ends_with('0') {
        frac.pop();
    }
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{frac}Z")
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `n` random bytes from the OS.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the OS random source is available");
    buf
}

/// `HostUiController.NewToken`: 24 random bytes, base64url without padding.
pub fn new_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(24))
}

/// Lower-case hex.
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A short random id (`Guid.NewGuid().ToString("N")[..n]`).
pub fn short_id(n: usize) -> String {
    let mut s = hex_lower(&random_bytes(n.div_ceil(2)));
    s.truncate(n);
    s
}

/// `Uri.EscapeDataString`: everything but unreserved characters percent-encoded.
pub fn escape_data(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

/// Argument `i` of a hub call as a string (`null` and missing count as `None`).
pub fn arg_str(args: &[Value], i: usize) -> Option<String> {
    match args.get(i) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// `text.Length` in UTF-16 units, as C# counts.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `n` with thousands separators (`N0`).
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_matches_dotnet_round_trip_text() {
        assert_eq!(iso_from_unix(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso_from_unix(1_764_064_800, 123_456_700),
            "2025-11-25T10:00:00.1234567Z"
        );
        assert_eq!(
            iso_from_unix(951_782_400, 500_000_000),
            "2000-02-29T00:00:00.5Z"
        );
    }

    #[test]
    fn tokens_are_url_safe_and_long_enough() {
        let t = new_token();
        assert_eq!(t.len(), 32);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(t, new_token());
    }

    #[test]
    fn escape_data_keeps_unreserved() {
        assert_eq!(escape_data("a b/c~d"), "a%20b%2Fc~d");
    }
}
