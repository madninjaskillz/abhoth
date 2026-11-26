//! Golden discovery and state payloads, and ha-bridge parity.
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p orca-homeassistant --test golden`.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use orca_homeassistant::discovery::{self, DiscoveryOptions, LEGACY_MAP};
use orca_homeassistant::state::{self, LocalState};
use orca_homeassistant::topics::Topics;
use serde_json::{Value, json};

fn golden(name: &str, actual: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    let text = serde_json::to_string_pretty(actual).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {}; run with UPDATE_GOLDEN=1", path.display()));
    let want: Value = serde_json::from_str(&want).unwrap();
    assert_eq!(&want, actual, "{name} differs from the golden file (UPDATE_GOLDEN=1 to accept)");
}

fn opts(legacy: bool) -> DiscoveryOptions {
    DiscoveryOptions::from_config(&common::mqtt_config(1883, legacy), "0.16.6")
}

fn as_json(msgs: &[discovery::DiscoveryMessage]) -> Value {
    Value::Array(msgs.iter().map(|m| json!({"topic": m.topic, "payload": m.payload})).collect())
}

#[test]
fn discovery_golden() {
    let t = Topics::new("abhoth", "homeassistant", "deepthought");
    let snap = common::deepthought();
    golden("discovery_legacy.json", &as_json(&discovery::build(&t, &opts(true), &snap)));
    golden("discovery_new.json", &as_json(&discovery::build(&t, &opts(false), &snap)));
}

#[test]
fn state_golden() {
    let t = Topics::new("abhoth", "homeassistant", "deepthought");
    let local = LocalState { layout_target: Some("merged-inkling-x2".into()), last_command: None, clock_cap_ttl_seconds: 3600 };
    let msgs = state::build(&t, &common::deepthought(), &local);
    let v = Value::Array(
        msgs.iter()
            .map(|m| json!({"topic": m.topic, "payload": serde_json::from_str::<Value>(&m.payload).unwrap_or(json!(m.payload))}))
            .collect(),
    );
    golden("state.json", &v);
}

/// Every entity ha-bridge publishes (recorded read-only from the broker, 2026-11-26).
const HA_BRIDGE: &[(&str, &str)] = &[
    ("sensor", "deepthought_gpu_temp"),
    ("sensor", "deepthought_hbm_temp"),
    ("sensor", "deepthought_power"),
    ("sensor", "deepthought_sm_clock"),
    ("sensor", "deepthought_gpu_util"),
    ("sensor", "deepthought_vram_gb"),
    ("sensor", "deepthought_tps"),
    ("sensor", "deepthought_model"),
    ("sensor", "deepthought_llm_profile"),
    ("sensor", "deepthought_llm_card_a"),
    ("sensor", "deepthought_llm_card_b"),
    ("sensor", "deepthought_llm_switch"),
    ("binary_sensor", "deepthought_bench_queue_running"),
    ("select", "deepthought_llm_target_profile"),
    ("button", "deepthought_llm_apply_profile"),
    ("select", "deepthought_maxclock"),
];

#[test]
fn ha_bridge_parity() {
    let t = Topics::new("abhoth", "homeassistant", "deepthought");
    let snap = common::deepthought();
    let msgs = discovery::build(&t, &opts(true), &snap);
    let retired = discovery::retired_topics(&t, &common::mqtt_config(1883, true));
    for (comp, uid) in HA_BRIDGE {
        let topic = format!("homeassistant/{comp}/{uid}/config");
        let m = msgs.iter().find(|m| m.unique_id == *uid);
        match m {
            Some(m) => {
                // Same component and topic, so the new config replaces ha-bridge's in place.
                assert_eq!(m.topic, topic);
                assert_eq!(m.payload["object_id"], *uid, "{uid} keeps its object id");
                assert_eq!(m.payload["default_entity_id"], format!("{comp}.{uid}"));
            }
            None => assert!(retired.contains(&topic), "{uid} is neither replaced nor retired"),
        }
        assert!(LEGACY_MAP.iter().any(|(k, _)| k == uid), "{uid} missing from LEGACY_MAP");
    }
    // Legacy GPU sensors live on card B (the only card ha-bridge saw), the per-card LLM
    // sensors on their own cards, and the server device keeps ha-bridge's identifier.
    let by = |uid: &str| msgs.iter().find(|m| m.unique_id == uid).unwrap().payload.clone();
    assert_eq!(by("deepthought_gpu_temp")["device"]["name"], "DeepThought card B");
    assert_eq!(by("deepthought_gpu_temp")["state_topic"], "abhoth/deepthought/card/b/state");
    assert_eq!(by("deepthought_llm_card_a")["device"]["name"], "DeepThought card A");
    assert_eq!(by("deepthought_tps")["device"]["identifiers"], json!(["deepthought", "abhoth_deepthought"]));
    // Unique ids are unique.
    let ids: BTreeSet<_> = msgs.iter().map(|m| m.unique_id.clone()).collect();
    assert_eq!(ids.len(), msgs.len());
    // Without legacy ids nothing reuses a deepthought_* id (safe to run beside ha-bridge).
    let fresh = discovery::build(&t, &opts(false), &snap);
    assert!(fresh.iter().all(|m| m.unique_id.starts_with("abhoth_deepthought_")));
    assert!(discovery::retired_topics(&t, &common::mqtt_config(1883, false)).is_empty());
}

#[test]
fn controls_follow_scopes_and_pinning() {
    let t = Topics::new("abhoth", "homeassistant", "deepthought");
    let snap = common::deepthought();
    let msgs = discovery::build(&t, &opts(false), &snap);
    let has = |uid: &str| msgs.iter().any(|m| m.unique_id == uid);
    assert!(has("abhoth_deepthought_card_b_clock_cap"));
    assert!(!has("abhoth_deepthought_card_a_clock_cap"), "no clock cap on pinned card A");
    assert!(has("abhoth_deepthought_card_a_online"));
    assert!(has("abhoth_deepthought_fan_mode"));
    let mut o = opts(false);
    o.scopes.retain(|s| s != "clock_cap" && s != "switch");
    let msgs = discovery::build(&t, &o, &snap);
    assert!(!msgs.iter().any(|m| m.unique_id.ends_with("_clock_cap") || m.unique_id.ends_with("layout_apply")));
    o.controls = false;
    let msgs = discovery::build(&t, &o, &snap);
    assert!(msgs.iter().all(|m| m.payload.get("command_topic").is_none()));
    // Card entities need both the server and the card to be available.
    let core = discovery::build(&t, &opts(false), &snap).into_iter().find(|m| m.unique_id == "abhoth_deepthought_card_d_core_temp").unwrap();
    assert_eq!(core.payload["availability_mode"], "all");
    assert_eq!(core.payload["availability"][1]["topic"], "abhoth/deepthought/card/d/availability");
}
