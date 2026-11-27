use super::*;

/// A GPU as nvidia-smi's telemetry query reports it (the parser init uses).
fn gpu(index: u32, uuid: &str, name: &str, mib: u64) -> GpuSample {
    let mut f: Vec<String> = vec!["0".into(); telemetry::QUERY_FIELDS.len()];
    f[0] = uuid.into();
    f[1] = index.to_string();
    f[2] = name.into();
    f[7] = mib.to_string();
    telemetry::parse(&f.join(", "))
        .pop()
        .expect("a parsed sample")
}

fn machine() -> Detected {
    Detected {
        hostname: "gpu-box".into(),
        gpus: vec![
            gpu(
                1,
                "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "Second GPU",
                24576,
            ),
            gpu(
                0,
                "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "First GPU",
                32768,
            ),
        ],
        cpu: Some(
            serde_json::json!({"model": "Test CPU", "threads": 32, "cores": 16, "ramGib": 125.7}),
        ),
        gpu_note: None,
    }
}

fn opts(dir: &Path) -> InitOptions {
    InitOptions {
        dir: dir.join("router"),
        program: vec!["abhoth".into(), "router".into()],
        ..InitOptions::default()
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn writes_a_minimal_valid_router_with_this_machine_and_a_key() {
    let t = tempfile::tempdir().unwrap();
    let o = opts(t.path());
    let out = init(&o, Some(&machine())).unwrap();

    let text = std::fs::read_to_string(&out.toml).unwrap();
    assert!(text.contains("[control]\nenabled = false"), "{text}");
    assert!(text.contains("[inventory]\nversion = 1"), "{text}");
    let loaded = crate::control::load::load(&out.toml).unwrap();
    let c = &loaded.config;
    assert!(!c.control.enabled);
    assert_eq!(c.inventory.as_ref().map(|i| i.version), Some(1));
    assert_eq!(
        PathBuf::from(&c.router.state_dir),
        out.state_dir,
        "state_dir is absolute"
    );
    assert!(out.state_dir.is_absolute() && out.state_dir.is_dir());
    let s = &c.servers["local"];
    assert_eq!(s.exec, crate::control::schema::ExecKind::Local);
    assert_eq!(s.title, "gpu-box");
    // GPUs by nvidia-smi index (A = index 0), then the CPU.
    assert_eq!(
        c.cards.keys().collect::<Vec<_>>(),
        ["A", "B", "cpu"],
        "{text}"
    );
    assert_eq!(
        c.cards["A"].gpu_uuid,
        "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    );
    assert_eq!(c.cards["A"].vram_gib, Some(32.0));
    assert_eq!(c.cards["B"].vram_gib, Some(24.0));
    assert!(!c.cards["A"].managed, "nothing is managed until set up");
    assert!(c.cards["cpu"].is_cpu());
    assert_eq!(c.cards["cpu"].cores, Some(16));
    assert_eq!(c.cards["cpu"].ram_gib, Some(125.7));
    let issues = crate::control::validate::validate_loaded(&loaded);
    assert!(!crate::control::validate::has_errors(&issues), "{issues:?}");

    // router.json: the router's own loader takes it.
    let snap = crate::config::Snapshot::load(&out.json).unwrap();
    assert!(snap.config.deployments.is_empty());

    // The key: in its file only, and a working admin key in router.db.
    let kf = out.key_file.clone().unwrap();
    let key = std::fs::read_to_string(&kf).unwrap().trim().to_string();
    assert!(key.starts_with(orca_core::router_link::API_KEY_PREFIX));
    assert_eq!(out.key.as_deref(), Some(key.as_str()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&kf).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let store =
        Arc::new(crate::control::store::Store::open(&out.state_dir.join("router.db")).unwrap());
    let keys = crate::access::LocalKeys::open(store).unwrap();
    assert!(keys.check(&key).is_some(), "the key signs in");
    assert_eq!(keys.list().unwrap()[0].name, "admin");

    // The summary: start command, /admin/ URLs, never the key.
    let mut w = Vec::new();
    report(&out, false, &mut w).unwrap();
    let r = String::from_utf8(w).unwrap();
    assert!(!r.contains(&key), "{r}");
    assert!(r.contains("abhoth router --config"), "{r}");
    assert!(r.contains("--control"), "{r}");
    assert!(r.contains("http://127.0.0.1:8010/admin/"), "{r}");
    assert!(r.contains("http://gpu-box:8010/admin/"), "{r}");
    let mut w = Vec::new();
    report(&out, true, &mut w).unwrap();
    assert!(String::from_utf8(w).unwrap().contains(&key), "--print-key");
}

#[test]
fn refuses_to_overwrite_and_force_backs_everything_up() {
    let t = tempfile::tempdir().unwrap();
    let mut o = opts(t.path());
    o.services = vec![Service::Systemd];
    let first = init(&o, None).unwrap();
    let before_toml = std::fs::read_to_string(&first.toml).unwrap();
    std::fs::write(&first.toml, format!("{before_toml}# my edit\n")).unwrap();
    let old_key = first.key.clone().unwrap();

    // Again: refused, nothing touched.
    let e = init(&o, None).unwrap_err().to_string();
    assert!(
        e.contains("refusing to overwrite") && e.contains("--force"),
        "{e}"
    );
    assert!(
        e.contains("router.toml") && e.contains("router-admin-key"),
        "{e}"
    );
    assert!(
        std::fs::read_to_string(&first.toml)
            .unwrap()
            .ends_with("# my edit\n")
    );
    assert!(!o.dir.join("backups").exists());
    // One file is enough to refuse.
    let mut only_json = opts(&t.path().join("other"));
    std::fs::create_dir_all(&only_json.dir).unwrap();
    std::fs::write(only_json.dir.join("router.json"), "{}").unwrap();
    only_json.key_name = None;
    assert!(init(&only_json, None).is_err());
    assert!(!only_json.dir.join("router.toml").exists());

    // Forced: each file moved to backups/ first, then written anew.
    o.force = true;
    let second = init(&o, None).unwrap();
    let names: Vec<String> = second
        .backups
        .iter()
        .map(|(f, _)| f.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "router.toml",
            "router.json",
            "router-admin-key",
            "abhoth-router.service"
        ]
    );
    for (_, b) in &second.backups {
        assert!(b.starts_with(o.dir.join("backups")), "{}", b.display());
    }
    let toml_backup = &second.backups[0].1;
    assert!(
        std::fs::read_to_string(toml_backup)
            .unwrap()
            .ends_with("# my edit\n")
    );
    assert_eq!(
        std::fs::read_to_string(&second.backups[2].1)
            .unwrap()
            .trim(),
        old_key
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&second.backups[2].1)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the backed-up key stays owner-only"
        );
    }
    assert!(
        !std::fs::read_to_string(&second.toml)
            .unwrap()
            .contains("# my edit")
    );
    assert_ne!(second.key, first.key);
    // Forced twice in the same second: distinct backup names.
    let third = init(&o, None).unwrap();
    for (_, b) in &third.backups {
        assert!(b.exists());
    }
    let all: std::collections::BTreeSet<_> = std::fs::read_dir(o.dir.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(all.len(), 8);
}

#[test]
fn no_local_server_no_key_and_custom_paths() {
    let t = tempfile::tempdir().unwrap();
    let mut o = opts(t.path());
    o.add_local = false;
    o.key_name = None;
    o.state_dir = Some(t.path().join("elsewhere").join("state"));
    o.listen = "127.0.0.1:9010".into();
    o.id = "edge".into();
    let out = init(&o, Some(&machine())).unwrap();
    let c = crate::control::load::load(&out.toml).unwrap().config;
    assert!(c.servers.is_empty() && c.cards.is_empty());
    assert_eq!(c.router.id, "edge");
    assert_eq!(c.router.listen, "127.0.0.1:9010");
    assert!(t.path().join("elsewhere/state").is_dir());
    assert!(out.key_file.is_none() && out.key.is_none());
    assert!(!o.dir.join("secrets").exists());
    assert_eq!(out.admin_urls, ["http://127.0.0.1:9010/admin/"]);
    let mut w = Vec::new();
    report(&out, true, &mut w).unwrap();
    assert!(String::from_utf8(w).unwrap().contains("admin-key create"));
}

#[test]
fn a_machine_without_gpus_gets_its_cpu_only() {
    let t = tempfile::tempdir().unwrap();
    let d = Detected {
        hostname: String::new(),
        gpus: vec![],
        cpu: Some(serde_json::json!({"model": null, "threads": 8})),
        gpu_note: Some("no nvidia-smi on this machine".into()),
    };
    let out = init(&opts(t.path()), Some(&d)).unwrap();
    let c = crate::control::load::load(&out.toml).unwrap().config;
    assert_eq!(c.cards.keys().collect::<Vec<_>>(), ["cpu"]);
    assert_eq!(
        c.cards["cpu"].cores,
        Some(8),
        "threads when cores are unknown"
    );
    assert_eq!(c.cards["cpu"].ram_gib, None);
    assert_eq!(c.servers["local"].title, "this machine");
}

#[test]
fn paths_and_names_with_odd_characters_are_quoted() {
    let t = tempfile::tempdir().unwrap();
    let mut o = opts(&t.path().join("a \"quoted\" dir"));
    o.services = vec![Service::Compose, Service::Windows, Service::Systemd];
    let mut d = machine();
    d.gpus[0].name = "Weird \"GPU\" \\ name".into();
    d.hostname = "host \"x\"".into();
    let out = init(&o, Some(&d)).unwrap();
    let c = crate::control::load::load(&out.toml).unwrap().config;
    assert_eq!(c.cards["B"].label, "Weird \"GPU\" \\ name");
    assert_eq!(c.servers["local"].title, "host \"x\"");
    assert_eq!(out.services.len(), 3);
    let compose = std::fs::read_to_string(o.dir.join("compose.router.yaml")).unwrap();
    assert!(compose.contains("--control"), "{compose}");
    assert!(compose.contains("8010:8010"), "{compose}");
    let unit = std::fs::read_to_string(o.dir.join("abhoth-router.service")).unwrap();
    assert!(
        unit.contains(&format!("ExecStart={}", out.start_command)),
        "{unit}"
    );
    let win = std::fs::read_to_string(o.dir.join("windows-service.txt")).unwrap();
    assert!(win.contains("schtasks"), "{win}");
    for f in &out.services {
        assert!(
            !std::fs::read_to_string(f)
                .unwrap()
                .contains(out.key.as_ref().unwrap())
        );
    }
}

#[test]
fn bad_names_are_refused_before_anything_is_written() {
    let t = tempfile::tempdir().unwrap();
    let mut o = opts(t.path());
    o.key_name = Some("bad/name".into());
    assert!(init(&o, None).is_err());
    let mut o = opts(t.path());
    o.server_id = "no spaces".into();
    assert!(init(&o, Some(&machine())).is_err());
    assert!(!t.path().join("router").exists());
}

#[test]
fn card_ids_go_past_z() {
    assert_eq!(card_id(0), "A");
    assert_eq!(card_id(25), "Z");
    assert_eq!(card_id(26), "AA");
    assert_eq!(card_id(27), "AB");
}

#[test]
fn admin_urls_follow_the_listen_address() {
    assert_eq!(
        admin_urls("0.0.0.0:8010", "box"),
        ["http://127.0.0.1:8010/admin/", "http://box:8010/admin/"]
    );
    assert_eq!(
        admin_urls("10.0.0.2:8010", "box"),
        ["http://10.0.0.2:8010/admin/"]
    );
    assert_eq!(
        admin_urls("[::]:8010", ""),
        ["http://127.0.0.1:8010/admin/"]
    );
}

// ── the command line ─────────────────────────────────────────────────────────────────────────

fn cli(a: &[&str], interactive: bool, input: &str) -> (Result<i32>, String) {
    let mut out = Vec::new();
    let mut inp = std::io::Cursor::new(input.as_bytes().to_vec());
    let r = run_with(&args(a), interactive, &mut inp, &mut out, &machine);
    (r, String::from_utf8(out).unwrap())
}

#[test]
fn yes_takes_the_defaults_and_flags_without_asking() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("r");
    let d = dir.display().to_string();
    let (r, out) = cli(&[&d, "--yes", "--service", "systemd"], false, "");
    assert_eq!(r.unwrap(), 0, "{out}");
    assert!(!out.contains('?'), "no questions: {out}");
    assert!(out.contains("Start the router:"), "{out}");
    assert!(dir.join("abhoth-router.service").exists());
    let key = std::fs::read_to_string(dir.join("secrets/router-admin-key")).unwrap();
    assert!(!out.contains(key.trim()), "never printed: {out}");
    let c = crate::control::load::load(&dir.join("router.toml"))
        .unwrap()
        .config;
    assert_eq!(c.cards.len(), 3, "this machine by default");

    // Again without --force: refused, exit is an error.
    let (r, _) = cli(&[&d, "--yes"], false, "");
    assert!(r.unwrap_err().to_string().contains("refusing"));
    // --force --print-key: backed up, and the key shown.
    let (r, out) = cli(
        &[&d, "--yes", "--force", "--print-key", "--no-local"],
        false,
        "",
    );
    assert_eq!(r.unwrap(), 0);
    let key = std::fs::read_to_string(dir.join("secrets/router-admin-key")).unwrap();
    assert!(out.contains(&format!("admin key: {}", key.trim())), "{out}");
    assert!(out.contains("backed up"), "{out}");
}

#[test]
fn without_a_terminal_it_needs_yes() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("r").display().to_string();
    let (r, _) = cli(&[&d], false, "");
    assert!(r.unwrap_err().to_string().contains("--yes"));
    assert!(!t.path().join("r").exists());
    let (r, out) = cli(&["--help"], false, "");
    assert_eq!(r.unwrap(), 0);
    assert!(out.contains("--print-key"));
    let (r, _) = cli(&["--yes", "--service", "launchd"], false, "");
    assert!(r.is_err());
}

#[test]
fn interactive_asks_then_writes() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("asked");
    // folder, listen, add this machine, make a key, its name, service, go ahead
    let answers = format!("{}\n127.0.0.1:8111\nn\ny\nops\ncompose\n\n", dir.display());
    let (r, out) = cli(&[], true, &answers);
    assert_eq!(r.unwrap(), 0, "{out}");
    assert!(out.contains("This machine: First GPU (32 GiB)"), "{out}");
    let c = crate::control::load::load(&dir.join("router.toml"))
        .unwrap()
        .config;
    assert!(c.servers.is_empty(), "answered no");
    assert_eq!(c.router.listen, "127.0.0.1:8111");
    assert!(dir.join("compose.router.yaml").exists());
    assert!(out.contains("http://127.0.0.1:8111/admin/"), "{out}");

    // Saying no at the end writes nothing.
    let other = t.path().join("declined");
    let answers = format!("{}\n\n\nn\nnone\nn\n", other.display());
    let (r, out) = cli(&[], true, &answers);
    assert_eq!(r.unwrap(), 1, "{out}");
    assert!(!other.exists());
    // Running out of answers is an error, not a guess.
    let (r, _) = cli(&[], true, "");
    assert!(r.is_err());
}
