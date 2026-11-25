//! `AutoFormatter`: runs `formatter.commands[<extension>]` (with `{file}` replaced by the full
//! path) after a file was modified, through `cmd.exe /c` or `/bin/sh -c`, 10 s timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::config::FormatterConfig;

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct AutoFormatter {
    config: FormatterConfig,
}

impl AutoFormatter {
    pub fn new(config: FormatterConfig) -> Self {
        Self { config }
    }

    /// `FormatFileAsync`: the formatter's trimmed output (stdout on success, `Formatter
    /// warning: <stderr>` on failure), or `None` when nothing is configured, there was no
    /// output, or it failed to run. `file_path` is resolved against `cwd`.
    pub async fn format_file(&self, file_path: &str, cwd: &Path) -> Option<String> {
        if !self.config.enabled {
            return None;
        }
        let ext = extension(file_path)?;
        // Dictionary<string, string> with the default (ordinal, case-sensitive) comparer.
        let template = self.config.commands.get(&ext)?;
        let full = orca_abstractions::full_path(cwd, file_path)
            .display()
            .to_string();
        let command = template.replace("{file}", &full);
        tracing::debug!("Running formatter for {full}: {command}");

        let (shell, flag) = if cfg!(windows) {
            ("cmd.exe", "/c")
        } else {
            ("/bin/sh", "-c")
        };
        let mut cmd = super::hidden_command(shell);
        cmd.arg(flag)
            .arg(&command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to start formatter process for {full}: {e}");
                return None;
            }
        };
        let output = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                tracing::warn!("Formatter failed for {full}: {e}");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    "Formatter timed out after {}ms for {full}",
                    TIMEOUT.as_millis()
                );
                return None;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            tracing::debug!(
                "Formatter exited with code {:?} for {full}: {stderr}",
                output.status.code()
            );
            return (!stderr.trim().is_empty())
                .then(|| format!("Formatter warning: {}", stderr.trim()));
        }
        (!stdout.trim().is_empty()).then(|| stdout.trim().to_string())
    }
}

/// `Path.GetExtension`: the last `.xxx` of the file name (with the dot), or `None`.
fn extension(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = name.rfind('.')?;
    let ext = &name[dot..];
    (ext.len() > 1).then(|| ext.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatter(enabled: bool, ext: &str, command: &str) -> AutoFormatter {
        let mut c = FormatterConfig {
            enabled,
            ..FormatterConfig::default()
        };
        c.commands.insert(ext.into(), command.into());
        AutoFormatter::new(c)
    }

    #[test]
    fn extension_like_path_get_extension() {
        assert_eq!(extension("a/b.rs").as_deref(), Some(".rs"));
        assert_eq!(extension("a.tar.gz").as_deref(), Some(".gz"));
        assert_eq!(extension("dir.x/README"), None);
        assert_eq!(extension("trailing."), None);
    }

    #[tokio::test]
    async fn disabled_or_unconfigured_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            formatter(false, ".txt", "echo hi")
                .format_file("a.txt", dir.path())
                .await
                .is_none()
        );
        assert!(
            formatter(true, ".rs", "echo hi")
                .format_file("a.txt", dir.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn success_returns_trimmed_stdout_with_the_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = formatter(true, ".txt", "echo formatted {file}")
            .format_file("a.txt", dir.path())
            .await
            .unwrap();
        assert!(out.starts_with("formatted "), "{out}");
        assert!(out.ends_with("a.txt"), "{out}");
        assert!(out.contains(&dir.path().display().to_string()), "{out}");
    }

    #[tokio::test]
    async fn failure_returns_stderr_as_warning() {
        let dir = tempfile::tempdir().unwrap();
        let out = formatter(true, ".txt", "echo bad 1>&2 && exit 3")
            .format_file("a.txt", dir.path())
            .await;
        assert_eq!(out.as_deref(), Some("Formatter warning: bad"));
    }
}
