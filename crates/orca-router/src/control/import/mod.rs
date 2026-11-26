//! The importer (ROUTER-CONFIG.md 13.1): `router.toml` from DeepThought's current files.
//!
//! Inputs (read-only copies): `layoutctl.py` (its constants are read with [`pylit`], not
//! copied into code), `profiles.json` (schema v3), `spool/user/{saved,composed}.json`,
//! `gateway.json`, `ondemand.py` (guard paths), LiteLLM's `config.yaml` and qfn-lb's
//! `nginx.conf` (cross-checks), `state/live.json` and `state/offline.json` (the `router.db`
//! seed), optionally `nvidia-smi --query-gpu=uuid,name,memory.total --format=csv,noheader`.
//!
//! **Keys are never copied**: secrets become references the admin fills in. Host paths become
//! server `vars` so the file reads like the design example.

pub mod pylit;

use std::collections::BTreeMap;
use std::path::Path;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use super::schema::*;

/// The importer's inputs as text / JSON.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub layoutctl: String,
    pub ondemand_py: String,
    pub profiles: Value,
    pub saved: Value,
    pub composed: Value,
    pub gateway: Value,
    pub litellm_yaml: Option<String>,
    pub nginx: Option<String>,
    pub live: Option<Value>,
    pub offline: Option<Value>,
    pub nvidia_smi: Option<String>,
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

impl Sources {
    /// A directory laid out as the host's: `layouts/…` (layoutctl.py, profiles.json,
    /// gateway.json, ondemand.py, spool/user, state), plus `litellm-config.yaml`, `nginx.conf`
    /// and `nvidia-smi.csv` beside it.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let l = dir.join("layouts");
        let need =
            |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        Ok(Self {
            layoutctl: need(&l.join("layoutctl.py"))?,
            ondemand_py: std::fs::read_to_string(l.join("ondemand.py")).unwrap_or_default(),
            profiles: serde_json::from_str(&need(&l.join("profiles.json"))?)
                .map_err(|e| format!("profiles.json: {e}"))?,
            saved: read_json(&l.join("spool/user/saved.json")).unwrap_or(Value::Null),
            composed: read_json(&l.join("spool/user/composed.json")).unwrap_or(Value::Null),
            gateway: read_json(&l.join("gateway.json")).unwrap_or(Value::Null),
            litellm_yaml: std::fs::read_to_string(dir.join("litellm-config.yaml")).ok(),
            nginx: std::fs::read_to_string(dir.join("nginx.conf")).ok(),
            live: read_json(&l.join("state/live.json")),
            offline: read_json(&l.join("state/offline.json")),
            nvidia_smi: std::fs::read_to_string(dir.join("nvidia-smi.csv")).ok(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyChoice {
    /// `--api-key-file` (needs the flag in the engine: ninfer has none yet, see the report).
    #[default]
    FileFlag,
    /// Read inside the container by a wrapper; works without any engine change.
    Wrapper,
    /// layoutctl parity (`--api-key` in argv, read on the host by the shell).
    Argv,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The server id the host becomes (`deepthought`).
    pub server: String,
    /// Key delivery per engine; engines not listed get the default.
    pub keys: BTreeMap<String, KeyChoice>,
    /// Make catalog-model routes `cold = "on_demand"` (the cutover decision) even while
    /// gateway.json has on-demand off.
    pub enable_on_demand: bool,
    /// Builds that read `--api-key-file`, per engine (`key_delivery.file_flag_builds`): their
    /// instances get the flag, the others keep the wrapper.
    pub key_file_builds: BTreeMap<String, Vec<String>>,
}

/// What the importer produced.
#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    #[serde(skip)]
    pub config: ControlConfig,
    pub toml: String,
    pub seed: Seed,
    pub report: Vec<ReportLine>,
}

/// The `router.db` seed: the active layout and offline cards as the old tooling has them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Seed {
    pub server: String,
    pub active_layout: Option<String>,
    pub offline: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReportLine {
    /// `mapped` | `dropped` | `guess` | `todo` | `check`.
    pub kind: String,
    pub source: String,
    pub text: String,
}

#[derive(Default)]
struct Report(Vec<ReportLine>);

impl Report {
    fn add(&mut self, kind: &str, source: &str, text: impl Into<String>) {
        self.0.push(ReportLine {
            kind: kind.into(),
            source: source.into(),
            text: text.into(),
        });
    }
}

/// Replace the longest matching var value prefix with `{var:name}`.
fn varize(vars: &IndexMap<String, String>, s: &str) -> String {
    let mut best: Option<(&String, &String)> = None;
    for (k, v) in vars {
        if !v.is_empty()
            && (s == v || s.starts_with(&format!("{v}/")))
            && best.is_none_or(|(_, bv)| v.len() > bv.len())
        {
            best = Some((k, v));
        }
    }
    match best {
        Some((k, v)) => format!("{{var:{k}}}{}", &s[v.len()..]),
        None => s.to_string(),
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Pairs `(("k", v), …)` -> ordered map.
fn pairs(v: &Value) -> IndexMap<String, Value> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Option strings are templates in the config (`"{lanes}"`): literal braces are doubled.
fn lit(v: Value) -> Value {
    match v {
        Value::String(t) => Value::String(t.replace('{', "{{").replace('}', "}}")),
        other => other,
    }
}

/// Options with nulls split out (TOML has no null).
fn split_null(o: &Value) -> (IndexMap<String, Value>, Vec<String>) {
    let mut opts = IndexMap::new();
    let mut unset = Vec::new();
    if let Some(m) = o.as_object() {
        for (k, v) in m {
            if v.is_null() {
                unset.push(k.clone());
            } else {
                opts.insert(k.clone(), lit(v.clone()));
            }
        }
    }
    (opts, unset)
}

/// A container name template when all cards follow `<prefix><card_lc>`.
fn name_template(names: &[(String, String)]) -> Option<String> {
    let (c0, n0) = names.first()?;
    let prefix = n0.strip_suffix(&c0.to_lowercase())?;
    names
        .iter()
        .all(|(c, n)| *n == format!("{prefix}{}", c.to_lowercase()))
        .then(|| format!("{prefix}{{card_lc}}"))
}

fn key_delivery(choice: KeyChoice) -> KeyDelivery {
    KeyDelivery {
        mode: match choice {
            KeyChoice::FileFlag => KeyMode::FileFlag,
            KeyChoice::Wrapper => KeyMode::Wrapper,
            KeyChoice::Argv => KeyMode::Argv,
        },
        ..Default::default()
    }
}

/// Run the import.
pub fn import(src: &Sources, opt: &Options) -> Result<Imported, String> {
    let mut rep = Report::default();
    let k = pylit::constants(&src.layoutctl);
    let need = |n: &str| {
        k.get(n)
            .cloned()
            .ok_or_else(|| format!("layoutctl.py: constant {n} not found or not a literal"))
    };
    let server_id = if opt.server.is_empty() {
        "deepthought".to_string()
    } else {
        opt.server.clone()
    };
    let p = &src.profiles;
    if p["version"].as_i64() != Some(3) {
        rep.add(
            "check",
            "profiles.json",
            format!("schema version {} (the importer knows v3)", p["version"]),
        );
    }
    let srv_json = &p["server"];
    let cards_k = need("CARDS")?;
    let card_ids: Vec<String> = cards_k
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // ── server vars (host paths) ─────────────────────────────────────────────────────────
    let mut vars: IndexMap<String, String> = IndexMap::new();
    for (var, cst) in [
        ("qfn", "QFN"),
        ("ninfer", "NINFER"),
        ("llama_models", "LLAMA_MODELS"),
        ("llama_slots", "LLAMA_SLOTS"),
        ("pig_data", "PIG_DATA"),
    ] {
        if let Some(v) = k.get(cst).and_then(Value::as_str) {
            vars.insert(var.into(), v.into());
        }
    }
    if let Some(m) = srv_json["ninferModelDir"].as_str() {
        vars.insert("ninfer_models".into(), m.into());
    }
    if let Some(m) = k.get("MANIFEST").and_then(Value::as_str) {
        vars.insert("archive_manifest".into(), m.into());
    }
    let od = pylit::constants(&src.ondemand_py);
    if let Some(v) = od.get("BENCH_QUEUE").and_then(Value::as_str) {
        vars.insert("bench_queue".into(), v.into());
    }
    let host = k
        .get("HOST")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| srv_json["host"].as_str().map(String::from))
        .unwrap_or_else(|| "gpu-host".into());

    let mut c = ControlConfig {
        version: SCHEMA_VERSION,
        ..Default::default()
    };

    // ── router, control, secrets ─────────────────────────────────────────────────────────
    c.router.listen = "0.0.0.0:8010".into();
    c.router.state_dir = "/state".into();
    c.router.compat = CompatListeners {
        lb: Some("0.0.0.0:8092".into()),
        slots: Some("0.0.0.0:8099".into()),
        profiles: Some("0.0.0.0:8098".into()),
    };
    let rs = &src.gateway["router_settings"];
    if let Some(t) = rs["timeout"].as_u64() {
        c.router.limits.timeout_seconds = t;
    }
    if let Some(t) = rs["cooldown_time"].as_u64() {
        c.router.limits.cooldown_seconds = t;
    }
    rep.add("mapped", "gateway.json router_settings", "timeout and cooldown_time -> [router.limits]; num_retries 2 -> one failover before the first byte (D-R10); simple-shuffle -> placement");
    c.control.enabled = false;
    c.control.mode = ControlMode::Observe;
    c.secrets.insert(
        "engine_key".into(),
        SecretRef {
            file: Some("/run/secrets/engine-key".into()),
            env: None,
        },
    );
    c.secrets.insert(
        format!("ssh_{server_id}"),
        SecretRef {
            file: Some(format!("/run/secrets/ssh-{server_id}")),
            env: None,
        },
    );
    if let Some(kf) = k.get("KEY_FILE").and_then(Value::as_str) {
        rep.add("todo", "layoutctl KEY_FILE", format!("the engine key is in {} on the host: copy it to the router's secret file /run/secrets/engine-key (0600); never into the config", varize(&vars, kf)));
    }

    // ── server ───────────────────────────────────────────────────────────────────────────
    let mut server = Server {
        title: "DeepThought".into(),
        host: host.clone(),
        exec: ExecKind::Ssh,
        ssh: Some(SshSettings {
            user: "abhoth-exec".into(),
            port: 22,
            key: format!("secret:ssh_{server_id}"),
            host_key: "SHA256:REPLACE-WITH-THE-HOST-KEY-FINGERPRINT".into(),
            connect_timeout: 5,
            keepalive_seconds: 15,
        }),
        hostctl: HostctlMode::Strict,
        vars: vars.clone(),
        ready_timeout_seconds: 1500,
        ..Default::default()
    };
    rep.add("todo", "server", "fill ssh.host_key with the host's ed25519 fingerprint (ssh-keyscan | ssh-keygen -lf -) and install the hostctl allowlist (deploy/hostctl/README.md)");
    for l in ["WATCHDOG_LOCK", "START_LOCK"] {
        if let Some(v) = k.get(l).and_then(Value::as_str) {
            server.locks.push(v.into());
        }
    }
    server.ready = vec![
        Hook {
            builtin: Some("docker-up".into()),
            ..Default::default()
        },
        Hook {
            builtin: Some("mounted".into()),
            params: [("path".to_string(), Value::from("/mnt/.ix-apps"))]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        Hook {
            builtin: Some("gpus-visible".into()),
            ..Default::default()
        },
        Hook {
            command: Some("midclt-apps-settled".into()),
            timeout: Some(60),
            ..Default::default()
        },
    ];
    server.templates.insert(
        "midclt-apps-settled".into(),
        r#"midclt call app.query '[]' '{{"select":["name","state"]}}' | grep -vq DEPLOYING"#.into(),
    );
    rep.add(
        "mapped",
        "layoutctl wait_ready",
        "docker, the ix-apps mount, both cards by UUID and the TrueNAS middleware -> servers.ready",
    );
    let live_layout = src
        .live
        .as_ref()
        .and_then(|l| l["layout"].as_str())
        .map(String::from);
    server.state = Some(ServerState {
        initial_layout: live_layout.clone(),
    });
    server.gpu_power = None;
    server.ignore_holders = vec!["clockctl".into(), "gpu-power".into(), "ha-bridge".into()];
    rep.add("guess", "layoutctl gpu_containers", "utility-only containers (clockctl, gpu-power, ha-bridge) listed as ignore_holders; layoutctl skips any container without compute capability");

    // ── cards ────────────────────────────────────────────────────────────────────────────
    let vram = 32.0;
    for cid in &card_ids {
        let d = &cards_k[cid];
        c.cards.insert(
            cid.clone(),
            Card {
                server: server_id.clone(),
                gpu_uuid: s(&d["uuid"]),
                label: s(&d["label"]),
                vram_gib: Some(vram),
                reserve_gib: 1.5,
                cpus: d["cpus"].as_str().map(String::from),
                clocks: None,
                managed: true,
                on_demand: true,
            },
        );
    }
    rep.add(
        "mapped",
        "layoutctl CARDS",
        format!(
            "cards {} (UUID, CPU set, label); VRAM 32 GiB and the 1.5 GiB reserve from profilecat",
            card_ids.join(", ")
        ),
    );
    rep.add("todo", "cards", "clock expectations (A boosts to 1597 MHz unlocked, B app clocks 877,1380) come from the survey, not from files: add clocks = {…} per card after checking nvidia-smi");
    if let Some(csv) = &src.nvidia_smi {
        let known: Vec<String> = c
            .cards
            .values()
            .map(|x| x.gpu_uuid.to_lowercase())
            .collect();
        let mut n = 0;
        for line in csv.lines() {
            let parts: Vec<&str> = line.split(',').map(str::trim).collect();
            let Some(uuid) = parts.first().filter(|u| u.starts_with("GPU-")) else {
                continue;
            };
            if known.contains(&uuid.to_lowercase()) {
                continue;
            }
            n += 1;
            let id = format!("X{n}");
            c.cards.insert(
                id.clone(),
                Card {
                    server: server_id.clone(),
                    gpu_uuid: uuid.to_string(),
                    label: parts.get(1).unwrap_or(&"").to_string(),
                    vram_gib: None,
                    reserve_gib: 1.5,
                    cpus: None,
                    clocks: None,
                    managed: false,
                    on_demand: false,
                },
            );
            rep.add(
                "mapped",
                "nvidia-smi",
                format!(
                    "card {id}: {} (not a layout card: managed = false, never touched)",
                    parts.get(1).unwrap_or(&"?")
                ),
            );
        }
    }

    // ── engines ──────────────────────────────────────────────────────────────────────────
    let model_ids = k.get("MODEL_IDS").cloned().unwrap_or(Value::Null);
    let health_ok = k.get("HEALTH_OK").cloned().unwrap_or(Value::Null);
    let ctx = k.get("ENGINE_CTX").cloned().unwrap_or(Value::Null);
    let pe = &p["engines"];
    let est = &srv_json["estimates"];
    let gw = &src.gateway;
    let per_card = |eng: &str, field: &str| -> Vec<(String, Value)> {
        card_ids
            .iter()
            .filter_map(|cid| Some((cid.clone(), cards_k[cid][eng].get(field)?.clone())))
            .collect()
    };
    let ports = |eng: &str| -> PerCard<u16> {
        PerCard::Each(
            per_card(eng, "port")
                .into_iter()
                .filter_map(|(cid, v)| Some((cid, v.as_u64()? as u16)))
                .collect(),
        )
    };
    let containers = |eng: &str| -> Vec<(String, String)> {
        per_card(eng, "container")
            .into_iter()
            .map(|(c, v)| (c, s(&v)))
            .collect()
    };
    let labels: IndexMap<String, String> = [
        ("layouts.managed", "1"),
        ("layouts.card", "{card}"),
        ("layouts.spec", "{spec_json}"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    let health = |eng: &str| HealthSpec {
        expect_body: health_ok.get(eng).and_then(Value::as_str).map(String::from),
        ..Default::default()
    };
    let key_for = |eng: &str| {
        let mut k = key_delivery(opt.keys.get(eng).copied().unwrap_or_default());
        k.file_flag_builds = opt.key_file_builds.get(eng).cloned().unwrap_or_default();
        k
    };
    let est_card = |eng: &str| -> Option<PerCard<u64>> {
        let all = est[eng].as_u64();
        let each: IndexMap<String, u64> = card_ids
            .iter()
            .filter_map(|cid| Some((cid.clone(), est[format!("{eng}{cid}")].as_u64().or(all)?)))
            .collect();
        (!each.is_empty()).then_some(PerCard::Each(each))
    };
    // Request params (gateway.json -> engine params).
    let central_effort = gw["litellmParams"]["reasoning_effort"]
        .as_str()
        .map(String::from);
    let params_for = |eng: &str| -> RequestParams {
        let ep = &gw["engineParams"][eng];
        let mut rp = RequestParams {
            effort_levels: gw["effortLevels"].get(eng).map(strs),
            thinking_off: gw["thinkingOffEffort"][eng].as_str().map(String::from),
            drop: strs(&ep["additional_drop_params"]),
            ..Default::default()
        };
        // A null in engineParams leaves the central default out for that engine.
        rp.default_effort = if ep.get("reasoning_effort").is_some_and(Value::is_null) {
            None
        } else {
            central_effort.clone()
        };
        rp
    };
    rep.add(
        "mapped",
        "gateway.json litellmParams/engineParams/effortLevels/thinkingOffEffort",
        "-> engines.*.params (modelEffortLevels / modelThinkingOffEffort -> artifact params)",
    );

    let ninfer_builds: IndexMap<String, String> = {
        let mut b: IndexMap<String, String> = k
            .get("NINFER_BUILDS")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(a, v)| (a.clone(), s(v))).collect())
            .unwrap_or_default();
        for src_b in [&srv_json["ninferBuilds"], &pe["ninfer"]["builds"]] {
            if let Some(m) = src_b.as_object() {
                for (a, v) in m {
                    b.insert(a.clone(), s(v));
                }
            }
        }
        b.into_iter().map(|(a, v)| (a, varize(&vars, &v))).collect()
    };
    let defaults_of = |cst: &str| -> IndexMap<String, Value> {
        pairs(k.get(cst).unwrap_or(&Value::Null))
            .into_iter()
            .map(|(a, v)| {
                (
                    a,
                    if v.as_str() == Some("slots") {
                        Value::from("{lanes}")
                    } else {
                        lit(v)
                    },
                )
            })
            .collect()
    };
    let sw = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    if pe.get("ninfer").is_some() {
        let mut fixed = strs(k.get("NINFER_FIXED").unwrap_or(&Value::Null));
        fixed.push("api-key-file".into());
        let e = Engine {
            title: s(&pe["ninfer"]["title"]),
            api: ApiKind::OpenaiChat,
            driver: DriverKind::Docker,
            image: k
                .get("NINFER_IMAGE")
                .and_then(Value::as_str)
                .map(String::from),
            container: name_template(&containers("ninfer")),
            port: ports("ninfer"),
            key: Some("secret:engine_key".into()),
            key_delivery: key_for("ninfer"),
            served_id: s(&model_ids["ninfer"]),
            max_context: ctx["ninfer"].as_u64(),
            health: health("ninfer"),
            start_estimate_seconds: est_card("ninfer"),
            idle_probe: Some(IdleProbe {
                path: "/slots".into(),
                busy_when: "any(.is_processing)".into(),
            }),
            request_log: Some("{var:ninfer}/logs/req-layout-{card_lc}.jsonl".into()),
            tool_parser: Some("none".into()),
            labels: labels.clone(),
            run: Some(RunSpec {
                docker: sw(&[
                    "--restart",
                    "no",
                    "--runtime=nvidia",
                    "--cpuset-cpus={cpus}",
                    "-e",
                    "NVIDIA_VISIBLE_DEVICES={gpu_uuid}",
                    "-e",
                    "NVIDIA_DRIVER_CAPABILITIES=compute,utility",
                    "{env_args}",
                    "{mems_args}",
                    "--shm-size",
                    "2g",
                    "--user",
                    "950:950",
                    "-p",
                    "{port}:{port}",
                    "{label_args}",
                    "{key_mount_args}",
                    "-v",
                    "{build_dir}:/app:ro",
                    "-v",
                    "{model_dir}:/models:ro",
                    "-v",
                    "{var:ninfer}/logs:/logs",
                    "{image}",
                    "/app/ninfer-serve",
                    "/models/{model_file}",
                    "--host",
                    "0.0.0.0",
                    "--port",
                    "{port}",
                    "{key_args}",
                    "--model-id",
                    "{served_id}",
                    "{options}",
                    "--request-log-jsonl",
                    "/logs/req-layout-{card_lc}.jsonl",
                ]),
                fixed,
                pop: vec!["alias".into()],
                ..Default::default()
            }),
            model_dir: Some("{var:ninfer_models}".into()),
            spec_label: SpecLabel {
                keys: sw(&[
                    "engine", "slots", "model", "build", "options", "env", "mems",
                ]),
                omit_build: Some("stock".into()),
            },
            defaults: defaults_of("NINFER_DEFAULTS"),
            builds: ninfer_builds,
            default_build: Some("stock".into()),
            params: params_for("ninfer"),
            restart_if_peer_starts: vec!["strata".into()],
            ..Default::default()
        };
        if opt.keys.get("ninfer").copied().unwrap_or_default() == KeyChoice::FileFlag {
            rep.add("todo", "engines.ninfer.key_delivery", "file_flag needs a ninfer build with `--api-key-file` (our fork, 2026-11-26) on every profile: prefer wrapper plus `--key-file-build ninfer=<build>` for the builds that have it");
        }
        if let Some(b) = opt.key_file_builds.get("ninfer") {
            rep.add(
                "mapped",
                "--key-file-build",
                format!("ninfer builds with --api-key-file: {}", b.join(", ")),
            );
        }
        c.engines.insert("ninfer".into(), e);
        rep.add(
            "mapped",
            "layoutctl start_ninfer, NINFER_DEFAULTS/FIXED/BUILDS/IMAGE",
            "engines.ninfer (docker words in layoutctl's order; container names kept for rollback)",
        );
        rep.add("mapped", "layoutctl restart_set", "a ninfer server that stays while Strata (re)starts restarts after it -> restart_if_peer_starts = [\"strata\"]");
    }
    if pe.get("llamacpp").is_some() {
        let mut builds: IndexMap<String, String> = k
            .get("LLAMA_BUILDS")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(a, v)| (a.clone(), s(v))).collect())
            .unwrap_or_default();
        if let Some(m) = pe["llamacpp"]["builds"].as_object() {
            for (a, v) in m {
                builds.insert(a.clone(), s(v));
            }
        }
        let mut fixed = strs(k.get("LLAMA_FIXED").unwrap_or(&Value::Null));
        fixed.push("api-key-file".into());
        let e = Engine {
            title: s(&pe["llamacpp"]["title"]),
            driver: DriverKind::Docker,
            image: Some("{build_image}".into()),
            container: name_template(&containers("llamacpp")),
            port: ports("llamacpp"),
            key: Some("secret:engine_key".into()),
            key_delivery: key_for("llamacpp"),
            served_id: s(&model_ids["llamacpp"]),
            max_context: ctx["llamacpp"].as_u64(),
            health: health("llamacpp"),
            start_estimate_seconds: est_card("llamacpp"),
            idle_probe: Some(IdleProbe {
                path: "/slots".into(),
                busy_when: "any(.is_processing)".into(),
            }),
            tool_parser: Some("auto".into()),
            labels: labels.clone(),
            run: Some(RunSpec {
                docker: sw(&[
                    "--restart",
                    "no",
                    "--runtime=nvidia",
                    "--cpuset-cpus={cpus}",
                    "--cpuset-mems=0,1",
                    "-e",
                    "NVIDIA_VISIBLE_DEVICES={gpu_uuid}",
                    "-e",
                    "NVIDIA_DRIVER_CAPABILITIES=compute,utility",
                    "--shm-size",
                    "2g",
                    "-p",
                    "{port}:{port}",
                    "{label_args}",
                    "{key_mount_args}",
                    "-v",
                    "{model_parent}:/models:ro",
                    "-v",
                    "{var:llama_slots}:/slots",
                    "--entrypoint",
                    "/app/llama-server",
                    "{image}",
                    "--model",
                    "/models/{model_basename}",
                    "--alias",
                    "{served_id}",
                    "--host",
                    "0.0.0.0",
                    "--port",
                    "{port}",
                    "--n-gpu-layers",
                    "all",
                    "--flash-attn",
                    "on",
                    "--parallel",
                    "{lanes}",
                    "{opt_args:ctx-per-slot}",
                    "{options}",
                    "--slot-save-path",
                    "/slots",
                    "--metrics",
                    "--perf",
                    "{opt_args:mmproj}",
                    "{key_args}",
                ]),
                fixed,
                pop: vec!["alias".into()],
                option_args: [
                    (
                        "ctx-per-slot".to_string(),
                        sw(&["--ctx-size", "{value_x_lanes}"]),
                    ),
                    ("mmproj".to_string(), sw(&["--mmproj", "/models/{value}"])),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            }),
            model_dir: Some("{var:llama_models}".into()),
            spec_label: SpecLabel {
                keys: sw(&["engine", "slots", "model", "build", "options"]),
                omit_build: None,
            },
            defaults: defaults_of("LLAMA_DEFAULTS"),
            builds,
            default_build: pe["llamacpp"]["defaultBuild"]
                .as_str()
                .or(k.get("LLAMA_DEFAULT_BUILD").and_then(Value::as_str))
                .map(String::from),
            params: params_for("llamacpp"),
            ..Default::default()
        };
        c.engines.insert("llamacpp".into(), e);
        rep.add(
            "mapped",
            "layoutctl start_llamacpp, LLAMA_DEFAULTS/FIXED/BUILDS",
            "engines.llamacpp (ctx-per-slot x lanes -> --ctx-size; mmproj after --perf)",
        );
        rep.add("check", "layoutctl start_llamacpp", "layoutctl adds --mmproj only when the file exists next to the model; the router adds it whenever the option is set and the plan's preconditions check the file");
        rep.add("check", "layoutctl restart_set", "llama.cpp does not restart after a Strata start (as layoutctl: only ninfer pins NUMA node 1 memory); add restart_if_peer_starts = [\"strata\"] if that changes");
    }
    if pe.get("strata").is_some() {
        let svc: IndexMap<String, String> = per_card("strata", "service")
            .into_iter()
            .map(|(c, v)| (c, s(&v)))
            .collect();
        let cont: IndexMap<String, String> = containers("strata").into_iter().collect();
        let cfg_tpl = {
            let cfgs: Vec<(String, String)> = per_card("strata", "cfg")
                .into_iter()
                .map(|(c, v)| (c, s(&v)))
                .collect();
            let t = cfgs.first().map(|(c0, f0)| {
                f0.replacen(
                    &format!("-{}.json", c0.to_lowercase()),
                    "-{card_lc}.json",
                    1,
                )
            });
            match t {
                Some(t)
                    if cfgs
                        .iter()
                        .all(|(c, f)| t.replace("{card_lc}", &c.to_lowercase()) == *f) =>
                {
                    varize(&vars, &t)
                }
                _ => "{var:qfn}/sharedarena/strata-sa-{card_lc}.json".into(),
            }
        };
        let need_gib = srv_json["strataNode0FreeGiB"]
            .as_f64()
            .or(k.get("STRATA_NODE0_FREE_GIB").and_then(Value::as_f64))
            .unwrap_or(64.0);
        let e = Engine {
            title: s(&pe["strata"]["title"]),
            driver: DriverKind::Compose,
            compose: Some(ComposeSpec {
                project: "ix-qfn-llm".into(),
                file: "$(ls -d /mnt/.ix-apps/app_configs/qfn-llm/versions/*/ | sort -V | tail -n 1)templates/rendered/docker-compose.yaml".into(),
                service: PerCard::Each(svc),
                container: PerCard::Each(cont),
                env_file: None,
            }),
            port: ports("strata"),
            served_id: s(&model_ids["strata"]),
            max_context: ctx["strata"].as_u64(),
            health: health("strata"),
            stop: StopSpec { mode: StopMode::Stop, timeout_seconds: 30, ..Default::default() },
            start_estimate_seconds: est_card("strata"),
            start_order: StartOrder::Sequential,
            share: Some("expert-arena".into()),
            spec_label: SpecLabel { keys: sw(&["engine", "slots"]), omit_build: None },
            idle_probe: Some(IdleProbe { path: "/slots".into(), busy_when: "any(.is_processing)".into() }),
            before_start: vec![
                Hook {
                    builtin: Some("numa-room".into()),
                    params: [("node".to_string(), Value::from(0)), ("need_gib".into(), Value::from(need_gib)), ("drop_caches".into(), Value::from(true))]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                },
                Hook {
                    builtin: Some("json-set".into()),
                    params: [
                        ("file".to_string(), Value::from(cfg_tpl)),
                        ("key".into(), Value::from("parallel")),
                        ("value".into(), Value::from("{lanes}")),
                        ("type".into(), Value::from("int")),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            ],
            params: params_for("strata"),
            ..Default::default()
        };
        c.engines.insert("strata".into(), e);
        rep.add("mapped", "layoutctl start_strata/stop_card/set_strata_slots/node0_room", "engines.strata: compose services, docker stop -t 30, parallel via json-set, page-cache drop when node 0 is short, one card at a time (start_order) with the arena coupling (share)");
        rep.add("check", "layoutctl rendered_compose", "the compose file is the newest TrueNAS rendered version, evaluated on the host by a $(…) expression in the allowlisted command");
    }

    // ── engine profiles ──────────────────────────────────────────────────────────────────
    if let Some(m) = p["engineProfiles"].as_object() {
        for (name, ep) in m {
            let eng = s(&ep["engine"]);
            let Some(e) = c.engines.get_mut(&eng) else {
                rep.add(
                    "dropped",
                    "profiles.json engineProfiles",
                    format!("{name}: unknown engine {eng}"),
                );
                continue;
            };
            let (options, unset) = split_null(&ep["options"]);
            let env: IndexMap<String, String> = ep["env"]
                .as_object()
                .map(|m| m.iter().map(|(a, v)| (a.clone(), s(v))).collect())
                .unwrap_or_default();
            e.profiles.insert(
                name.clone(),
                EngineProfile {
                    title: s(&ep["title"]),
                    description: ep["description"].as_str().map(String::from),
                    lanes: ep["slots"].as_u64().unwrap_or(1) as u32,
                    ctx: ep["ctx"].as_u64(),
                    vram_gib: ep["vramGiB"].as_f64().unwrap_or(0.0),
                    build: ep["build"].as_str().map(String::from),
                    mems: ep["mems"].as_str().map(String::from),
                    options,
                    unset,
                    env,
                    notes: ep["notes"].as_str().map(String::from),
                    source: None,
                },
            );
        }
    }

    // ── models ───────────────────────────────────────────────────────────────────────────
    let model_effort = &gw["modelEffortLevels"];
    let model_thinking = &gw["modelThinkingOffEffort"];
    if let Some(m) = p["models"].as_object() {
        for (mid, mv) in m {
            let mut model = Model {
                title: s(&mv["title"]),
                description: mv["description"].as_str().map(String::from),
                vision: mv["vision"].as_bool().unwrap_or(true),
                artifacts: IndexMap::new(),
            };
            if let Some(arts) = mv["artifacts"].as_object() {
                for (eng, a) in arts {
                    let a = if a.is_string() {
                        serde_json::json!({ "file": a })
                    } else {
                        a.clone()
                    };
                    let (options, unset) = split_null(&a["options"]);
                    let id = a["id"].as_str().map(String::from);
                    let mut params = RequestParams::default();
                    if let Some(id) = &id {
                        if let Some(l) = model_effort.get(id) {
                            params.effort_levels = Some(strs(l));
                        }
                        if let Some(t) = model_thinking[id].as_str() {
                            params.thinking_off = Some(t.into());
                        }
                    }
                    model.artifacts.insert(
                        eng.clone(),
                        Artifact {
                            file: a["file"]
                                .as_str()
                                .map(|f| {
                                    if f.starts_with('/') {
                                        varize(&vars, f)
                                    } else {
                                        f.to_string()
                                    }
                                })
                                .unwrap_or_default(),
                            served_id: id,
                            vram_gib: a["vramGiB"]
                                .as_f64()
                                .or(mv["vramGiB"].as_f64())
                                .unwrap_or(0.0),
                            profiles: strs(&a["engineProfiles"]),
                            restore: (eng == "ninfer" && vars.contains_key("archive_manifest"))
                                .then(|| Restore {
                                    from_manifest: "{var:archive_manifest}".into(),
                                }),
                            options,
                            unset,
                            params: (params != RequestParams::default()).then_some(params),
                            default_profile: None,
                        },
                    );
                }
            }
            c.models.insert(mid.clone(), model);
        }
    }
    rep.add("mapped", "profiles.json models", "-> [models.*] with one artifact per engine; ninfer artifacts restore from the archive manifest (sha256)");

    // ── layouts (profiles with cards), client profiles ───────────────────────────────────
    let aliases = p["aliases"].as_object().cloned().unwrap_or_default();
    let layout_aliases = p["layoutAliases"].as_object().cloned().unwrap_or_default();
    let add_layout = |c: &mut ControlConfig,
                      rep: &mut Report,
                      name: &str,
                      pv: &Value,
                      source: Option<&str>| {
        let Some(cards) = pv["cards"].as_object() else {
            c.client_profiles.insert(
                name.into(),
                ClientProfile {
                    title: s(&pv["title"]),
                    description: pv["description"].as_str().map(String::from),
                    mode: pv["mode"].as_str().map(String::from),
                    client: pv["client"]
                        .as_object()
                        .map(|m| m.iter().map(|(a, v)| (a.clone(), v.clone())).collect())
                        .unwrap_or_default(),
                    extra: pv["notes"]
                        .as_str()
                        .map(|n| {
                            [("notes".to_string(), Value::from(n))]
                                .into_iter()
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            );
            rep.add(
                "mapped",
                "profiles.json profiles",
                format!("{name}: no cards -> [client_profiles.{name}]"),
            );
            return;
        };
        let mut lc = IndexMap::new();
        for cid in &card_ids {
            let v = cards.get(cid).cloned().unwrap_or(Value::from("off"));
            let card = match &v {
                Value::String(w) => LayoutCard::Word(w.clone()),
                Value::Object(o)
                    if o.get("engine")
                        .and_then(Value::as_str)
                        .is_none_or(|e| e == "off") =>
                {
                    LayoutCard::Word("off".into())
                }
                Value::Object(o) => LayoutCard::Spec(CardSpec {
                    engine: s(&o["engine"]),
                    profile: s(&o["engineProfile"]),
                    model: s(&o["model"]),
                }),
                _ => LayoutCard::Word("off".into()),
            };
            lc.insert(cid.clone(), card);
        }
        let mut al: Vec<String> = aliases
            .iter()
            .chain(layout_aliases.iter())
            .filter(|(_, to)| to.as_str() == Some(name))
            .map(|(a, _)| a.clone())
            .collect();
        al.sort();
        al.dedup();
        let tuning = pv.get("tuning").or_else(|| pv.get("client"));
        let mut client: IndexMap<String, Value> = tuning
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(a, v)| (a.clone(), v.clone())).collect())
            .unwrap_or_default();
        // Endpoint lists generated from the layout carry host addresses; the profiles shim
        // regenerates them from the config.
        let before = client.len();
        client.retain(|k, _| !matches!(k.as_str(), "slots.registryUrl" | "slots.copies"));
        if client.len() != before {
            rep.add("dropped", &format!("profiles.json profiles.{name}.tuning"), "slots.registryUrl / slots.copies: generated by the profiles shim from the active layout");
        }
        let mut extra = IndexMap::new();
        if let Some(n) = pv["notes"].as_str() {
            extra.insert("notes".into(), Value::from(n));
        }
        c.layouts.insert(
            name.into(),
            Layout {
                title: s(&pv["title"]),
                description: pv["description"].as_str().map(String::from),
                mode: pv["mode"].as_str().map(String::from),
                cards: lc,
                client,
                aliases: al,
                source: source.map(String::from),
                force: pv["force"].as_bool().unwrap_or(false),
                hidden: source == Some("composed"),
                extra,
            },
        );
    };
    if let Some(m) = p["profiles"].as_object() {
        for (name, pv) in m {
            add_layout(&mut c, &mut rep, name, pv, None);
        }
    }
    for (srcv, label) in [(&src.saved, "user"), (&src.composed, "composed")] {
        if let Some(m) = srcv.as_object() {
            for (name, pv) in m {
                if !c.layouts.contains_key(name) {
                    add_layout(&mut c, &mut rep, name, pv, Some(label));
                }
            }
        }
    }
    rep.add(
        "mapped",
        "profiles.json profiles + spool/user",
        format!(
            "{} layouts, {} client profiles",
            c.layouts.len(),
            c.client_profiles.len()
        ),
    );

    // ── routes ───────────────────────────────────────────────────────────────────────────
    let mut routes: IndexMap<String, Route> = IndexMap::new();
    for a in strs(&gw["aliases"]) {
        routes.insert(
            a,
            Route {
                targets: vec![Target {
                    pool: Some("live".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
    }
    if let Some(m) = gw["pinned"].as_object() {
        for (name, card) in m {
            let card = s(card);
            let mut t = vec![Target {
                card: Some(card.clone()),
                ..Default::default()
            }];
            for other in card_ids.iter().filter(|x| **x != card) {
                t.push(Target {
                    card: Some(other.clone()),
                    fallback: true,
                    ..Default::default()
                });
            }
            routes.insert(
                name.clone(),
                Route {
                    targets: t,
                    ..Default::default()
                },
            );
        }
    }
    rep.add("mapped", "gateway.json aliases / pinned", "generic aliases -> { pool = \"live\" } (they never switch); pinned -a/-b -> { card } with the other card as fallback");
    // modelAliases: alias -> served id -> the catalog models serving it.
    if let Some(m) = gw["modelAliases"].as_object() {
        for (alias, sid) in m {
            let sid = s(sid);
            let models: Vec<String> = c
                .models
                .iter()
                .filter(|(_, mv)| {
                    mv.artifacts
                        .values()
                        .any(|a| a.served_id.as_deref() == Some(sid.as_str()))
                })
                .map(|(k, _)| k.clone())
                .collect();
            if models.is_empty() {
                rep.add(
                    "dropped",
                    "gateway.json modelAliases",
                    format!("{alias}: no catalog model serves {sid}"),
                );
                continue;
            }
            routes.insert(
                alias.clone(),
                Route {
                    targets: models
                        .iter()
                        .map(|m| Target {
                            model: Some(m.clone()),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
            );
        }
    }
    // Catalog names (on-demand).
    let ond = &gw["onDemand"];
    let od_enabled = ond["enabled"].as_bool().unwrap_or(false) || opt.enable_on_demand;
    let exclude = strs(&ond["exclude"]);
    let on_demand_cards: Vec<String> = card_ids.clone();
    for (mid, mv) in &c.models {
        if routes.contains_key(mid) {
            continue;
        }
        let pick = ond["picks"]
            .get(mid)
            .map(|pk| Pick {
                engine: s(&pk["engine"]),
                profile: pk["engineProfile"].as_str().map(String::from),
            })
            .or_else(|| {
                // Default pick: the engine and profile of its first built-in x2 layout.
                c.layouts
                    .values()
                    .filter(|l| l.source.is_none())
                    .find_map(|l| {
                        resolve_cards(l).into_iter().find_map(|(_, sp)| {
                            sp.filter(|sp| sp.model == *mid).map(|sp| Pick {
                                engine: sp.engine,
                                profile: Some(sp.profile),
                            })
                        })
                    })
            });
        let excluded = exclude.contains(mid);
        let route = if od_enabled && !excluded && pick.is_some() {
            Route {
                targets: vec![Target {
                    model: Some(mid.clone()),
                    ..Default::default()
                }],
                cold: Some(ColdPolicy::OnDemand),
                on_demand: Some(RouteOnDemand {
                    layout_from: "pick".into(),
                    pick,
                    cards: on_demand_cards.clone(),
                    access: Some("admins".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        } else {
            Route {
                targets: vec![Target {
                    model: Some(mid.clone()),
                    ..Default::default()
                }],
                cold: Some(ColdPolicy::Reject),
                ..Default::default()
            }
        };
        let _ = mv;
        routes.insert(mid.clone(), route);
    }
    rep.add(
        if od_enabled { "mapped" } else { "check" },
        "gateway.json onDemand",
        if od_enabled {
            format!("catalog names -> cold = \"on_demand\" (switch: admins only; issue `switch` keys to the callers that should), except {:?} (reject)", exclude)
        } else {
            "on-demand is off in gateway.json: catalog names -> cold = \"reject\" (re-run with --enable-on-demand at cutover)".to_string()
        },
    );
    c.routes = routes;
    c.route_defaults = RouteDefaults::default();
    c.on_demand = OnDemandSection {
        queue_seconds: ond["queueSeconds"].as_u64().unwrap_or(300),
        min_stay_seconds: ond["minStaySeconds"].as_u64().unwrap_or(300),
        idle_grace_seconds: ond["idleGraceSeconds"].as_u64().unwrap_or(10),
        switch_timeout_seconds: ond["switchTimeoutSeconds"].as_u64().unwrap_or(900),
        failure_hold_seconds: ond["failureHoldSeconds"].as_u64().unwrap_or(60),
        allow_both_cards: ond["allowBothCards"].as_bool().unwrap_or(false),
        stream_note: ond["streamNote"].as_str().unwrap_or("reasoning").into(),
        ..Default::default()
    };
    if let Some(first) = strs(&gw["aliases"]).first() {
        server.post_check = Some(PostCheck {
            route: first.clone(),
            max_tokens: 64,
        });
    }
    c.servers.insert(server_id.clone(), server);

    // ── upstreams: LiteLLM kept for the rollback window ──────────────────────────────────
    c.upstreams.insert(
        "litellm-legacy".into(),
        UpstreamDef {
            kind: UpstreamKind::Router,
            base_url: format!("http://{host}:8010"),
            flavour: Some("litellm".into()),
            health_path: Some("/health/liveliness".into()),
            enabled: false,
            ..Default::default()
        },
    );

    // ── guards ───────────────────────────────────────────────────────────────────────────
    if let Some(q) = k.get("QUIET").and_then(Value::as_array)
        && q.len() == 2
    {
        let hm = |v: &Value| {
            format!(
                "{:02}:{:02}",
                v[0].as_u64().unwrap_or(0),
                v[1].as_u64().unwrap_or(0)
            )
        };
        c.guards.insert(
            "quiet-window".into(),
            Guard {
                kind: GuardKind::Schedule,
                server: None,
                windows: vec![format!("{}-{}", hm(&q[0]), hm(&q[1]))],
                blocks: vec![GuardAction::Switch, GuardAction::OnDemand],
                ..guard()
            },
        );
    }
    c.guards.insert(
        "video".into(),
        Guard {
            kind: GuardKind::Process,
            server: Some(server_id.clone()),
            matcher: Some(ProcessMatch {
                argv_any: "run_daily.sh".into(),
                exe: sw(&["bash", "sh", "dash", "run_daily.sh"]),
                label_arg: Some("--channel".into()),
                label_default: Some("ai-news".into()),
            }),
            blocks: vec![GuardAction::OnDemand],
            retry_seconds: 300,
            ..guard()
        },
    );
    if vars.contains_key("bench_queue") {
        c.guards.insert(
            "bench".into(),
            Guard {
                kind: GuardKind::JsonFile,
                server: Some(server_id.clone()),
                path: Some("{var:bench_queue}".into()),
                active_when: sw(&["running", "current"]),
                blocks: vec![GuardAction::OnDemand],
                retry_seconds: 600,
                ..guard()
            },
        );
    }
    if let Some(m) = k.get("MUSICMODE").and_then(Value::as_str) {
        c.guards.insert(
            "music".into(),
            Guard {
                kind: GuardKind::FileExists,
                server: Some(server_id.clone()),
                path: Some(m.into()),
                blocks: vec![
                    GuardAction::Switch,
                    GuardAction::OnDemand,
                    GuardAction::Ensure,
                ],
                ..guard()
            },
        );
    }
    if let Some(m) = k.get("DENSE_LEAD").and_then(Value::as_str) {
        c.guards.insert(
            "dense-lead".into(),
            Guard {
                kind: GuardKind::FileExists,
                server: Some(server_id.clone()),
                path: Some(varize(&vars, m)),
                blocks: vec![GuardAction::Switch],
                ..guard()
            },
        );
    }
    rep.add(
        "mapped",
        "layoutctl QUIET/MUSICMODE/DENSE_LEAD, ondemand.py video_runs/bench_running",
        "[guards.*]; a stale guard source counts as active for switches",
    );
    rep.add(
        "dropped",
        "gateway.json enabled/lbUrl/litellmHotReload/general_settings/genericExclude",
        "no generated gateway any more: routes follow the active layout atomically",
    );
    rep.add(
        "dropped",
        "gateway.json litellm_settings.callbacks (effort_hook.py)",
        "the clamp and thinking-off rules are engine/artifact params",
    );

    // ── cross-checks against the generated LiteLLM and nginx files ──────────────────────
    if let Some(y) = &src.litellm_yaml {
        // layoutctl writes it as JSON (valid YAML); a hand-written YAML file is read by lines.
        let body: String = y
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        let (mut names, master_key) = match serde_json::from_str::<Value>(&body) {
            Ok(j) => (
                j["model_list"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["model_name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                j["general_settings"]["master_key"]
                    .as_str()
                    .is_some_and(|k| !k.is_empty()),
            ),
            Err(_) => (
                y.lines()
                    .filter_map(|l| {
                        l.trim()
                            .trim_start_matches("- ")
                            .strip_prefix("model_name:")
                    })
                    .map(|n| n.trim().trim_matches('"').to_string())
                    .collect::<Vec<String>>(),
                y.lines().any(|l| {
                    l.trim()
                        .strip_prefix("master_key:")
                        .is_some_and(|v| !v.trim().is_empty())
                }),
            ),
        };
        names.sort();
        names.dedup();
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| !c.routes.contains_key(*n))
            .collect();
        for n in &missing {
            rep.add(
                "check",
                "LiteLLM config.yaml",
                format!("model name {n} has no route"),
            );
        }
        rep.add(
            "check",
            "LiteLLM config.yaml",
            format!(
                "{} model names, {} without a route",
                names.len(),
                missing.len()
            ),
        );
        rep.add(
            "check",
            "LiteLLM config.yaml",
            if master_key {
                "a master_key is set: some callers already send keys"
            } else {
                "no master_key: every LiteLLM caller is keyless today (see the keyless-caller list)"
            },
        );
    }
    if let Some(n) = &src.nginx {
        let weights: Vec<u32> = n
            .lines()
            .filter_map(|l| {
                l.split("weight=")
                    .nth(1)?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .collect();
        rep.add("check", "qfn-lb nginx.conf", format!("upstream weights {weights:?} = lanes of the live layout (placement uses lanes; nothing to import)"));
    }

    // ── seed ─────────────────────────────────────────────────────────────────────────────
    let offline: Vec<String> = src
        .offline
        .as_ref()
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(l) = &live_layout
        && !c.layouts.contains_key(l)
    {
        rep.add(
            "check",
            "state/live.json",
            format!("live layout {l} is not a layout in the import"),
        );
    }
    let seed = Seed {
        server: server_id.clone(),
        active_layout: live_layout,
        offline,
    };

    let body = inline_layout_cards(
        &toml::to_string_pretty(&c).map_err(|e| format!("serialising: {e}"))?,
        &c,
    )?;
    let header = "# router.toml: generated by `abhoth router import` from the GPU host's layout tooling.\n\
                  # Review the import report, fill the TODOs (SSH host key, secrets) and run `abhoth router config check`.\n\
                  # Keys are never imported: [secrets] only names the files they must be copied to.\n\n";
    Ok(Imported {
        config: c,
        toml: format!("{header}{body}"),
        seed,
        report: rep.0,
    })
}

/// Layout cards as inline tables in card order (`A = { engine = … }`, `B = "same"`): the
/// serialiser would put the words first and the tables after, and "same" copies the card
/// before it.
fn inline_layout_cards(text: &str, c: &ControlConfig) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("re-reading the output: {e}"))?;
    if let Some(layouts) = doc.get_mut("layouts").and_then(|l| l.as_table_like_mut()) {
        for (name, l) in layouts.iter_mut() {
            let (Some(layout), Some(lt)) = (c.layouts.get(name.get()), l.as_table_like_mut())
            else {
                continue;
            };
            let mut it = toml_edit::InlineTable::new();
            for (card, lc) in &layout.cards {
                let v: toml_edit::Value = match lc {
                    LayoutCard::Word(w) => w.as_str().into(),
                    LayoutCard::Spec(sp) => {
                        let mut t = toml_edit::InlineTable::new();
                        t.insert("engine", sp.engine.as_str().into());
                        t.insert("profile", sp.profile.as_str().into());
                        t.insert("model", sp.model.as_str().into());
                        toml_edit::Value::InlineTable(t)
                    }
                };
                it.insert(card, v);
            }
            lt.insert(
                "cards",
                toml_edit::Item::Value(toml_edit::Value::InlineTable(it)),
            );
        }
    }
    Ok(doc.to_string())
}

fn guard() -> Guard {
    Guard {
        kind: GuardKind::Schedule,
        server: None,
        windows: Vec::new(),
        blocks: Vec::new(),
        retry_seconds: 300,
        path: None,
        active_when: Vec::new(),
        matcher: None,
        url: None,
        json_path: None,
        reason: None,
        stale_seconds: 45,
    }
}

/// Callers of the old endpoints that send no key today (LiteLLM runs without a master key,
/// qfn-lb adds the engine key itself, pig-slots and the profiles API take none). Keys are
/// required from day one, so each needs a router key before the cutover. From the host survey
/// (2026-11): the gateway's access log by client, plus the files that name the ports.
pub const KEYLESS_CALLERS: &[(&str, &str, &str)] = &[
    (
        "Jarvis (Home Assistant conversation agent)",
        "LiteLLM :8010 chat",
        "the Home Assistant host's OpenAI integration: base URL + key",
    ),
    (
        "Home Assistant bridge (ha-bridge)",
        "profiles API :8098",
        "ha_bridge.py: add the admin key header",
    ),
    (
        "Video pipelines (AI News, Video Game News)",
        "LiteLLM :8010 chat",
        "{var:video_root}/config.json base URL + key; research_local.py",
    ),
    (
        "Music check (music/chk.sh)",
        "qfn-lb :8092",
        "send the router key",
    ),
    (
        "Dash buttons and panels",
        "profiles API :8098, pig-slots :8099",
        "dash/app/server.py and its page's POSTs to :8098",
    ),
    (
        "Workstation clients (Ollama-style /api/show, /api/v0/models, chat)",
        "LiteLLM :8010",
        "per-tool settings on the workstation",
    ),
    (
        "Abhoth/OpenOrca agents and benches",
        "pig-slots :8099, qfn-lb :8092",
        "the agents' slot registry URL + key",
    ),
    (
        "Old watchdog/boot scripts (qfn-llm-watchdog.sh, boot.sh, nwlead.sh)",
        ":8092 / :8010",
        "retired by the cutover (the router reconciles); remove, do not key",
    ),
    (
        "Gateway probes (on-demand relay, dash model/info)",
        "LiteLLM :8010 /v1/model/info",
        "become router-internal",
    ),
];

/// The report as Markdown.
pub fn report_markdown(im: &Imported) -> String {
    let mut out = String::from("# Router import report\n\n");
    for kind in ["todo", "check", "guess", "dropped", "mapped"] {
        let rows: Vec<&ReportLine> = im.report.iter().filter(|r| r.kind == kind).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("## {kind}\n\n"));
        for r in rows {
            out.push_str(&format!("- **{}**: {}\n", r.source, r.text));
        }
        out.push('\n');
    }
    out.push_str("## keyless callers (each needs a router key before the cutover)\n\n| caller | calls | where to set the key |\n|---|---|---|\n");
    for (who, what, how) in KEYLESS_CALLERS {
        out.push_str(&format!("| {who} | {what} | {how} |\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "## seed for router.db\n\n- server `{}`: active layout {:?}, offline cards {:?}\n",
        im.seed.server, im.seed.active_layout, im.seed.offline
    ));
    out
}

#[cfg(test)]
mod tests;
