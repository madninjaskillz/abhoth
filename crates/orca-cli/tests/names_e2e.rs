//! The components' names (docs/NAMES.md): `abhoth sothoth|nodens|migo` and the old
//! `abhoth router|supervisor|bridge` are the same commands, and the old and new config file
//! names stand in for each other.

use std::process::{Command, Output};

fn abhoth(args: &[&str], home: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_abhoth"))
        .args(args)
        .env("ABHOTH_HOME", home)
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn new_and_old_subcommands_are_the_same() {
    let home = tempfile::tempdir().unwrap();
    for (new, old) in [("sothoth", "router"), ("migo", "bridge")] {
        let a = abhoth(&[new, "--help"], home.path());
        let b = abhoth(&[old, "--help"], home.path());
        assert!(a.status.success(), "{new}: {}", text(&a));
        assert!(b.status.success(), "{old}: {}", text(&b));
        assert_eq!(text(&a), text(&b), "{new} and {old} print the same help");
    }
    // Nodens / supervisor: the same usage line (and exit code) without arguments.
    let a = abhoth(&["nodens"], home.path());
    let b = abhoth(&["supervisor"], home.path());
    assert_eq!(a.status.code(), Some(2));
    assert_eq!(b.status.code(), Some(2));
    assert!(text(&a).contains("usage"), "{}", text(&a));
    assert_eq!(text(&a), text(&b));
}

#[test]
fn either_config_name_is_found() {
    let home = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    // A folder made by `init` under the old names (as live routers have).
    std::fs::write(d.path().join("router.json"), r#"{"rev": "r1"}"#).unwrap();
    let new = d.path().join("sothoth.json");
    let o = abhoth(
        &["sothoth", "check", "--config", &new.display().to_string()],
        home.path(),
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).starts_with("ok: rev r1"), "{}", text(&o));
    // ...and the other way round: only the new name, asked for by the old one.
    let e = tempfile::tempdir().unwrap();
    std::fs::write(e.path().join("sothoth.json"), r#"{"rev": "r2"}"#).unwrap();
    let old = e.path().join("router.json");
    let o = abhoth(
        &["router", "check", "--config", &old.display().to_string()],
        home.path(),
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).starts_with("ok: rev r2"), "{}", text(&o));
    // Nodens: `check` on a supervisor.toml asked for as nodens.toml reaches the file (its
    // problems, not "No such file").
    std::fs::write(
        d.path().join("supervisor.toml"),
        "[supervisor]\nserver = \"x\"\n",
    )
    .unwrap();
    let n = d.path().join("nodens.toml");
    let o = abhoth(
        &["nodens", "check", "--config", &n.display().to_string()],
        home.path(),
    );
    assert!(
        !text(&o).to_lowercase().contains("no such file"),
        "{}",
        text(&o)
    );
    assert!(!text(&o).contains("cannot find"), "{}", text(&o));
}
