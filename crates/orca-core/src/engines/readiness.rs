//! Checks a bridge CLI before its engine is enabled: installed, new enough, logged in
//! (`EngineReadinessChecker`). Claude Code: `claude auth status --json` (JSON `loggedIn`).
//! Codex: `codex login status` (exit code 0 = logged in). Nothing these print is passed on
//! except the yes/no: the status output carries account details.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;

use super::cli_locator::{self, Version};
use super::transport::{RunError, current_environment, env, run_captured};
use super::{AgentEngines, CancelToken, EngineError, claude, paths};
use crate::config::OrcaConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineReadinessState {
    Ready,
    NotInstalled,
    TooOld,
    NotLoggedIn,
    Error,
}

/// Whether an engine can run now. For `NotLoggedIn`, `login_command` is what to run (in a
/// terminal the user can type into) to log in.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineReadiness {
    pub engine_id: String,
    pub state: EngineReadinessState,
    pub message: String,
    pub executable: Option<String>,
    pub version: Option<Version>,
    pub login_command: Option<Vec<String>>,
}

impl EngineReadiness {
    fn new(engine_id: &str, state: EngineReadinessState, message: impl Into<String>) -> Self {
        Self {
            engine_id: engine_id.to_string(),
            state,
            message: message.into(),
            executable: None,
            version: None,
            login_command: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state == EngineReadinessState::Ready
    }

    /// The login command as one line ("claude auth login").
    pub fn login_command_line(&self) -> Option<String> {
        self.login_command
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|c| c.join(" "))
    }
}

/// "Log in first": a front end should open a terminal, run `login_command`, and check again.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineAuthRequired {
    pub engine_id: String,
    pub message: String,
    pub login_command: Vec<String>,
}

impl EngineAuthRequired {
    pub fn login_command_line(&self) -> String {
        self.login_command.join(" ")
    }
}

/// Codex's minimum for `exec --json` / app-server as the bridge uses them.
pub const CODEX_MINIMUM_VERSION: Version = Version::new(0, 100, 0);

/// Runs a CLI and returns (exit code, output); `Err` is the failure's message. Swappable for
/// tests (`CommandRunner`).
pub type CommandRunner = dyn Fn(String, Vec<String>, bool) -> BoxFuture<'static, Result<(i32, String), RunFailure>>
    + Send
    + Sync;

/// A [`CommandRunner`] failure: the message, or cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    Message(String),
    Cancelled,
}

/// Is `engine` usable now? `Err` for an unknown engine (C# `ArgumentException`) or a failure
/// the C# check lets through (the `--version` run itself failing).
pub async fn check(
    engine: &str,
    config: &OrcaConfig,
    cancel: &CancelToken,
) -> Result<EngineReadiness, EngineError> {
    check_with(engine, config, cancel, None, None).await
}

/// [`check`] with a swapped command runner and PATH (tests).
pub async fn check_with(
    engine: &str,
    config: &OrcaConfig,
    cancel: &CancelToken,
    runner: Option<&CommandRunner>,
    path_variable: Option<&str>,
) -> Result<EngineReadiness, EngineError> {
    let id = AgentEngines::normalize(Some(engine)).unwrap_or(engine);
    if id == AgentEngines::NATIVE {
        return Ok(EngineReadiness::new(
            id,
            EngineReadinessState::Ready,
            "native",
        ));
    }
    let (command, cli, display, minimum, update) = match id {
        AgentEngines::CLAUDE => (
            "claude",
            &config.bridges.claude,
            "Claude Code",
            claude::MINIMUM_VERSION,
            "Update it with `claude update`.",
        ),
        AgentEngines::CODEX => (
            "codex",
            &config.bridges.codex,
            "Codex",
            CODEX_MINIMUM_VERSION,
            "Update it with `npm install -g @openai/codex`.",
        ),
        _ => {
            return Err(EngineError::Failed(format!(
                "Unknown engine {engine} (Parameter 'engine')"
            )));
        }
    };

    let Some(exe) = cli_locator::find(command, cli.path.as_deref(), path_variable) else {
        let message = match cli.path.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("{display} CLI not found at bridges.{id}.path = {p}."),
            None => format!(
                "{display} CLI (`{command}`) not found on PATH. Install it, or set bridges.{id}.path."
            ),
        };
        return Ok(EngineReadiness::new(
            id,
            EngineReadinessState::NotInstalled,
            message,
        ));
    };

    let run = |args: &[&str]| -> BoxFuture<'static, Result<(i32, String), RunFailure>> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        match runner {
            Some(r) => r(exe.clone(), args, cli.pass_api_key),
            None => {
                let (exe, pass, cancel) = (exe.clone(), cli.pass_api_key, cancel.clone());
                Box::pin(async move { run_command(&exe, &args, pass, &cancel).await })
            }
        }
    };

    let version = match run(&["--version"]).await {
        Ok((_, out)) => match cli_locator::check_version(&out, display, minimum, update) {
            Ok(v) => v,
            Err(e) => {
                let message = e.to_string();
                let state = if message.contains("too old") {
                    EngineReadinessState::TooOld
                } else {
                    EngineReadinessState::Error
                };
                let mut r = EngineReadiness::new(id, state, message);
                r.executable = Some(exe);
                return Ok(r);
            }
        },
        Err(RunFailure::Cancelled) => return Err(EngineError::Cancelled),
        Err(RunFailure::Message(m)) => return Err(EngineError::Failed(m)),
    };

    let (logged_in, login) = if id == AgentEngines::CLAUDE {
        match run(&["auth", "status", "--json"]).await {
            Ok((_, status)) => (
                parse_claude_logged_in(&status),
                vec![exe.clone(), "auth".into(), "login".into()],
            ),
            Err(f) => return login_failure(id, display, exe, version, f),
        }
    } else {
        match run(&["login", "status"]).await {
            Ok((code, _)) => (
                code == 0,
                vec![exe.clone(), "login".into(), "--device-auth".into()],
            ),
            Err(f) => return login_failure(id, display, exe, version, f),
        }
    };

    let mut r = if logged_in {
        EngineReadiness::new(
            id,
            EngineReadinessState::Ready,
            format!("{display} {version}"),
        )
    } else {
        let mut r = EngineReadiness::new(
            id,
            EngineReadinessState::NotLoggedIn,
            format!(
                "{display} is not logged in. Log in with `{} {}`.",
                paths::file_name_without_extension(&exe),
                login[1..].join(" ")
            ),
        );
        r.login_command = Some(login);
        r
    };
    r.executable = Some(exe);
    r.version = Some(version);
    Ok(r)
}

fn login_failure(
    id: &str,
    display: &str,
    exe: String,
    version: Version,
    failure: RunFailure,
) -> Result<EngineReadiness, EngineError> {
    match failure {
        RunFailure::Cancelled => Err(EngineError::Cancelled),
        RunFailure::Message(m) => {
            let mut r = EngineReadiness::new(
                id,
                EngineReadinessState::Error,
                format!("Could not check {display}'s login: {m}"),
            );
            r.executable = Some(exe);
            r.version = Some(version);
            Ok(r)
        }
    }
}

/// True when `claude auth status --json` says `"loggedIn": true`.
pub fn parse_claude_logged_in(output: &str) -> bool {
    let (Some(start), Some(end)) = (output.find('{'), output.rfind('}')) else {
        return false;
    };
    if end <= start {
        return false;
    }
    match serde_json::from_str::<Value>(&output[start..=end]) {
        Ok(Value::Object(o)) => o.get("loggedIn") == Some(&Value::Bool(true)),
        _ => false,
    }
}

/// Run a CLI with the bridge's environment (API keys stripped unless passed), 20 s limit.
pub async fn run_command(
    executable: &str,
    args: &[String],
    pass_api_key: bool,
    cancel: &CancelToken,
) -> Result<(i32, String), RunFailure> {
    let mut spec = cli_locator::start_spec(executable, args);
    let mut environment = current_environment();
    claude::apply_environment(&mut environment, pass_api_key, None);
    if !pass_api_key {
        env::remove(&mut environment, "OPENAI_API_KEY");
    }
    spec.environment = Some(environment);
    match run_captured(&spec, Duration::from_secs(20), cancel).await {
        Ok(r) => Ok(r),
        Err(RunError::Cancelled) => Err(RunFailure::Cancelled),
        Err(RunError::TimedOut) => Err(RunFailure::Message(format!(
            "{} {} did not finish within 20 s",
            paths::file_name(executable),
            args.join(" ")
        ))),
        Err(RunError::Start(e)) => Err(RunFailure::Message(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake CLIs (scripts on a private PATH): logged in, logged out, missing.
    struct FakeDir(tempfile::TempDir);

    impl FakeDir {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self) -> String {
            self.0.path().to_string_lossy().into_owned()
        }

        /// --version prints `version`; the status command prints and exits as given.
        fn fake_cli(
            &self,
            name: &str,
            version: &str,
            status_arg: &str,
            status_output: &str,
            status_exit: i32,
        ) {
            if cfg!(windows) {
                let esc = |s: &str| s.replace('(', "^(").replace(')', "^)");
                std::fs::write(
                    self.0.path().join(format!("{name}.cmd")),
                    format!(
                        "@echo off\r\nif \"%~1\"==\"--version\" goto version\r\nif \"%~1\"==\"{status_arg}\" goto status\r\nexit /b 9\r\n:version\r\necho {}\r\nexit /b 0\r\n:status\r\necho {}\r\nexit /b {status_exit}\r\n",
                        esc(version),
                        esc(status_output)
                    ),
                )
                .unwrap();
            } else {
                let path = self.0.path().join(name);
                std::fs::write(
                    &path,
                    format!(
                        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nif [ \"$1\" = \"{status_arg}\" ]; then echo '{status_output}'; exit {status_exit}; fi\nexit 9\n"
                    ),
                )
                .unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
            }
        }

        async fn check(&self, engine: &str) -> EngineReadiness {
            // Linux: running a script just written can fail with ETXTBSY ("Text file busy",
            // os error 26) while another test's fork still holds the write descriptor, between
            // its fork and exec. Busier with more test threads (16 on the CI runners); retry.
            for _ in 0..20 {
                match check_with(
                    engine,
                    &OrcaConfig::default(),
                    &CancelToken::new(),
                    None,
                    Some(&self.path()),
                )
                .await
                {
                    Err(e) if format!("{e:?}").contains("os error 26") => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    r => return r.unwrap(),
                }
            }
            panic!("{engine}: the fake CLI stayed busy (ETXTBSY)");
        }
    }

    #[tokio::test]
    async fn claude_logged_in_is_ready() {
        let d = FakeDir::new();
        d.fake_cli(
            "claude",
            "2.1.326 (Claude Code)",
            "auth",
            r#"{"loggedIn": true, "authMethod": "claude.ai"}"#,
            0,
        );
        let r = d.check("claude").await;
        assert_eq!(r.state, EngineReadinessState::Ready, "{}", r.message);
        assert_eq!(r.version, Some(Version::new(2, 1, 326)));
        assert_eq!(r.message, "Claude Code 2.1.326");
    }

    #[tokio::test]
    async fn claude_logged_out_suggests_login() {
        let d = FakeDir::new();
        d.fake_cli(
            "claude",
            "2.1.326 (Claude Code)",
            "auth",
            r#"{"loggedIn": false, "authMethod": "none"}"#,
            0,
        );
        let r = d.check("claude").await;
        assert_eq!(r.state, EngineReadinessState::NotLoggedIn);
        assert_eq!(r.login_command.as_ref().unwrap()[1..], ["auth", "login"]);
        assert!(r.message.contains("claude auth login"), "{}", r.message);
    }

    #[tokio::test]
    async fn claude_too_old() {
        let d = FakeDir::new();
        d.fake_cli(
            "claude",
            "1.0.3 (Claude Code)",
            "auth",
            r#"{"loggedIn": true}"#,
            0,
        );
        assert_eq!(d.check("claude").await.state, EngineReadinessState::TooOld);
    }

    #[tokio::test]
    async fn codex_logged_in_and_logged_out() {
        let d = FakeDir::new();
        d.fake_cli(
            "codex",
            "codex-cli 0.167.1",
            "login",
            "Logged in using ChatGPT",
            0,
        );
        assert_eq!(d.check("codex").await.state, EngineReadinessState::Ready);

        d.fake_cli("codex", "codex-cli 0.167.1", "login", "Not logged in", 1);
        let r = d.check("codex").await;
        assert_eq!(r.state, EngineReadinessState::NotLoggedIn);
        assert_eq!(
            r.login_command.as_ref().unwrap()[1..],
            ["login", "--device-auth"]
        );
    }

    #[tokio::test]
    async fn missing_binary_not_installed() {
        let d = FakeDir::new();
        let r = d.check("claude").await;
        assert_eq!(r.state, EngineReadinessState::NotInstalled);
        assert!(r.message.contains("not found on PATH"));
    }

    #[tokio::test]
    async fn native_always_ready() {
        assert!(FakeDir::new().check("native").await.is_ready());
    }

    #[test]
    fn parse_claude_status() {
        assert!(parse_claude_logged_in(r#"{"loggedIn": true}"#));
        assert!(!parse_claude_logged_in(r#"noise {"loggedIn": false} "#));
        assert!(!parse_claude_logged_in("garbage"));
    }

    #[tokio::test]
    async fn swapped_runner_login_failure_is_an_error_state() {
        let d = FakeDir::new();
        d.fake_cli("claude", "2.1.326", "auth", "{}", 0);
        let runner: Box<CommandRunner> = Box::new(|_exe, args, _pass| {
            Box::pin(async move {
                if args[0] == "--version" {
                    Ok((0, "2.1.326 (Claude Code)".to_string()))
                } else {
                    Err(RunFailure::Message("boom".into()))
                }
            })
        });
        let r = check_with(
            "claude",
            &OrcaConfig::default(),
            &CancelToken::new(),
            Some(runner.as_ref()),
            Some(&d.path()),
        )
        .await
        .unwrap();
        assert_eq!(r.state, EngineReadinessState::Error);
        assert_eq!(r.message, "Could not check Claude Code's login: boom");
    }
}
