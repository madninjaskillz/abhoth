//! `driver = "package"` (D153, ROUTER-MEDIA-PLAN.md 13.3): an engine that runs an installed
//! engine package. The package's docker and compose methods become the same commands the docker
//! and compose drivers send (through the server's executor, so hostctl allowlists see them);
//! host-process methods (pip, binary, git, existing programs) are the bridge's job and are
//! refused here, since the router's executor runs commands, not supervised processes.
//!
//! Installs live under the router's state dir: `<state_dir>/packages/<server>/engines/<id>/
//! <version>/installed.json` with `package/abhoth-engine.json` beside it (the orca-engines
//! [`Layout`]). Unlike the other drivers this one reads those two files when it plans a start.
//!
//! Host paths: an instance's data folder is `<engine_state>/<engine>-<card_lc>` and the package
//! folder `<engine_state>/packages/<id>/<version>`, where `engine_state` is the server var of
//! that name (default [`DEFAULT_ENGINE_STATE`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use orca_engines::host::CardSel;
use orca_engines::manifest::GpuKind;
use orca_engines::render::{Limits, ModelRef, RunSpec, StartPlan, plan_start};
use orca_engines::runner::Layout;
use orca_engines::{Installed, Manifest};
use serde_json::{Value, json};

use super::driver::{Cmd, DriverError, EngineDriver, InstanceRef, Observed, Resolved};
use super::drivers::parse_docker_inspect;
use super::exec::ExecOutput;
use super::schema::{DriverKind, Engine, PackageRef};
use super::template::{TplValue, shell_quote};

/// Where package instances keep their data on a host without an `engine_state` var.
pub const DEFAULT_ENGINE_STATE: &str = "/var/lib/abhoth/engines";

/// The package layout for one server under the router's state dir.
pub fn layout(state_dir: &Path, server: &str) -> Layout {
    Layout {
        root: state_dir.join("packages").join(server),
    }
}

pub struct Package {
    pkg: Option<PackageRef>,
}

impl From<&Engine> for Package {
    fn from(e: &Engine) -> Self {
        Package {
            pkg: e.package.clone(),
        }
    }
}

fn word(v: &super::template::Vars, k: &str) -> String {
    match v.get(k) {
        Some(TplValue::Word(w)) => w.clone(),
        _ => String::new(),
    }
}

const RUN_TIMEOUT: Duration = Duration::from_secs(900);
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

impl Package {
    fn pkg(&self) -> Result<&PackageRef, DriverError> {
        self.pkg.as_ref().ok_or_else(|| {
            DriverError("driver = \"package\" needs package = { id, version }".into())
        })
    }

    /// The install record and manifest, with the method to run.
    fn load(&self, i: &InstanceRef) -> Result<(Installed, Manifest), DriverError> {
        let p = self.pkg()?;
        let state_dir = PathBuf::from(word(&i.vars, "state_dir"));
        let l = layout(&state_dir, &i.server);
        let mut inst = l.find_installed(&p.id, &p.version).ok_or_else(|| {
            DriverError(format!(
                "package {}@{} is not installed on `{}` (Engine store → install)",
                p.id, p.version, i.server
            ))
        })?;
        let m = l.installed_manifest(&inst).map_err(DriverError)?;
        if let Some(t) = &p.method {
            let k = m
                .methods
                .iter()
                .position(|x| x.r#type.as_str() == t)
                .ok_or_else(|| DriverError(format!("the package has no `{t}` method")))?;
            inst.method = k;
            inst.method_type = m.methods[k].r#type;
        }
        Ok((inst, m))
    }

    /// The run spec for this instance, from the resolved variables.
    pub fn run_spec(&self, r: &Resolved, m: &Manifest) -> Result<RunSpec, DriverError> {
        let p = self.pkg()?;
        let v = &r.iref.vars;
        let card_lc = word(v, "card_lc");
        let card = if word(v, "device") == "cpu" {
            CardSel::Cpu {
                card: r.iref.card.clone(),
                cores: 0,
                ram_gib: 0.0,
            }
        } else {
            CardSel::Gpu {
                card: r.iref.card.clone(),
                index: 0,
                uuid: word(v, "gpu_uuid"),
                vendor: GpuKind::Cuda,
                vram_gib: 0.0,
            }
        };
        let mut s = RunSpec::new(&r.iref.engine, card, r.port);
        s.name = r.iref.name.clone();
        let host_state = {
            let x = word(v, "var:engine_state");
            if x.is_empty() {
                DEFAULT_ENGINE_STATE.to_string()
            } else {
                x
            }
        };
        let host_state = host_state.trim_end_matches('/').to_string();
        s.state = Some(format!("{host_state}/{}-{card_lc}", r.iref.engine));
        s.package_dir = Some(format!("{host_state}/packages/{}/{}", p.id, p.version));
        let md = word(v, "model_dir");
        let md = if md.is_empty() {
            word(v, "server_model_dir")
        } else {
            md
        };
        if !md.is_empty() {
            s.model_dir = Some(md);
        }
        if !r.spec.model.is_empty() {
            s.model = Some(ModelRef {
                id: r.served_id.clone(),
                file: r
                    .model_file
                    .as_ref()
                    .map(|_| word(v, "model_file"))
                    .filter(|f| !f.is_empty()),
            });
        }
        let mut params: BTreeMap<String, Value> = BTreeMap::new();
        for (k, prm) in &m.params {
            if let Some(d) = &prm.default {
                params.insert(k.clone(), d.clone());
            }
        }
        for (k, x) in &p.params {
            params.insert(k.clone(), x.clone());
        }
        s.params = params;
        s.ports = p.ports.iter().map(|(k, v)| (k.clone(), *v)).collect();
        // CPU sets stay the server's business; the package declares no limits of its own.
        s.limits = Limits::default();
        Ok(s)
    }

    /// The plan a start would run (also for the admin API's preview).
    pub fn plan(&self, r: &Resolved) -> Result<StartPlan, DriverError> {
        let (inst, m) = self.load(&r.iref)?;
        let s = self.run_spec(r, &m)?;
        plan_start(&m, &inst, &s, &inst.approvals).map_err(DriverError)
    }

    fn compose_path(i: &InstanceRef) -> String {
        let x = word(&i.vars, "var:engine_state");
        let root = if x.is_empty() {
            DEFAULT_ENGINE_STATE.to_string()
        } else {
            x
        };
        format!("{}/run/{}.compose.yml", root.trim_end_matches('/'), i.name)
    }
}

/// `docker run` words with the instance's labels (the `layouts.spec` label first) after `-d`.
fn with_labels(mut argv: Vec<String>, r: &Resolved) -> Vec<String> {
    let at = argv.iter().position(|w| w == "-d").map_or(2, |k| k + 1);
    let mut words = vec![
        "--label".to_string(),
        format!("layouts.spec={}", r.spec_json),
    ];
    for (k, x) in &r.labels {
        words.push("--label".into());
        words.push(format!("{k}={x}"));
    }
    argv.splice(at..at, words);
    argv
}

fn quote_all(argv: &[String]) -> String {
    argv.iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

impl EngineDriver for Package {
    fn kind(&self) -> DriverKind {
        DriverKind::Package
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let id = format!("{}:{}", r.iref.engine, r.iref.card);
        match self.plan(r)? {
            StartPlan::Docker { name, argv } => Ok(vec![
                Cmd::builtin(
                    "docker-rm-robust",
                    json!({"name": name, "timeout": 120}),
                    true,
                    Duration::from_secs(150),
                ),
                Cmd::shell(
                    &format!("package-run:{id}"),
                    quote_all(&with_labels(argv, r)),
                    true,
                    RUN_TIMEOUT,
                ),
            ]),
            StartPlan::Compose { yaml, up, .. } => {
                let file = Self::compose_path(&r.iref);
                let up: Vec<String> = up
                    .into_iter()
                    .map(|w| w.replace(orca_engines::render::COMPOSE_FILE, &file))
                    .collect();
                let dir = file.rsplit_once('/').map(|x| x.0).unwrap_or("/");
                let text = format!(
                    "mkdir -p {} && printf '%s' {} > {} && {}",
                    shell_quote(dir),
                    shell_quote(&yaml),
                    shell_quote(&file),
                    quote_all(&up)
                );
                Ok(vec![Cmd::shell(
                    &format!("package-compose-up:{id}"),
                    text,
                    true,
                    RUN_TIMEOUT,
                )])
            }
            StartPlan::Process { .. } => Err(DriverError(
                "this package method runs a host process: the router starts docker and compose \
                 methods only (install a container method, or run it under the bridge)"
                    .into(),
            )),
            StartPlan::External { .. } => Ok(vec![]),
        }
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        let (inst, _) = self.load(i)?;
        if inst.method_type == orca_engines::MethodType::Compose {
            let file = Self::compose_path(i);
            return Ok(vec![
                Cmd::shell(
                    &format!("package-compose-down:{}:{}", i.engine, i.card),
                    format!(
                        "docker compose -p {} -f {} down -t {}",
                        shell_quote(&i.name),
                        shell_quote(&file),
                        timeout.as_secs()
                    ),
                    true,
                    timeout + Duration::from_secs(60),
                )
                .allow_fail(),
            ]);
        }
        Ok(vec![Cmd::builtin(
            "docker-rm-robust",
            json!({"name": i.name, "timeout": timeout.as_secs()}),
            true,
            timeout + Duration::from_secs(30),
        )])
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        Ok(Cmd::shell(
            &format!("docker-inspect:{}:{}", i.engine, i.card),
            format!(
                "docker inspect -f '{{{{json .State}}}}|{{{{index .Config.Labels \"layouts.spec\"}}}}' {} 2>&1",
                shell_quote(&i.name)
            ),
            true,
            STATUS_TIMEOUT,
        ))
    }
    fn parse_status(&self, _i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError> {
        Ok(parse_docker_inspect(out))
    }
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError> {
        Ok(Cmd::shell(
            &format!("docker-logs:{}:{}", i.engine, i.card),
            format!("docker logs --tail {lines} {} 2>&1", shell_quote(&i.name)),
            true,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::engine_store;
    use crate::control::schema::ControlConfig;
    use crate::control::validate::tests::SAMPLE;
    use orca_engines::exec::{ExecResult, FakeExec, FakeFetch};
    use orca_engines::{Approvals, LocalRunner, Runner};
    use std::sync::Arc;

    /// Install the default store's kokoro on card A (a fake docker), add its engine the way the
    /// engine store does, and check the config validates and the start commands are right.
    #[tokio::test]
    async fn an_installed_package_becomes_a_package_engine_that_starts_with_docker() {
        let dir = tempfile::tempdir().expect("tmp");
        let state = dir
            .path()
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let text = SAMPLE.replace(
            "state_dir = \"/state\"",
            &format!("state_dir = \"{state}\""),
        );
        let c: ControlConfig = toml::from_str(&text).expect("sample");
        let p = orca_engines::store::embedded::packages()
            .into_iter()
            .find(|p| p.manifest.id == "kokoro")
            .expect("kokoro");
        let sel = engine_store::card_sel(&c, "A").expect("card A");
        let h = engine_store::host_facts(&c, "gpu");
        let f = orca_engines::fit(
            &p.manifest,
            &h,
            &sel,
            &orca_engines::MethodFilter::containers_only(),
        );
        let mi = f.method.expect("a method fits");
        let exec = FakeExec::new();
        exec.on("docker", vec![ExecResult::ok_with("")]);
        let runner = LocalRunner::new(
            layout(dir.path(), "gpu").root,
            Arc::new(exec.clone()),
            Arc::new(FakeFetch::default()),
            false,
            h,
        );
        let inst = runner
            .install(&p, mi, &sel, &Approvals::default())
            .await
            .expect("install");
        assert!(
            exec.joined()
                .iter()
                .any(|x| x.starts_with("docker pull ghcr.io/remsky/kokoro-fastapi-gpu"))
        );

        let edits = engine_store::engine_edits(
            &c,
            "kokoro",
            &p.manifest,
            &inst,
            "A",
            &Default::default(),
            "default",
        );
        let new = crate::control_api::form::apply_edits(&text, &edits).expect("edits");
        assert!(new.contains("driver = \"package\""), "{new}");
        let c2: ControlConfig = toml::from_str(&new).expect("parses");
        let errs: Vec<String> = crate::control::validate::validate(&c2)
            .into_iter()
            .filter(|i| i.level == crate::control::validate::Level::Error)
            .map(|i| i.to_string())
            .collect();
        assert!(errs.is_empty(), "{errs:?}");

        let e = &c2.engines["kokoro"];
        assert_eq!(e.driver, DriverKind::Package);
        let port = *e.port.get("A").expect("port");
        assert!(port >= engine_store::FIRST_PORT);
        let iref = crate::control::resolve::instance_ref(&c2, "A", "kokoro").expect("iref");
        assert_eq!(iref.name, "abhoth-kokoro-a");
        let r = Resolved {
            iref: iref.clone(),
            port,
            spec_json: "{\"engine\":\"kokoro\"}".into(),
            ..Default::default()
        };
        let d = super::super::drivers::for_engine(e);
        let cmds = d.start(&r).expect("start");
        let run = cmds.last().expect("run").text().to_string();
        assert!(run.contains("--gpus device=GPU-aaaaaaaa"), "{run}");
        assert!(run.contains(&format!("127.0.0.1:{port}:8880")), "{run}");
        assert!(run.contains("layouts.spec="), "{run}");
        assert!(run.contains("kokoro-fastapi-gpu:v0.2.4@sha256:"), "{run}");
        assert!(
            d.status(&iref)
                .expect("status")
                .text()
                .contains("abhoth-kokoro-a")
        );

        // Not installed: a clear error, not a guess.
        let mut e2 = e.clone();
        e2.package.as_mut().expect("pkg").version = "9.9.9".into();
        let err = super::super::drivers::for_engine(&e2)
            .start(&r)
            .expect_err("missing");
        assert!(err.0.contains("not installed"), "{}", err.0);
    }
}
