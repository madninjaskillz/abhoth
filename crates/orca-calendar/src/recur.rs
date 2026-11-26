//! Recurrence: RRULE (RFC 5545, the subset calendars use), 5-field cron (Vixie semantics, as
//! TrueNAS runs it) and one-off times, expanded in **local wall-clock time** of a time zone and
//! only then turned into UTC. That is what makes "every day at 07:00" stay at 07:00 across DST.
//!
//! Local times that do not exist or exist twice are resolved as RFC 5545 3.3.5 says:
//! - in the spring-forward gap (01:30 on the last Sunday of March in London) the time is read
//!   with the offset from before the gap, so it lands just after it (01:30 GMT = 02:30 BST);
//! - in the autumn overlap (01:30 on the last Sunday of October) the first of the two is used.

use chrono::{
    Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike, Weekday,
};
use chrono_tz::Tz;

/// UTC epoch of a local wall-clock time in `tz` (gap: offset before it; overlap: the first).
pub fn to_utc(tz: Tz, local: NaiveDateTime) -> i64 {
    match tz.from_local_datetime(&local) {
        chrono::LocalResult::Single(d) => d.timestamp(),
        chrono::LocalResult::Ambiguous(a, b) => a.timestamp().min(b.timestamp()),
        chrono::LocalResult::None => {
            // The offset in force just before the gap (gaps are at most a few hours).
            let before = (1..=6)
                .find_map(|h| {
                    tz.from_local_datetime(&(local - Duration::hours(h)))
                        .earliest()
                })
                .map(|d| d.offset().fix().local_minus_utc())
                .unwrap_or(0);
            local.and_utc().timestamp() - i64::from(before)
        }
    }
}

/// The local wall-clock time of a UTC epoch in `tz`.
pub fn to_local(tz: Tz, epoch: i64) -> NaiveDateTime {
    tz.timestamp_opt(epoch, 0)
        .single()
        .map(|d| d.naive_local())
        .unwrap_or_default()
}

/// Parse a local date-time: `2026-12-01T20:00`, `2026-12-01T20:00:00`, `2026-12-01 20:00` or a
/// bare date (midnight).
pub fn parse_local(s: &str) -> Result<NaiveDateTime, String> {
    let t = s.trim();
    for f in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, f) {
            return Ok(d);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Ok(d.and_time(NaiveTime::MIN));
    }
    Err(format!("bad local time '{s}' (use 2026-12-01T20:00)"))
}

// ── RRULE ─────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    Local(NaiveDateTime),
    Utc(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rrule {
    pub freq: Freq,
    pub interval: u32,
    pub count: Option<u32>,
    pub until: Option<Until>,
    /// (ordinal, weekday): `1SU` = (Some(1), Sun), `MO` = (None, Mon)
    pub by_day: Vec<(Option<i32>, Weekday)>,
    pub by_hour: Vec<u32>,
    pub by_minute: Vec<u32>,
    pub by_second: Vec<u32>,
    pub by_month_day: Vec<i32>,
    pub by_month: Vec<u32>,
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

pub fn weekday_code(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

fn nums<T: std::str::FromStr>(k: &str, v: &str, ok: impl Fn(&T) -> bool) -> Result<Vec<T>, String> {
    v.split(',')
        .map(|x| {
            x.trim()
                .parse::<T>()
                .ok()
                .filter(|n| ok(n))
                .ok_or_else(|| format!("RRULE {k}: bad value '{x}'"))
        })
        .collect()
}

/// `20261201`, `20261201T070000`, `20261201T070000Z`.
fn parse_ics_time(v: &str) -> Result<Until, String> {
    let z = v.ends_with('Z');
    let t = v.trim_end_matches('Z');
    let d = if t.len() == 8 {
        NaiveDate::parse_from_str(t, "%Y%m%d")
            .map(|d| d.and_hms_opt(23, 59, 59).unwrap_or_default())
            .map_err(|_| format!("bad date '{v}'"))?
    } else {
        NaiveDateTime::parse_from_str(t, "%Y%m%dT%H%M%S")
            .map_err(|_| format!("bad date-time '{v}'"))?
    };
    Ok(if z {
        Until::Utc(d.and_utc().timestamp())
    } else {
        Until::Local(d)
    })
}

impl Rrule {
    pub fn parse(s: &str) -> Result<Self, String> {
        let body = s.trim().trim_start_matches("RRULE:");
        let mut r = Rrule {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            by_day: vec![],
            by_hour: vec![],
            by_minute: vec![],
            by_second: vec![],
            by_month_day: vec![],
            by_month: vec![],
        };
        let mut freq = None;
        for part in body.split(';').filter(|p| !p.trim().is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("RRULE: '{part}' is not KEY=VALUE"))?;
            let k = k.trim().to_ascii_uppercase();
            let v = v.trim().to_ascii_uppercase();
            match k.as_str() {
                "FREQ" => {
                    freq = Some(match v.as_str() {
                        "MINUTELY" => Freq::Minutely,
                        "HOURLY" => Freq::Hourly,
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return Err(format!("RRULE FREQ '{v}' is not supported")),
                    })
                }
                "INTERVAL" => {
                    r.interval = v
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or("RRULE INTERVAL must be 1 or more")?
                }
                "COUNT" => r.count = Some(v.parse().map_err(|_| "RRULE COUNT must be a number")?),
                "UNTIL" => r.until = Some(parse_ics_time(&v)?),
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim();
                        if d.len() < 2 {
                            return Err(format!("RRULE BYDAY: bad day '{d}'"));
                        }
                        let (n, w) = d.split_at(d.len() - 2);
                        let w = weekday(w).ok_or_else(|| format!("RRULE BYDAY: bad day '{d}'"))?;
                        let n = if n.is_empty() {
                            None
                        } else {
                            Some(
                                n.trim_start_matches('+')
                                    .parse::<i32>()
                                    .ok()
                                    .filter(|x| *x != 0 && x.abs() <= 53)
                                    .ok_or_else(|| format!("RRULE BYDAY: bad ordinal '{d}'"))?,
                            )
                        };
                        r.by_day.push((n, w));
                    }
                }
                "BYHOUR" => r.by_hour = nums(&k, &v, |n: &u32| *n < 24)?,
                "BYMINUTE" => r.by_minute = nums(&k, &v, |n: &u32| *n < 60)?,
                "BYSECOND" => r.by_second = nums(&k, &v, |n: &u32| *n < 60)?,
                "BYMONTHDAY" => r.by_month_day = nums(&k, &v, |n: &i32| *n != 0 && n.abs() <= 31)?,
                "BYMONTH" => r.by_month = nums(&k, &v, |n: &u32| (1..=12).contains(n))?,
                "WKST" => {
                    if v != "MO" {
                        return Err("RRULE WKST: only MO is supported".into());
                    }
                }
                "BYSETPOS" | "BYYEARDAY" | "BYWEEKNO" => {
                    return Err(format!("RRULE {k} is not supported"));
                }
                _ => return Err(format!("RRULE: unknown part '{k}'")),
            }
        }
        r.freq = freq.ok_or("RRULE: FREQ is missing")?;
        if r.count.is_some() && r.until.is_some() {
            return Err("RRULE: COUNT and UNTIL together are not allowed".into());
        }
        Ok(r)
    }

    pub fn to_text(&self) -> String {
        let mut p = vec![format!(
            "FREQ={}",
            match self.freq {
                Freq::Minutely => "MINUTELY",
                Freq::Hourly => "HOURLY",
                Freq::Daily => "DAILY",
                Freq::Weekly => "WEEKLY",
                Freq::Monthly => "MONTHLY",
                Freq::Yearly => "YEARLY",
            }
        )];
        if self.interval > 1 {
            p.push(format!("INTERVAL={}", self.interval));
        }
        if let Some(c) = self.count {
            p.push(format!("COUNT={c}"));
        }
        let j = |v: Vec<String>| v.join(",");
        if !self.by_month.is_empty() {
            p.push(format!(
                "BYMONTH={}",
                j(self.by_month.iter().map(|x| x.to_string()).collect())
            ));
        }
        if !self.by_month_day.is_empty() {
            p.push(format!(
                "BYMONTHDAY={}",
                j(self.by_month_day.iter().map(|x| x.to_string()).collect())
            ));
        }
        if !self.by_day.is_empty() {
            p.push(format!(
                "BYDAY={}",
                j(self
                    .by_day
                    .iter()
                    .map(|(n, w)| format!(
                        "{}{}",
                        n.map(|n| n.to_string()).unwrap_or_default(),
                        weekday_code(*w)
                    ))
                    .collect())
            ));
        }
        if !self.by_hour.is_empty() {
            p.push(format!(
                "BYHOUR={}",
                j(self.by_hour.iter().map(|x| x.to_string()).collect())
            ));
        }
        if !self.by_minute.is_empty() {
            p.push(format!(
                "BYMINUTE={}",
                j(self.by_minute.iter().map(|x| x.to_string()).collect())
            ));
        }
        if !self.by_second.is_empty() {
            p.push(format!(
                "BYSECOND={}",
                j(self.by_second.iter().map(|x| x.to_string()).collect())
            ));
        }
        if let Some(u) = &self.until {
            p.push(format!(
                "UNTIL={}",
                match u {
                    Until::Local(d) => d.format("%Y%m%dT%H%M%S").to_string(),
                    Until::Utc(e) => chrono::DateTime::from_timestamp(*e, 0)
                        .unwrap_or_default()
                        .format("%Y%m%dT%H%M%SZ")
                        .to_string(),
                }
            ));
        }
        p.join(";")
    }

    fn months_between(a: NaiveDate, b: NaiveDate) -> i64 {
        (i64::from(b.year()) - i64::from(a.year())) * 12 + i64::from(b.month())
            - i64::from(a.month())
    }

    fn days_in_month(d: NaiveDate) -> u32 {
        let (y, m) = (d.year(), d.month());
        let next = if m == 12 {
            NaiveDate::from_ymd_opt(y + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(y, m + 1, 1)
        };
        next.and_then(|n| n.pred_opt())
            .map(|l| l.day())
            .unwrap_or(28)
    }

    fn month_day_ok(&self, d: NaiveDate) -> bool {
        let dim = Self::days_in_month(d) as i32;
        self.by_month_day.iter().any(|&md| {
            let want = if md > 0 { md } else { dim + 1 + md };
            want == d.day() as i32
        })
    }

    /// BYDAY with ordinals counted within the month.
    fn by_day_ok(&self, d: NaiveDate) -> bool {
        let dim = Self::days_in_month(d) as i32;
        let day = d.day() as i32;
        self.by_day.iter().any(|(n, w)| {
            if d.weekday() != *w {
                return false;
            }
            match n {
                None => true,
                Some(n) if *n > 0 => (day - 1) / 7 + 1 == *n,
                Some(n) => (dim - day) / 7 + 1 == -*n,
            }
        })
    }

    /// Does day `d` take part (day-level parts and the interval)?
    fn day_ok(&self, anchor: NaiveDateTime, d: NaiveDate) -> bool {
        let a = anchor.date();
        if d < a {
            return false;
        }
        if !self.by_month.is_empty() && !self.by_month.contains(&d.month()) {
            return false;
        }
        let iv = i64::from(self.interval);
        match self.freq {
            Freq::Minutely | Freq::Hourly => {
                (self.by_day.is_empty() || self.by_day_ok(d))
                    && (self.by_month_day.is_empty() || self.month_day_ok(d))
            }
            Freq::Daily => {
                (d - a).num_days() % iv == 0
                    && (self.by_day.is_empty() || self.by_day_ok(d))
                    && (self.by_month_day.is_empty() || self.month_day_ok(d))
            }
            Freq::Weekly => {
                let wk = |x: NaiveDate| {
                    x - Duration::days(i64::from(x.weekday().num_days_from_monday()))
                };
                let weeks = (wk(d) - wk(a)).num_days() / 7;
                let day = if self.by_day.is_empty() {
                    d.weekday() == a.weekday()
                } else {
                    self.by_day.iter().any(|(_, w)| *w == d.weekday())
                };
                weeks % iv == 0 && day && (self.by_month_day.is_empty() || self.month_day_ok(d))
            }
            Freq::Monthly => {
                if Self::months_between(a, d) % iv != 0 {
                    return false;
                }
                match (self.by_day.is_empty(), self.by_month_day.is_empty()) {
                    (true, true) => d.day() == a.day(),
                    (false, true) => self.by_day_ok(d),
                    (true, false) => self.month_day_ok(d),
                    (false, false) => self.by_day_ok(d) && self.month_day_ok(d),
                }
            }
            Freq::Yearly => {
                if (i64::from(d.year()) - i64::from(a.year())) % iv != 0 {
                    return false;
                }
                if self.by_month.is_empty() && d.month() != a.month() {
                    return false;
                }
                match (self.by_day.is_empty(), self.by_month_day.is_empty()) {
                    (true, true) => d.day() == a.day(),
                    (false, true) => self.by_day_ok(d),
                    (true, false) => self.month_day_ok(d),
                    (false, false) => self.by_day_ok(d) && self.month_day_ok(d),
                }
            }
        }
    }

    /// The local start times on day `d`, in order.
    fn times(&self, anchor: NaiveDateTime, d: NaiveDate) -> Vec<NaiveDateTime> {
        let pick = |v: &Vec<u32>, dflt: u32| if v.is_empty() { vec![dflt] } else { v.clone() };
        let secs = pick(&self.by_second, anchor.second());
        let iv = i64::from(self.interval);
        let mut out = Vec::new();
        match self.freq {
            Freq::Minutely => {
                for h in 0..24 {
                    if !self.by_hour.is_empty() && !self.by_hour.contains(&h) {
                        continue;
                    }
                    for m in 0..60 {
                        if !self.by_minute.is_empty() && !self.by_minute.contains(&m) {
                            continue;
                        }
                        let t = d.and_hms_opt(h, m, 0).unwrap_or_default();
                        let since = (t - anchor.with_second(0).unwrap_or(anchor)).num_minutes();
                        if since % iv == 0 {
                            out.extend(secs.iter().filter_map(|s| d.and_hms_opt(h, m, *s)));
                        }
                    }
                }
            }
            Freq::Hourly => {
                let mins = pick(&self.by_minute, anchor.minute());
                for h in 0..24 {
                    if !self.by_hour.is_empty() && !self.by_hour.contains(&h) {
                        continue;
                    }
                    let t = d.and_hms_opt(h, 0, 0).unwrap_or_default();
                    let base = anchor
                        .with_minute(0)
                        .and_then(|x| x.with_second(0))
                        .unwrap_or(anchor);
                    if (t - base).num_hours() % iv != 0 {
                        continue;
                    }
                    for m in &mins {
                        out.extend(secs.iter().filter_map(|s| d.and_hms_opt(h, *m, *s)));
                    }
                }
            }
            _ => {
                let hours = pick(&self.by_hour, anchor.hour());
                let mins = pick(&self.by_minute, anchor.minute());
                for h in &hours {
                    for m in &mins {
                        out.extend(secs.iter().filter_map(|s| d.and_hms_opt(*h, *m, *s)));
                    }
                }
            }
        }
        out.sort();
        out
    }
}

// ── cron ──────────────────────────────────────────────────────────────────────────────────────

/// A 5-field cron line: minute hour day-of-month month day-of-week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    pub text: String,
    minutes: Vec<u32>,
    hours: Vec<u32>,
    dom: Vec<u32>,
    months: Vec<u32>,
    dow: Vec<u32>,
    dom_star: bool,
    dow_star: bool,
}

fn cron_field(f: &str, lo: u32, hi: u32, names: &[&str]) -> Result<Vec<u32>, String> {
    let name = |s: &str| -> Option<u32> {
        let l = s.to_ascii_lowercase();
        names.iter().position(|n| *n == l).map(|i| i as u32 + lo)
    };
    let num = |s: &str| -> Result<u32, String> {
        s.parse::<u32>()
            .ok()
            .or_else(|| name(s))
            .ok_or_else(|| format!("cron: bad value '{s}'"))
    };
    let mut out = Vec::new();
    for part in f.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("cron: bad step in '{part}'"))?,
            ),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a)?, num(b)?)
        } else {
            let a = num(range)?;
            (a, if part.contains('/') { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("cron: '{part}' is outside {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            out.push(v);
            v += step;
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

impl Cron {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let expanded = match t {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "cron: '{s}' needs 5 fields (minute hour day month weekday)"
            ));
        }
        const MONTHS: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let mut dow = cron_field(f[4], 0, 7, &DAYS)?;
        for d in dow.iter_mut() {
            if *d == 7 {
                *d = 0;
            }
        }
        dow.sort();
        dow.dedup();
        Ok(Cron {
            text: t.to_string(),
            minutes: cron_field(f[0], 0, 59, &[])?,
            hours: cron_field(f[1], 0, 23, &[])?,
            dom: cron_field(f[2], 1, 31, &[])?,
            months: cron_field(f[3], 1, 12, &MONTHS)?,
            dow,
            dom_star: f[2].starts_with('*'),
            dow_star: f[4].starts_with('*'),
        })
    }

    fn day_ok(&self, d: NaiveDate) -> bool {
        if !self.months.contains(&d.month()) {
            return false;
        }
        let dom = self.dom.contains(&d.day());
        let dow = self.dow.contains(&d.weekday().num_days_from_sunday());
        // Vixie cron: when both day fields are restricted, either may match.
        if self.dom_star || self.dow_star {
            dom && dow
        } else {
            dom || dow
        }
    }

    fn times(&self, d: NaiveDate) -> Vec<NaiveDateTime> {
        let mut out = Vec::new();
        for h in &self.hours {
            for m in &self.minutes {
                out.extend(d.and_hms_opt(*h, *m, 0));
            }
        }
        out
    }

    /// Runs per day (for `info` entries that fire often: shown as one band, not 144 blocks).
    pub fn runs_per_day(&self) -> usize {
        self.hours.len() * self.minutes.len()
    }
}

// ── one recurrence ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Recurrence {
    Once(NaiveDateTime),
    Rrule { rule: Rrule, anchor: NaiveDateTime },
    Cron(Cron),
}

/// Safety valve: no expansion walks more than this many days.
const MAX_DAYS: i64 = 366 * 30;

impl Recurrence {
    /// Local start times with `from <= start < to` (both local), in order. COUNT and UNTIL
    /// apply (UNTIL in UTC needs `tz`).
    pub fn starts(&self, tz: Tz, from: NaiveDateTime, to: NaiveDateTime) -> Vec<NaiveDateTime> {
        match self {
            Recurrence::Once(t) => {
                if *t >= from && *t < to {
                    vec![*t]
                } else {
                    vec![]
                }
            }
            Recurrence::Cron(c) => {
                let mut out = Vec::new();
                let mut d = from.date();
                let mut n = 0;
                while d <= to.date() && n < MAX_DAYS {
                    if c.day_ok(d) {
                        out.extend(c.times(d).into_iter().filter(|t| *t >= from && *t < to));
                    }
                    d = d.succ_opt().unwrap_or(d);
                    n += 1;
                }
                out
            }
            Recurrence::Rrule { rule, anchor } => {
                let mut out = Vec::new();
                // With COUNT, occurrences are counted from the anchor; otherwise start near `from`.
                let mut d = if rule.count.is_some() {
                    anchor.date()
                } else {
                    from.date().max(anchor.date())
                };
                let mut seen: u32 = 0;
                let mut n = 0;
                'days: while d <= to.date() && n < MAX_DAYS {
                    n += 1;
                    if rule.day_ok(*anchor, d) {
                        for t in rule.times(*anchor, d) {
                            if t < *anchor {
                                continue;
                            }
                            match &rule.until {
                                Some(Until::Local(u)) if t > *u => break 'days,
                                Some(Until::Utc(u)) if to_utc(tz, t) > *u => break 'days,
                                _ => {}
                            }
                            if let Some(c) = rule.count {
                                if seen >= c {
                                    break 'days;
                                }
                                seen += 1;
                            }
                            if t >= from && t < to {
                                out.push(t);
                            }
                        }
                    }
                    d = match d.succ_opt() {
                        Some(x) => x,
                        None => break,
                    };
                }
                out
            }
        }
    }

    /// A short human description (`daily 07:00`, `cron 0 7 * * 0`, `once 2026-12-01 20:00`).
    pub fn describe(&self) -> String {
        match self {
            Recurrence::Once(t) => format!("once {}", t.format("%Y-%m-%d %H:%M")),
            Recurrence::Cron(c) => format!("cron {}", c.text),
            Recurrence::Rrule { rule, .. } => {
                let at = match (rule.by_hour.as_slice(), rule.by_minute.as_slice()) {
                    ([h], [m]) => format!(" {h:02}:{m:02}"),
                    _ => String::new(),
                };
                let days = if rule.by_day.is_empty() {
                    String::new()
                } else {
                    format!(
                        " on {}",
                        rule.by_day
                            .iter()
                            .map(|(n, w)| format!(
                                "{}{}",
                                n.map(|n| n.to_string()).unwrap_or_default(),
                                weekday_code(*w)
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                let f = match rule.freq {
                    Freq::Minutely => "every minute",
                    Freq::Hourly => "hourly",
                    Freq::Daily => "daily",
                    Freq::Weekly => "weekly",
                    Freq::Monthly => "monthly",
                    Freq::Yearly => "yearly",
                };
                let every = if rule.interval > 1 {
                    format!(" (every {})", rule.interval)
                } else {
                    String::new()
                };
                format!("{f}{every}{days}{at}")
            }
        }
    }
}
