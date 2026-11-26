//! `calendar import-cron` (SUPERVISOR.md 8.3, 9.7 step 1): TrueNAS cron jobs
//! (`midclt call cronjob.query`, read through a hostctl builtin) as calendar entries.
//!
//! - `run_daily.sh` runs become the pipeline entries (`ai-news`, `game-news`, `car-news`):
//!   effects as the old guards had them (block switches and on-demand on the server, the
//!   15-minute lead that replaces the 06:45 quiet window), live detection of the channel's
//!   process (`both`, grace 10m: the 12:15 and 13:10 runs are covered), `--after` and
//!   `--wait-lock` as `after` sequencing, and a key policy (`chat` + `use_reserved`) without a
//!   key: keys are issued later (9.7 step 2) and `reserve_card` is turned on after that (step 3).
//! - Everything else becomes an `info` entry so the calendar shows the machine's whole
//!   schedule. Disabled jobs are imported disabled.
//! - Commands are never copied into the calendar (they can hold secrets); only the job's id,
//!   description and schedule are, plus what the pipeline detection needs.
//! - **Cron stays what starts the jobs.** The calendar describes and protects.

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::entry::{DetectConfig, DetectMode, EntryConfig, Priority, ProcessDetect};
use crate::keys::KeyPolicy;

/// Durations of the pipelines as planned (observed runs: 07:00 → 07:28/07:35 normally).
fn pipeline_duration(channel: &str) -> &'static str {
    match channel {
        "ai-news" => "90m",
        _ => "60m",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub id: String,
    pub entry: EntryConfig,
    /// what the importer made of it: `pipeline`, `info`
    pub kind: &'static str,
    pub cron_id: i64,
}

fn word_after(argv: &[&str], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| *a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(|s| s.to_string())
}

fn slug(s: &str) -> String {
    let v: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let parts: Vec<&str> = v.split('-').filter(|x| !x.is_empty()).take(6).collect();
    let j = parts.join("-");
    j[..j.len().min(48)].trim_end_matches('-').to_string()
}

/// The schedule `{minute, hour, dom, month, dow}` as a cron line.
pub fn schedule_line(s: &Value) -> Option<String> {
    let f = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("*").to_string();
    s.as_object()?;
    Some(format!(
        "{} {} {} {} {}",
        f("minute"),
        f("hour"),
        f("dom"),
        f("month"),
        f("dow")
    ))
}

/// The server the pipeline entries block (`server:<name>`).
pub fn import(jobs: &Value, server: &str, owner: &str) -> Vec<Imported> {
    let mut out = Vec::new();
    let Some(list) = jobs.as_array() else {
        return out;
    };
    for j in list {
        let id = j.get("id").and_then(Value::as_i64).unwrap_or(0);
        let enabled = j.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let desc = j
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let command = j.get("command").and_then(Value::as_str).unwrap_or("");
        let Some(cron) = j.get("schedule").and_then(schedule_line) else {
            continue;
        };
        if crate::recur::Cron::parse(&cron).is_err() {
            continue;
        }
        let argv: Vec<&str> = command.split_whitespace().collect();
        let pipeline = argv
            .iter()
            .take(3)
            .any(|a| a.rsplit('/').next() == Some("run_daily.sh"));
        let base = EntryConfig {
            title: if desc.is_empty() {
                format!("cron job {id}")
            } else {
                desc.clone()
            },
            owner: Some(owner.to_string()),
            at: None,
            rrule: None,
            start: None,
            cron: Some(cron.clone()),
            ics: None,
            duration: None,
            tz: None,
            lead: None,
            tail: None,
            detect: None,
            after: None,
            effects: vec![json!("info")],
            priority: Priority::Normal,
            key: None,
            enabled,
            until: None,
            exdates: vec![],
            source: Some(format!("cron:{id}")),
        };
        if pipeline {
            let channel = word_after(&argv, "--channel").unwrap_or_else(|| "ai-news".into());
            let after = word_after(&argv, "--after").or_else(|| {
                // `--wait-lock` without `--after`: it queues behind AI News on the run lock.
                (argv.contains(&"--wait-lock") && channel != "ai-news")
                    .then(|| "ai-news".to_string())
            });
            let srv = format!("server:{server}");
            let mut e = base;
            e.title = match channel.as_str() {
                "ai-news" => "AI News daily pipeline".into(),
                "game-news" => "Video Game News daily pipeline".into(),
                "car-news" => "Car News weekly pipeline (Sundays)".into(),
                c => format!("{c} pipeline"),
            };
            e.duration = Some(pipeline_duration(&channel).into());
            if channel == "ai-news" {
                // The 06:45-07:30 quiet window becomes this lead.
                e.lead = Some("15m".into());
            }
            e.detect = Some(DetectConfig {
                process: Some(ProcessDetect {
                    argv_any: "run_daily.sh".into(),
                    label_arg: Some("--channel".into()),
                    label: Some(channel.clone()),
                    label_default: Some("ai-news".into()),
                }),
                mode: DetectMode::Both,
                grace: Some("10m".into()),
                ..Default::default()
            });
            e.after = after;
            e.effects = vec![
                json!({"block_switch": srv}),
                json!({"block_on_demand": srv}),
            ];
            e.key = Some(KeyPolicy::default());
            out.push(Imported {
                id: channel,
                entry: e,
                kind: "pipeline",
                cron_id: id,
            });
        } else {
            let mut e = base;
            e.duration = Some("1m".into());
            let mut name = slug(&desc);
            if name.is_empty() {
                name = format!("cron-{id}");
            }
            out.push(Imported {
                id: format!("cron-{id}-{name}")[..format!("cron-{id}-{name}").len().min(60)]
                    .trim_end_matches('-')
                    .to_string(),
                entry: e,
                kind: "info",
                cron_id: id,
            });
        }
    }
    // A pipeline's `after` must name an imported pipeline; else drop it.
    let ids: Vec<String> = out.iter().map(|i| i.id.clone()).collect();
    for i in out.iter_mut() {
        if i.entry.after.as_ref().is_some_and(|a| !ids.contains(a)) {
            i.entry.after = None;
        }
    }
    out
}

/// The imported entries as an `IndexMap` for `[calendar.entries]`.
pub fn as_entries(v: &[Imported]) -> IndexMap<String, EntryConfig> {
    v.iter().map(|i| (i.id.clone(), i.entry.clone())).collect()
}
