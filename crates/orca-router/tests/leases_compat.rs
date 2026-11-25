//! The lease shim against pig-slots' real API: `fixtures/pigslots-golden.json` is a transcript
//! recorded from pig-slots' own `server.py` (run locally, never against the GPU host) by playing
//! claims, renewals, releases, a freeze, a partial withdrawal, a new slot list and expiry. Every
//! step is replayed (1) against the lease table with the recorded clock and (2) over HTTP against
//! the router's `:8099` listener, whose slot list follows the same layout file.

mod common;

use common::*;
use orca_router::leases::{LeaseTable, SlotView};
use orca_router::server::Mode;
use serde_json::{Map, Value, json};

fn golden() -> Value {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pigslots-golden.json"
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Steps whose answer differs on purpose (error paths no client takes): (path, recorded status).
fn known_difference(step: &Value) -> Option<(u16, Value)> {
    // A JSON array body: pig-slots crashes into a 500 with a Python message, we answer 400.
    if step.get("raw").and_then(Value::as_str) == Some("[1,2]") {
        return Some((400, json!({"error": "bad json"})));
    }
    None
}

/// Equal, except `age_s` / `idle_s` may differ by one second (they are truncated wall-clock
/// differences, and the recording's clock is not ours).
fn same(want: &Value, got: &Value, path: &str) -> Result<(), String> {
    match (want, got) {
        (Value::Object(a), Value::Object(b)) => {
            let ka: Vec<&String> = a.keys().collect();
            let mut kb: Vec<&String> = b.keys().collect();
            let mut ka2 = ka.clone();
            ka2.sort();
            kb.sort();
            if ka2 != kb {
                return Err(format!("{path}: keys {ka2:?} != {kb:?}"));
            }
            for (k, va) in a {
                let vb = &b[k];
                if (k == "age_s" || k == "idle_s") && va.is_i64() && vb.is_i64() {
                    if (va.as_i64().unwrap() - vb.as_i64().unwrap()).abs() > 1 {
                        return Err(format!("{path}.{k}: {va} vs {vb}"));
                    }
                    continue;
                }
                same(va, vb, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                same(x, y, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        // pig-slots' TTL is a float (90.0); serde writes 3.0 as 3.0 too, but compare numerically
        (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => Ok(()),
        _ if want == got => Ok(()),
        _ => Err(format!("{path}: {want} vs {got}")),
    }
}

fn body_of(step: &Value) -> Result<Map<String, Value>, ()> {
    if let Some(raw) = step.get("raw") {
        if raw.get("big_body_bytes").is_some() {
            return Ok(Map::new()); // >= 64 KiB: both treat it as {}
        }
        return match serde_json::from_str::<Value>(raw.as_str().unwrap()) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(()),
        };
    }
    Ok(step
        .get("body")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

#[test]
fn lease_table_replays_the_pig_slots_transcript() {
    let g = golden();
    let ttl = g["env"]["TTL"].as_f64().unwrap();
    let t0 = 1_800_000_000.0;
    let cards = vec!["A".to_string(), "B".to_string()];
    let mut view = SlotView {
        slots: ["A1", "A2", "A3", "B1", "B2"].map(String::from).to_vec(),
        cards: cards.clone(),
        ..SlotView::default()
    };
    let mut t = LeaseTable::default();
    let mut checked = 0;
    for step in g["steps"].as_array().unwrap() {
        let now = t0 + step["t"].as_f64().unwrap();
        if let Some(lf) = step.get("layout_file") {
            view = SlotView {
                slots: lf["slots"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect(),
                frozen: lf["frozen"].as_bool().unwrap(),
                layout: lf["layout"].as_str().map(String::from),
                reason: lf["reason"].as_str().map(String::from),
                cards: cards.clone(),
            };
            continue;
        }
        let Some(method) = step.get("method").and_then(Value::as_str) else {
            continue;
        };
        let path = step["path"].as_str().unwrap().split('?').next().unwrap();
        let (code, got): (u16, Value) = match (method, path) {
            ("GET", "/health") => continue, // the HTTP replay covers it
            ("GET", "/list" | "/") => (200, t.list(&view, now, ttl).0),
            ("POST", p) => match body_of(step) {
                Err(()) => (400, json!({"error": "bad json"})),
                Ok(b) => match p {
                    "/claim" => {
                        let (c, v, _) = t.claim(&view, &b, now, ttl, None);
                        (c, v)
                    }
                    "/renew" => {
                        let (c, v, _) = t.renew(&b, now, ttl);
                        (c, v)
                    }
                    "/release" => {
                        let (c, v, _) = t.release(&b);
                        (c, v)
                    }
                    _ => (404, json!({"error": "not found"})),
                },
            },
            _ => (404, json!({"error": "not found"})),
        };
        let (want_code, want) = known_difference(step).unwrap_or((
            step["status"].as_u64().unwrap() as u16,
            step["response"].clone(),
        ));
        assert_eq!(code, want_code, "status of {step}");
        if let Err(e) = same(&want, &got, "") {
            panic!("step {step}\n  {e}\n  got {got}");
        }
        checked += 1;
    }
    assert!(checked > 40, "only {checked} steps replayed");
}

/// The same transcript over HTTP against the `:8099` compatibility listener, with the layout
/// file written as pig-slots' was (expiry steps use the real clock, so the TTL is the recorded
/// one: a 3 s test).
#[tokio::test]
async fn pig_slots_listener_replays_the_transcript() {
    let g = golden();
    let dir = tempfile::tempdir().unwrap();
    let a = Fake::ok("A").await;
    let b = Fake::ok("B").await;
    let mut cfg = two_cards(&a.url(), &b.url(), None, dir.path());
    // pig-slots' env list before any layout file: 3 lanes on A, 2 on B
    cfg.upstreams.get_mut("A").unwrap().lanes = 3;
    cfg.upstreams.get_mut("B").unwrap().lanes = 2;
    cfg.slots.enabled = true;
    cfg.slots.ttl_seconds = g["env"]["TTL"].as_f64().unwrap();
    let layout_file = dir.path().join("layout.json");
    cfg.slots.layout_file = Some(layout_file.to_string_lossy().to_string());
    cfg.slots.state_file = Some(dir.path().join("leases.json").to_string_lossy().to_string());
    let r = start_router(cfg, None, Mode::Slots).await;
    let c = http();
    let start = std::time::Instant::now();
    let mut checked = 0;
    for step in g["steps"].as_array().unwrap() {
        let at = std::time::Duration::from_secs_f64(step["t"].as_f64().unwrap());
        if let Some(lf) = step.get("layout_file") {
            // a fresh mtime per write, as pig-slots re-reads on a change
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            std::fs::write(&layout_file, lf.to_string()).unwrap();
            continue;
        }
        if step.get("sleep_until").is_some() {
            if let Some(d) = at.checked_sub(start.elapsed()) {
                tokio::time::sleep(d).await;
            }
            continue;
        }
        let method = step["method"].as_str().unwrap();
        let url = format!("{}{}", r.url, step["path"].as_str().unwrap());
        let resp = if method == "GET" {
            c.get(&url).send().await.unwrap()
        } else {
            let raw: Vec<u8> = match step.get("raw") {
                Some(Value::String(s)) => s.as_bytes().to_vec(),
                Some(o) if o.get("big_body_bytes").is_some() => {
                    format!("{{\"key\": \"{}\"}}", "k".repeat(70_000)).into_bytes()
                }
                _ => step
                    .get("body")
                    .map(|b| b.to_string().into_bytes())
                    .unwrap_or_default(),
            };
            c.post(&url)
                .header("content-type", "application/json")
                .body(raw)
                .send()
                .await
                .unwrap()
        };
        let code = resp.status().as_u16();
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "{step}"
        );
        let mut got: Value = resp.json().await.unwrap();
        if step["path"] == "/health" {
            // the router adds which implementation answers; pig-slots' fields are unchanged
            assert_eq!(got["impl"], "abhoth-router");
            got.as_object_mut().unwrap().remove("impl");
        }
        let (want_code, want) = known_difference(step).unwrap_or((
            step["status"].as_u64().unwrap() as u16,
            step["response"].clone(),
        ));
        assert_eq!(code, want_code, "status of {step}");
        if let Err(e) = same(&want, &got, "") {
            panic!("step {step}\n  {e}\n  got {got}");
        }
        checked += 1;
    }
    assert!(checked > 40);
    // leases.json keeps pig-slots' shape: {slot: {key, owner, host, pid, label, kind, since, renewed}}
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("leases.json")).unwrap())
            .unwrap();
    for (_, l) in saved.as_object().unwrap() {
        let mut k: Vec<&String> = l.as_object().unwrap().keys().collect();
        k.sort();
        assert_eq!(
            k,
            [
                "host", "key", "kind", "label", "owner", "pid", "renewed", "since"
            ]
        );
    }
}
