//! Render parity (ROUTER-CONFIG.md 13.1): the config the importer builds from a copy of the GPU
//! host's files must start every layout exactly as `layoutctl.py` does.
//!
//! Needs a read-only copy of the host's files and the harness output, so it only runs when
//! both are given (never against the host itself):
//!
//! ```text
//! python3 -I tools/router-golden/layoutctl_argv.py <copy>/layouts > golden.json
//! ROUTER_GOLDEN_DIR=<copy> ROUTER_GOLDEN_JSON=golden.json cargo test -p orca-router --test import_golden -- --nocapture
//! ```
//!
//! Compared per (layout, card): the full `docker run` argv (key delivery `argv`, the key
//! masked), the `layouts.spec` label, the Strata container started; per (from, to) layout
//! pair: the restart set.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orca_router::control::import::{self, KeyChoice, Options, Sources};
use orca_router::control::{resolve, schema, template};
use orca_router::switch::{self, wiring};
use serde_json::Value;

#[test]
fn imported_config_renders_like_layoutctl() {
    let (Some(dir), Some(golden)) = (
        std::env::var_os("ROUTER_GOLDEN_DIR"),
        std::env::var_os("ROUTER_GOLDEN_JSON"),
    ) else {
        eprintln!("skipped: set ROUTER_GOLDEN_DIR and ROUTER_GOLDEN_JSON");
        return;
    };
    let src = Sources::from_dir(&PathBuf::from(dir)).expect("sources");
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(golden).unwrap()).unwrap();
    let mut keys = BTreeMap::new();
    for e in ["ninfer", "llamacpp", "strata"] {
        keys.insert(e.to_string(), KeyChoice::Argv);
    }
    let im = import::import(
        &src,
        &Options {
            keys,
            ..Default::default()
        },
    )
    .expect("import");
    // Through the written file, as an operator would load it.
    let loaded =
        orca_router::control::load::load_with(std::path::Path::new("router.toml"), im.toml.clone())
            .unwrap_or_else(|e| panic!("the imported router.toml does not load: {e:?}"));
    let cfg = &loaded.config;
    let errors: Vec<String> = orca_router::control::validate::validate(cfg)
        .into_iter()
        .filter(|i| i.level == orca_router::control::validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    assert!(errors.is_empty(), "imported config has errors: {errors:#?}");

    let mut checked = 0;
    let mut diffs: Vec<String> = Vec::new();
    for (layout, cards) in golden["layouts"].as_object().unwrap() {
        let Some(l) = cfg.layouts.get(layout) else {
            diffs.push(format!("{layout}: not imported"));
            continue;
        };
        let resolved = schema::resolve_cards(l);
        for (card, want) in cards.as_object().unwrap() {
            let have = resolved
                .iter()
                .find(|(c, _)| c == card)
                .and_then(|(_, s)| s.clone());
            if want.is_null() {
                if have.is_some() {
                    diffs.push(format!("{layout}/{card}: layoutctl has the card off"));
                }
                continue;
            }
            if let Some(e) = want.get("error") {
                diffs.push(format!("{layout}/{card}: layoutctl failed: {e}"));
                continue;
            }
            let Some(spec) = have else {
                diffs.push(format!("{layout}/{card}: off in the import"));
                continue;
            };
            let r = match resolve::resolve(cfg, card, &spec) {
                Ok(r) => r,
                Err(e) => {
                    diffs.push(format!("{layout}/{card}: {e}"));
                    continue;
                }
            };
            let want_argv: Vec<String> = want["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let have_argv: Vec<String> = if spec.engine == "strata" {
                vec!["docker".into(), "start".into(), r.iref.name.clone()]
            } else {
                let mut a = vec![
                    "docker".to_string(),
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    r.iref.name.clone(),
                ];
                a.extend(template::masked(&r.docker));
                a
            };
            if have_argv != want_argv {
                let at = have_argv
                    .iter()
                    .zip(&want_argv)
                    .position(|(a, b)| a != b)
                    .unwrap_or(have_argv.len().min(want_argv.len()));
                diffs.push(format!(
                    "{layout}/{card} argv differs at word {at}:\n  layoutctl: {}\n  router:    {}",
                    want_argv[at.min(want_argv.len())..]
                        .iter()
                        .take(6)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                    have_argv[at.min(have_argv.len())..]
                        .iter()
                        .take(6)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            if spec.engine != "strata" || want["label"].is_string() {
                let want_label = want["label"].as_str().unwrap_or_default();
                if r.spec_json != want_label {
                    diffs.push(format!(
                        "{layout}/{card} label:\n  layoutctl: {want_label}\n  router:    {}",
                        r.spec_json
                    ));
                }
            }
            checked += 1;
        }
    }

    // Restart sets over every layout pair.
    let server = &im.seed.server;
    let sc = wiring::switch_config(cfg, server);
    let mut pairs = 0;
    let mut order_only = 0;
    for (k, want) in golden["restart_set"].as_object().unwrap() {
        let (a, b) = k.split_once('>').unwrap();
        let (Some(sa), Some(sb)) = (
            wiring::target_spec(cfg, server, a, &[]),
            wiring::target_spec(cfg, server, b, &[]),
        ) else {
            diffs.push(format!("restart_set {k}: layout missing"));
            continue;
        };
        let have = switch::restart_set(&sc, &sa, &sb);
        let want: Vec<String> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        // Membership is what must match: layoutctl lists cards in the order its rules add them,
        // the orchestrator in card order and plans the stop/start order itself (5.5).
        let (mut hs, mut ws) = (have.clone(), want.clone());
        hs.sort();
        ws.sort();
        if hs != ws {
            diffs.push(format!(
                "restart_set {k}: layoutctl {want:?}, router {have:?}"
            ));
        } else if have != want {
            order_only += 1;
        }
        pairs += 1;
    }
    eprintln!(
        "golden: {checked} (layout, card) starts and {pairs} layout pairs compared, {} differences ({order_only} restart sets differ in order only)",
        diffs.len()
    );
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
