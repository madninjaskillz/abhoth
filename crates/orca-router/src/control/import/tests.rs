//! Importer tests on a small synthetic copy of the host's files (placeholders only).

use serde_json::json;

use super::*;
use crate::control::{resolve, validate};

const LAYOUTCTL: &str = r#"
import os
AI = "/srv/ai"
QFN = AI + "/qfn"
NINFER = AI + "/ninfer"
LLAMA_MODELS = AI + "/llamacpp-models"
LLAMA_SLOTS = AI + "/layouts/llama-slots"
KEY_FILE = QFN + "/api-key"
MUSICMODE = "/run/musicmode.state"
QUIET = ((6, 45), (7, 30))
CARDS = {
    "A": {"uuid": "GPU-00000000-0000-0000-0000-00000000000a", "label": "card A", "cpus": "0-3",
          "ninfer": {"port": 8094, "container": "ninfer-a"},
          "strata": {"port": 8080, "container": "qfn-strata", "service": "strata-a", "cfg": QFN + "/sharedarena/strata-sa-a.json"}},
    "B": {"uuid": "GPU-00000000-0000-0000-0000-00000000000b", "label": "card B", "cpus": "4-7",
          "ninfer": {"port": 8096, "container": "ninfer-b"},
          "strata": {"port": 8091, "container": "qfn-strata-b", "service": "strata-b", "cfg": QFN + "/sharedarena/strata-sa-b.json"}},
}
MODEL_IDS = {"strata": "flash-next", "ninfer": "dense-27b"}
HEALTH_OK = {"strata": '"loaded": true', "ninfer": "ok"}
NINFER_IMAGE = "local/ninfer-build:test"
NINFER_DEFAULTS = (("max-context", 131072), ("max-concurrency", "slots"), ("kv-dtype", "int8"), ("lm-head-draft", True))
NINFER_FIXED = ("host", "port", "api-key", "model-id", "max-concurrency")
NINFER_BUILDS = {"stock": NINFER + "/src/build/apps"}
"#;

fn sources() -> Sources {
    Sources {
        layoutctl: LAYOUTCTL.into(),
        profiles: json!({
            "version": 3,
            "server": {"host": "gpu-host", "ninferModelDir": "/srv/ai/ninfer/models", "estimates": {"ninfer": 75, "strataA": 70, "strataB": 80},
                       "ninferBuilds": {"merged": "/srv/ai/ninfer/builds/merged"}},
            "engines": {"ninfer": {"title": "ninfer"}, "strata": {"title": "Strata"}},
            "engineProfiles": {
                "ninfer-4lane": {"engine": "ninfer", "title": "4 lanes", "slots": 4, "vramGiB": 7.5, "build": "merged", "mems": "1",
                                 "options": {"host-kv-mib": null, "vision": true}, "env": {"X_MODE": "on"}},
                "strata-3": {"engine": "strata", "title": "3 lanes", "slots": 3, "vramGiB": 6}
            },
            "models": {
                "dense": {"title": "Dense", "artifacts": {"ninfer": {"file": "dense.ninfer", "id": "dense-27b", "vramGiB": 20, "engineProfiles": ["ninfer-4lane"]}}},
                "flash": {"title": "Flash", "vision": false, "artifacts": {"strata": {"id": "flash-next", "vramGiB": 24, "engineProfiles": ["strata-3"]}}}
            },
            "profiles": {
                "dense-x2": {"title": "Dense x2", "cards": {"A": {"engine": "ninfer", "engineProfile": "ninfer-4lane", "model": "dense"},
                                                           "B": {"engine": "ninfer", "engineProfile": "ninfer-4lane", "model": "dense"}},
                             "tuning": {"slots.registryUrl": "http://host:1/x", "lead": 1}},
                "flash-x2": {"title": "Flash x2", "cards": {"A": {"engine": "strata", "engineProfile": "strata-3", "model": "flash"},
                                                           "B": {"engine": "strata", "engineProfile": "strata-3", "model": "flash"}}},
                "claude": {"title": "Claude bridge", "mode": "bridge"}
            },
            "aliases": {"dense": "dense-x2"},
            "layoutAliases": {}
        }),
        gateway: json!({
            "aliases": ["dense-27b", "jarvis-brain"],
            "pinned": {"flash-next-a": "A", "flash-next-b": "B"},
            "litellmParams": {"reasoning_effort": "low"},
            "router_settings": {"timeout": 3600, "cooldown_time": 10},
            "engineParams": {"ninfer": {"additional_drop_params": ["response_format"], "reasoning_effort": null}},
            "effortLevels": {"ninfer": ["none", "low", "medium", "xhigh"]},
            "thinkingOffEffort": {"ninfer": "drop"},
            "onDemand": {"enabled": false, "picks": {}, "exclude": []}
        }),
        live: Some(json!({"layout": "dense-x2"})),
        offline: Some(json!({})),
        ..Default::default()
    }
}

#[test]
fn imports_a_valid_config() {
    let im = import(&sources(), &Options::default()).unwrap();
    let issues = validate::validate(&im.config);
    let errors: Vec<_> = issues
        .iter()
        .filter(|i| i.level == validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:#?}\n{}", im.toml);
    // The TOML round-trips through the loader.
    let back =
        crate::control::load::load_with(std::path::Path::new("router.toml"), im.toml.clone())
            .unwrap();
    assert_eq!(back.config.layouts.len(), 2);
    assert_eq!(back.config.client_profiles.len(), 1);
    assert_eq!(im.seed.active_layout.as_deref(), Some("dense-x2"));
    // Host addresses in generated client lists are dropped.
    assert!(
        !im.config.layouts["dense-x2"]
            .client
            .contains_key("slots.registryUrl")
    );
    // No key material, only references.
    assert!(im.toml.contains("secret:engine_key"));
    assert!(im.report.iter().any(|r| r.kind == "todo"));
    // Nulls become `unset`.
    assert_eq!(
        im.config.engines["ninfer"].profiles["ninfer-4lane"].unset,
        vec!["host-kv-mib"]
    );
}

#[test]
fn imported_argv_matches_layoutctl_shape() {
    let mut keys = BTreeMap::new();
    keys.insert("ninfer".to_string(), KeyChoice::Argv);
    let im = import(
        &sources(),
        &Options {
            keys,
            ..Default::default()
        },
    )
    .unwrap();
    let spec = CardSpec {
        engine: "ninfer".into(),
        profile: "ninfer-4lane".into(),
        model: "dense".into(),
        devices: Vec::new(),
    };
    let r = resolve::resolve(&im.config, "A", &spec).unwrap();
    let argv = crate::control::template::masked(&r.docker);
    let line = argv.join(" ");
    assert!(
        line.starts_with(
            "--restart no --runtime=nvidia --cpuset-cpus=0-3 -e NVIDIA_VISIBLE_DEVICES=GPU-"
        ),
        "{line}"
    );
    assert!(
        line.contains("-e X_MODE=on --cpuset-mems=1 --shm-size 2g"),
        "{line}"
    );
    assert!(
        line.contains(
            "-v /srv/ai/ninfer/builds/merged:/app:ro -v /srv/ai/ninfer/models:/models:ro"
        ),
        "{line}"
    );
    assert!(line.contains("--api-key *** --model-id dense-27b --max-context 131072 --max-concurrency 4 --kv-dtype int8 --lm-head-draft --vision --request-log-jsonl /logs/req-layout-a.jsonl"), "{line}");
    assert!(!line.contains("--host-kv-mib"));
    assert!(
        line.contains(r#""host-kv-mib": null"#),
        "the label keeps the null like layoutctl"
    );
}

#[test]
fn on_demand_routes_follow_the_flag() {
    let im = import(&sources(), &Options::default()).unwrap();
    assert_eq!(im.config.routes["dense"].cold, Some(ColdPolicy::Reject));
    let im = import(
        &sources(),
        &Options {
            enable_on_demand: true,
            ..Default::default()
        },
    )
    .unwrap();
    let r = &im.config.routes["dense"];
    assert_eq!(r.cold, Some(ColdPolicy::OnDemand));
    assert_eq!(
        r.on_demand.as_ref().unwrap().pick.as_ref().unwrap().engine,
        "ninfer"
    );
    assert!(im.config.routes["flash-next-a"].targets[1].fallback);
}
