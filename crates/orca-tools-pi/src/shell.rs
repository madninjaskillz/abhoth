//! Port of Pi's getShellConfig (utils/shell.ts, `PiShell`): a configured shell path wins;
//! otherwise /bin/bash (then bash on PATH, then sh) on Unix, and Git Bash on Windows
//! (%ProgramFiles%\Git\bin\bash.exe, the x86 variant, the Git install that owns git.exe on PATH,
//! then bash.exe on PATH). The legacy WSL launcher (System32\bash.exe) is skipped as a PATH
//! fallback because it runs commands in a different filesystem view.

use std::path::{Path, PathBuf};

/// A resolved bash: the executable and whether it reads the command on stdin (`-s`, the legacy
/// WSL launcher) instead of `-c`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiShellConfig {
    pub path: String,
    pub command_from_stdin: bool,
}

impl PiShellConfig {
    pub fn new(path: impl Into<String>, command_from_stdin: bool) -> Self {
        Self {
            path: path.into(),
            command_from_stdin,
        }
    }
}

/// `PiShell.Resolve`: the bash to use, or Pi's error text.
pub fn resolve(custom_shell_path: Option<&str>) -> Result<PiShellConfig, String> {
    if let Some(custom) = custom_shell_path.filter(|p| !p.trim().is_empty()) {
        if Path::new(custom).is_file() {
            return Ok(PiShellConfig::new(custom, is_legacy_wsl_bash(custom)));
        }
        return Err(format!("Custom shell path not found: {custom}"));
    }

    if !cfg!(windows) {
        if Path::new("/bin/bash").is_file() {
            return Ok(PiShellConfig::new("/bin/bash", false));
        }
        if let Some(bash) = find_on_path("bash") {
            return Ok(PiShellConfig::new(bash.to_string_lossy(), false));
        }
        return Ok(PiShellConfig::new("sh", false));
    }

    let mut searched = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Ok(base) = std::env::var(var) else {
            continue;
        };
        if base.is_empty() {
            continue;
        }
        let candidate = Path::new(&base).join("Git").join("bin").join("bash.exe");
        searched.push(format!("  {}", candidate.display()));
        if candidate.is_file() {
            return Ok(PiShellConfig::new(candidate.to_string_lossy(), false));
        }
    }

    // Git installed elsewhere: <git root>\cmd\git.exe or <git root>\bin\git.exe.
    if let Some(git) = find_on_path("git.exe")
        && let Some(root) = git.parent().and_then(Path::parent)
    {
        for rel in [
            Path::new("bin").join("bash.exe"),
            Path::new("usr").join("bin").join("bash.exe"),
        ] {
            let candidate = root.join(rel);
            searched.push(format!("  {}", candidate.display()));
            if candidate.is_file() {
                return Ok(PiShellConfig::new(candidate.to_string_lossy(), false));
            }
        }
    }

    if let Some(path_bash) = find_on_path("bash.exe")
        && !is_legacy_wsl_bash(&path_bash.to_string_lossy())
    {
        return Ok(PiShellConfig::new(path_bash.to_string_lossy(), false));
    }

    Err(format!(
        "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set pi.shellPath in config.json\n\nSearched Git Bash in:\n{}",
        searched.join("\n")
    ))
}

/// `PiShell.IsLegacyWslBash`: `<drive>:\Windows\System32\bash.exe` (or Sysnative).
pub fn is_legacy_wsl_bash(path: &str) -> bool {
    let p = path.replace('/', "\\").to_lowercase();
    let b = p.as_bytes();
    p.len() > 3
        && b.get(1) == Some(&b':')
        && p.is_char_boundary(2)
        && matches!(
            &p[2..],
            "\\windows\\system32\\bash.exe" | "\\windows\\sysnative\\bash.exe"
        )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| {
            let s = d.to_string_lossy().trim_matches('"').to_string();
            PathBuf::from(s).join(name)
        })
        .find(|c| c.is_file())
}
