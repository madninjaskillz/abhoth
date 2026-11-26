//! The built-in drivers (ROUTER-CONFIG.md 4.6): docker, compose, systemd, process and plain
//! templates. Each turns a resolved instance into commands and parses status output.

use std::time::Duration;

use serde_json::{Value, json};

use super::driver::{Cmd, DriverError, EngineDriver, InstanceRef, Observed, Resolved};
use super::exec::ExecOutput;
use super::schema::{DriverKind, Engine, KeyMode, StopMode};
use super::template::{self, argv_to_shell, shell_quote};

/// The driver for an engine's `driver` kind, configured from the engine.
pub fn for_engine(e: &Engine) -> Box<dyn EngineDriver> {
    match e.driver {
        DriverKind::Docker => Box::new(Docker::from(e)),
        DriverKind::Compose => Box::new(Compose::from(e)),
        DriverKind::Systemd => Box::new(Systemd::from(e)),
        DriverKind::Process => Box::new(Process::from(e)),
        DriverKind::Template => Box::new(Template::from(e)),
    }
}

const RUN_TIMEOUT: Duration = Duration::from_secs(120);
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

fn word(i: &InstanceRef, k: &str) -> String {
    match i.vars.get(k) {
        Some(template::TplValue::Word(w)) => w.clone(),
        _ => String::new(),
    }
}

// ── docker ───────────────────────────────────────────────────────────────────────────────────

/// `docker rm -f` (robust) then `docker run -d --name … <run.docker words>`; stop is the
/// zombie-tolerant removal loop (layoutctl `remove_container`); status is `docker inspect`.
pub struct Docker {
    key_mode: KeyMode,
    key_flag: String,
    key_path: String,
    stop_mode: StopMode,
}

impl From<&Engine> for Docker {
    fn from(e: &Engine) -> Self {
        Self {
            key_mode: if e.key.is_some() {
                e.key_delivery.mode
            } else {
                KeyMode::None
            },
            key_flag: e.key_delivery.flag_or_default().to_string(),
            key_path: e.key_delivery.path.clone(),
            stop_mode: e.stop.mode,
        }
    }
}

impl Docker {
    /// The full `docker run` argv (raw words for the key in argv mode; [`template::masked`]
    /// shows them as `***`).
    pub fn run_argv(&self, r: &Resolved) -> Result<Vec<String>, DriverError> {
        if r.docker.is_empty() {
            return Err(DriverError(
                "docker driver: [engines.x.run] docker words are empty".into(),
            ));
        }
        let mut words = r.docker.clone();
        if r.key_mode.unwrap_or(self.key_mode) == KeyMode::Wrapper {
            words = wrap_key(words, r.image.as_deref(), &self.key_flag, &self.key_path)?;
        }
        let mut argv = vec![
            "docker".to_string(),
            "run".into(),
            "-d".into(),
            "--name".into(),
            r.iref.name.clone(),
        ];
        argv.extend(words);
        Ok(argv)
    }
}

/// Wrapper key delivery: run the engine through `sh -c 'exec <entrypoint> "$@" FLAG "$(cat PATH)"'`
/// so the key never appears in `docker inspect`, whatever flags the engine has.
fn wrap_key(
    mut words: Vec<String>,
    image: Option<&str>,
    flag: &str,
    path: &str,
) -> Result<Vec<String>, DriverError> {
    let image = image.ok_or_else(|| DriverError("wrapper key delivery needs an image".into()))?;
    let at = words.iter().position(|w| w == image).ok_or_else(|| {
        DriverError("wrapper key delivery: the image is not among the docker words".into())
    })?;
    let mut entry = String::new();
    let mut i = 0;
    let mut cut = at;
    while i < cut {
        if words[i] == "--entrypoint" && i + 1 < cut {
            entry = words[i + 1].clone();
            words.drain(i..i + 2);
            cut -= 2;
            continue;
        }
        i += 1;
    }
    let script = format!(
        "exec {}\"$@\" {} \"$(cat {})\"",
        if entry.is_empty() {
            String::new()
        } else {
            format!("{} ", shell_quote(&entry))
        },
        shell_quote(flag),
        shell_quote(path)
    );
    let mut out: Vec<String> = words[..cut].to_vec();
    out.extend([
        "--entrypoint".to_string(),
        "/bin/sh".into(),
        words[cut].clone(),
        "-c".into(),
        script,
        "sh".into(),
    ]);
    out.extend(words[cut + 1..].iter().cloned());
    Ok(out)
}

/// `docker inspect` printing `<state json>|<layouts.spec label>`.
fn docker_status_text(name: &str) -> String {
    format!(
        "docker inspect -f '{{{{json .State}}}}|{{{{index .Config.Labels \"layouts.spec\"}}}}' {} 2>&1",
        shell_quote(name)
    )
}

/// Parse `docker inspect -f '{{json .State}}|{{index .Config.Labels "layouts.spec"}}'`.
pub fn parse_docker_inspect(out: &ExecOutput) -> Observed {
    let text = out.stdout.trim();
    if !out.ok() || !text.starts_with('{') {
        return Observed {
            exists: false,
            detail: out.summary(),
            ..Default::default()
        };
    }
    let (state, label) = text.split_once('|').unwrap_or((text, ""));
    let st: Value = serde_json::from_str(state).unwrap_or(Value::Null);
    let label = label.trim();
    let spec_raw = if label.starts_with('{') {
        serde_json::from_str(label).ok()
    } else {
        None
    };
    Observed {
        exists: true,
        running: st["Running"].as_bool().unwrap_or(false),
        spec: None,
        spec_raw,
        started_at: st["StartedAt"].as_str().map(String::from),
        exit_code: st["ExitCode"].as_i64().map(|x| x as i32),
        oom_killed: st["OOMKilled"].as_bool().unwrap_or(false),
        detail: st["Status"].as_str().unwrap_or("").to_string(),
    }
}

impl EngineDriver for Docker {
    fn kind(&self) -> DriverKind {
        DriverKind::Docker
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let argv = self.run_argv(r)?;
        Ok(vec![
            Cmd::builtin(
                "docker-rm-robust",
                json!({"name": r.iref.name, "timeout": 120}),
                true,
                Duration::from_secs(150),
            ),
            Cmd::shell(
                &format!("docker-run:{}:{}", r.iref.engine, r.iref.card),
                argv_to_shell(&argv),
                true,
                RUN_TIMEOUT,
            ),
        ])
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        Ok(match self.stop_mode {
            StopMode::Remove => vec![Cmd::builtin(
                "docker-rm-robust",
                json!({"name": i.name, "timeout": timeout.as_secs()}),
                true,
                timeout + Duration::from_secs(30),
            )],
            StopMode::Stop => vec![Cmd::shell(
                &format!("docker-stop:{}:{}", i.engine, i.card),
                format!(
                    "docker stop -t {} {}",
                    timeout.as_secs(),
                    shell_quote(&i.name)
                ),
                true,
                timeout + Duration::from_secs(30),
            )],
        })
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        Ok(Cmd::shell(
            &format!("docker-inspect:{}:{}", i.engine, i.card),
            docker_status_text(&i.name),
            true,
            STATUS_TIMEOUT,
        )
        .allow_fail())
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

// ── compose ──────────────────────────────────────────────────────────────────────────────────

/// A service of a compose project (Strata in the TrueNAS app): `docker start` the container,
/// else `compose up -d --no-recreate --no-deps <service>`; stop is `docker stop -t T` (the
/// container is kept). The compose file may be a `$(…)` expression the host's shell evaluates
/// (TrueNAS keeps the rendered file under a versioned directory).
pub struct Compose {
    project: String,
    file: String,
    service: super::schema::PerCard<String>,
    stop_timeout: u64,
}

impl From<&Engine> for Compose {
    fn from(e: &Engine) -> Self {
        let c = e.compose.clone();
        Self {
            project: c.as_ref().map(|c| c.project.clone()).unwrap_or_default(),
            file: c.as_ref().map(|c| c.file.clone()).unwrap_or_default(),
            service: c.map(|c| c.service).unwrap_or_default(),
            stop_timeout: e.stop.timeout_seconds,
        }
    }
}

impl EngineDriver for Compose {
    fn kind(&self) -> DriverKind {
        DriverKind::Compose
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let i = &r.iref;
        let service = self
            .service
            .get(&i.card)
            .ok_or_else(|| DriverError(format!("compose: no service for card {}", i.card)))?;
        let file = if self.file.starts_with("$(") {
            self.file.clone()
        } else {
            shell_quote(&template::render_word(&self.file, &i.vars)?)
        };
        let text = format!(
            "docker start {name} || docker --config /etc/docker compose -p {p} -f {file} up -d --no-recreate --no-deps {s}",
            name = shell_quote(&i.name),
            p = shell_quote(&self.project),
            s = shell_quote(service),
        );
        Ok(vec![Cmd::shell(
            &format!("compose-start:{}:{}", i.engine, i.card),
            text,
            true,
            Duration::from_secs(300),
        )])
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        let t = if timeout.is_zero() {
            self.stop_timeout
        } else {
            timeout.as_secs()
        };
        Ok(vec![Cmd::shell(
            &format!("docker-stop:{}:{}", i.engine, i.card),
            format!("docker stop -t {t} {}", shell_quote(&i.name)),
            true,
            Duration::from_secs(t + 30),
        )])
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        Ok(Cmd::shell(
            &format!("docker-inspect:{}:{}", i.engine, i.card),
            docker_status_text(&i.name),
            true,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
    fn parse_status(&self, _i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError> {
        Ok(parse_docker_inspect(out))
    }
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError> {
        Docker {
            key_mode: KeyMode::None,
            key_flag: String::new(),
            key_path: String::new(),
            stop_mode: StopMode::Stop,
        }
        .logs(i, lines)
    }
}

// ── systemd ──────────────────────────────────────────────────────────────────────────────────

/// A unit the admin installs (`abhoth-engine@.service` with `EnvironmentFile=` and
/// `ExecStart=/bin/sh -c 'exec $ENGINE_CMD'`). Start writes the env file (the command line,
/// never the key: the key reaches the engine as a file) and the spec sidecar, then starts.
pub struct Systemd {
    unit: String,
    spec_file: Option<String>,
    env_file: Option<String>,
}

impl From<&Engine> for Systemd {
    fn from(e: &Engine) -> Self {
        let s = e.systemd.clone();
        Self {
            unit: s.as_ref().map(|s| s.unit.clone()).unwrap_or_default(),
            spec_file: s.as_ref().and_then(|s| s.spec_file.clone()),
            env_file: s.and_then(|s| s.env_file.clone()),
        }
    }
}

fn sidecar(
    spec_file: &Option<String>,
    i: &InstanceRef,
    default: String,
) -> Result<String, DriverError> {
    Ok(match spec_file {
        Some(t) => template::render_word(t, &i.vars)?,
        None => default,
    })
}

impl EngineDriver for Systemd {
    fn kind(&self) -> DriverKind {
        DriverKind::Systemd
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let i = &r.iref;
        let unit = template::render_word(&self.unit, &i.vars)?;
        let spec = sidecar(&self.spec_file, i, format!("/run/abhoth/{unit}.spec.json"))?;
        let env = sidecar(&self.env_file, i, format!("/run/abhoth/{unit}.env"))?;
        if r.args.is_empty() {
            return Err(DriverError(
                "systemd driver: [engines.x.run] command is empty".into(),
            ));
        }
        let text = format!(
            "mkdir -p {d1} {d2} && printf '%s\\n' {spec_json} > {spec} && printf 'ENGINE_CMD=%s\\n' {cmd} > {env} && systemctl start {unit}",
            d1 = shell_quote(parent(&spec)),
            d2 = shell_quote(parent(&env)),
            spec_json = shell_quote(&r.spec_json),
            spec = shell_quote(&spec),
            cmd = shell_quote(&argv_to_shell(&r.args)),
            env = shell_quote(&env),
            unit = shell_quote(&unit),
        );
        Ok(vec![Cmd::shell(
            &format!("systemd-start:{}:{}", i.engine, i.card),
            text,
            true,
            RUN_TIMEOUT,
        )])
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        let unit = template::render_word(&self.unit, &i.vars)?;
        Ok(vec![Cmd::shell(
            &format!("systemd-stop:{}:{}", i.engine, i.card),
            format!("systemctl stop {}", shell_quote(&unit)),
            true,
            timeout + Duration::from_secs(30),
        )])
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        let unit = template::render_word(&self.unit, &i.vars)?;
        let spec = sidecar(&self.spec_file, i, format!("/run/abhoth/{unit}.spec.json"))?;
        Ok(Cmd::shell(
            &format!("systemd-status:{}:{}", i.engine, i.card),
            format!(
                "systemctl show -p ActiveState,ExecMainStartTimestamp,ExecMainStatus {}; echo SPEC=$(cat {} 2>/dev/null)",
                shell_quote(&unit),
                shell_quote(&spec)
            ),
            false,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
    fn parse_status(&self, _i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError> {
        let mut o = Observed::default();
        for line in out.stdout.lines() {
            let (k, v) = line.split_once('=').unwrap_or((line, ""));
            match k {
                "ActiveState" => {
                    o.exists = !v.is_empty();
                    o.running = matches!(v, "active" | "activating" | "reloading");
                    o.detail = v.to_string();
                }
                "ExecMainStartTimestamp" if !v.is_empty() => o.started_at = Some(v.to_string()),
                "ExecMainStatus" => o.exit_code = v.parse().ok(),
                "SPEC" if v.starts_with('{') => o.spec_raw = serde_json::from_str(v).ok(),
                _ => {}
            }
        }
        Ok(o)
    }
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError> {
        let unit = template::render_word(&self.unit, &i.vars)?;
        Ok(Cmd::shell(
            &format!("systemd-logs:{}:{}", i.engine, i.card),
            format!("journalctl -u {} -n {lines} --no-pager", shell_quote(&unit)),
            true,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/')
        .map(|(a, _)| if a.is_empty() { "/" } else { a })
        .unwrap_or(".")
}

// ── process ──────────────────────────────────────────────────────────────────────────────────

/// A plain process started detached (`setsid`, pid file and spec sidecar in the run dir); stop
/// is SIGTERM, then SIGKILL after the timeout.
pub struct Process {
    run_dir: Option<String>,
    log_file: Option<String>,
    cwd: Option<String>,
}

impl From<&Engine> for Process {
    fn from(e: &Engine) -> Self {
        let p = e.process.clone();
        Self {
            run_dir: p.as_ref().and_then(|p| p.run_dir.clone()),
            log_file: p.as_ref().and_then(|p| p.log_file.clone()),
            cwd: p.and_then(|p| p.cwd),
        }
    }
}

impl Process {
    fn paths(&self, i: &InstanceRef) -> Result<(String, String, String), DriverError> {
        let dir = match &self.run_dir {
            Some(d) => template::render_word(d, &i.vars)?,
            None => format!("{}/proc", word(i, "state_dir").trim_end_matches('/')),
        };
        let log = match &self.log_file {
            Some(l) => template::render_word(l, &i.vars)?,
            None => format!("{dir}/{}.log", i.name),
        };
        Ok((
            format!("{dir}/{}.pid", i.name),
            format!("{dir}/{}.spec.json", i.name),
            log,
        ))
    }
}

impl EngineDriver for Process {
    fn kind(&self) -> DriverKind {
        DriverKind::Process
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let i = &r.iref;
        if r.args.is_empty() {
            return Err(DriverError(
                "process driver: [engines.x.run] command is empty".into(),
            ));
        }
        let (pid, spec, log) = self.paths(i)?;
        let cd = match &self.cwd {
            Some(c) => format!(
                "cd {} && ",
                shell_quote(&template::render_word(c, &i.vars)?)
            ),
            None => String::new(),
        };
        let text = format!(
            "mkdir -p {dir} && printf '%s\\n' {sj} > {spec} && {cd}setsid {cmd} > {log} 2>&1 < /dev/null & echo $! > {pid}",
            dir = shell_quote(parent(&pid)),
            sj = shell_quote(&r.spec_json),
            spec = shell_quote(&spec),
            cmd = argv_to_shell(&r.args),
            log = shell_quote(&log),
            pid = shell_quote(&pid),
        );
        Ok(vec![Cmd::shell(
            &format!("process-start:{}:{}", i.engine, i.card),
            text,
            false,
            Duration::from_secs(30),
        )])
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        let (pid, _, _) = self.paths(i)?;
        let t = timeout.as_secs().max(1);
        let text = format!(
            "p=$(cat {pid} 2>/dev/null) || exit 0; kill -TERM -- -$p 2>/dev/null || kill -TERM $p 2>/dev/null; \
             n=0; while kill -0 $p 2>/dev/null && [ $n -lt {t} ]; do sleep 1; n=$((n+1)); done; \
             kill -KILL -- -$p 2>/dev/null; kill -KILL $p 2>/dev/null; rm -f {pid}; exit 0",
            pid = shell_quote(&pid)
        );
        Ok(vec![Cmd::shell(
            &format!("process-stop:{}:{}", i.engine, i.card),
            text,
            false,
            timeout + Duration::from_secs(10),
        )])
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        let (pid, spec, _) = self.paths(i)?;
        let text = format!(
            "if [ -f {pid} ]; then p=$(cat {pid}); if kill -0 $p 2>/dev/null; then echo RUNNING=1; else echo RUNNING=0; fi; echo EXISTS=1; fi; echo SPEC=$(cat {spec} 2>/dev/null)",
            pid = shell_quote(&pid),
            spec = shell_quote(&spec)
        );
        Ok(Cmd::shell(
            &format!("process-status:{}:{}", i.engine, i.card),
            text,
            false,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
    fn parse_status(&self, _i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError> {
        let mut o = Observed::default();
        for line in out.stdout.lines() {
            match line.split_once('=') {
                Some(("RUNNING", v)) => o.running = v == "1",
                Some(("EXISTS", v)) => o.exists = v == "1",
                Some(("SPEC", v)) if v.starts_with('{') => {
                    o.spec_raw = serde_json::from_str(v).ok()
                }
                _ => {}
            }
        }
        Ok(o)
    }
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError> {
        let (_, _, log) = self.paths(i)?;
        Ok(Cmd::shell(
            &format!("process-logs:{}:{}", i.engine, i.card),
            format!("tail -n {lines} {}", shell_quote(&log)),
            false,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
}

// ── template ─────────────────────────────────────────────────────────────────────────────────

/// The admin writes start/stop/status/logs; status prints `{"running": bool, "spec": {...}}`.
pub struct Template {
    t: super::schema::DriverTemplates,
}

impl From<&Engine> for Template {
    fn from(e: &Engine) -> Self {
        Self {
            t: e.templates.clone().unwrap_or_default(),
        }
    }
}

impl Template {
    fn vars_with_lines(i: &InstanceRef, lines: u32) -> template::Vars {
        let mut v = i.vars.clone();
        v.insert("lines".into(), template::TplValue::Word(lines.to_string()));
        v
    }
}

impl EngineDriver for Template {
    fn kind(&self) -> DriverKind {
        DriverKind::Template
    }
    fn start(&self, r: &Resolved) -> Result<Vec<Cmd>, DriverError> {
        let mut v = r.iref.vars.clone();
        v.insert("args".into(), template::TplValue::List(r.args.clone()));
        let text = template::render_shell(&self.t.start, &v)?;
        Ok(vec![Cmd::shell(
            &format!("tpl-start:{}:{}", r.iref.engine, r.iref.card),
            text,
            false,
            RUN_TIMEOUT,
        )])
    }
    fn stop(&self, i: &InstanceRef, timeout: Duration) -> Result<Vec<Cmd>, DriverError> {
        let text = template::render_shell(&self.t.stop, &i.vars)?;
        Ok(vec![Cmd::shell(
            &format!("tpl-stop:{}:{}", i.engine, i.card),
            text,
            false,
            timeout + Duration::from_secs(30),
        )])
    }
    fn status(&self, i: &InstanceRef) -> Result<Cmd, DriverError> {
        let text = template::render_shell(&self.t.status, &i.vars)?;
        Ok(Cmd::shell(
            &format!("tpl-status:{}:{}", i.engine, i.card),
            text,
            false,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
    fn parse_status(&self, _i: &InstanceRef, out: &ExecOutput) -> Result<Observed, DriverError> {
        let line = out
            .stdout
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with('{'))
            .unwrap_or("");
        let v: Value = serde_json::from_str(line).map_err(|e| {
            DriverError(format!(
                "status template must print JSON {{\"running\": bool, \"spec\": {{…}}}}: {e}"
            ))
        })?;
        let running = v["running"].as_bool().unwrap_or(false);
        Ok(Observed {
            exists: v.get("exists").and_then(Value::as_bool).unwrap_or(running),
            running,
            spec_raw: v.get("spec").filter(|s| s.is_object()).cloned(),
            ..Default::default()
        })
    }
    fn logs(&self, i: &InstanceRef, lines: u32) -> Result<Cmd, DriverError> {
        let t = self
            .t
            .logs
            .clone()
            .unwrap_or_else(|| "echo no logs template".into());
        let text = template::render_shell(&t, &Self::vars_with_lines(i, lines))?;
        Ok(Cmd::shell(
            &format!("tpl-logs:{}:{}", i.engine, i.card),
            text,
            false,
            STATUS_TIMEOUT,
        )
        .allow_fail())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::resolve;
    use crate::control::schema::*;
    use crate::control::validate::tests::sample;

    fn spec(e: &str, p: &str, m: &str) -> CardSpec {
        CardSpec {
            engine: e.into(),
            profile: p.into(),
            model: m.into(),
        }
    }

    #[test]
    fn docker_start_is_rm_then_run() {
        let c = sample();
        let cmds = resolve::start_commands(&c, "A", &spec("ninfer", "4lane", "dense")).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].id, "docker-rm-robust");
        assert!(cmds[1].id.starts_with("docker-run:ninfer:A:"));
        assert!(
            cmds[1]
                .text()
                .starts_with("docker run -d --name ninfer-a --restart no")
        );
        assert!(
            cmds[1]
                .text()
                .contains(r#"'layouts.spec={"build": "merged", "engine": "ninfer""#),
            "{}",
            cmds[1].text()
        );
        assert!(cmds[1].root);
        // The id pins the text.
        let again = resolve::start_commands(&c, "A", &spec("ninfer", "4lane", "dense")).unwrap();
        assert_eq!(cmds[1].id, again[1].id);
        let other = resolve::start_commands(&c, "A", &spec("ninfer", "2lane", "dense")).unwrap();
        assert_ne!(cmds[1].id, other[1].id);
    }

    #[test]
    fn wrapper_moves_the_key_inside_the_container() {
        let mut c = sample();
        let e = c.engines.get_mut("ninfer").unwrap();
        e.key_delivery.mode = KeyMode::Wrapper;
        let r = resolve::resolve(&c, "A", &spec("ninfer", "2lane", "dense")).unwrap();
        let argv = Docker::from(&c.engines["ninfer"]).run_argv(&r).unwrap();
        let i = argv.iter().position(|w| w == "local/ninfer:test").unwrap();
        assert_eq!(&argv[i - 2..i], ["--entrypoint", "/bin/sh"]);
        assert_eq!(argv[i + 1], "-c");
        assert_eq!(
            argv[i + 2],
            r#"exec "$@" --api-key "$(cat /run/secrets/engine-key)""#
        );
        assert_eq!(argv[i + 3], "sh");
        assert_eq!(argv[i + 4], "/app/ninfer-serve");
        assert!(
            argv.iter()
                .any(|w| w == "/run/abhoth/secrets/engine_key:/run/secrets/engine-key:ro")
        );
    }

    /// A build that reads `--api-key-file` gets the flag; the others keep the wrapper.
    #[test]
    fn key_file_builds_skip_the_wrapper() {
        let mut c = sample();
        let e = c.engines.get_mut("ninfer").unwrap();
        e.key_delivery.mode = KeyMode::Wrapper;
        let build = resolve::build_name(e, &e.profiles["2lane"])
            .expect("the sample profile has a build")
            .to_string();
        e.key_delivery.file_flag_builds = vec![build];
        let r = resolve::resolve(&c, "A", &spec("ninfer", "2lane", "dense")).unwrap();
        let argv = Docker::from(&c.engines["ninfer"]).run_argv(&r).unwrap();
        assert!(!argv.iter().any(|w| w == "/bin/sh"), "{argv:?}");
        let i = argv.iter().position(|w| w == "--api-key-file").unwrap();
        assert_eq!(argv[i + 1], "/run/secrets/engine-key");
        assert!(
            argv.iter()
                .any(|w| w == "/run/abhoth/secrets/engine_key:/run/secrets/engine-key:ro")
        );
        assert!(!argv.iter().any(|w| w == "--api-key"));
    }

    #[test]
    fn wrapper_keeps_an_existing_entrypoint() {
        let words: Vec<String> = ["--entrypoint", "/app/llama-server", "img", "--model", "m"]
            .map(String::from)
            .to_vec();
        let out = wrap_key(words, Some("img"), "--api-key", "/k").unwrap();
        assert_eq!(out[..3], ["--entrypoint", "/bin/sh", "img"]);
        assert_eq!(
            out[4],
            r#"exec /app/llama-server "$@" --api-key "$(cat /k)""#
        );
    }

    #[test]
    fn compose_start_and_stop() {
        let c = sample();
        let cmds = resolve::start_commands(&c, "B", &spec("strata", "2slot", "flash")).unwrap();
        assert_eq!(
            cmds[0].text(),
            "docker start moe-b || docker --config /etc/docker compose -p ix-moe -f /srv/compose.yaml up -d --no-recreate --no-deps strata-b"
        );
        let i = resolve::instance_ref(&c, "B", "strata").unwrap();
        let stop = for_engine(&c.engines["strata"])
            .stop(&i, Duration::from_secs(30))
            .unwrap();
        assert_eq!(stop[0].text(), "docker stop -t 30 moe-b");
    }

    #[test]
    fn docker_stop_and_status_parse() {
        let c = sample();
        let i = resolve::instance_ref(&c, "A", "ninfer").unwrap();
        let d = for_engine(&c.engines["ninfer"]);
        let stop = d.stop(&i, Duration::from_secs(120)).unwrap();
        assert_eq!(stop[0].id, "docker-rm-robust");
        let st = d.status(&i).unwrap();
        assert!(
            st.text()
                .contains(r#"{{json .State}}|{{index .Config.Labels "layouts.spec"}}"#),
            "{}",
            st.text()
        );
        let out = ExecOutput::ok_with(
            r#"{"Status":"running","Running":true,"OOMKilled":false,"ExitCode":0,"StartedAt":"2026-11-25T10:00:00Z"}|{"engine": "strata", "slots": 2}"#,
        );
        let o = d.parse_status(&i, &out).unwrap();
        assert!(o.exists && o.running);
        assert_eq!(o.spec_raw.unwrap()["slots"], 2);
        let gone = d
            .parse_status(&i, &ExecOutput::fail(1, "Error: No such object: ninfer-a"))
            .unwrap();
        assert!(!gone.exists && !gone.running);
        let oom = d
            .parse_status(
                &i,
                &ExecOutput::ok_with(
                    r#"{"Status":"exited","Running":false,"OOMKilled":true,"ExitCode":137}|"#,
                ),
            )
            .unwrap();
        assert!(oom.oom_killed && oom.exists && !oom.running);
        assert_eq!(oom.exit_code, Some(137));
    }

    #[test]
    fn process_and_systemd_and_template() {
        let mut c = sample();
        let mut e = c.engines["ninfer"].clone();
        e.driver = DriverKind::Process;
        e.process = Some(ProcessSpec {
            command: "x".into(),
            cwd: None,
            run_dir: Some("/tmp/run".into()),
            log_file: None,
        });
        e.run.as_mut().unwrap().command = vec![
            "/opt/engine".into(),
            "--port".into(),
            "{port}".into(),
            "{key_args}".into(),
            "{options}".into(),
        ];
        c.engines.insert("proc".into(), e.clone());
        let art = c.models["dense"].artifacts["ninfer"].clone();
        c.models
            .get_mut("dense")
            .unwrap()
            .artifacts
            .insert("proc".into(), art);
        let cmds = resolve::start_commands(&c, "A", &spec("proc", "4lane", "dense")).unwrap();
        let t = cmds[0].text();
        assert!(t.contains("setsid /opt/engine --port 8094 --api-key-file /run/secrets/engine-key --max-context"), "{t}");
        assert!(t.contains("echo $! > /tmp/run/ninfer-a.pid"), "{t}");
        let p = Process::from(&e);
        let o = p
            .parse_status(
                &resolve::instance_ref(&c, "A", "proc").unwrap(),
                &ExecOutput::ok_with("RUNNING=1\nEXISTS=1\nSPEC={\"engine\": \"proc\"}\n"),
            )
            .unwrap();
        assert!(o.running && o.exists && o.spec_raw.is_some());

        let mut s = e.clone();
        s.driver = DriverKind::Systemd;
        s.systemd = Some(SystemdSpec {
            unit: "abhoth-engine@{card}".into(),
            spec_file: None,
            env_file: None,
        });
        c.engines.insert("unit".into(), s.clone());
        let art = c.models["dense"].artifacts["ninfer"].clone();
        c.models
            .get_mut("dense")
            .unwrap()
            .artifacts
            .insert("unit".into(), art);
        let cmds = resolve::start_commands(&c, "B", &spec("unit", "4lane", "dense")).unwrap();
        assert!(
            cmds[0].text().ends_with("systemctl start abhoth-engine@B"),
            "{}",
            cmds[0].text()
        );
        let o = Systemd::from(&s)
            .parse_status(
                &resolve::instance_ref(&c, "B", "unit").unwrap(),
                &ExecOutput::ok_with("ActiveState=active\nExecMainStatus=0\nSPEC=\n"),
            )
            .unwrap();
        assert!(o.running);

        let tpl = Template {
            t: DriverTemplates {
                start: "run {card} {args}".into(),
                stop: "halt {card}".into(),
                status: "st {card}".into(),
                logs: None,
            },
        };
        let o = tpl
            .parse_status(
                &InstanceRef::default(),
                &ExecOutput::ok_with("noise\n{\"running\": true, \"spec\": {\"a\": 1}}"),
            )
            .unwrap();
        assert!(o.running && o.exists);
        assert!(
            tpl.parse_status(&InstanceRef::default(), &ExecOutput::ok_with("nope"))
                .is_err()
        );
    }
}
