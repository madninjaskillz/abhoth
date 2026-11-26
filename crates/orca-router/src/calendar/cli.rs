//! `abhoth router calendar ...` (SUPERVISOR.md 8, SV12): checks and imports against a
//! calendar.toml, offline (no router needed).
//!
//! ```text
//! abhoth router calendar check --file calendar.toml [--tz Europe/London] [--days 7]
//!               [--effect block_switch|block_on_demand|block_ensure|reserve_card] [--card B] [--at 2026-11-29T07:10]
//! abhoth router calendar import-cron --file calendar.toml --jobs cronjobs.json [--server deepthought] [--apply]
//! abhoth router calendar import-ics --file calendar.toml --ics events.ics
//!               [--effects '[{"block_switch":"all"}]'] [--priority normal] [--apply]
//! ```
//!
//! `check` exits 1 when the file has errors, or (with `--effect`) when something blocks at
//! `--at` (default now), so a script can ask "may I switch now?". Imports only show what they
//! would add unless `--apply`; applying keeps the file's comments and writes a backup next to it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_calendar::now::BlockAction;
use orca_calendar::{Priority, Safety};
use serde_json::Value;

use super::{CalendarService, now_epoch};
use crate::control::store::Store;

pub const HELP: &str = "abhoth router calendar: check and import a calendar.toml (offline)

USAGE:
  abhoth router calendar check --file calendar.toml [--tz Europe/London] [--days 7]
                [--effect block_switch|block_on_demand|block_ensure|reserve_card] [--card B] [--at LOCAL]
      Errors and warnings, the entries, the next occurrences and overlaps. With --effect: what
      blocks at --at (default now); exit 1 when something does.
  abhoth router calendar import-cron --file calendar.toml --jobs cronjobs.json [--server ID] [--apply]
      TrueNAS cron jobs (the cronjob.query JSON) as entries: the video pipelines with detection
      and blocks, the rest info-only. Without --apply, only shows them.
  abhoth router calendar import-ics --file calendar.toml --ics FILE [--effects JSON] [--priority P] [--apply]
";

struct Args {
    file: Option<PathBuf>,
    tz: String,
    days: i64,
    effect: Option<String>,
    card: Option<String>,
    at: Option<String>,
    jobs: Option<PathBuf>,
    ics: Option<PathBuf>,
    server: String,
    effects: Option<String>,
    priority: Option<String>,
    apply: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        file: None,
        tz: "Europe/London".into(),
        days: 7,
        effect: None,
        card: None,
        at: None,
        jobs: None,
        ics: None,
        server: "deepthought".into(),
        effects: None,
        priority: None,
        apply: false,
    };
    let mut i = 0;
    let take = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i).cloned().ok_or(format!("{flag} needs a value"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--file" => a.file = Some(take(&mut i, "--file")?.into()),
            "--tz" => a.tz = take(&mut i, "--tz")?,
            "--days" => {
                a.days = take(&mut i, "--days")?
                    .parse()
                    .map_err(|_| "--days: a number")?
            }
            "--effect" => a.effect = Some(take(&mut i, "--effect")?),
            "--card" => a.card = Some(take(&mut i, "--card")?),
            "--at" => a.at = Some(take(&mut i, "--at")?),
            "--jobs" => a.jobs = Some(take(&mut i, "--jobs")?.into()),
            "--ics" => a.ics = Some(take(&mut i, "--ics")?.into()),
            "--server" => a.server = take(&mut i, "--server")?,
            "--effects" => a.effects = Some(take(&mut i, "--effects")?),
            "--priority" => a.priority = Some(take(&mut i, "--priority")?),
            "--apply" => a.apply = true,
            x => return Err(format!("unknown argument {x}")),
        }
        i += 1;
    }
    Ok(a)
}

fn open(a: &Args) -> Result<Arc<CalendarService>, String> {
    let file = a.file.clone().ok_or("--file is required")?;
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let store = Arc::new(Store::memory()?);
    CalendarService::open(&a.server, &a.tz, &file, &dir, store, Safety::default())
}

fn when(svc: &CalendarService, t: i64) -> String {
    orca_calendar::recur::to_local(svc.tz(), t)
        .format("%a %Y-%m-%d %H:%M")
        .to_string()
}

/// Runs a subcommand (`args` start after `calendar`); prints to stdout, returns the exit code.
pub fn run(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h" | "help") => {
            print!("{HELP}");
            Ok(0)
        }
        Some("check") => check(&parse(&args[1..])?),
        Some("import-cron") => import_cron(&parse(&args[1..])?),
        Some("import-ics") => import_ics(&parse(&args[1..])?),
        Some(x) => Err(format!("unknown calendar command {x} (see --help)")),
    }
}

fn check(a: &Args) -> Result<i32, String> {
    let svc = open(a)?;
    let mut out = String::new();
    let issues = svc.issues();
    let errors = issues.iter().filter(|i| i.is_error()).count();
    for i in &issues {
        out.push_str(&format!(
            "{}: {}: {}\n",
            if i.is_error() { "error" } else { "warning" },
            i.path,
            i.message
        ));
    }
    let now = match &a.at {
        Some(at) => orca_calendar::recur::parse_local(at)
            .map(|l| orca_calendar::recur::to_utc(svc.tz(), l))?,
        None => now_epoch(),
    };
    if let Some(effect) = &a.effect {
        let action = match effect.as_str() {
            "block_switch" => BlockAction::Switch,
            "block_on_demand" => BlockAction::OnDemand,
            "block_ensure" => BlockAction::Ensure,
            "reserve_card" => BlockAction::Use,
            x => {
                return Err(format!(
                    "--effect {x}: block_switch, block_on_demand, block_ensure or reserve_card"
                ));
            }
        };
        let blocks = svc.check(action, a.card.as_deref(), now);
        print!("{out}");
        if blocks.is_empty() {
            println!("free at {}: nothing blocks {effect}", when(&svc, now));
            return Ok(if errors > 0 { 1 } else { 0 });
        }
        for b in &blocks {
            println!("blocked at {}: {b}", when(&svc, now));
        }
        return Ok(1);
    }
    let entries = svc.entries();
    out.push_str(&format!(
        "{} entries ({} errors, {} warnings), time zone {}\n",
        entries.len(),
        errors,
        issues.len() - errors,
        svc.tz()
    ));
    for e in &entries {
        out.push_str(&format!(
            "  {:<24} {}{}{}  [{}]\n",
            e.id,
            e.schedule,
            e.entry
                .duration
                .as_deref()
                .map(|d| format!(", {d}"))
                .unwrap_or_default(),
            if e.entry.enabled { "" } else { " (disabled)" },
            e.effects_text.join("; ")
        ));
    }
    let to = now + a.days.max(1) * 86_400;
    let occ = svc.occurrences(now, to, now);
    out.push_str(&format!(
        "next {} days: {} occurrences\n",
        a.days,
        occ.len()
    ));
    for o in occ.iter().take(40) {
        out.push_str(&format!(
            "  {}  {:<24} blocks {}-{}{}\n",
            when(&svc, o.start),
            o.entry,
            when(&svc, o.block_start)
                .split(' ')
                .next_back()
                .unwrap_or(""),
            when(&svc, o.block_end).split(' ').next_back().unwrap_or(""),
            if o.skipped { " (skipped)" } else { "" }
        ));
    }
    for ov in svc.overlaps(now, to) {
        out.push_str(&format!("overlap: {}\n", ov.message));
    }
    print!("{out}");
    Ok(if errors > 0 { 1 } else { 0 })
}

fn summary(v: &Value) {
    for f in v["found"].as_array().into_iter().flatten() {
        println!(
            "  {:<28} {:<8} {}{}",
            f["id"].as_str().unwrap_or(""),
            f["kind"].as_str().unwrap_or(""),
            f["title"].as_str().unwrap_or(""),
            if f["exists"] == true {
                "  (already in the calendar)"
            } else {
                ""
            }
        );
    }
}

fn import_cron(a: &Args) -> Result<i32, String> {
    let svc = open(a)?;
    let path = a.jobs.clone().ok_or("--jobs is required")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let jobs: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let v = svc.import_cron(&jobs, "cli", a.apply).map_err(|(_, m)| m)?;
    summary(&v);
    if a.apply {
        println!("written: {}", v["written"]);
    } else {
        if let Some(t) = v["toml"].as_str() {
            print!("\n{t}");
        }
        println!("(dry run: --apply writes them)");
    }
    Ok(0)
}

fn import_ics(a: &Args) -> Result<i32, String> {
    let svc = open(a)?;
    let path = a.ics.clone().ok_or("--ics is required")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let effects: Vec<Value> = match &a.effects {
        Some(j) => serde_json::from_str(j).map_err(|e| format!("--effects: {e}"))?,
        None => vec![Value::String("info".into())],
    };
    let priority: Priority = match &a.priority {
        Some(p) => serde_json::from_value(Value::String(p.clone()))
            .map_err(|_| format!("--priority {p}: low, normal, high or critical"))?,
        None => Priority::Normal,
    };
    let v = svc
        .import_ics(&text, &effects, priority, "cli", a.apply)
        .map_err(|(_, m)| m)?;
    for i in v["issues"].as_array().into_iter().flatten() {
        println!("warning: {}", i["message"].as_str().unwrap_or(""));
    }
    for e in v["entries"].as_array().into_iter().flatten() {
        println!("  {}", e.as_str().unwrap_or(""));
    }
    if !a.apply {
        if let Some(t) = v["toml"].as_str() {
            print!("\n{t}");
        }
        println!("(dry run: --apply writes them)");
    }
    Ok(0)
}
