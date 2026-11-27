//! The components' names (docs/NAMES.md). Three parts go by names from the lore Abhoth itself
//! comes from; their old plain names keep working everywhere a person or a script types them:
//!
//! - **Sothoth** (the router; Yog-Sothoth, "the Gate and the Key"): the gateway every request
//!   passes through. `abhoth sothoth ...` = `abhoth router ...`; `sothoth.toml` / `sothoth.json`
//!   or `router.toml` / `router.json`; `ABHOTH_SOTHOTH_*` or `ABHOTH_ROUTER_*`.
//! - **Nodens** (the supervisor; the guardian Elder God): watches fans, thermals, clocks and GPU
//!   health. `abhoth nodens ...` = `abhoth supervisor ...`; `nodens.toml` or `supervisor.toml`.
//! - **Migo** (the bridge; the Mi-Go ferry things between worlds): brings other machines in.
//!   `abhoth migo ...` = `abhoth bridge ...`; `migo.toml` or `bridge.toml`; `ABHOTH_MIGO_*` or
//!   `ABHOTH_BRIDGE_*`.
//!
//! Only what people see and type changed: crates, modules, wire messages, descriptors, state
//! files and API paths keep the old names, so old and new builds work together.

use std::path::{Path, PathBuf};

/// The router's name.
pub const SOTHOTH: &str = "Sothoth";
/// The supervisor's name.
pub const NODENS: &str = "Nodens";
/// The bridge's name.
pub const MIGO: &str = "Migo";

/// The internal (old) subcommand word for a component word typed on the command line:
/// `sothoth` -> `router`, `nodens` -> `supervisor`, `migo` -> `bridge`; any other word as is.
pub fn subcommand(word: &str) -> &str {
    match word {
        "sothoth" => "router",
        "nodens" => "supervisor",
        "migo" => "bridge",
        w => w,
    }
}

/// Config file names: (new, old).
pub const CONFIG_NAMES: &[(&str, &str)] = &[
    ("sothoth.toml", "router.toml"),
    ("sothoth.json", "router.json"),
    ("nodens.toml", "supervisor.toml"),
    ("migo.toml", "bridge.toml"),
];

/// The file to read for a config path someone gave: the path itself when it exists, else the
/// same folder's file under the other name (new <-> old, [`CONFIG_NAMES`]) when that exists,
/// else the path as given (so the error names what was asked for). Files are never renamed.
pub fn config_path(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return path.to_path_buf();
    };
    let other = CONFIG_NAMES.iter().find_map(|&(new, old)| {
        if name == new {
            Some(old)
        } else if name == old {
            Some(new)
        } else {
            None
        }
    });
    match other.map(|o| path.with_file_name(o)) {
        Some(p) if p.exists() => p,
        _ => path.to_path_buf(),
    }
}

/// An environment variable under its new name (`ABHOTH_SOTHOTH_URL`), else its old one
/// (`ABHOTH_ROUTER_URL`, then `OPENORCA_ROUTER_URL` as [`crate::brand::env_var`] reads it).
/// `new` and `old` are the parts after the prefix. Empty values count as unset.
pub fn env_var(new: &str, old: &str) -> Option<String> {
    env_with(|k| std::env::var(k).ok(), new, old)
}

/// [`env_var`] with `get` for the environment.
pub fn env_with(get: impl Fn(&str) -> Option<String>, new: &str, old: &str) -> Option<String> {
    [
        format!("ABHOTH_{new}"),
        format!("ABHOTH_{old}"),
        format!("OPENORCA_{old}"),
    ]
    .iter()
    .find_map(|k| get(k).filter(|v| !v.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommands_map_to_the_old_words() {
        assert_eq!(subcommand("sothoth"), "router");
        assert_eq!(subcommand("nodens"), "supervisor");
        assert_eq!(subcommand("migo"), "bridge");
        assert_eq!(subcommand("router"), "router");
        assert_eq!(subcommand("bridge"), "bridge");
        assert_eq!(subcommand("engine"), "engine");
    }

    #[test]
    fn config_paths_fall_back_both_ways() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        // Nothing there: the path as given.
        assert_eq!(config_path(&p.join("sothoth.toml")), p.join("sothoth.toml"));
        // Only the old name: a new-name path finds it.
        std::fs::write(p.join("router.toml"), "").unwrap();
        std::fs::write(p.join("bridge.toml"), "").unwrap();
        std::fs::write(p.join("supervisor.toml"), "").unwrap();
        std::fs::write(p.join("router.json"), "").unwrap();
        assert_eq!(config_path(&p.join("sothoth.toml")), p.join("router.toml"));
        assert_eq!(config_path(&p.join("sothoth.json")), p.join("router.json"));
        assert_eq!(config_path(&p.join("migo.toml")), p.join("bridge.toml"));
        assert_eq!(
            config_path(&p.join("nodens.toml")),
            p.join("supervisor.toml")
        );
        // The old name itself, as before.
        assert_eq!(config_path(&p.join("router.toml")), p.join("router.toml"));
        // Only the new name: an old-name path finds it.
        let e = tempfile::tempdir().unwrap();
        std::fs::write(e.path().join("migo.toml"), "").unwrap();
        assert_eq!(
            config_path(&e.path().join("bridge.toml")),
            e.path().join("migo.toml")
        );
        // Both: each as named (nothing is merged or renamed).
        std::fs::write(p.join("sothoth.toml"), "").unwrap();
        assert_eq!(config_path(&p.join("sothoth.toml")), p.join("sothoth.toml"));
        assert_eq!(config_path(&p.join("router.toml")), p.join("router.toml"));
        // Other names are left alone.
        assert_eq!(config_path(&p.join("other.toml")), p.join("other.toml"));
    }

    #[test]
    fn env_reads_the_new_name_first() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let both = env(&[("ABHOTH_SOTHOTH_URL", "new"), ("ABHOTH_ROUTER_URL", "old")]);
        assert_eq!(
            env_with(both, "SOTHOTH_URL", "ROUTER_URL").as_deref(),
            Some("new")
        );
        let old = env(&[("ABHOTH_ROUTER_URL", "old")]);
        assert_eq!(
            env_with(old, "SOTHOTH_URL", "ROUTER_URL").as_deref(),
            Some("old")
        );
        let legacy = env(&[("ABHOTH_SOTHOTH_URL", ""), ("OPENORCA_ROUTER_URL", "oo")]);
        assert_eq!(
            env_with(legacy, "SOTHOTH_URL", "ROUTER_URL").as_deref(),
            Some("oo")
        );
        assert_eq!(env_with(env(&[]), "SOTHOTH_URL", "ROUTER_URL"), None);
    }
}
