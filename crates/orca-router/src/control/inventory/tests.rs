use super::*;
use crate::control::schema::ControlConfig;
use crate::control::validate::tests::SAMPLE;

fn sample() -> ControlConfig {
    toml::from_str(SAMPLE).expect("sample parses")
}

/// The sample plus a TTS engine (maya, A only) and a TTS model on two engines.
fn with_tts() -> ControlConfig {
    let extra = r#"
[engines.maya]
title = "Maya"
driver = "docker"
image = "local/maya:test"
port = { A = 8200, B = 8201 }
[engines.maya.profiles.one]
lanes = 1
vram_gib = 1.0

[models.voice]
title = "Voice"
kind = "tts"
primary = "maya"
[models.voice.artifacts.ninfer]
file = "voice.ninfer"
vram_gib = 4.0
[models.voice.artifacts.maya]
file = "voice.safetensors"
vram_gib = 6.0
"#;
    let mut c = sample();
    let x: ControlConfig = toml::from_str(&format!("version = 1\n{extra}")).unwrap();
    c.engines.extend(x.engines);
    c.models.extend(x.models);
    c
}

fn tts_too() -> crate::control::schema::EngineCaps {
    crate::control::schema::EngineCaps {
        add: crate::control::schema::CapsDelta {
            kinds: vec![ModelKind::Tts],
            ..Default::default()
        },
        ..Default::default()
    }
}

fn ok_cards(v: &[CardOption]) -> Vec<(String, String, String)> {
    v.iter()
        .filter(|o| o.ok)
        .map(|o| (o.card.clone(), o.engine.clone(), o.profile.clone()))
        .collect()
}

#[test]
fn legacy_cards_allow_every_engine_with_a_port_for_llms() {
    let c = sample();
    let v = place(&c, "dense", &Pin::default());
    // A and B run it on ninfer; C is not managed.
    assert_eq!(ok_cards(&v).len(), 2);
    assert!(
        v.iter()
            .any(|o| o.card == "C" && !o.ok && o.why[0].contains("not managed"))
    );
    // the preset (layout dense-x2: 4lane) is the profile used
    assert!(v.iter().filter(|o| o.ok).all(|o| o.profile == "4lane"));
    assert_eq!(v[0].preset.as_deref(), Some("4lane"));
}

#[test]
fn primary_engine_first_then_the_others() {
    let mut c = with_tts();
    // B allows only ninfer for tts, A allows both
    c.cards["A"].engines = Some([("tts".to_string(), vec!["maya".into(), "ninfer".into()])].into());
    c.cards["B"].engines = Some([("tts".to_string(), vec!["ninfer".into()])].into());
    c.engines["ninfer"].caps = Some(tts_too());
    let v = place(&c, "voice", &Pin::default());
    let a = v.iter().find(|o| o.card == "A").unwrap();
    let b = v.iter().find(|o| o.card == "B").unwrap();
    assert!(a.ok && a.engine == "maya" && a.rank == 0, "{a:?}");
    assert!(b.ok && b.engine == "ninfer" && b.rank == 1, "{b:?}");
    // usable first, primary first
    assert_eq!(v[0].card, "A");
    // switching the primary flips the ranks
    c.models["voice"].primary = Some("ninfer".into());
    let v = place(&c, "voice", &Pin::default());
    assert_eq!(v.iter().find(|o| o.card == "A").unwrap().engine, "ninfer");
}

#[test]
fn model_type_must_match_the_engine_and_the_card() {
    let mut c = with_tts();
    c.cards["A"].engines = Some([("tts".to_string(), vec!["maya".into(), "ninfer".into()])].into());
    // ninfer runs llm only: the tts model can't use it, even where the card allows it
    let v = place(&c, "voice", &Pin::default());
    let a = v.iter().find(|o| o.card == "A").unwrap();
    assert_eq!(a.engine, "maya");
    // B has no allow-list: before the inventory only LLMs ran there
    let b = v.iter().find(|o| o.card == "B").unwrap();
    assert!(!b.ok);
    assert!(
        b.why.iter().any(|w| w.contains("does not allow maya")),
        "{:?}",
        b.why
    );
    // a card that lists an LLM engine for tts only, not llm, refuses LLMs on it
    c.cards["B"].engines = Some([("tts".to_string(), vec!["maya".into()])].into());
    let v = place(&c, "dense", &Pin::default());
    let b = v.iter().find(|o| o.card == "B").unwrap();
    assert!(!b.ok && b.why[0].contains("does not allow ninfer for language models"));
    // "*" allows any engine for that type
    c.cards["B"].engines = Some([("llm".to_string(), vec!["*".into()])].into());
    assert!(
        place(&c, "dense", &Pin::default())
            .iter()
            .any(|o| o.card == "B" && o.ok)
    );
}

#[test]
fn vram_must_fit_and_the_profile_that_fits_is_chosen() {
    let mut c = sample();
    // dense: 22.5 + 4lane 7.5 = 30.0 fits 32 - 1.5 = 30.5; on a 30 GiB card only 2lane (5.0) fits
    c.cards["B"].vram_gib = Some(29.5);
    let v = place(&c, "dense", &Pin::default());
    let b = v.iter().find(|o| o.card == "B").unwrap();
    assert!(b.ok);
    assert_eq!(b.profile, "2lane");
    assert_eq!(b.need_gib, Some(27.5));
    assert_eq!(b.room_gib, Some(28.0));
    // nothing fits on 20 GiB
    c.cards["B"].vram_gib = Some(20.0);
    let v = place(&c, "dense", &Pin::default());
    let b = v.iter().find(|o| o.card == "B").unwrap();
    assert!(!b.ok && b.why[0].contains("GiB"), "{:?}", b.why);
}

#[test]
fn files_must_be_on_the_server_unless_a_restore_brings_them() {
    let mut c = sample();
    c.models["dense"].servers = Some(vec![]);
    let v = place(&c, "dense", &Pin::default());
    assert!(v.iter().all(|o| !o.ok));
    assert!(
        v.iter()
            .any(|o| o.why.iter().any(|w| w.contains("files are not on gpu")))
    );
    c.models["dense"].artifacts["ninfer"].restore = Some(crate::control::schema::Restore {
        from_manifest: "/srv/MANIFEST.tsv".into(),
    });
    let v = place(&c, "dense", &Pin::default());
    assert!(v.iter().filter(|o| o.ok).all(|o| o.restore));
    assert_eq!(ok_cards(&v).len(), 2);
    c.models["dense"].servers = Some(vec!["gpu".into()]);
    assert!(
        place(&c, "dense", &Pin::default())
            .iter()
            .filter(|o| o.ok)
            .all(|o| !o.restore)
    );
}

#[test]
fn a_pin_overrides_the_automatic_rules_but_not_the_hard_ones() {
    let mut c = sample();
    // the card does not list ninfer, and the pinned profile is too big: warnings, still usable
    c.cards["A"].engines = Some([("llm".to_string(), vec!["strata".into()])].into());
    c.cards["A"].vram_gib = Some(20.0);
    let pin = Pin {
        engine: Some("ninfer".into()),
        profile: Some("2lane".into()),
        cards: vec!["A".into(), "B".into()],
    };
    let v = place(&c, "small", &pin);
    let a = v.iter().find(|o| o.card == "A").unwrap();
    assert!(a.ok, "{a:?}");
    assert_eq!(a.profile, "2lane");
    assert!(a.why.iter().any(|w| w.contains("pinned by the route")));
    // hard: the route's cards and the port
    let pin = Pin {
        cards: vec!["B".into()],
        ..pin
    };
    let v = place(&c, "small", &pin);
    assert_eq!(
        ok_cards(&v),
        vec![("B".into(), "ninfer".into(), "2lane".into())]
    );
    c.engines["ninfer"].port = PerCard::Each([("A".to_string(), 1u16)].into());
    let v = place(
        &c,
        "small",
        &Pin {
            cards: vec![],
            ..pin
        },
    );
    assert!(v.iter().find(|o| o.card == "B").unwrap().why[0].contains("no port"));
}

#[test]
fn route_picks_match_the_old_behaviour_for_pinned_routes() {
    let c = sample();
    let od = c.routes["small-model"].on_demand.clone();
    let p = route_picks(&c, "small", od.as_ref());
    assert_eq!(p.keys().cloned().collect::<Vec<_>>(), vec!["A", "B"]);
    assert!(
        p.values()
            .all(|(e, pr, r)| e == "ninfer" && pr == "2lane" && *r == 0)
    );
}

#[test]
fn migration_is_explicit_and_changes_no_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("router.toml");
    let text = SAMPLE.replace(
        "state_dir = \"/state\"",
        &format!(
            "state_dir = {:?}",
            dir.path().to_string_lossy().replace('\\', "/")
        ),
    );
    std::fs::write(&main, &text).unwrap();
    let before = crate::control::load::load(&main).unwrap().config;
    let line = migrate_file(&main);
    assert!(line.contains("migrated"), "{line}");
    let after_text = std::fs::read_to_string(&main).unwrap();
    // comments kept, marker written
    assert!(after_text.contains("# A small control-plane config"));
    assert!(after_text.contains("[inventory]"));
    let after = crate::control::load::load(&main).unwrap().config;
    assert!(migrated(&after));
    assert_eq!(after.models["dense"].kind, Some(ModelKind::Llm));
    assert_eq!(after.models["dense"].primary.as_deref(), Some("ninfer"));
    assert_eq!(after.models["dense"].servers, Some(vec!["gpu".to_string()]));
    // strata loads its own build (no file format): its launch settings keep it, no allow needed
    assert!(after.models["flash"].allow.is_empty());
    assert_eq!(
        crate::control::caps::supported(&after, &after.models["flash"])
            .iter()
            .find(|x| x.engine == "strata")
            .unwrap()
            .via,
        "settings"
    );
    assert!(after.models["dense"].allow.is_empty());
    assert_eq!(
        after.cards["A"].engines.as_ref().unwrap()["llm"],
        vec!["ninfer".to_string(), "strata".to_string()]
    );
    assert!(after.cards["C"].engines.as_ref().unwrap()["llm"].is_empty());
    // same placement for every model, same on-demand picks
    for m in before.models.keys() {
        assert_eq!(
            place(&before, m, &Pin::default()),
            place(&after, m, &Pin::default()),
            "{m}"
        );
    }
    assert_eq!(
        crate::control::startup::on_demand_models(&before),
        crate::control::startup::on_demand_models(&after)
    );
    // the backup holds the old text; a second run does nothing
    let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
        .unwrap()
        .collect();
    assert_eq!(backups.len(), 1);
    let b = backups[0].as_ref().unwrap().path();
    assert_eq!(std::fs::read_to_string(b).unwrap(), text);
    assert_eq!(migrate_file(&main), "");
    // and the old text still loads (old files stay readable)
    assert!(crate::control::load::load_with(&main, text).is_ok());
}

#[test]
fn migration_leaves_included_items_alone() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("router.toml");
    let (head, models) = SAMPLE.split_at(SAMPLE.find("[models.dense]").unwrap());
    let (models, rest) = models.split_at(models.find("[layouts.dense-x2]").unwrap());
    std::fs::write(dir.path().join("catalog.toml"), models).unwrap();
    std::fs::write(&main, format!("include = [\"catalog.toml\"]\n{head}{rest}")).unwrap();
    let text = std::fs::read_to_string(&main).unwrap();
    let new = migrate_text(&main, &text).unwrap().unwrap();
    assert!(!new.contains("[models.dense]"), "{new}");
    let l = crate::control::load::load_with(&main, new).unwrap();
    assert!(l.config.models["dense"].kind.is_none());
    assert!(migrated(&l.config));
}

#[test]
fn auto_routes_get_per_card_picks_and_decide_prefers_the_primary() {
    use crate::ondemand::{CardIn, Decision, decide};
    let mut c = with_tts();
    c.cards["A"].engines = Some([("tts".to_string(), vec!["maya".into()])].into());
    c.cards["B"].engines = Some([("tts".to_string(), vec!["maya".into(), "ninfer".into()])].into());
    c.engines["ninfer"].caps = Some(tts_too());
    c.models["voice"].primary = Some("ninfer".into());
    let r: crate::control::schema::Route = toml::from_str(
        "targets = [{ model = \"voice\" }]\ncold = \"on_demand\"\non_demand = { layout_from = \"auto\" }",
    )
    .unwrap();
    c.routes.insert("voice".into(), r);
    let models = crate::control::startup::on_demand_models(&c);
    let md = &models["voice"];
    assert_eq!(md.per_card["B"].engine, "ninfer");
    assert_eq!(md.per_card["B"].rank, 0);
    assert_eq!(md.per_card["A"].engine, "maya");
    assert_eq!(md.engine, "ninfer");
    let cfg = crate::config::OnDemandConfig {
        enabled: true,
        min_stay_seconds: 0,
        idle_grace_seconds: 0,
        ..Default::default()
    };
    let idle = |lb: f64| CardIn {
        busy: Some(false),
        last_busy: Some(lb),
        ..Default::default()
    };
    let guards = crate::ondemand::Guards {
        at: 1000.0,
        ..Default::default()
    };
    // both idle: B (primary engine) wins even though A has been idle longer
    let cards = [("A".to_string(), idle(1.0)), ("B".to_string(), idle(500.0))].into();
    let d = decide(
        "voice",
        1000.0,
        &cfg,
        &models,
        &cards,
        Some(&guards),
        None,
        false,
        None,
        Some(&[]),
        None,
    );
    match d {
        Decision::Switch { card, reason } => {
            assert_eq!(card, "B");
            assert!(reason.contains("primary"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // B busy: A with maya (the next engine)
    let mut busy_b = idle(999.0);
    busy_b.busy = Some(true);
    let cards = [("A".to_string(), idle(1.0)), ("B".to_string(), busy_b)].into();
    let d = decide(
        "voice",
        1000.0,
        &cfg,
        &models,
        &cards,
        Some(&guards),
        None,
        false,
        None,
        Some(&[]),
        None,
    );
    assert!(
        matches!(&d, Decision::Switch { card, .. } if card == "A"),
        "{d:?}"
    );
    // a lease on A and B busy: wait, never switch
    let leases = vec![("A1".to_string(), "agent".to_string())];
    let mut busy_b = idle(999.0);
    busy_b.busy = Some(true);
    let cards = [("A".to_string(), idle(1.0)), ("B".to_string(), busy_b)].into();
    let d = decide(
        "voice",
        1000.0,
        &cfg,
        &models,
        &cards,
        Some(&guards),
        None,
        false,
        None,
        Some(&leases),
        None,
    );
    assert!(matches!(d, Decision::Wait { .. }), "{d:?}");
    // the per-card pick is what the switch would start
    assert_eq!(md.for_card("A").engine, "maya");
    assert_eq!(md.for_card("A").engine_profile, "one");
}

#[test]
fn a_model_nowhere_to_run_is_refused_not_switched() {
    use crate::ondemand::{Decision, decide};
    let mut c = sample();
    c.models["small"].servers = Some(vec![]);
    let r: crate::control::schema::Route = toml::from_str(
        "targets = [{ model = \"small\" }]\ncold = \"on_demand\"\non_demand = { layout_from = \"auto\" }",
    )
    .unwrap();
    c.routes.insert("small-auto".into(), r);
    let models = crate::control::startup::on_demand_models(&c);
    let cfg = crate::config::OnDemandConfig {
        enabled: true,
        ..Default::default()
    };
    let d = decide(
        "small-auto",
        1000.0,
        &cfg,
        &models,
        &Default::default(),
        None,
        None,
        false,
        None,
        Some(&[]),
        None,
    );
    assert!(
        matches!(&d, Decision::Block { reason, .. } if reason.contains("no card can run")),
        "{d:?}"
    );
    // and validation warns about it
    let issues = crate::control::validate::validate(&c);
    assert!(
        issues
            .iter()
            .any(|i| i.path == "routes.small-auto.on_demand"
                && i.message.contains("no card can run"))
    );
}

#[test]
fn validation_of_the_inventory_fields() {
    let mut c = with_tts();
    c.models["voice"].primary = Some("vllm".into());
    c.models["voice"].servers = Some(vec!["nope".into()]);
    c.cards["A"].engines = Some([("speech".to_string(), vec!["vllm".into()])].into());
    c.servers["gpu"].model_dir = Some("models".into());
    let issues = crate::control::validate::validate(&c);
    let has = |p: &str, m: &str| issues.iter().any(|i| i.path == p && i.message.contains(m));
    assert!(has("models.voice.primary", "can't run it now"));
    assert!(has("models.voice.servers", "no server `nope`"));
    assert!(has("cards.A.engines.speech", "not a model type"));
    assert!(has("cards.A.engines.speech", "not set up yet"));
    assert!(has("servers.gpu.model_dir", "absolute"));
}

#[test]
fn the_view_lists_servers_cards_engines_and_models() {
    let c = with_tts();
    let v = view(&c);
    assert_eq!(v["servers"][0]["id"], "gpu");
    assert_eq!(v["servers"][0]["cards"].as_array().unwrap().len(), 3);
    assert_eq!(
        v["servers"][0]["cards"][0]["engines"]["llm"],
        json!(["ninfer", "strata"])
    );
    assert_eq!(v["servers"][0]["cards"][0]["explicit"], false);
    let voice = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "voice")
        .unwrap();
    assert_eq!(voice["kind"], "tts");
    assert_eq!(voice["primary"], "maya");
    assert_eq!(voice["order"][0], "maya");
    assert_eq!(
        voice["supported"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["engine"] == "maya")
            .unwrap()["reason"],
        "safetensors ✓"
    );
    assert_eq!(voice["files"][0]["format"], "ninfer");
    let small = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "small")
        .unwrap();
    assert_eq!(small["routes"][0]["name"], "small-model");
    assert_eq!(small["pinned"][0]["engine"], "ninfer");
    assert_eq!(v["migrated"], false);
}

/// A real config (not in the repo): `ABHOTH_INVENTORY_LIVE=<dir with router.toml> cargo test
/// -p orca-router --lib live_config_migrates -- --ignored --nocapture`.
#[test]
#[ignore]
fn live_config_migrates() {
    let dir = std::path::PathBuf::from(std::env::var("ABHOTH_INVENTORY_LIVE").unwrap());
    let main = dir.join("router.toml");
    let text = std::fs::read_to_string(&main).unwrap();
    let before = crate::control::load::load_with(&main, text.clone())
        .unwrap()
        .config;
    let l = crate::control::load::load_with(&main, text.clone()).unwrap();
    let main_p = l.main.clone();
    let in_main = |item: &str| l.origin.get(item).is_none_or(|p| *p == main_p);
    let edits = migrate_edits(&before, &in_main);
    let new = crate::control_api::form::apply_edits(&text, &edits).unwrap();
    let after = crate::control::load::load_with(&main, new.clone())
        .unwrap()
        .config;
    let errs: Vec<String> = crate::control::validate::validate(&after)
        .into_iter()
        .filter(|i| i.level == crate::control::validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    let errs0: Vec<String> = crate::control::validate::validate(&before)
        .into_iter()
        .filter(|i| i.level == crate::control::validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    assert_eq!(errs, errs0, "migration adds no validation error");
    for m in before.models.keys() {
        assert_eq!(
            place(&before, m, &Pin::default()),
            place(&after, m, &Pin::default()),
            "{m}"
        );
    }
    let (a, b) = (
        crate::control::startup::on_demand_models(&before),
        crate::control::startup::on_demand_models(&after),
    );
    assert_eq!(a, b);
    for (name, m) in &a {
        println!(
            "{name}: {} / {} on {:?}",
            m.engine,
            m.engine_profile,
            m.per_card.keys().collect::<Vec<_>>()
        );
    }
    println!(
        "{} edits; {} models; {} cards; errors before/after: {}",
        edits.len(),
        after.models.len(),
        after.cards.len(),
        errs.len()
    );
    std::fs::write(dir.join("router.migrated.toml"), new).unwrap();
}
