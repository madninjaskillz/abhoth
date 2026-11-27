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

// ── D153: CPU cards, components ─────────────────────────────────────────────────────────────

const MEDIA: &str = r#"
[cards.CPU]
server = "gpu"
device = "cpu"
label = "EPYC"
cores = 48
ram_gib = 64.0
managed = false

[engines.comfyui]
driver = "template"
port = { C = 8188 }
templates = { start = "true", stop = "true", status = "true" }

[models.qwen-unet]
kind = "image"
role = "diffusion"
[models.qwen-unet.artifacts.comfyui]
file = "qwen-image-2.1-turbo-Q4_K_M.gguf"

[models.qwen-image]
kind = "image"
needs = ["qwen-unet"]
aliases = ["qwen"]
"#;

fn media(extra: &str) -> ControlConfig {
    toml::from_str(&format!("{SAMPLE}\n{MEDIA}\n{extra}")).expect("parses")
}

#[test]
fn cpu_cards_and_components_are_valid() {
    let c = media("");
    let e = errors(&c);
    assert!(e.is_empty(), "{e:#?}");
    assert!(c.cards["CPU"].is_cpu());
    assert_eq!(c.cards["CPU"].memory_gib(), Some(64.0));
    // a CPU card is never one of the cards a switch manages
    assert!(!c.managed_cards("gpu").contains(&"CPU"));
}

#[test]
fn cpu_card_rules() {
    let mut c = media("");
    {
        let cpu = c.cards.get_mut("CPU").unwrap();
        cpu.gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into();
        cpu.on_demand = true;
        cpu.ram_gib = None;
    }
    assert!(has(&c, "cards.CPU.gpu_uuid", "no GPU UUID"));
    assert!(has(&c, "cards.CPU.on_demand", "never part of a layout"));
    assert!(has(&c, "cards.CPU.ram_gib", "set the RAM"));
    let mut c = media("");
    c.cards.get_mut("A").unwrap().ram_gib = Some(8.0);
    assert!(has(&c, "cards.A.ram_gib", "for CPU cards"));
}

#[test]
fn roles_needs_and_aliases_are_checked() {
    use crate::control::schema::ModelKind;
    let mut c = media("");
    c.models.get_mut("qwen-unet").unwrap().role = Some("nope".into());
    assert!(has(&c, "models.qwen-unet.role", "not a component role"));
    let mut c = media("");
    c.models.get_mut("qwen-unet").unwrap().kind = Some(ModelKind::Llm);
    assert!(has(&c, "models.qwen-unet.role", "only image and video"));
    let mut c = media("");
    c.models.get_mut("qwen-image").unwrap().needs = vec!["ghost".into(), "qwen-image".into()];
    assert!(has(&c, "models.qwen-image.needs", "no model `ghost`"));
    assert!(has(&c, "models.qwen-image.needs", "not a component"));
    let mut c = media("");
    c.models.get_mut("qwen-unet").unwrap().aliases = vec!["qwen".into()];
    assert!(has(&c, "models.qwen-unet.aliases", "also an alias"));
    let mut c = media("");
    c.models.get_mut("qwen-image").unwrap().aliases = vec!["qwen-unet".into()];
    assert!(has(&c, "models.qwen-image.aliases", "already a model"));
    let mut c = media("");
    c.servers.get_mut("gpu").unwrap().comfy_root = Some("rel/path".into());
    assert!(has(&c, "servers.gpu.comfy_root", "absolute"));
}

#[test]
fn the_video_kind_is_a_card_engine_kind() {
    let mut c = media("");
    c.cards.get_mut("C").unwrap().engines = Some(
        [("video".to_string(), vec!["comfyui".to_string()])]
            .into_iter()
            .collect(),
    );
    assert!(errors(&c).is_empty(), "{:#?}", errors(&c));
    c.cards.get_mut("C").unwrap().engines = Some(
        [("lora".to_string(), vec!["comfyui".to_string()])]
            .into_iter()
            .collect(),
    );
    assert!(has(&c, "cards.C.engines.lora", "not a model type"));
}

#[test]
fn external_engines_never_sit_on_managed_cards() {
    let mut c = media("");
    let e = c.engines.get_mut("comfyui").unwrap();
    e.driver = DriverKind::External;
    e.templates = None;
    assert!(errors(&c).is_empty(), "{:#?}", errors(&c));
    c.engines.get_mut("comfyui").unwrap().port =
        super::super::schema::PerCard::Each([("A".to_string(), 8188u16)].into_iter().collect());
    assert!(has(&c, "engines.comfyui.port.A", "never observes it"));
    assert!(errors(&c).is_empty());
}

/// The sample plus `dense-tp`: one dense server over A+B (A lists both, B is "held").
pub(crate) fn sample_tp() -> ControlConfig {
    let text = format!(
        "{SAMPLE}\n[layouts.dense-tp]\ntitle = \"Dense over both\"\n\
         cards = {{ A = {{ engine = \"ninfer\", profile = \"4lane\", model = \"dense\", devices = [\"A\", \"B\"] }}, B = \"held\" }}\n"
    );
    toml::from_str(&text).expect("sample + dense-tp parses")
}

#[test]
fn one_instance_over_two_cards() {
    let c = sample_tp();
    let e = errors(&c);
    assert!(e.is_empty(), "{e:#?}");
    // B runs nothing of its own; A's spec carries both cards
    let r = c.resolve_layout("dense-tp").unwrap();
    assert_eq!(r[0].1.as_ref().unwrap().devices, ["A", "B"]);
    assert!(r[1].1.is_none());

    // "held" with nobody holding it
    let mut c = sample_tp();
    if let LayoutCard::Spec(s) = c
        .layouts
        .get_mut("dense-tp")
        .unwrap()
        .cards
        .get_mut("A")
        .unwrap()
    {
        s.devices.clear();
    }
    assert!(has(
        &c,
        "layouts.dense-tp.cards.B",
        "no other card of the layout lists this card"
    ));

    // a held card must be "held", and the own card comes first
    let mut c = sample_tp();
    c.layouts
        .get_mut("dense-tp")
        .unwrap()
        .cards
        .insert("B".into(), LayoutCard::Word("off".into()));
    assert!(has(
        &c,
        "layouts.dense-tp.cards.A.devices",
        "card `B` must be \"held\""
    ));
    let mut c = sample_tp();
    if let LayoutCard::Spec(s) = c
        .layouts
        .get_mut("dense-tp")
        .unwrap()
        .cards
        .get_mut("A")
        .unwrap()
    {
        s.devices = vec!["B".into(), "A".into()];
    }
    assert!(has(
        &c,
        "layouts.dense-tp.cards.A.devices",
        "must start with its own card `A`"
    ));
    // unmanaged or unknown cards cannot be spanned
    let mut c = sample_tp();
    if let LayoutCard::Spec(s) = c
        .layouts
        .get_mut("dense-tp")
        .unwrap()
        .cards
        .get_mut("A")
        .unwrap()
    {
        s.devices = vec!["A".into(), "B".into(), "C".into(), "Z".into()];
    }
    assert!(has(
        &c,
        "layouts.dense-tp.cards.A.devices",
        "card `C` is not a managed GPU card"
    ));
    assert!(has(&c, "layouts.dense-tp.cards.A.devices", "no card `Z`"));
    // an unknown word names "held" among the choices
    let mut c = sample_tp();
    c.layouts
        .get_mut("dense-tp")
        .unwrap()
        .cards
        .insert("B".into(), LayoutCard::Word("hold".into()));
    assert!(has(&c, "layouts.dense-tp.cards.B", "\"held\""));
}

#[test]
fn a_spanning_instance_is_fitted_per_card_share() {
    // 22.5 + 7.5 = 30 GiB: over budget on one 17 GiB card (15.5 after the 1.5 reserve), fits
    // as 15 GiB on each of two
    let mut c = sample_tp();
    for k in ["A", "B"] {
        c.cards.get_mut(k).unwrap().vram_gib = Some(17.0);
    }
    assert!(has(&c, "layouts.dense-x2.cards.A", "needs 30.0 GiB"));
    assert!(
        !validate(&c)
            .iter()
            .any(|i| i.path.starts_with("layouts.dense-tp")),
        "{:#?}",
        validate(&c)
    );
    // ... but not when B has only 14.5 GiB
    c.cards.get_mut("B").unwrap().vram_gib = Some(16.0);
    assert!(has(
        &c,
        "layouts.dense-tp.cards.A",
        "over 2 cards) but card `B` has"
    ));
}
