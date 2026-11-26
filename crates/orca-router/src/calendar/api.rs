//! `/admin/api/calendar/*` (admins; reached through the control plane's admin API, which has
//! already checked the caller): the Calendar page's reads and edits.
//!
//! | Call | What |
//! |---|---|
//! | `GET calendar` | rev, time zone, entries (with their keys), issues, the state now |
//! | `GET calendar/occurrences?from=&to=` | occurrences in a window (epoch seconds) |
//! | `POST calendar/preview` `{id, entry}` | issues, next 10 occurrences, overlaps |
//! | `PUT calendar/entries/<id>` `{entry, rev, reason}` | save to `calendar.toml` |
//! | `DELETE calendar/entries/<id>?rev=` | delete |
//! | `POST calendar/occurrences/<occ>/skip\|cancel\|restore` | one occurrence |
//! | `POST calendar/keys/<event>/issue\|rotate` | a new key, shown once |
//! | `DELETE calendar/keys/<id>` | revoke |
//! | `GET calendar/uses/<event>` | the key's audit |
//! | `POST calendar/import-cron` `{jobs, apply}` / `import-ics` `{text, effects, priority, apply}` | imports |
//! | `POST calendar/bench` `{run, cards, layouts, seconds}` / `calendar/bench/<id>/extend` `{seconds}` / `calendar/bench/<id>/end` | bench protection |
//! | `GET calendar/check?effect=block_switch&card=B` | free or blocked, with reasons |

use orca_calendar::entry::EntryConfig;
use orca_calendar::now::BlockAction;
use serde_json::{Value, json};

use super::CalendarService;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn q<'a>(qs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    qs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..(i + 3).min(b.len())])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn bad(status: u16, msg: impl Into<String>) -> (u16, Value) {
    (status, json!({"error": msg.into()}))
}

fn entry_of(body: &Value) -> Result<EntryConfig, (u16, Value)> {
    serde_json::from_value(body.get("entry").cloned().unwrap_or(Value::Null))
        .map_err(|e| bad(400, format!("entry: {e}")))
}

/// Handle one call; `parts` is the path after `/admin/api/` (`["calendar", ...]`).
pub fn handle(
    svc: &CalendarService,
    method: &str,
    parts: &[&str],
    qs: &[(String, String)],
    body: &Value,
    who: &str,
) -> (u16, Value) {
    let s = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match (method, parts) {
        ("GET", ["calendar"]) => {
            let t = now();
            (
                200,
                json!({
                    "rev": svc.rev(),
                    "tz": svc.tz().name(),
                    "server": svc.server,
                    "entries": svc.entries(),
                    "issues": svc.issues(),
                    "now": svc.now_state(t),
                    "overlaps": svc.overlaps(t, t + 28 * 86_400),
                    "at": t,
                }),
            )
        }
        ("GET", ["calendar", "now"]) => (200, json!(svc.now_state(now()))),
        ("GET", ["calendar", "occurrences"]) => {
            let t = now();
            let from = q(qs, "from")
                .and_then(|x| x.parse().ok())
                .unwrap_or(t - 86_400);
            let to = q(qs, "to")
                .and_then(|x| x.parse().ok())
                .unwrap_or(t + 7 * 86_400);
            if to <= from || to - from > 92 * 86_400 {
                return bad(400, "from/to: a window of at most 92 days");
            }
            (
                200,
                json!({"from": from, "to": to, "at": t, "occurrences": svc.occurrences(from, to, t)}),
            )
        }
        ("POST", ["calendar", "preview"]) => match entry_of(body) {
            Ok(e) => (200, svc.preview(&s("id"), &e)),
            Err(x) => x,
        },
        ("PUT", ["calendar", "entries", id]) => {
            let e = match entry_of(body) {
                Ok(e) => e,
                Err(x) => return x,
            };
            let rev = body.get("rev").and_then(Value::as_str);
            match svc.save_entry(id, Some(&e), rev, who, &s("reason")) {
                Ok(v) => (200, v),
                Err((st, m)) => bad(st, m),
            }
        }
        ("DELETE", ["calendar", "entries", id]) => {
            match svc.save_entry(id, None, q(qs, "rev"), who, "deleted in the UI") {
                Ok(v) => (200, v),
                Err((st, m)) => bad(st, m),
            }
        }
        ("POST", ["calendar", "occurrences", occ, action]) => {
            let occ = pct_decode(occ);
            match svc.occurrence_action(&occ, action, who, &s("reason")) {
                Ok(()) => (200, json!({"ok": true})),
                Err((st, m)) => bad(st, m),
            }
        }
        ("GET", ["calendar", "keys"]) => (200, json!(svc.keys(q(qs, "event")))),
        ("POST", ["calendar", "keys", event, "issue"]) => match svc.issue_key(event, who) {
            Ok((rec, secret)) => (
                201,
                json!({"record": rec, "key": secret, "note": "shown once: store it in a key file (ABHOTH_API_KEY_FILE), never in argv or crontab"}),
            ),
            Err(m) => bad(400, m),
        },
        ("POST", ["calendar", "keys", event, "rotate"]) => match svc.rotate_key(event, who) {
            Ok((rec, secret, retire)) => (
                201,
                json!({"record": rec, "key": secret, "old_retire_at": retire}),
            ),
            Err(m) => bad(400, m),
        },
        ("DELETE", ["calendar", "keys", id]) => match id.parse::<i64>() {
            Ok(id) => match svc.revoke_key(id, who) {
                Ok(true) => (200, json!({"ok": true})),
                Ok(false) => bad(404, "no such live key"),
                Err(m) => bad(500, m),
            },
            Err(_) => bad(400, "a key id"),
        },
        ("GET", ["calendar", "uses", event]) => (200, json!(svc.key_uses(event, 200))),
        ("POST", ["calendar", "import-cron"]) => {
            let apply = body.get("apply").and_then(Value::as_bool).unwrap_or(false);
            match svc.import_cron(body.get("jobs").unwrap_or(&Value::Null), who, apply) {
                Ok(v) => (200, v),
                Err((st, m)) => bad(st, m),
            }
        }
        ("POST", ["calendar", "import-ics"]) => {
            let apply = body.get("apply").and_then(Value::as_bool).unwrap_or(false);
            let effects: Vec<Value> = body
                .get("effects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| vec![json!("info")]);
            let priority = body
                .get("priority")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            match svc.import_ics(&s("text"), &effects, priority, who, apply) {
                Ok(v) => (200, v),
                Err((st, m)) => bad(st, m),
            }
        }
        ("POST", ["calendar", "bench"]) => {
            let list = |k: &str| -> Vec<String> {
                body.get(k)
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let secs = body.get("seconds").and_then(Value::as_i64).unwrap_or(3600);
            match svc.bench_begin(&s("run"), &list("cards"), &list("layouts"), secs, who) {
                Ok((id, key)) => (201, json!({"id": id, "key": key})),
                Err(m) => bad(400, m),
            }
        }
        ("POST", ["calendar", "bench", id, "extend"]) => {
            let secs = body.get("seconds").and_then(Value::as_i64).unwrap_or(3600);
            match svc.bench_extend(id, secs, who) {
                Ok(until) => (200, json!({"id": id, "until": until})),
                Err(m) => bad(404, m),
            }
        }
        ("POST", ["calendar", "bench", id, "end"]) => match svc.bench_end(id, who) {
            Ok(()) => (200, json!({"ok": true})),
            Err(m) => bad(400, m),
        },
        ("GET", ["calendar", "check"]) => {
            let action = match q(qs, "effect").unwrap_or("block_switch") {
                "block_switch" => BlockAction::Switch,
                "block_on_demand" => BlockAction::OnDemand,
                "block_ensure" => BlockAction::Ensure,
                "reserve_card" => BlockAction::Use,
                x => {
                    return bad(
                        400,
                        format!(
                            "effect '{x}': block_switch, block_on_demand, block_ensure or reserve_card"
                        ),
                    );
                }
            };
            let reasons = svc.check(action, q(qs, "card"), now());
            (200, json!({"free": reasons.is_empty(), "reasons": reasons}))
        }
        _ => bad(404, "no such calendar call"),
    }
}
