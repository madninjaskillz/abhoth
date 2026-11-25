//! The Rust build's product name (D88). The Rust port ships as **Abhoth**; the C# build stays
//! OpenOrca. Only what a person sees is rebranded: the state on disk (`~/.openorca`, config.json,
//! sessions, `OPENORCA_*` variables, the `X-OpenOrca-*` headers of the shared web client) keeps
//! the OpenOrca names so both builds share it.

/// The product name shown to people (TUI header, `--version`, web UI, desktop window).
pub const PRODUCT_NAME: &str = "Abhoth";
/// The executable's name (`abhoth`, `abhoth.exe`).
pub const BINARY_NAME: &str = "abhoth";
/// The name the C# build (and the state both share) goes by.
pub const LEGACY_NAME: &str = "OpenOrca";

/// The system prompt's self-name: a prompt that introduces itself as OpenOrca ("You are
/// OpenOrca, ...", the shipped template and unedited prompt files) introduces itself as
/// [`PRODUCT_NAME`]. Prompt files on disk keep the C# text (their hashes are shared), and other
/// mentions are left alone.
pub fn self_named(prompt: &str) -> String {
    const OLD: &str = "You are OpenOrca,";
    match prompt.find(OLD) {
        Some(i) if prompt[..i].trim().is_empty() || prompt[..i].ends_with('\n') => {
            let mut out = String::with_capacity(prompt.len());
            out.push_str(&prompt[..i]);
            out.push_str("You are ");
            out.push_str(PRODUCT_NAME);
            out.push(',');
            out.push_str(&prompt[i + OLD.len()..]);
            out
        }
        _ => prompt.to_string(),
    }
}

/// An environment variable read under its `ABHOTH_*` name, or its `OPENORCA_*` name when the
/// former is unset or empty (`name` is the part after the prefix, e.g. `HOME`).
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(format!("ABHOTH_{name}"))
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var(format!("OPENORCA_{name}")).ok())
}

/// The state folder's name in the user profile (`~/.abhoth`).
pub const HOME_DIR_NAME: &str = ".abhoth";
/// The C# build's state folder (`~/.openorca`), copied once into [`HOME_DIR_NAME`].
pub const LEGACY_HOME_DIR_NAME: &str = ".openorca";
/// Written into the new home after the copy.
pub const MIGRATION_MARKER: &str = ".migrated-from-openorca";

/// What [`migrate_home`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// The new home exists already, or there is nothing to copy.
    NotNeeded,
    /// The old home was copied (files copied).
    Copied(usize),
}

/// One-time copy of `legacy` into `home` when `home` does not exist yet (D89). A copy, not a
/// move: the C# build keeps using the old folder until it is retired. Logs and file checkpoints
/// stay behind (per-run history, and large), and so do `config.json.bak*` files.
pub fn migrate_home(
    legacy: &std::path::Path,
    home: &std::path::Path,
) -> std::io::Result<Migration> {
    // A home holding nothing but `logs` was made by a run that logged before migrating (or
    // without a config); it still gets the copy, and keeps its logs (D133).
    if (home.exists() && !only_logs(home)) || !legacy.is_dir() {
        return Ok(Migration::NotNeeded);
    }
    // Copy into a sibling first so a failed copy leaves no half-made home behind.
    let staging = home.with_extension("migrating");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let mut copied = 0;
    copy_tree(legacy, &staging, true, &mut copied)?;
    std::fs::write(
        staging.join(MIGRATION_MARKER),
        format!("Copied from {} by {PRODUCT_NAME}.\n", legacy.display()),
    )?;
    if home.exists() {
        let logs = home.join("logs");
        if logs.exists() {
            std::fs::rename(&logs, staging.join("logs"))?;
        }
        std::fs::remove_dir_all(home)?;
    }
    std::fs::rename(&staging, home)?;
    Ok(Migration::Copied(copied))
}

/// True when `dir` holds no entries other than a `logs` folder.
fn only_logs(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .all(|e| e.file_name() == "logs" && e.path().is_dir())
    })
}

fn copy_tree(
    from: &std::path::Path,
    to: &std::path::Path,
    top: bool,
    copied: &mut usize,
) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if top
            && (name_s == "logs"
                || name_s == "checkpoints"
                || name_s.starts_with("config.json.bak"))
        {
            continue;
        }
        let kind = entry.file_type()?;
        let target = to.join(&name);
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, false, copied)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
            *copied += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_the_opening_line_only() {
        let p = "You are OpenOrca, an autonomous AI coding agent.\nOpenOrca keeps going.";
        assert_eq!(
            self_named(p),
            "You are Abhoth, an autonomous AI coding agent.\nOpenOrca keeps going."
        );
    }

    #[test]
    fn renames_after_a_stamp_line() {
        let p = "<!-- stamp -->\nYou are OpenOrca, x";
        assert_eq!(self_named(p), "<!-- stamp -->\nYou are Abhoth, x");
    }

    #[test]
    fn migration_copies_once_and_skips_logs() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(".openorca");
        let new = root.path().join(".abhoth");
        std::fs::create_dir_all(old.join("sessions")).unwrap();
        std::fs::create_dir_all(old.join("logs")).unwrap();
        std::fs::write(old.join("config.json"), "{}").unwrap();
        std::fs::write(old.join("config.json.bak-1"), "{}").unwrap();
        std::fs::write(old.join("sessions").join("a.json"), "{}").unwrap();
        std::fs::write(old.join("logs").join("x.log"), "x").unwrap();
        assert_eq!(migrate_home(&old, &new).unwrap(), Migration::Copied(2));
        assert!(new.join("config.json").is_file());
        assert!(new.join("sessions").join("a.json").is_file());
        assert!(new.join(MIGRATION_MARKER).is_file());
        assert!(!new.join("logs").exists());
        assert!(!new.join("config.json.bak-1").exists());
        // The old folder is untouched; a second run does nothing.
        assert!(old.join("logs").join("x.log").is_file());
        std::fs::write(new.join("config.json"), "{\"changed\":1}").unwrap();
        assert_eq!(migrate_home(&old, &new).unwrap(), Migration::NotNeeded);
        assert_eq!(
            std::fs::read_to_string(new.join("config.json")).unwrap(),
            "{\"changed\":1}"
        );
    }

    #[test]
    fn a_home_with_only_logs_still_gets_the_copy_and_keeps_its_logs() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(".openorca");
        let new = root.path().join(".abhoth");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config.json"), "{}").unwrap();
        std::fs::create_dir_all(new.join("logs")).unwrap();
        std::fs::write(new.join("logs").join("early.log"), "e").unwrap();
        assert_eq!(migrate_home(&old, &new).unwrap(), Migration::Copied(1));
        assert!(new.join("config.json").is_file());
        assert!(new.join(MIGRATION_MARKER).is_file());
        assert_eq!(
            std::fs::read_to_string(new.join("logs").join("early.log")).unwrap(),
            "e"
        );
        // Anything besides logs means the home is in use: no copy.
        let other = root.path().join(".abhoth2");
        std::fs::create_dir_all(other.join("logs")).unwrap();
        std::fs::write(other.join("projects.json"), "[]").unwrap();
        assert_eq!(migrate_home(&old, &other).unwrap(), Migration::NotNeeded);
    }

    #[test]
    fn nothing_to_migrate_without_an_old_home() {
        let root = tempfile::tempdir().unwrap();
        let r = migrate_home(&root.path().join("none"), &root.path().join("new")).unwrap();
        assert_eq!(r, Migration::NotNeeded);
        assert!(!root.path().join("new").exists());
    }

    #[test]
    fn leaves_other_prompts_alone() {
        assert_eq!(self_named("You are pi."), "You are pi.");
        assert_eq!(
            self_named("Say: You are OpenOrca, then stop"),
            "Say: You are OpenOrca, then stop"
        );
    }
}
