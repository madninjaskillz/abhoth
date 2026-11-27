use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::exec::{ExecResult, FakeExec, FakeFetch, scrub};
use crate::host::{self, CardSel, Cpu, Gpu, HostFacts, MethodFilter};
use crate::manifest::{GpuKind, Manifest, MethodType};
use crate::pins;
use crate::render::{self, RunSpec, StartPlan};
#[cfg(unix)]
use crate::runner::supervisor::Policy;
use crate::runner::{LocalRunner, Runner};
use crate::state::{Approvals, Handle, Phase};
use crate::store::{self, Package};
use crate::validate::{self, Files, Mode};

fn linux_gpu_host() -> HostFacts {
    HostFacts {
        os: "linux".into(),
        arch: "amd64".into(),
        docker: true,
        compose: true,
        pythons: vec!["3.12.4".into()],
        gpus: vec![Gpu {
            index: 2,
            uuid: "GPU-aaaa".into(),
            vendor: GpuKind::Cuda,
            name: "Quadro M4000".into(),
            vram_gib: 8.0,
        }],
        cpu: Cpu {
            cores: 48,
            ram_gib: 251.0,
            simd: vec!["avx".into(), "avx2".into()],
        },
        path_tools: Some(vec!["docker".into(), "git".into(), "python3".into()]),
    }
}

fn windows_pc() -> HostFacts {
    HostFacts {
        os: "windows".into(),
        arch: "amd64".into(),
        docker: false,
        compose: false,
        pythons: vec!["3.12".into()],
        gpus: vec![Gpu {
            index: 0,
            uuid: "GPU-pc".into(),
            vendor: GpuKind::Cuda,
            name: "RTX 3070 Ti".into(),
            vram_gib: 8.0,
        }],
        cpu: Cpu {
            cores: 16,
            ram_gib: 64.0,
            simd: vec!["avx2".into()],
        },
        path_tools: None,
    }
}

fn gpu_card(h: &HostFacts) -> CardSel {
    CardSel::for_gpu("X1", &h.gpus[0])
}

fn default_pkg(id: &str) -> Package {
    store::embedded::packages()
        .into_iter()
        .find(|p| p.manifest.id == id)
        .unwrap_or_else(|| panic!("no default package {id}"))
}

fn minimal(extra: Value) -> String {
    let mut v = json!({
        "schema": 1, "id": "demo", "name": "Demo", "version": "1.0", "kinds": ["tts"],
        "api": "openai-speech", "licence": "MIT", "source": "https://example.org/demo",
        "methods": [{"type": "docker", "image": format!("example/demo@sha256:{}", "a".repeat(64)),
                     "ports": [{"id": "http", "container": 8000}], "health": {"http": "/health"}}]
    });
    if let (Value::Object(a), Value::Object(b)) = (&mut v, extra) {
        for (k, x) in b {
            a.insert(k, x);
        }
    }
    v.to_string()
}

fn check(text: &str, files: &Files, mode: Mode) -> validate::Report {
    validate::validate_text(text, files, mode)
}

fn errors(r: &validate::Report) -> String {
    r.errors()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── the default store ────────────────────────────────────────────────────────────────────────

#[test]
fn every_default_package_validates_as_a_store_package() {
    let files = store::embedded::files();
    let mut dirs: BTreeMap<&str, Files> = BTreeMap::new();
    for (p, t) in files {
        if let Some((d, rest)) = p.split_once('/') {
            dirs.entry(d)
                .or_default()
                .insert(rest.into(), t.to_string());
        }
    }
    assert!(dirs.len() >= 6, "the starter packages: {:?}", dirs.keys());
    for (d, files) in &dirs {
        let r = check(&files["abhoth-engine.json"], files, Mode::Store);
        assert!(r.ok, "{d}:\n{}", r.text());
    }
    let ids: Vec<String> = store::embedded::packages()
        .iter()
        .map(|p| p.manifest.id.clone())
        .collect();
    for want in [
        "comfyui",
        "chatterbox",
        "whisper",
        "kokoro",
        "sa3",
        "llamacpp-bin",
    ] {
        assert!(ids.iter().any(|i| i == want), "{want} in {ids:?}");
    }
}

#[test]
fn the_store_folder_passes_the_validator_and_the_index_is_current() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stores/default");
    let (index, reports) = store::build_index(&dir, "default");
    for (d, r) in &reports {
        assert!(r.ok, "{d}:\n{}", r.text());
    }
    if std::env::var_os("ABHOTH_WRITE_INDEX").is_some() {
        let text = serde_json::to_string_pretty(&index).expect("json")
            + "
";
        std::fs::write(dir.join("index.json"), text).expect("write index.json");
    }
    let on_disk: store::Index =
        serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).expect("index.json"))
            .expect("index.json parses");
    assert_eq!(
        on_disk, index,
        "stores/default/index.json is stale: run `abhoth engine index stores/default`"
    );
}

#[test]
fn the_json_schema_names_every_field_the_rust_types_have() {
    let schema: Value = serde_json::from_str(store::embedded::schema()).expect("schema parses");
    let top = schema["properties"].as_object().expect("properties");
    // a manifest with every top-level field set, serialised by the Rust types
    let m = Manifest::parse(&default_pkg("chatterbox").files["abhoth-engine.json"]).expect("parse");
    let v = serde_json::to_value(&m).expect("to value");
    for k in v.as_object().expect("object").keys() {
        assert!(top.contains_key(k), "schema lacks top-level {k}");
    }
    let method_props = schema["properties"]["methods"]["items"]["properties"]
        .as_object()
        .expect("method properties");
    let mut all = crate::manifest::Method::of(MethodType::Docker);
    all.label = Some("x".into());
    all.platforms = vec!["*".into()];
    all.image = Some("x".into());
    all.entrypoint = Some("x".into());
    all.docker_args = vec!["x".into()];
    all.compose = Some("x".into());
    all.compose_file = Some("x".into());
    all.python = Some("x".into());
    all.package = Some("x".into());
    all.requirements = vec!["x".into()];
    all.requirements_file = Some("x".into());
    all.extra_index_urls = vec!["x".into()];
    all.environment_file = Some("x".into());
    all.files = vec!["x".into()];
    all.repo = Some("x".into());
    all.commit = Some("x".into());
    all.workdir = Some("x".into());
    all.run = vec!["x".into()];
    all.detect = Some(Default::default());
    let v = serde_json::to_value(&all).expect("to value");
    for k in v.as_object().expect("object").keys() {
        assert!(
            method_props.contains_key(k),
            "schema lacks method field {k}"
        );
    }
    for k in method_props.keys() {
        let known = [
            "gpu",
            "requires",
            "env",
            "ports",
            "health",
            "mounts",
            "update",
            "uninstall",
            "assets",
            "build",
        ];
        assert!(
            v.get(k).is_some() || known.contains(&k.as_str()),
            "schema has a method field the types do not: {k}"
        );
    }
}

// ── validation ───────────────────────────────────────────────────────────────────────────────

#[test]
fn a_minimal_manifest_is_valid() {
    let r = check(&minimal(json!({})), &Files::new(), Mode::Store);
    assert!(r.ok, "{}", r.text());
}

#[test]
fn unknown_keys_and_bad_ids_are_refused() {
    let r = check(
        &minimal(json!({"colour": "red"})),
        &Files::new(),
        Mode::Store,
    );
    assert!(!r.ok);
    assert!(errors(&r).contains("colour"), "{}", r.text());
    let r = check(
        &minimal(json!({"id": "Bad Id"})),
        &Files::new(),
        Mode::Store,
    );
    assert!(errors(&r).contains("id"), "{}", r.text());
    let r = check(
        &minimal(json!({"kinds": ["sing"]})),
        &Files::new(),
        Mode::Store,
    );
    assert!(errors(&r).contains("sing is not a kind"), "{}", r.text());
    let r = check(&minimal(json!({"schema": 2})), &Files::new(), Mode::Store);
    assert!(errors(&r).contains("schema 2"), "{}", r.text());
}

#[test]
fn unpinned_artifacts_are_refused_in_a_store_and_warned_about_when_private() {
    let unpinned = json!({"methods": [{"type": "docker", "image": "example/demo:latest",
        "ports": [{"id": "http", "container": 80}]}]});
    let r = check(&minimal(unpinned.clone()), &Files::new(), Mode::Store);
    assert!(errors(&r).contains("not pinned by digest"), "{}", r.text());
    let mut private = unpinned.clone();
    private["private"] = json!(true);
    let r = check(&minimal(private), &Files::new(), Mode::Private);
    assert!(r.ok, "{}", r.text());
    assert!(
        r.issues
            .iter()
            .any(|i| i.message.contains("allowed in a private package"))
    );

    let git = json!({"methods": [{"type": "git", "repo": "https://example.org/x.git", "commit": "main",
        "run": ["x"], "ports": [{"id": "http"}]}]});
    let r = check(&minimal(git), &Files::new(), Mode::Store);
    assert!(errors(&r).contains("40-hex commit"), "{}", r.text());

    let pip = json!({"methods": [{"type": "pip", "requirements": ["torch>=2", "numpy==2.1.0 --hash=sha256:".to_string() + &"b".repeat(64)],
        "run": ["{{python}}", "-m", "x"], "ports": [{"id": "http"}]}]});
    let r = check(&minimal(pip), &Files::new(), Mode::Store);
    let e = errors(&r);
    assert!(e.contains("torch>=2 is not pinned"), "{e}");
    assert!(e.contains("some lines have --hash"), "{e}");

    let bin = json!({"methods": [{"type": "binary", "assets": [{"platform": "linux/amd64",
        "url": "https://example.org/x.zip", "sha256": "nope"}], "run": ["{{install}}/x"], "ports": [{"id": "http"}]}]});
    let r = check(&minimal(bin), &Files::new(), Mode::Store);
    assert!(errors(&r).contains("needs its sha256"), "{}", r.text());
}

#[test]
fn the_validator_refuses_binaries_archives_blobs_and_big_packages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    std::fs::write(d.join("abhoth-engine.json"), minimal(json!({}))).expect("write");
    assert!(validate::validate_path(d, Mode::Store).ok);

    // an ELF binary with an innocent name
    std::fs::write(d.join("helper.txt"), b"\x7fELF\x02\x01\x01\x00rest").expect("write");
    let r = validate::validate_path(d, Mode::Store);
    assert!(errors(&r).contains("an ELF binary"), "{}", r.text());
    std::fs::remove_file(d.join("helper.txt")).expect("rm");

    // a wheel by extension, and a zip by content
    std::fs::write(d.join("dep.whl"), b"PK\x03\x04").expect("write");
    std::fs::write(d.join("notes.md"), b"PK\x03\x04....").expect("write");
    let r = validate::validate_path(d, Mode::Store);
    let e = errors(&r);
    assert!(e.contains("a .whl file"), "{e}");
    assert!(e.contains("a zip archive"), "{e}");
    std::fs::remove_file(d.join("dep.whl")).expect("rm");
    std::fs::remove_file(d.join("notes.md")).expect("rm");

    // a model file
    std::fs::write(d.join("model.gguf"), b"GGUF").expect("write");
    assert!(!validate::validate_path(d, Mode::Private).ok);
    std::fs::remove_file(d.join("model.gguf")).expect("rm");

    // base64 smuggling in a text file
    std::fs::write(d.join("blob.txt"), "QUJD".repeat(2000)).expect("write");
    assert!(errors(&validate::validate_path(d, Mode::Store)).contains("blob"));
    std::fs::remove_file(d.join("blob.txt")).expect("rm");

    // over 1 MiB of text in all
    let line = "a line of text that is perfectly ordinary\n".repeat(30_000);
    std::fs::write(d.join("big.md"), line).expect("write");
    let r = validate::validate_path(d, Mode::Store);
    assert!(
        errors(&r).contains("metadata only and at most 1024 KiB"),
        "{}",
        r.text()
    );
}

#[test]
fn templates_mounts_and_docker_options_are_checked() {
    let bad = json!({"methods": [{"type": "docker", "image": format!("x@sha256:{}", "c".repeat(64)),
        "run": ["--port", "{{prot}}"], "ports": [{"id": "http", "container": 80}],
        "mounts": [{"source": "/etc", "target": "/host-etc"}],
        "docker_args": ["--privileged", "--shm-size=1g"]}]});
    let r = check(&minimal(bad), &Files::new(), Mode::Store);
    let e = errors(&r);
    assert!(e.contains("unknown template variable {{prot}}"), "{e}");
    assert!(e.contains("mounts start at"), "{e}");
    assert!(e.contains("--privileged is not allowed"), "{e}");
    assert!(!e.contains("docker_args[1]"), "{e}");
}

#[test]
fn compose_files_may_not_ask_for_more_than_security_declares() {
    let c = "services:\n  x:\n    image: x@sha256:".to_string()
        + &"d".repeat(64)
        + "\n    privileged: true\n    network_mode: host\n    ports:\n      - \"{{host}}:{{port}}:80\"\n    volumes:\n      - /var/run/docker.sock:/var/run/docker.sock\n      - {{model_dir}}:/models\n";
    let mut files = Files::new();
    files.insert("compose.yml".into(), c);
    let m = json!({"methods": [{"type": "compose", "compose_file": "compose.yml", "ports": [{"id": "http"}]}]});
    let r = check(&minimal(m), &files, Mode::Store);
    let e = errors(&r);
    assert!(e.contains("asks for privileged"), "{e}");
    assert!(e.contains("asks for host_network"), "{e}");
    assert!(e.contains("the docker socket"), "{e}");
    assert!(e.contains("mounts /var/run/docker.sock"), "{e}");
    assert!(!e.contains("{{host}}"), "ports are not mounts: {e}");
    assert!(!e.contains("mounts {{model_dir}}"), "{e}");
}

// ── method choice ────────────────────────────────────────────────────────────────────────────

#[test]
fn deepthought_takes_docker_and_the_windows_pc_takes_pip_or_binary() {
    let dt = linux_gpu_host();
    let pc = windows_pc();
    let kokoro = default_pkg("kokoro").manifest;
    let (_, m) = host::choose_method(
        &kokoro,
        &dt,
        &gpu_card(&dt),
        &MethodFilter::containers_only(),
    )
    .expect("dt");
    assert_eq!(m.r#type, MethodType::Docker);
    assert!(m.image.as_deref().unwrap_or("").contains("gpu"));
    let (_, m) =
        host::choose_method(&kokoro, &pc, &gpu_card(&pc), &MethodFilter::local()).expect("pc");
    assert_eq!(m.r#type, MethodType::Pip);
    // the CPU card on DeepThought gets the CPU image
    let cpu = CardSel::cpu("DT-CPU", &dt);
    let (_, m) =
        host::choose_method(&kokoro, &dt, &cpu, &MethodFilter::containers_only()).expect("cpu");
    assert!(m.image.as_deref().unwrap_or("").contains("cpu"), "{m:?}");

    let llama = default_pkg("llamacpp-bin").manifest;
    let (_, m) =
        host::choose_method(&llama, &pc, &gpu_card(&pc), &MethodFilter::local()).expect("pc llama");
    assert_eq!(m.label(), "Windows CUDA 12.4");
    assert_eq!(
        host::pick_assets(&llama, m, &pc, &gpu_card(&pc)).len(),
        2,
        "the build and its CUDA runtime"
    );
}

#[test]
fn refused_methods_say_why() {
    let dt = linux_gpu_host();
    let comfy = default_pkg("comfyui").manifest;
    // a CPU card: ComfyUI needs CUDA
    let rej = host::choose_method(
        &comfy,
        &dt,
        &CardSel::cpu("DT-CPU", &dt),
        &MethodFilter::containers_only(),
    )
    .expect_err("no method on a CPU card");
    assert!(
        rej.iter()
            .all(|r| r.reasons.iter().any(|x| x.contains("the card is cpu"))),
        "{rej:?}"
    );
    // a router in a container cannot run the git method
    let mut nodocker = dt.clone();
    nodocker.docker = false;
    let rej = host::choose_method(
        &comfy,
        &nodocker,
        &gpu_card(&dt),
        &MethodFilter::containers_only(),
    )
    .expect_err("no docker");
    let all: Vec<String> = rej.iter().flat_map(|r| r.reasons.clone()).collect();
    assert!(all.iter().any(|x| x == "needs docker"), "{all:?}");
    assert!(
        all.iter().any(|x| x.contains("only in containers")),
        "{all:?}"
    );
    // a small card
    let small = CardSel::Gpu {
        card: "S".into(),
        index: 0,
        uuid: String::new(),
        vendor: GpuKind::Cuda,
        vram_gib: 4.0,
    };
    let f = host::fit(&comfy, &dt, &small, &MethodFilter::local());
    assert!(!f.ok);
    assert!(
        f.rejections[0]
            .reasons
            .iter()
            .any(|x| x.contains("needs 6 GiB VRAM")),
        "{f:?}"
    );
    // the fit lists approvals for host methods
    let f = host::fit(
        &default_pkg("kokoro").manifest,
        &windows_pc(),
        &gpu_card(&windows_pc()),
        &MethodFilter::local(),
    );
    assert!(
        f.ok && f.approvals.iter().any(|a| a.contains("not sandboxed")),
        "{f:?}"
    );
    assert!(
        f.rejections.iter().all(|r| r
            .reasons
            .iter()
            .any(|x| x.contains("docker") || x.contains("built for"))),
        "{f:?}"
    );
}

#[test]
fn platform_patterns() {
    assert!(host::platform_matches("*", "linux", "amd64"));
    assert!(host::platform_matches("linux/*", "linux", "arm64"));
    assert!(host::platform_matches("*/amd64", "windows", "amd64"));
    assert!(!host::platform_matches("windows/amd64", "linux", "amd64"));
    assert!(host::platform_matches("linux", "linux", "arm64"));
}

// ── rendering ────────────────────────────────────────────────────────────────────────────────

fn installed_for(p: &Package, method: usize, dir: &Path) -> crate::Installed {
    let meth = &p.manifest.methods[method];
    crate::Installed {
        id: p.manifest.id.clone(),
        version: p.manifest.version.clone(),
        method,
        method_type: meth.r#type,
        dir: dir.to_path_buf(),
        os: "linux".into(),
        venv: Some(dir.join("venv")),
        install: Some(dir.join("app")),
        bin: None,
        image: meth.image.clone(),
        pins: vec![],
        found_version: None,
        at: 0,
        approvals: Approvals::default(),
        store: "default".into(),
    }
}

#[test]
fn a_docker_plan_pins_the_card_ports_mounts_and_limits() {
    let p = default_pkg("comfyui");
    let i = installed_for(&p, 0, Path::new("/state/engines/comfyui/0.45.0"));
    let dt = linux_gpu_host();
    let mut s = RunSpec::new("comfyui", CardSel::for_gpu("X2", &dt.gpus[0]), 8188);
    s.model_dir = Some("/tank/models/comfyui".into());
    s.state = Some("/tank/state/abhoth-comfyui-x2".into());
    s.limits.mem_gib = Some(24.0);
    let plan = render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect("plan");
    let StartPlan::Docker { argv, name } = plan else {
        panic!("docker")
    };
    assert_eq!(name, "abhoth-comfyui-x2");
    let a = argv.join(" ");
    assert!(
        a.starts_with("docker run -d --name abhoth-comfyui-x2"),
        "{a}"
    );
    assert!(a.contains("--gpus device=GPU-aaaa"), "{a}");
    assert!(a.contains("-p 127.0.0.1:8188:8188"), "{a}");
    assert!(a.contains("-v /tank/models/comfyui:/models:ro"), "{a}");
    assert!(
        a.contains("-v /tank/state/abhoth-comfyui-x2/root:/root"),
        "{a}"
    );
    assert!(a.contains("--memory 24576m"), "{a}");
    assert!(a.contains("--shm-size=2g"), "{a}");
    assert!(a.contains("--preview-method none"), "{a}");
    assert!(
        !a.contains("--privileged") && !a.contains("--network host"),
        "{a}"
    );
    assert!(argv.iter().any(|x| {
        x.ends_with("@sha256:2860c103583fbbd03f2e324116395a47e68ad0ef84fc3689f03b42a63fba41a6")
    }));
}

#[test]
fn a_cpu_card_hides_the_gpus() {
    let p = default_pkg("kokoro");
    let i = installed_for(&p, 1, Path::new("/s/kokoro"));
    let dt = linux_gpu_host();
    let s = RunSpec::new("kokoro", CardSel::cpu("DT-CPU", &dt), 8880);
    let StartPlan::Docker { argv, .. } =
        render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect("plan")
    else {
        panic!()
    };
    let a = argv.join(" ");
    assert!(!a.contains("--gpus"), "{a}");
    assert!(a.contains("NVIDIA_VISIBLE_DEVICES=void"), "{a}");
    assert!(a.contains("--cpus 48"), "{a}");
    // a host process on a CUDA card gets CUDA_VISIBLE_DEVICES
    let i = installed_for(&p, 2, Path::new("/s/kokoro"));
    let s = RunSpec::new("kokoro", CardSel::for_gpu("X1", &dt.gpus[0]), 8880);
    let StartPlan::Process {
        program, args, env, ..
    } = render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect("plan")
    else {
        panic!()
    };
    // Paths are joined with the OS separator; compare them with `/`.
    assert_eq!(
        program.replace(char::from(92u8), "/"),
        "/s/kokoro/venv/bin/python"
    );
    assert_eq!(
        args[0].replace(char::from(92u8), "/"),
        "/s/kokoro/app/server.py"
    );
    assert!(
        env.contains(&("CUDA_VISIBLE_DEVICES".into(), "2".into())),
        "{env:?}"
    );
}

#[test]
fn host_mounts_need_approval_and_required_params_must_be_set() {
    let p = default_pkg("sa3");
    let i = installed_for(&p, 0, Path::new("/s/sa3"));
    let dt = linux_gpu_host();
    let mut s = RunSpec::new("sa3", CardSel::cpu("DT-CPU", &dt), 8095);
    let e = render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect_err("params");
    assert!(e.contains("service_dir is required"), "{e}");
    s.params.insert("service_dir".into(), json!("/srv/sa3"));
    s.params.insert("src_dir".into(), json!("/srv/sa3-src"));
    s.params.insert("hf_cache".into(), json!("/srv/hf"));
    let e =
        render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect_err("not approved");
    assert!(e.contains("host mount /srv/sa3"), "{e}");
    let ok = Approvals {
        host_mounts: vec!["/srv/sa3".into(), "/srv/sa3-src".into(), "/srv/hf".into()],
        ..Default::default()
    };
    let StartPlan::Docker { argv, .. } =
        render::plan_start(&p.manifest, &i, &s, &ok).expect("approved")
    else {
        panic!()
    };
    let a = argv.join(" ");
    assert!(a.contains("-v /srv/sa3:/srv/sa3 "), "{a}");
    assert!(a.contains("-v /srv/hf:/srv/hf:ro"), "{a}");
    assert!(a.contains("-e SA3_THREADS=24"), "{a}");
    assert!(a.ends_with("local/sa3:1"), "the image's own command: {a}");
}

#[test]
fn a_compose_plan_renders_and_checks_mounts() {
    let p = default_pkg("whisper");
    let i = installed_for(&p, 1, Path::new("/nonexistent"));
    let dt = linux_gpu_host();
    let mut s = RunSpec::new("whisper", CardSel::cpu("DT-CPU", &dt), 10300);
    s.model_dir = Some("/tank/whisper".into());
    // the template is read from the installed package folder
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("package")).expect("mkdir");
    std::fs::write(
        dir.path().join("package/compose.yml"),
        &p.files["compose.yml"],
    )
    .expect("write");
    let i = crate::Installed {
        dir: dir.path().to_path_buf(),
        ..i
    };
    let StartPlan::Compose {
        yaml, up, project, ..
    } = render::plan_start(&p.manifest, &i, &s, &Approvals::default()).expect("plan")
    else {
        panic!()
    };
    assert_eq!(project, "abhoth-whisper-dt-cpu");
    assert!(yaml.contains("\"127.0.0.1:10300:10300\""), "{yaml}");
    assert!(yaml.contains("- /tank/whisper:/data"), "{yaml}");
    assert!(
        yaml.contains("container_name: abhoth-whisper-dt-cpu"),
        "{yaml}"
    );
    assert_eq!(up.last().map(String::as_str), Some("-d"));
    // a value with a newline cannot add YAML keys
    s.params
        .insert("model".into(), json!("small\n    privileged: true"));
    assert!(render::plan_start(&p.manifest, &i, &s, &Approvals::default()).is_err());
}

#[test]
fn render_refuses_unknown_names() {
    let mut v = render::Vars::new();
    v.insert("port".into(), "80".into());
    assert_eq!(
        render::render("--port={{port}}", &v).expect("ok"),
        "--port=80"
    );
    assert!(render::render("{{nope}}", &v).is_err());
    assert!(render::render("{{port", &v).is_err());
}

// ── pins ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn pin_checks() {
    assert!(pins::image_pinned(&format!(
        "ghcr.io/a/b:v1@sha256:{}",
        "0".repeat(64)
    )));
    assert!(!pins::image_pinned("ghcr.io/a/b:v1"));
    assert!(!pins::image_pinned(&format!("a@sha256:{}", "0".repeat(63))));
    assert!(pins::requirement_pinned("kokoro==0.9.4"));
    assert!(pins::requirement_pinned(
        "torch[cuda]==2.7.1 --hash=sha256:abc"
    ));
    assert!(!pins::requirement_pinned("torch>=2"));
    assert!(!pins::requirement_pinned("git+https://example.org/x"));
    assert!(pins::is_commit(&"a".repeat(40)));
    assert!(!pins::is_commit("main"));
    let dir = tempfile::tempdir().expect("tmp");
    let f = dir.path().join("x");
    std::fs::write(&f, b"hello").expect("write");
    let good = pins::sha256_bytes(b"hello");
    assert!(pins::verify_file(&f, &good).is_ok());
    assert!(pins::verify_file(&f, &"0".repeat(64)).is_err());
}

// ── the runner ───────────────────────────────────────────────────────────────────────────────

fn runner(
    dir: &Path,
    exec: FakeExec,
    fetch: FakeFetch,
    facts: HostFacts,
    host_processes: bool,
) -> LocalRunner {
    LocalRunner::new(
        dir.to_path_buf(),
        Arc::new(exec),
        Arc::new(fetch),
        host_processes,
        facts,
    )
}

#[tokio::test]
async fn docker_install_start_status_stop_with_a_fake_docker() {
    let dir = tempfile::tempdir().expect("tmp");
    let exec = FakeExec::new();
    exec.on(
        "docker inspect",
        vec![ExecResult::ok_with(
            r#"{"Status":"running","Pid":4242,"ExitCode":0,"Health":{"Status":"healthy"}}"#,
        )],
    );
    let dt = linux_gpu_host();
    let r = runner(
        dir.path(),
        exec.clone(),
        FakeFetch::default(),
        dt.clone(),
        false,
    );
    let p = default_pkg("kokoro");
    let card = gpu_card(&dt);
    let (mi, _) = host::choose_method(&p.manifest, &dt, &card, &MethodFilter::containers_only())
        .expect("method");
    let i = r
        .install(&p, mi, &card, &Approvals::default())
        .await
        .expect("install");
    assert!(
        exec.joined()[0]
            .starts_with("docker pull ghcr.io/remsky/kokoro-fastapi-gpu:v0.2.4@sha256:")
    );
    assert!(i.pins[0].starts_with("image "));
    assert!(
        dir.path()
            .join("engines/kokoro/0.9.4/installed.json")
            .is_file()
    );
    assert!(
        dir.path()
            .join("engines/kokoro/0.9.4/package/abhoth-engine.json")
            .is_file()
    );
    assert_eq!(r.layout.list_installed().len(), 1);

    let s = RunSpec::new("kokoro", card, 8880);
    let h = r.start(&p.manifest, &i, &s).await.expect("start");
    assert_eq!(
        h,
        Handle::Container {
            name: "abhoth-kokoro-x1".into()
        }
    );
    let calls = exec.joined();
    assert!(calls.iter().any(|c| c == "docker rm -f abhoth-kokoro-x1"));
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("docker run -d --name abhoth-kokoro-x1")
                && c.contains("--gpus device=GPU-aaaa"))
    );
    let st = r.status(&h).await.expect("status");
    assert_eq!(st.phase, Phase::Healthy);
    assert_eq!(st.pid, Some(4242));
    assert_eq!(r.adopt().await, vec![h.clone()]);
    r.stop(&h, Duration::from_secs(5)).await.expect("stop");
    assert!(
        exec.joined()
            .iter()
            .any(|c| c == "docker stop -t 5 abhoth-kokoro-x1")
    );
    assert!(r.adopt().await.is_empty());
    r.uninstall(&p.manifest, &i).await.expect("uninstall");
    assert!(!dir.path().join("engines/kokoro/0.9.4").exists());
    assert!(
        !exec
            .joined()
            .iter()
            .any(|c| c.contains("docker rmi") || c.contains("image rm")),
        "images are never removed"
    );
}

#[tokio::test]
async fn host_methods_need_approval_and_a_host_that_allows_them() {
    let dir = tempfile::tempdir().expect("tmp");
    let pc = windows_pc();
    let p = default_pkg("kokoro");
    let card = gpu_card(&pc);
    let r = runner(
        dir.path(),
        FakeExec::new(),
        FakeFetch::default(),
        pc.clone(),
        true,
    );
    let e = r
        .install(&p, 2, &card, &Approvals::default())
        .await
        .expect_err("needs approval");
    assert!(e.0.contains("not sandboxed"), "{e}");
    let r2 = runner(
        dir.path(),
        FakeExec::new(),
        FakeFetch::default(),
        pc.clone(),
        false,
    );
    let e = r2
        .install(
            &p,
            2,
            &card,
            &Approvals {
                host_run: true,
                ..Default::default()
            },
        )
        .await
        .expect_err("containers only");
    assert!(e.0.contains("only in containers"), "{e}");
    // an untrusted store's package
    let mut u = p.clone();
    u.trusted = false;
    let e = r
        .install(&u, 0, &card, &Approvals::default())
        .await
        .expect_err("untrusted");
    assert!(e.0.contains("untrusted"), "{e}");
}

#[tokio::test]
async fn pip_install_makes_a_venv_and_pins_the_requirements() {
    let dir = tempfile::tempdir().expect("tmp");
    let exec = FakeExec::new();
    exec.on("python3.12 -c", vec![ExecResult::ok_with("3.12\n")]);
    let mut host = linux_gpu_host();
    host.os = "linux".into();
    let r = runner(
        dir.path(),
        exec.clone(),
        FakeFetch::default(),
        host.clone(),
        true,
    );
    let p = default_pkg("kokoro");
    let i = r
        .install(
            &p,
            2,
            &gpu_card(&host),
            &Approvals {
                host_run: true,
                ..Default::default()
            },
        )
        .await
        .expect("install");
    let calls = exec.joined();
    let venv = dir
        .path()
        .join("engines")
        .join("kokoro")
        .join("0.9.4")
        .join("venv");
    assert!(
        calls
            .iter()
            .any(|c| *c == format!("python3.12 -m venv {}", venv.display())),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("-m pip install --no-input") && c.contains("requirements.txt")),
        "{calls:#?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("--require-hashes")),
        "version pins only"
    );
    let req = std::fs::read_to_string(dir.path().join("engines/kokoro/0.9.4/requirements.txt"))
        .expect("req");
    assert_eq!(req, "kokoro==0.9.4\nsoundfile==0.13.1\n");
    assert!(
        i.install.as_ref().expect("app").join("server.py").is_file(),
        "the shim is copied"
    );
    assert!(i.pins.iter().any(|x| x == "pip kokoro==0.9.4"));
}

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        for (n, b) in files {
            z.start_file(*n, zip::write::SimpleFileOptions::default())
                .expect("start");
            z.write_all(b).expect("write");
        }
        z.finish().expect("finish");
    }
    buf.into_inner()
}

fn binary_pkg(url: &str, sha: &str) -> Package {
    let text = minimal(
        json!({"id": "tool", "kinds": ["llm"], "api": "openai-chat", "methods": [{
        "type": "binary", "platforms": ["*"], "gpu": ["cpu", "cuda"],
        "assets": [{"platform": "*", "url": url, "sha256": sha, "strip": 1}],
        "run": ["{{install}}/server{{exe}}", "--port", "{{port}}"], "ports": [{"id": "http"}]}]}),
    );
    let m = Manifest::parse(&text).expect("parse");
    let mut files = Files::new();
    files.insert("abhoth-engine.json".into(), text);
    Package {
        manifest: m,
        files,
        store: "default".into(),
        trusted: true,
    }
}

#[tokio::test]
async fn binary_install_verifies_the_sha256_and_unpacks_safely() {
    let dir = tempfile::tempdir().expect("tmp");
    let z = zip_bytes(&[
        ("tool-1/server", b"#!/bin/sh\necho hi\n"),
        ("tool-1/lib/x.so", b"lib"),
    ]);
    let url = "https://example.org/tool-1.zip";
    let fetch = FakeFetch::default().with(url, &z);
    let host = linux_gpu_host();
    let card = gpu_card(&host);
    let ok = Approvals {
        host_run: true,
        ..Default::default()
    };
    // a wrong pin is refused and leaves nothing behind
    let r = runner(
        dir.path(),
        FakeExec::new(),
        fetch.clone(),
        host.clone(),
        true,
    );
    let e = r
        .install(&binary_pkg(url, &"0".repeat(64)), 0, &card, &ok)
        .await
        .expect_err("bad pin");
    assert!(e.0.contains("does not match the pin"), "{e}");
    assert!(!dir.path().join("engines/tool").join("1.0").exists());
    // the right pin installs, stripped of the top folder
    let sha = pins::sha256_bytes(&z);
    let i = r
        .install(&binary_pkg(url, &sha), 0, &card, &ok)
        .await
        .expect("install");
    let bin = i.install.clone().expect("bin");
    assert!(bin.join("server").is_file());
    assert!(bin.join("lib/x.so").is_file());
    assert!(i.pins[0].starts_with(&format!("sha256:{sha}")));
    // a zip with a path that escapes is refused
    let evil = zip_bytes(&[("../../evil", b"x")]);
    let url2 = "https://example.org/evil.zip";
    let r = runner(
        dir.path(),
        FakeExec::new(),
        FakeFetch::default().with(url2, &evil),
        host,
        true,
    );
    let e = r
        .install(&binary_pkg(url2, &pins::sha256_bytes(&evil)), 0, &card, &ok)
        .await
        .expect_err("escape");
    assert!(e.0.contains("unsafe path"), "{e}");
}

#[tokio::test]
async fn git_install_checks_out_the_pinned_commit() {
    let dir = tempfile::tempdir().expect("tmp");
    let commit = "989d93d0d968e704c589ce0146bb18c77f6b50cc";
    let exec = FakeExec::new();
    exec.on("git -C", vec![ExecResult::ok_with("")]);
    exec.on("python3.12 -c", vec![ExecResult::ok_with("3.12\n")]);
    // rev-parse answers a different commit: refused
    let fake_rev = FakeExec::new();
    fake_rev.on(
        "git -C",
        vec![
            ExecResult::ok_with(""),
            ExecResult::ok_with(&"f".repeat(40)),
        ],
    );
    let host = linux_gpu_host();
    let card = gpu_card(&host);
    let p = default_pkg("comfyui");
    let ok = Approvals {
        host_run: true,
        ..Default::default()
    };
    let r = runner(
        dir.path(),
        fake_rev.clone(),
        FakeFetch::default(),
        host.clone(),
        true,
    );
    let e = r.install(&p, 1, &card, &ok).await.expect_err("wrong head");
    assert!(e.0.contains("not the pinned"), "{e}");
    let good = FakeExec::new();
    good.on(
        "git -C",
        vec![
            ExecResult::ok_with(""),
            ExecResult::ok_with(&format!("{commit}\n")),
        ],
    );
    good.on("python3.12 -c", vec![ExecResult::ok_with("3.12\n")]);
    let r = runner(dir.path(), good.clone(), FakeFetch::default(), host, true);
    let i = r.install(&p, 1, &card, &ok).await.expect("install");
    let calls = good.joined();
    assert!(calls.iter().any(|c| {
        c.starts_with("git clone --no-checkout -- https://github.com/comfyanonymous/ComfyUI")
    }));
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with(&format!("checkout --detach {commit}")))
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("--extra-index-url https://download.pytorch.org/whl/cu126"))
    );
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("-m pip install --no-input -r requirements.txt")),
        "{calls:#?}"
    );
    assert!(
        i.pins
            .iter()
            .any(|x| x.starts_with(&format!("git {commit}")))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn existing_programs_are_found_and_version_checked() {
    let dir = tempfile::tempdir().expect("tmp");
    let tool = dir.path().join("llama-server");
    std::fs::write(&tool, "#!/bin/sh\necho 'version: 12292 (abc)'\n").expect("write");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let mut p = default_pkg("llamacpp-bin");
    let mi = p
        .manifest
        .methods
        .iter()
        .position(|m| m.r#type == MethodType::Existing)
        .expect("existing");
    p.manifest.methods[mi]
        .detect
        .as_mut()
        .expect("detect")
        .paths = vec![tool.to_string_lossy().to_string()];
    let host = linux_gpu_host();
    let r = LocalRunner::new(
        dir.path().join("state"),
        Arc::new(crate::exec::LocalExec),
        Arc::new(FakeFetch::default()),
        true,
        host.clone(),
    );
    let i = r
        .install(&p, mi, &gpu_card(&host), &Approvals::default())
        .await
        .expect("found");
    assert_eq!(i.found_version.as_deref(), Some("12292"));
    assert_eq!(i.bin.as_deref(), Some(tool.as_path()));
    p.manifest.methods[mi]
        .detect
        .as_mut()
        .expect("detect")
        .version_regex = Some("version: 99999".into());
    let e = r
        .install(&p, mi, &gpu_card(&host), &Approvals::default())
        .await
        .expect_err("wrong version");
    assert!(e.0.contains("does not match"), "{e}");
}

#[cfg(unix)]
fn sh_pkg(script: &str) -> (Package, crate::Installed) {
    let text = minimal(
        json!({"id": "fake", "methods": [{"type": "existing", "platforms": ["*"], "gpu": ["cpu", "cuda"],
        "detect": {"command": "sh"}, "run": ["{{bin}}", "-c", script], "ports": [{"id": "http"}]}]}),
    );
    let m = Manifest::parse(&text).expect("parse");
    let mut files = Files::new();
    files.insert("abhoth-engine.json".into(), text);
    let p = Package {
        manifest: m,
        files,
        store: "default".into(),
        trusted: true,
    };
    let i = crate::Installed {
        id: "fake".into(),
        version: "1.0".into(),
        method: 0,
        method_type: MethodType::Existing,
        dir: std::env::temp_dir(),
        os: "linux".into(),
        venv: None,
        install: Some("/bin".into()),
        bin: Some("/bin/sh".into()),
        image: None,
        pins: vec![],
        found_version: None,
        at: 0,
        approvals: Approvals::default(),
        store: "default".into(),
    };
    (p, i)
}

#[cfg(unix)]
fn fast_policy() -> Policy {
    Policy {
        first_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        healthy_after: Duration::from_secs(600),
        max_restarts: 3,
        window: Duration::from_secs(60),
        log_max_bytes: 1024 * 1024,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_crashing_process_is_restarted_with_backoff_then_given_up() {
    let dir = tempfile::tempdir().expect("tmp");
    let host = linux_gpu_host();
    let r = LocalRunner::new(
        dir.path().to_path_buf(),
        Arc::new(crate::exec::LocalExec),
        Arc::new(FakeFetch::default()),
        true,
        host.clone(),
    )
    .with_policy(fast_policy());
    let (p, i) = sh_pkg("echo started; echo token hf_abcdefghijklmnop; exit 3");
    let s = RunSpec::new("fake", CardSel::cpu("CPU", &host), 9999);
    let h = r.start(&p.manifest, &i, &s).await.expect("start");
    let Handle::Process { name, .. } = &h else {
        panic!("{h:?}")
    };
    let mut st = r.status(&h).await.expect("status");
    for _ in 0..100 {
        if st.phase == Phase::Failed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        st = r.status(&h).await.expect("status");
    }
    assert_eq!(st.phase, Phase::Failed, "{st:?}");
    assert_eq!(st.restarts, 3);
    assert_eq!(st.last_exit, Some(3));
    let logs = r.logs(&h, 100).await.expect("logs");
    assert_eq!(logs.matches("started\n").count(), 4, "{logs}");
    assert!(
        logs.contains("hf_***") && !logs.contains("abcdefghijklmnop"),
        "secrets scrubbed: {logs}"
    );
    assert_eq!(name, "abhoth-fake-cpu");
}

#[cfg(unix)]
#[tokio::test]
async fn stop_kills_by_exact_pid_and_adopt_finds_survivors() {
    let dir = tempfile::tempdir().expect("tmp");
    let host = linux_gpu_host();
    let r = LocalRunner::new(
        dir.path().to_path_buf(),
        Arc::new(crate::exec::LocalExec),
        Arc::new(FakeFetch::default()),
        true,
        host.clone(),
    )
    .with_policy(fast_policy());
    // ignores SIGTERM: the grace ends in a kill
    let (p, i) = sh_pkg("trap '' TERM; while true; do sleep 0.1; done");
    let s = RunSpec::new("fake", CardSel::cpu("CPU", &host), 9999);
    let h = r.start(&p.manifest, &i, &s).await.expect("start");
    let Handle::Process { pid, started, .. } = h.clone() else {
        panic!()
    };
    assert!(crate::runner::proc::alive(pid, started).await);
    // a fresh runner (a restart) adopts it from the handle file
    let r2 = LocalRunner::new(
        dir.path().to_path_buf(),
        Arc::new(crate::exec::LocalExec),
        Arc::new(FakeFetch::default()),
        true,
        host.clone(),
    );
    assert_eq!(r2.adopt().await, vec![h.clone()]);
    let st = r2.status(&h).await.expect("status");
    assert_eq!(st.phase, Phase::Running);
    // a wrong start time is never killed
    crate::runner::proc::stop_exact(pid, started + 1, Duration::from_millis(100))
        .await
        .expect("noop");
    assert!(
        crate::runner::proc::alive(pid, started).await,
        "a mismatched start time leaves it alone"
    );
    let t = std::time::Instant::now();
    r.stop(&h, Duration::from_millis(300)).await.expect("stop");
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(!crate::runner::proc::alive(pid, started).await);
    assert!(r2.adopt().await.is_empty());
}

// ── stores ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn store_urls() {
    assert_eq!(
        store::index_base("https://github.com/someone/engines").expect("gh"),
        "https://raw.githubusercontent.com/someone/engines/HEAD"
    );
    assert_eq!(
        store::index_base("https://github.com/someone/engines/tree/v2/stores/default").expect("gh"),
        "https://raw.githubusercontent.com/someone/engines/v2/stores/default"
    );
    assert_eq!(
        store::index_base("https://example.org/s/index.json").expect("plain"),
        "https://example.org/s"
    );
    assert!(store::index_base("http://example.org/s").is_err());
}

#[tokio::test]
async fn fetching_a_store_package_checks_every_sha256() {
    let p = default_pkg("whisper");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stores/default");
    let (index, _) = store::build_index(&dir, "default");
    let e = index
        .packages
        .iter()
        .find(|e| e.id == "whisper")
        .expect("entry")
        .clone();
    let base = "https://example.org/store";
    let mut fetch = FakeFetch::default();
    for (rel, t) in &p.files {
        fetch = fetch.with(&format!("{base}/whisper/{rel}"), t.as_bytes());
    }
    let got = store::fetch_package(&fetch, base, &e, "ex", false)
        .await
        .expect("fetch");
    assert_eq!(got.manifest, p.manifest);
    assert!(!got.trusted);
    // a tampered template is refused
    let bad = fetch
        .clone()
        .with(&format!("{base}/whisper/compose.yml"), b"services: {}\n");
    let err = store::fetch_package(&bad, base, &e, "ex", false)
        .await
        .expect_err("tampered");
    assert!(err.contains("does not match"), "{err}");
}

#[test]
fn scrub_hides_tokens() {
    let t = scrub("HF_TOKEN=hf_abcdefghijk and sk-ant-api03-xxxxxxxxxxxx and abk_1234567890ab ok");
    assert!(
        !t.contains("abcdefghijk") && !t.contains("api03-xxxx") && !t.contains("1234567890ab"),
        "{t}"
    );
}

#[test]
fn wizard_drafts_parse_sources_and_export_zips() {
    use crate::wizard::*;
    assert_eq!(
        parse_image("nginx").unwrap(),
        ("docker.io".into(), "library/nginx".into(), "latest".into())
    );
    assert_eq!(
        parse_image("ghcr.io/a/b:v1@sha256:00").unwrap(),
        ("ghcr.io".into(), "a/b".into(), "sha256:00".into())
    );
    assert!(parse_image("bad image").is_err());
    assert_eq!(slug("Kokoro FastAPI (GPU)"), "kokoro-fastapi-gpu");
    let probe = ImageProbe {
        reference: "docker.io/x/tts:1.2".into(),
        digest: format!("sha256:{}", "a".repeat(64)),
        pinned: format!("docker.io/x/tts:1.2@sha256:{}", "a".repeat(64)),
        platforms: vec!["linux/amd64".into()],
        ports: vec!["8880/tcp".into()],
        licence: Some("MIT".into()),
        ..Default::default()
    };
    let d = draft_docker("x/tts:1.2", Some(probe), None);
    assert_eq!(d.manifest["version"], "1.2");
    assert_eq!(d.manifest["methods"][0]["ports"][0]["container"], 8880);
    assert!(crate::pins::image_pinned(
        d.manifest["methods"][0]["image"].as_str().unwrap()
    ));
    // Complete the draft the way the form would; it then validates as a private package.
    let mut m = d.manifest.clone();
    m["kinds"] = serde_json::json!(["tts"]);
    let r =
        crate::validate::validate_text(&m.to_string(), &Default::default(), crate::Mode::Private);
    assert!(r.ok, "{}", r.text());
    let assets = vec![
        ReleaseAsset {
            name: "srv-linux-x64.tar.gz".into(),
            url: "https://example.org/a.tgz".into(),
            sha256: Some("b".repeat(64)),
            size: 10,
        },
        ReleaseAsset {
            name: "srv-win-cuda-x64.zip".into(),
            url: "https://example.org/b.zip".into(),
            sha256: None,
            size: 10,
        },
        ReleaseAsset {
            name: "checksums.txt".into(),
            url: "https://example.org/c".into(),
            sha256: None,
            size: 1,
        },
    ];
    let d = draft_release("o", "srv", "v2.0", &assets);
    let a = d.manifest["methods"][0]["assets"].as_array().unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(a[1]["gpu"], "cuda");
    assert!(d.notes.iter().any(|n| n.contains("srv-win-cuda-x64.zip")));
    let mut files = crate::validate::Files::new();
    files.insert("abhoth-engine.json".into(), "{}".into());
    let z = zip_files("srv", &files).unwrap();
    assert_eq!(&z[..2], b"PK");
}

#[test]
fn index_hashes_ignore_crlf_line_ends() {
    let lf = [97u8, 10, 98, 10];
    let crlf = [97u8, 13, 10, 98, 13, 10];
    assert_eq!(
        crate::pins::text_sha256(&lf),
        crate::pins::text_sha256(&crlf)
    );
    assert_eq!(
        crate::pins::text_sha256(&lf),
        crate::pins::sha256_bytes(&lf)
    );
    // A lone CR is content, not a line end.
    assert_ne!(
        crate::pins::text_sha256(&[97u8, 13, 98]),
        crate::pins::text_sha256(&[97u8, 98])
    );
}
