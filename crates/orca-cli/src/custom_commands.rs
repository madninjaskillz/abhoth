//! `CustomCommandLoader`: user slash commands from markdown files. `~/.openorca/commands/*.md`
//! first, then `<project>/.orca/commands/*.md` (a project command overrides a global one of
//! the same name). The file name without `.md` is the command name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct CustomCommandLoader {
    config_dir: PathBuf,
    cwd: PathBuf,
}

impl CustomCommandLoader {
    /// C# reads the process's current directory; the port takes it explicitly.
    pub fn new(config_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            cwd: cwd.into(),
        }
    }

    /// Lowercased name -> (name as written, file path).
    pub fn discover_commands(&self) -> BTreeMap<String, (String, PathBuf)> {
        let mut commands = BTreeMap::new();
        scan(&self.config_dir.join("commands"), &mut commands);
        if let Some(root) = orca_core::config::project_instructions::find_project_root(&self.cwd) {
            scan(&root.join(".orca").join("commands"), &mut commands);
        }
        commands
    }

    /// The command names as written.
    pub fn names(&self) -> Vec<String> {
        self.discover_commands()
            .into_values()
            .map(|(n, _)| n)
            .collect()
    }

    /// `LoadTemplateAsync`: the file's text, or None when there is no such command.
    pub fn load_template(&self, name: &str) -> Option<String> {
        let (_, path) = self.discover_commands().remove(&name.to_lowercase())?;
        let bytes = std::fs::read(&path).ok()?;
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        Some(String::from_utf8_lossy(bytes).to_string())
    }
}

fn scan(dir: &Path, commands: &mut BTreeMap<String, (String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && !stem.trim().is_empty()
        {
            commands.insert(stem.to_lowercase(), (stem.to_string(), path.clone()));
        }
    }
}

/// `CustomCommand.ExecuteAsync`'s template expansion: `{{ARGS}}` is all arguments joined with
/// spaces, `{{ARG1}}`, `{{ARG2}}`, ... the single ones.
pub fn render_template(template: &str, args: &[String]) -> String {
    let mut rendered = template.replace("{{ARGS}}", &args.join(" "));
    for (i, a) in args.iter().enumerate() {
        rendered = rendered.replace(&format!("{{{{ARG{}}}}}", i + 1), a);
    }
    rendered
}

#[cfg(test)]
mod tests {
    //! Ported from CustomCommandLoaderTests.cs (the working directory is passed in instead of
    //! changing the process's current directory).
    use super::*;

    fn project() -> (tempfile::TempDir, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        (dir, tempfile::tempdir().unwrap())
    }

    #[test]
    fn load_template_async_reads_file_content() {
        let (dir, home) = project();
        let cmds = dir.path().join(".orca").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(
            cmds.join("review-pr.md"),
            "Review PR #{{ARG1}} with focus on {{ARGS}}",
        )
        .unwrap();
        let loader = CustomCommandLoader::new(home.path(), dir.path());
        let t = loader.load_template("review-pr").unwrap();
        assert!(t.contains("{{ARG1}}") && t.contains("{{ARGS}}"));
    }

    #[test]
    fn discover_commands_finds_md_files() {
        let (dir, home) = project();
        let cmds = dir.path().join(".orca").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("test-cmd.md"), "# Test Command\nDo something.").unwrap();
        std::fs::write(cmds.join("deploy.md"), "# Deploy\nRun deploy.").unwrap();
        let names = CustomCommandLoader::new(home.path(), dir.path()).names();
        assert!(names.contains(&"test-cmd".to_string()));
        assert!(names.contains(&"deploy".to_string()));
    }

    #[test]
    fn load_template_async_returns_null_for_missing() {
        let (dir, home) = project();
        assert!(
            CustomCommandLoader::new(home.path(), dir.path())
                .load_template("nonexistent")
                .is_none()
        );
    }

    #[test]
    fn discover_commands_handles_nonexistent_dirs() {
        let (dir, home) = project();
        assert!(
            CustomCommandLoader::new(home.path(), dir.path())
                .discover_commands()
                .is_empty()
        );
    }

    #[test]
    fn project_command_overrides_global() {
        let (dir, home) = project();
        std::fs::create_dir_all(home.path().join("commands")).unwrap();
        std::fs::write(home.path().join("commands").join("x.md"), "global").unwrap();
        let cmds = dir.path().join(".orca").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("x.md"), "project").unwrap();
        let loader = CustomCommandLoader::new(home.path(), dir.path());
        assert_eq!(loader.load_template("x").as_deref(), Some("project"));
    }

    #[test]
    fn render_template_fills_args() {
        let args = vec!["123".to_string(), "perf".to_string()];
        assert_eq!(
            render_template("PR {{ARG1}} ({{ARG2}}): {{ARGS}} {{ARG3}}", &args),
            "PR 123 (perf): 123 perf {{ARG3}}"
        );
    }
}
