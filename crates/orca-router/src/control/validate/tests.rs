use super::*;
use crate::control::load;

pub(crate) const SAMPLE: &str = include_str!("../../../tests/fixtures/control/sample.toml");

pub(crate) fn sample() -> ControlConfig {
    toml::from_str(SAMPLE).expect("sample parses")
}

fn errors(c: &ControlConfig) -> Vec<Issue> {
    validate(c)
        .into_iter()
        .filter(|i| i.level == Level::Error)
        .collect()
}

fn has(c: &ControlConfig, path: &str, needle: &str) -> bool {
    validate(c)
        .iter()
        .any(|i| i.path == path && i.message.contains(needle))
}

#[test]
fn sample_is_valid() {
    let e = errors(&sample());
    assert!(e.is_empty(), "{e:#?}");
}

#[test]
fn dangling_references() {
    let mut c = sample();
    c.cards.get_mut("A").unwrap().server = "nope".into();
    assert!(has(&c, "cards.A.server", "no server `nope`"));

    let mut c = sample();
    if let LayoutCard::Spec(s) = c
        .layouts
        .get_mut("dense-x2")
        .unwrap()
        .cards
        .get_mut("A")
        .unwrap()
    {
        s.profile = "9lane".into();
    }
    assert!(has(
        &c,
        "layouts.dense-x2.cards.A.profile",
        "no profile `9lane`"
    ));

    let mut c = sample();
    c.routes.get_mut("dense-27b").unwrap().targets[0] = Target {
        model: Some("ghost".into()),
        ..Default::default()
    };
    assert!(has(
        &c,
        "routes.dense-27b.targets.0.model",
        "no catalog model"
    ));

    let mut c = sample();
    c.engines.get_mut("ninfer").unwrap().key = Some("secret:missing".into());
    assert!(has(&c, "engines.ninfer.key", "not defined in [secrets]"));
}

#[test]
fn duplicate_uuid_and_bad_uuid() {
    let mut c = sample();
    c.cards.get_mut("B").unwrap().gpu_uuid = c.cards["A"].gpu_uuid.clone();
    assert!(has(&c, "cards.B.gpu_uuid", "same GPU as card `A`"));
    let mut c = sample();
    c.cards.get_mut("B").unwrap().gpu_uuid = "1".into();
    assert!(has(&c, "cards.B.gpu_uuid", "not a GPU UUID"));
}

#[test]
fn vram_over_budget_and_force() {
    let mut c = sample();
    c.models
        .get_mut("dense")
        .unwrap()
        .artifacts
        .get_mut("ninfer")
        .unwrap()
        .vram_gib = 25.0;
    // 25 + 7.5 > 32 - 1.5
    assert!(has(&c, "layouts.dense-x2.cards.A", "needs 32.5 GiB"));
    assert!(has(&c, "layouts.dense-x2.cards.B", "needs 32.5 GiB"));
    c.layouts.get_mut("dense-x2").unwrap().force = true;
    let v = validate(&c);
    let i = v
        .iter()
        .find(|i| i.path == "layouts.dense-x2.cards.A")
        .unwrap();
    assert_eq!(i.level, Level::Warning);
}

#[test]
fn on_demand_never_on_pool_or_card_routes() {
    let mut c = sample();
    let od = c.routes["small-model"].on_demand.clone();
    let r = c.routes.get_mut("dense-27b").unwrap();
    r.cold = Some(ColdPolicy::OnDemand);
    r.on_demand = od;
    assert!(has(
        &c,
        "routes.dense-27b.cold",
        "only allowed on routes whose targets are all catalog models"
    ));

    let mut c = sample();
    c.routes
        .get_mut("small-model")
        .unwrap()
        .on_demand
        .as_mut()
        .unwrap()
        .access = Some("anonymous".into());
    assert!(has(
        &c,
        "routes.small-model.on_demand.access",
        "anonymous callers never switch"
    ));

    let mut c = sample();
    c.routes
        .get_mut("small-model")
        .unwrap()
        .on_demand
        .as_mut()
        .unwrap()
        .cards = vec!["C".into()];
    assert!(has(
        &c,
        "routes.small-model.on_demand.cards",
        "managed with on_demand = true"
    ));
}

#[test]
fn same_as_cycles_and_route_case() {
    let mut c = sample();
    c.routes.insert(
        "x".into(),
        Route {
            same_as: Some("y".into()),
            ..Default::default()
        },
    );
    c.routes.insert(
        "y".into(),
        Route {
            same_as: Some("x".into()),
            ..Default::default()
        },
    );
    assert!(has(&c, "routes.x.same_as", "cycle"));
    let mut c = sample();
    c.routes.insert(
        "Dense-27B".into(),
        Route {
            same_as: Some("dense-27b".into()),
            ..Default::default()
        },
    );
    assert!(has(&c, "routes.Dense-27B", "case-insensitive"));
}

#[test]
fn inline_secrets_are_refused() {
    let mut c = sample();
    c.upstreams.insert(
        "cloud".into(),
        UpstreamDef {
            kind: UpstreamKind::Openai,
            base_url: "https://example.invalid/v1".into(),
            ..Default::default()
        },
    );
    c.upstreams
        .get_mut("cloud")
        .unwrap()
        .headers
        .insert("Authorization".into(), "Bearer abcdefghijklmnop".into());
    assert!(has(
        &c,
        "upstreams.cloud.headers.Authorization",
        "looks like a key"
    ));
    assert!(looks_like_secret("sk-ant-api03-abcdefghijklmnopqrstuvwxyz"));
    assert!(looks_like_secret("abk_0123456789abcdef"));
    assert!(!looks_like_secret("secret:engine_key"));
    assert!(!looks_like_secret(
        "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    ));
    assert!(!looks_like_secret(
        "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1"
    ));
    assert!(!looks_like_secret("/srv/ai/ninfer/builds/merged-20261123"));
}

#[test]
fn litellm_health_probe_refused_and_bridge_reserved() {
    let mut c = sample();
    c.upstreams.insert(
        "legacy".into(),
        UpstreamDef {
            kind: UpstreamKind::Router,
            base_url: "http://gpu-host:8010".into(),
            flavour: Some("litellm".into()),
            health_path: Some("/health".into()),
            ..Default::default()
        },
    );
    c.upstreams.insert(
        "br".into(),
        UpstreamDef {
            kind: UpstreamKind::Bridge,
            enabled: true,
            ..Default::default()
        },
    );
    assert!(has(
        &c,
        "upstreams.legacy.health_path",
        "never probe LiteLLM's /health"
    ));
    assert!(has(&c, "upstreams.br.enabled", "reserved"));
}

#[test]
fn fixed_options_and_bad_hooks() {
    let mut c = sample();
    c.engines
        .get_mut("ninfer")
        .unwrap()
        .profiles
        .get_mut("4lane")
        .unwrap()
        .options
        .insert("port".into(), 1.into());
    assert!(has(
        &c,
        "engines.ninfer.profiles.4lane.options.port",
        "fixed"
    ));
    let mut c = sample();
    c.engines
        .get_mut("strata")
        .unwrap()
        .before_start
        .push(Hook {
            builtin: Some("rm-rf".into()),
            ..Default::default()
        });
    assert!(has(
        &c,
        "engines.strata.before_start.2.builtin",
        "unknown builtin"
    ));
}

#[test]
fn guard_windows_and_trusted_warning() {
    let mut c = sample();
    c.guards.get_mut("quiet").unwrap().windows = vec!["6:45-7:30".into()];
    assert!(has(&c, "guards.quiet.windows", "HH:MM-HH:MM"));
    let mut c = sample();
    c.servers.get_mut("gpu").unwrap().hostctl = HostctlMode::Trusted;
    assert!(
        validate(&c)
            .iter()
            .any(|i| i.level == Level::Warning && i.message.contains("TRUSTED"))
    );
}

#[test]
fn issues_get_file_and_line() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("router.toml");
    std::fs::write(
        &p,
        SAMPLE.replace(
            "profile = \"2lane\", model = \"small\"",
            "profile = \"2lane\", model = \"smal\"",
        ),
    )
    .unwrap();
    let l = load::load(&p).unwrap();
    let issues = validate_loaded(&l);
    let i = issues
        .iter()
        .find(|i| i.path == "layouts.dense-a-small-b.cards.B.model")
        .expect("issue");
    let want = SAMPLE
        .lines()
        .position(|l| l.contains("model = \"small\" }"))
        .unwrap()
        + 1;
    assert_eq!(i.line, Some(want), "{i}");
    assert!(i.to_string().contains("router.toml"));
}
