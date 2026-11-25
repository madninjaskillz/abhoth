//! `DateTime` the way System.Text.Json reads and writes it: ISO 8601 with up to seven fraction
//! digits (100 ns ticks), trailing zeros trimmed, `Z` for UTC values.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// .NET ticks (100 ns) from 0001-01-01 to the Unix epoch.
const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;
const TICKS_PER_DAY: i64 = TICKS_PER_SECOND * 86_400;

/// A UTC point in time with .NET tick precision (`DateTime` with `Kind = Utc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DotNetDateTime {
    /// 100 ns ticks since 0001-01-01T00:00:00Z.
    pub ticks: i64,
}

impl DotNetDateTime {
    /// `DateTime.UtcNow`.
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(t: SystemTime) -> Self {
        let ticks = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => UNIX_EPOCH_TICKS + (d.as_nanos() / 100) as i64,
            Err(e) => UNIX_EPOCH_TICKS - (e.duration().as_nanos() / 100) as i64,
        };
        Self { ticks }
    }

    pub fn to_system_time(self) -> SystemTime {
        let rel = self.ticks - UNIX_EPOCH_TICKS;
        let d = std::time::Duration::from_nanos(rel.unsigned_abs() * 100);
        if rel >= 0 {
            UNIX_EPOCH + d
        } else {
            UNIX_EPOCH - d
        }
    }

    /// (year, month, day, hour, minute, second, ticks within the second).
    fn parts(self) -> (i64, u32, u32, u32, u32, u32, i64) {
        let days = self.ticks.div_euclid(TICKS_PER_DAY);
        let rem = self.ticks.rem_euclid(TICKS_PER_DAY);
        // Days from 0001-01-01 to 1970-01-01.
        let (y, m, d) = civil_from_days(days - 719_162);
        let secs = rem / TICKS_PER_SECOND;
        (
            y,
            m,
            d,
            (secs / 3600) as u32,
            (secs / 60 % 60) as u32,
            (secs % 60) as u32,
            rem % TICKS_PER_SECOND,
        )
    }

    /// `ToString("yyyyMMdd_HHmmss")`.
    pub fn compact_stamp(self) -> String {
        let (y, mo, d, h, mi, s, _) = self.parts();
        format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")
    }

    /// Parse what System.Text.Json accepts for a `DateTime` (the ISO 8601 profile it writes):
    /// `yyyy-MM-ddTHH:mm[:ss[.fffffff]]` with an optional `Z` or `±hh:mm` offset. A value with
    /// an offset is converted to UTC; one without is taken as UTC.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        let num = |from: usize, len: usize| -> Option<i64> {
            let part = s.get(from..from + len)?;
            part.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| part.parse().ok())?
        };
        let year = num(0, 4)?;
        if b.get(4) != Some(&b'-') {
            return None;
        }
        let month = num(5, 2)?;
        if b.get(7) != Some(&b'-') {
            return None;
        }
        let day = num(8, 2)?;
        let mut ticks_in_day = 0i64;
        let mut i = 10;
        if matches!(b.get(10), Some(b'T') | Some(b't') | Some(b' ')) {
            let hour = num(11, 2)?;
            if b.get(13) != Some(&b':') {
                return None;
            }
            let minute = num(14, 2)?;
            let mut second = 0;
            i = 16;
            if b.get(16) == Some(&b':') {
                second = num(17, 2)?;
                i = 19;
            }
            if hour > 23 || minute > 59 || second > 59 {
                return None;
            }
            ticks_in_day = (hour * 3600 + minute * 60 + second) * TICKS_PER_SECOND;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                let digits = &s[start..i];
                if digits.is_empty() || digits.len() > 16 {
                    return None;
                }
                // Seven digits are ticks; more are truncated.
                let mut frac = String::from(&digits[..digits.len().min(7)]);
                while frac.len() < 7 {
                    frac.push('0');
                }
                ticks_in_day += frac.parse::<i64>().ok()?;
            }
        }
        let mut offset_ticks = 0i64;
        match b.get(i) {
            None => {}
            Some(b'Z') | Some(b'z') if i + 1 == b.len() => {}
            Some(&sign @ (b'+' | b'-')) => {
                let oh = num(i + 1, 2)?;
                if b.get(i + 3) != Some(&b':') || i + 6 != b.len() {
                    return None;
                }
                let om = num(i + 4, 2)?;
                let off = (oh * 60 + om) * 60 * TICKS_PER_SECOND;
                offset_ticks = if sign == b'+' { off } else { -off };
            }
            _ => return None,
        }
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u32) as i64 {
            return None;
        }
        let days = days_from_civil(year, month as u32, day as u32) + 719_162;
        Some(Self {
            ticks: days * TICKS_PER_DAY + ticks_in_day - offset_ticks,
        })
    }
}

impl fmt::Display for DotNetDateTime {
    /// The round-trip form System.Text.Json writes: `2026-11-25T09:30:12.3456789Z`, fraction
    /// trimmed of trailing zeros (and left out when zero).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, mo, d, h, mi, s, frac) = self.parts();
        write!(f, "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")?;
        if frac > 0 {
            let digits = format!("{frac:07}");
            write!(f, ".{}", digits.trim_end_matches('0'))?;
        }
        f.write_str("Z")
    }
}

impl Serialize for DotNetDateTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DotNetDateTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("not a DateTime: {s:?}")))
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_system_text_json_form() {
        for s in [
            "2026-11-25T09:30:12.3456789Z",
            "2026-11-25T09:30:12.1Z",
            "2026-11-25T09:30:12Z",
            "2024-02-29T23:59:59.9999999Z",
            "0001-01-01T00:00:00Z",
            "1969-12-31T23:59:59.5Z",
        ] {
            assert_eq!(DotNetDateTime::parse(s).unwrap().to_string(), s);
        }
    }

    #[test]
    fn offsets_convert_to_utc_and_plain_values_are_utc() {
        let a = DotNetDateTime::parse("2026-11-25T10:30:00+01:00").unwrap();
        assert_eq!(a.to_string(), "2026-11-25T09:30:00Z");
        let b = DotNetDateTime::parse("2026-11-25T09:30:00").unwrap();
        assert_eq!(a, b);
        assert!(DotNetDateTime::parse("2026-13-01T00:00:00Z").is_none());
        assert!(DotNetDateTime::parse("yesterday").is_none());
    }

    #[test]
    fn unix_epoch_and_stamp() {
        let t = DotNetDateTime::from_system_time(UNIX_EPOCH);
        assert_eq!(t.ticks, UNIX_EPOCH_TICKS);
        assert_eq!(t.to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(
            DotNetDateTime::parse("2026-11-05T07:08:09.5Z")
                .unwrap()
                .compact_stamp(),
            "20261105_070809"
        );
        let now = DotNetDateTime::now();
        assert_eq!(DotNetDateTime::from_system_time(now.to_system_time()), now);
    }
}
