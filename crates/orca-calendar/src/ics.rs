//! iCal (RFC 5545) VEVENTs as calendar entries (SUPERVISOR.md 8.3): import once
//! (`calendar import-ics file.ics`) or a read-only subscription (`[calendar.subscriptions]`,
//! off by default). RRULE, EXDATE, DURATION/DTEND and time zones (TZID or UTC) are kept;
//! cancelled events are skipped. Every imported entry gets the subscription's default effects.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use chrono_tz::Tz;
use indexmap::IndexMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::entry::{EntryConfig, Priority};
use crate::{Issue, recur};

#[derive(Debug, Clone, PartialEq)]
pub struct IcsEvent {
    pub uid: String,
    pub summary: String,
    pub start: NaiveDateTime,
    /// `Some(name)` from TZID, `Some("UTC")` for `…Z`, `None` = floating
    pub tz: Option<String>,
    pub all_day: bool,
    pub duration: i64,
    pub rrule: Option<String>,
    pub exdates: Vec<NaiveDateTime>,
}

/// Unfold continuation lines (a leading space or tab continues the previous line).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            if let Some(l) = out.last_mut() {
                l.push_str(&line[1..]);
            }
        } else if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// `NAME;PARAM=X;P2=Y:value` -> (NAME, params, value).
fn split_prop(line: &str) -> (String, Vec<(String, String)>, String) {
    let (head, value) = match line.find(':') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, ""),
    };
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or("").to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        .collect();
    (name, params, value.to_string())
}

fn unescape(s: &str) -> String {
    s.replace("\\n", " ")
        .replace("\\N", " ")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

/// A DATE or DATE-TIME value: (local time, all-day, utc).
fn parse_dt(v: &str, params: &[(String, String)]) -> Result<(NaiveDateTime, bool, bool), String> {
    let date_only = params.iter().any(|(k, x)| k == "VALUE" && x == "DATE") || v.len() == 8;
    if date_only {
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").map_err(|_| format!("bad date '{v}'"))?;
        return Ok((d.and_time(NaiveTime::MIN), true, false));
    }
    let utc = v.ends_with('Z');
    let t = NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
        .map_err(|_| format!("bad date-time '{v}'"))?;
    Ok((t, false, utc))
}

/// `PT1H30M`, `P1D`, `-PT5M` (sign ignored), `P1W`.
pub fn parse_ics_duration(v: &str) -> Result<i64, String> {
    let t = v.trim().trim_start_matches(['+', '-']);
    let t = t
        .strip_prefix('P')
        .ok_or_else(|| format!("bad duration '{v}'"))?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in t.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            u => {
                let n: i64 = num.parse().map_err(|_| format!("bad duration '{v}'"))?;
                num.clear();
                total += n * match (u, in_time) {
                    ('W', false) => 604_800,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return Err(format!("bad duration '{v}'")),
                };
            }
        }
    }
    Ok(total)
}

/// Parse the VEVENTs of an `.ics` text. Problems in one event skip it with an issue.
pub fn parse(text: &str) -> (Vec<IcsEvent>, Vec<Issue>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in unfold(text) {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            cur = Some(Vec::new());
            continue;
        }
        if upper == "END:VEVENT" {
            if let Some(props) = cur.take() {
                match event(&props) {
                    Ok(Some(e)) => out.push(e),
                    Ok(None) => {}
                    Err(m) => issues.push(Issue::warning("ics", m)),
                }
            }
            continue;
        }
        if let Some(c) = cur.as_mut() {
            c.push(line);
        }
    }
    (out, issues)
}

fn event(props: &[String]) -> Result<Option<IcsEvent>, String> {
    let mut uid = None;
    let mut summary = String::new();
    let mut start = None;
    let mut end = None;
    let mut duration = None;
    let mut rrule = None;
    let mut exdates = Vec::new();
    for p in props {
        let (name, params, value) = split_prop(p);
        match name.as_str() {
            "UID" => uid = Some(value),
            "SUMMARY" => summary = unescape(&value),
            "STATUS" if value.eq_ignore_ascii_case("CANCELLED") => return Ok(None),
            "DTSTART" => {
                let tz = params
                    .iter()
                    .find(|(k, _)| k == "TZID")
                    .map(|(_, v)| v.clone());
                let (t, all_day, utc) = parse_dt(&value, &params)?;
                start = Some((t, all_day, if utc { Some("UTC".to_string()) } else { tz }));
            }
            "DTEND" => end = Some(parse_dt(&value, &params)?.0),
            "DURATION" => duration = Some(parse_ics_duration(&value)?),
            "RRULE" => rrule = Some(value),
            "EXDATE" => {
                for v in value.split(',') {
                    exdates.push(parse_dt(v, &params)?.0);
                }
            }
            _ => {}
        }
    }
    let uid = uid.ok_or("a VEVENT without UID was skipped")?;
    let (start, all_day, tz) = start.ok_or_else(|| format!("event {uid}: no DTSTART"))?;
    let duration = match (duration, end) {
        (Some(d), _) => d,
        (None, Some(e)) => (e - start).num_seconds().max(0),
        (None, None) if all_day => 86_400,
        (None, None) => 0,
    };
    Ok(Some(IcsEvent {
        uid,
        summary: if summary.is_empty() {
            "(untitled)".into()
        } else {
            summary
        },
        start,
        tz,
        all_day,
        duration,
        rrule,
        exdates,
    }))
}

/// An entry id for an imported event: `ics-<slug>-<6 hex of the UID>`.
pub fn entry_id(prefix: &str, e: &IcsEvent) -> String {
    let slug: String = e
        .summary
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");
    let h: String = Sha256::digest(e.uid.as_bytes())
        .iter()
        .take(3)
        .map(|b| format!("{b:02x}"))
        .collect();
    let slug = if slug.is_empty() {
        "event".into()
    } else {
        slug
    };
    format!("{prefix}-{}-{h}", &slug[..slug.len().min(40)])
}

/// The events as entries. `tz` reads floating times; a TZID chrono-tz does not know falls back
/// to it with a warning.
pub fn to_entries(
    events: &[IcsEvent],
    prefix: &str,
    source: &str,
    effects: &[Value],
    priority: Priority,
    tz: Tz,
    issues: &mut Vec<Issue>,
) -> IndexMap<String, EntryConfig> {
    let mut out = IndexMap::new();
    for e in events {
        let id = entry_id(prefix, e);
        let zone = match &e.tz {
            Some(z) if z.parse::<Tz>().is_ok() => z.clone(),
            Some(z) => {
                issues.push(Issue::warning(
                    &format!("ics.{id}"),
                    format!("unknown TZID '{z}', read in {}", tz.name()),
                ));
                tz.name().to_string()
            }
            None => tz.name().to_string(),
        };
        let fmt = |t: &NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut c = EntryConfig {
            title: e.summary.clone(),
            owner: None,
            at: None,
            rrule: None,
            start: None,
            cron: None,
            ics: Some(e.uid.clone()),
            duration: Some(crate::dur::format(e.duration.max(60))),
            tz: Some(zone),
            lead: None,
            tail: None,
            detect: None,
            after: None,
            effects: effects.to_vec(),
            priority,
            key: None,
            enabled: true,
            until: None,
            exdates: e.exdates.iter().map(fmt).collect(),
            source: Some(source.to_string()),
        };
        match &e.rrule {
            Some(r) => match recur::Rrule::parse(r) {
                Ok(_) => {
                    c.rrule = Some(r.clone());
                    c.start = Some(fmt(&e.start));
                }
                Err(m) => {
                    issues.push(Issue::warning(
                        &format!("ics.{id}"),
                        format!("{m}; imported as a one-off"),
                    ));
                    c.at = Some(fmt(&e.start));
                }
            },
            None => c.at = Some(fmt(&e.start)),
        }
        out.insert(id, c);
    }
    out
}
