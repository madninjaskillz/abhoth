//! Time for the bench: .NET's `DateTimeOffset` texts (UTC), run-id stamps, UK local time (the
//! quiet window) and ISO 8601 parsing, without a date crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::util::civil_from_days;

/// Seconds and nanoseconds since the Unix epoch (negative times clamp to the epoch).
pub fn unix(t: SystemTime) -> (i64, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos())
}

/// A Unix time in seconds as a `SystemTime`.
pub fn from_unix(secs: i64, nanos: u32) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos)
    } else {
        UNIX_EPOCH - Duration::from_secs((-secs) as u64) + Duration::from_nanos(nanos as u64)
    }
}

/// (year, month, day) to days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parts(secs: i64) -> (i64, u32, u32, i64, i64, i64) {
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    (y, mo, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `DateTimeOffset.UtcNow.ToString("O")`: `2026-11-22T02:28:41.1586046+00:00`.
pub fn iso_o(t: SystemTime) -> String {
    let (secs, nanos) = unix(t);
    let (y, mo, d, h, mi, s) = parts(secs);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:07}+00:00",
        nanos / 100
    )
}

/// A UTC `DateTimeOffset` as System.Text.Json writes it: the fraction's trailing zeros dropped.
pub fn iso_json(t: SystemTime) -> String {
    let (secs, nanos) = unix(t);
    let (y, mo, d, h, mi, s) = parts(secs);
    let mut frac = if nanos / 100 == 0 {
        String::new()
    } else {
        format!(".{:07}", nanos / 100)
    };
    while frac.ends_with('0') {
        frac.pop();
    }
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}+00:00")
}

/// `ToString("yyyyMMdd-HHmmss")` of a UTC time: the run ids' stamp.
pub fn stamp(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = parts(unix(t).0);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// The last Sunday of a month, as days since the epoch.
fn last_sunday(y: i64, m: u32) -> i64 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let last = days_from_civil(ny, nm, 1) - 1;
    // 1970-01-01 was a Thursday: weekday 0 = Sunday is (days + 4) mod 7.
    last - (last + 4).rem_euclid(7)
}

/// The UK's offset from UTC at a UTC instant: one hour (BST) from 01:00 UTC on the last Sunday
/// of March to 01:00 UTC on the last Sunday of October, else none.
pub fn uk_offset_secs(utc_secs: i64) -> i64 {
    let (y, _, _) = civil_from_days(utc_secs.div_euclid(86_400));
    let start = last_sunday(y, 3) * 86_400 + 3600;
    let end = last_sunday(y, 10) * 86_400 + 3600;
    if utc_secs >= start && utc_secs < end {
        3600
    } else {
        0
    }
}

/// `InQuietWindow`: true when a run starting at `now` (up to `length` long) would touch the
/// quiet window, `from`..`to` UK time (minutes after midnight).
pub fn in_quiet_window(now: SystemTime, length: Duration, from: u32, to: u32) -> bool {
    let (secs, nanos) = unix(now);
    let local_ms = (secs + uk_offset_secs(secs)) * 1000 + (nanos / 1_000_000) as i64;
    let start = local_ms;
    let end = start + length.as_millis() as i64;
    let mut day = start.div_euclid(86_400_000) - 1;
    let last = end.div_euclid(86_400_000);
    while day <= last {
        let q_from = day * 86_400_000 + from as i64 * 60_000;
        let q_to = day * 86_400_000 + to as i64 * 60_000;
        if start < q_to && end > q_from {
            return true;
        }
        day += 1;
    }
    false
}

/// "06:45" for minutes after midnight.
pub fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

/// Minutes after midnight, UK time, at `t`.
pub fn uk_minutes(t: SystemTime) -> u32 {
    let secs = unix(t).0;
    ((secs + uk_offset_secs(secs)).rem_euclid(86_400) / 60) as u32
}

/// An ISO 8601 date-time (`2026-11-17T20:00:00.123+00:00`, `...Z`, or no offset = UTC).
pub fn parse_iso(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos: u32 = 0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let mut padded: String = digits.chars().take(9).collect();
        while padded.len() < 9 {
            padded.push('0');
        }
        nanos = padded.parse().ok()?;
        rest = &r[digits.len()..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let o = &rest[1..];
            let (oh, om) = match o.len() {
                5 if o.as_bytes()[2] == b':' => {
                    (o[..2].parse::<i64>().ok()?, o[3..].parse::<i64>().ok()?)
                }
                4 => (o[..2].parse::<i64>().ok()?, o[2..].parse::<i64>().ok()?),
                2 => (o.parse::<i64>().ok()?, 0),
                _ => return None,
            };
            sign * (oh * 3600 + om * 60)
        }
    };
    let secs = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + sec - offset;
    Some(from_unix(secs, nanos))
}

/// `Math.Round(x, digits)` (midpoints to even, as .NET does).
pub fn round(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round_ties_even() / f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> SystemTime {
        parse_iso(s).unwrap()
    }

    #[test]
    fn quiet_window_is_uk_time_and_counts_the_run_length() {
        let (from, to) = (6 * 60 + 45, 7 * 60 + 30);
        let cases = [
            ("2026-11-17T06:50:00Z", 0, true),   // inside (GMT in November)
            ("2026-11-17T06:00:00Z", 30, false), // ends 06:30
            ("2026-11-17T06:00:00Z", 60, true),  // would run into it
            ("2026-11-17T07:31:00Z", 600, false),
            ("2026-06-17T05:50:00Z", 0, true),   // 06:50 BST
            ("2026-11-16T23:00:00Z", 480, true), // overnight run past 06:45
        ];
        for (utc, minutes, blocked) in cases {
            assert_eq!(
                in_quiet_window(at(utc), Duration::from_secs(minutes * 60), from, to),
                blocked,
                "{utc} +{minutes} min"
            );
        }
    }

    #[test]
    fn texts_match_dotnet() {
        let t = from_unix(1_795_314_521, 158_604_600);
        assert_eq!(iso_o(t), "2026-11-22T02:28:41.1586046+00:00");
        assert_eq!(iso_json(t), "2026-11-22T02:28:41.1586046+00:00");
        assert_eq!(
            iso_json(from_unix(1_795_314_521, 0)),
            "2026-11-22T02:28:41+00:00"
        );
        assert_eq!(stamp(t), "20261122-022841");
        assert_eq!(at("2026-11-22T02:28:41.1586046+00:00"), t);
        assert_eq!(at("2026-11-22T03:28:41.1586046+01:00"), t);
        assert_eq!(round(2.25, 1), 2.2);
        assert_eq!(round(2.35, 1), 2.4);
    }

    #[test]
    fn bst_starts_and_ends_on_the_last_sundays() {
        // 2026: BST from 29 March 01:00 UTC to 25 October 01:00 UTC.
        assert_eq!(uk_offset_secs(unix(at("2026-03-29T00:59:00Z")).0), 0);
        assert_eq!(uk_offset_secs(unix(at("2026-03-29T01:00:00Z")).0), 3600);
        assert_eq!(uk_offset_secs(unix(at("2026-10-25T00:59:00Z")).0), 3600);
        assert_eq!(uk_offset_secs(unix(at("2026-10-25T01:00:00Z")).0), 0);
    }
}
