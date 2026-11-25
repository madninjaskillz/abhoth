//! Finds a bridge CLI on disk and checks its version (`CliLocator`).

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::transport::{ProcessSpec, RunError, run_captured};
use super::{CancelToken, EngineError, paths};

/// A `System.Version` with three parts ("2.1.326").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: i32,
    pub minor: i32,
    pub build: i32,
}

impl Version {
    pub const fn new(major: i32, minor: i32, build: i32) -> Self {
        Self {
            major,
            minor,
            build,
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

static VERSION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("valid regex"));

/// The full path of `command`: `configured_path` when set (must exist), else the first match
/// on PATH (with PATHEXT on Windows). `None` when not found.
pub fn find(
    command: &str,
    configured_path: Option<&str>,
    path_variable: Option<&str>,
) -> Option<String> {
    if let Some(configured) = configured_path.filter(|p| !p.trim().is_empty()) {
        return std::path::Path::new(configured)
            .is_file()
            .then(|| paths::get_full_path(configured));
    }
    let path = match path_variable {
        Some(p) => p.to_string(),
        None => std::env::var("PATH").unwrap_or_default(),
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
    for dir in path.split(separator).filter(|d| !d.is_empty()) {
        let dir = dir.trim().trim_matches('"');
        for ext in &extensions {
            let candidate = paths::combine(dir, &format!("{command}{ext}"));
            if std::path::Path::new(&candidate).is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A start spec for `executable`: Windows .cmd/.bat shims (npm installs) run through cmd.exe.
pub fn start_spec(executable: &str, args: &[String]) -> ProcessSpec {
    let lower = executable.to_lowercase();
    if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
        let mut arguments = vec!["/d".to_string(), "/c".to_string(), executable.to_string()];
        arguments.extend(args.iter().cloned());
        ProcessSpec::new("cmd.exe", arguments)
    } else {
        ProcessSpec::new(executable, args.to_vec())
    }
}

/// The first "x.y.z" in a `--version` output, or `None`.
pub fn parse_version(output: Option<&str>) -> Option<Version> {
    let output = output.filter(|o| !o.trim().is_empty())?;
    let m = VERSION_PATTERN.find(output)?;
    let mut parts = m.as_str().split('.').map(|p| p.parse::<i32>().ok());
    Some(Version::new(parts.next()??, parts.next()??, parts.next()??))
}

/// Run `<exe> --version` (15 s limit) and check it against `minimum`.
pub async fn check_version_async(
    executable: &str,
    display_name: &str,
    minimum: Version,
    update_hint: &str,
    cancel: &CancelToken,
) -> Result<Version, EngineError> {
    let spec = start_spec(executable, &["--version".to_string()]);
    let output = match run_captured(&spec, Duration::from_secs(15), cancel).await {
        Ok((_, output)) => output,
        Err(RunError::Cancelled) => return Err(EngineError::Cancelled),
        Err(RunError::TimedOut) => {
            return Err(EngineError::Unavailable(format!(
                "{display_name} did not answer `--version` within 15 seconds ({executable})."
            )));
        }
        Err(RunError::Start(e)) => {
            return Err(EngineError::Unavailable(format!(
                "Could not run {display_name} ({executable}): {e}"
            )));
        }
    };
    check_version(&output, display_name, minimum, update_hint)
}

/// The version in `version_output`, if it is at least `minimum`.
pub fn check_version(
    version_output: &str,
    display_name: &str,
    minimum: Version,
    update_hint: &str,
) -> Result<Version, EngineError> {
    let Some(version) = parse_version(Some(version_output)) else {
        return Err(EngineError::Unavailable(format!(
            "Could not read {display_name}'s version from `--version` (got \"{}\").",
            version_output.trim()
        )));
    };
    if version < minimum {
        return Err(EngineError::Unavailable(format!(
            "{display_name} {version} is too old for the OpenOrca bridge (needs {minimum} or newer). {update_hint}"
        )));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::claude::MINIMUM_VERSION;

    #[test]
    fn version_check() {
        for (output, ok) in [
            ("2.1.326 (Claude Code)", true),
            ("2.0.0", true),
            ("1.0.120 (Claude Code)", false),
        ] {
            let r = check_version(output, "Claude Code", MINIMUM_VERSION, "update");
            if ok {
                assert!(r.is_ok(), "{output}");
            } else {
                assert!(r.unwrap_err().to_string().contains("too old"));
            }
        }
    }

    #[test]
    fn version_check_unreadable() {
        assert!(matches!(
            check_version(
                "command not found",
                "Claude Code",
                Version::new(2, 0, 0),
                ""
            ),
            Err(EngineError::Unavailable(_))
        ));
    }

    #[test]
    fn version_messages() {
        assert_eq!(
            check_version("1.0.3", "Claude Code", Version::new(2, 0, 0), "Update it.")
                .unwrap_err()
                .to_string(),
            "Claude Code 1.0.3 is too old for the OpenOrca bridge (needs 2.0.0 or newer). Update it."
        );
        assert_eq!(
            check_version(" nope \n", "Codex", Version::new(0, 100, 0), "")
                .unwrap_err()
                .to_string(),
            "Could not read Codex's version from `--version` (got \"nope\")."
        );
        assert_eq!(
            parse_version(Some("codex-cli 0.167.1")),
            Some(Version::new(0, 167, 1))
        );
        assert_eq!(parse_version(Some("99999999999.1.1")), None);
    }
}
