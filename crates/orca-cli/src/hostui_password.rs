//! Web UI accounts from the command line (D140), on the database `<home>/abhoth.db`:
//!
//! - `abhoth admin create <username> [--user]`: a new admin (or, `--user`, a regular user);
//! - `abhoth admin reset-password <username>`: a new password; the user's sessions end;
//! - `abhoth admin reset-2fa <username>`: two-factor sign-in off (set up again later);
//! - `abhoth admin list`: the users.
//!
//! `--db=<file>` names another database. The password is typed twice without echo, or read from
//! stdin when it is redirected (one line); it is hashed with Argon2id and never printed or
//! logged. The old flags `--hostui-set-password <user>` (create, or reset when the user exists)
//! and `--hostui-reset-2fa [<user>]` still work. The old single account (`webui/account.json`)
//! is imported first when the database has no users.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_webui::audit::AuditLog;
use orca_webui::login::db::{CreateError, UserDb};
use orca_webui::login::{LoginAuth, LoginOptions, password};
use serde_json::json;

pub const SET_FLAG: &str = "--hostui-set-password";
pub const RESET_2FA_FLAG: &str = "--hostui-reset-2fa";

/// The database: `--db=<file>`, else `abhoth.db` in the state home.
fn db_path(args: &[String]) -> PathBuf {
    args.iter()
        .find_map(|a| a.strip_prefix("--db=").map(PathBuf::from))
        .unwrap_or_else(|| orca_core::config::default_config_dir().join("abhoth.db"))
}

/// The old account's folder (`--hostui-state=<dir>`, else `<home>/webui`).
fn legacy_dir(args: &[String]) -> PathBuf {
    args.iter()
        .find_map(|a| a.strip_prefix("--hostui-state=").map(PathBuf::from))
        .unwrap_or_else(orca_webui::hostui::default_state_dir)
}

fn read_password() -> Option<String> {
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        return Some(line.trim_end_matches(['\r', '\n']).to_string());
    }
    let first = read_hidden("New password: ");
    if read_hidden("Again: ") != first {
        eprintln!("abhoth: the passwords differ; nothing changed.");
        return None;
    }
    Some(first)
}

/// `abhoth admin ...` or one of the old flags; `None` when the arguments are neither (else the
/// exit code).
pub fn try_run(args: &[String]) -> Option<i32> {
    let command: Vec<String> = if args.first().map(String::as_str) == Some("admin") {
        args[1..].to_vec()
    } else if let Some(i) = args.iter().position(|a| a == SET_FLAG) {
        let mut c = vec!["set-password".to_string()];
        c.extend(args.get(i + 1).filter(|a| !a.starts_with("--")).cloned());
        c.extend(args.iter().filter(|a| a.starts_with("--")).cloned());
        c
    } else {
        let i = args.iter().position(|a| a == RESET_2FA_FLAG)?;
        let mut c = vec!["reset-2fa".to_string()];
        c.extend(args.get(i + 1).filter(|a| !a.starts_with("--")).cloned());
        c.extend(args.iter().filter(|a| a.starts_with("--")).cloned());
        c
    };
    Some(run(
        &command,
        &db_path(args),
        Some(&legacy_dir(args)),
        read_password,
    ))
}

fn usage() -> i32 {
    let bin = orca_core::brand::BINARY_NAME;
    eprintln!(
        "usage: {bin} admin create <username> [--user]\n       {bin} admin reset-password <username>\n       {bin} admin reset-2fa <username>\n       {bin} admin list\n  (--db=<file>: another database than <home>/abhoth.db)"
    );
    2
}

/// Run an `admin` command (`args` without the word "admin") on the database at `db`.
pub fn run(
    args: &[String],
    db: &Path,
    legacy: Option<&Path>,
    password: impl FnOnce() -> Option<String>,
) -> i32 {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(cmd) = positional.first().map(|s| s.as_str()) else {
        return usage();
    };
    let user_arg = positional.get(1).map(|s| s.as_str());
    let audit = Arc::new(AuditLog::new(
        db.parent().map(|d| d.join("webui").join("audit.log")),
    ));
    let options = LoginOptions {
        legacy_dir: legacy.map(Path::to_path_buf),
        ..Default::default()
    };
    let auth = match UserDb::open(db)
        .and_then(|d| LoginAuth::with_db(options, d, String::new(), false, audit))
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("abhoth: {e}");
            return 1;
        }
    };
    let cli = json!({"via": "cli"});
    match cmd {
        "list" => {
            for u in auth.db.users() {
                println!(
                    "{:>4}  {:<24} {}{}{}",
                    u.id,
                    u.username,
                    if u.admin { "admin" } else { "user" },
                    if u.disabled { ", disabled" } else { "" },
                    if u.totp_enabled { ", 2FA" } else { "" }
                );
            }
            0
        }
        "create" | "set-password" | "reset-password" => {
            let Some(name) = user_arg else {
                return usage();
            };
            let existing = auth.db.user_by_name(name);
            match (cmd, &existing) {
                ("create", Some(_)) => {
                    eprintln!("abhoth: {name} exists already; use reset-password.");
                    return 1;
                }
                ("reset-password", None) => {
                    eprintln!("abhoth: no user {name}.");
                    return 1;
                }
                _ => {}
            }
            let name = match orca_webui::login::check_username(name) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("abhoth: {e}");
                    return 1;
                }
            };
            let Some(pw) = password() else { return 1 };
            if let Err(e) = password::check(&pw) {
                eprintln!("abhoth: {e}");
                return 1;
            }
            let hash = password::hash(&pw, password::Cost::default());
            match existing {
                Some(u) => {
                    auth.db.set_password_hash(u.id, &hash);
                    let ended = auth.db.delete_sessions_of(u.id, None);
                    auth.record("user.password_reset", None, Some(&u.username), None, cli);
                    println!(
                        "Password set for {} ({}); {ended} session(s) ended.",
                        u.username,
                        auth.db.path().display()
                    );
                    0
                }
                None => {
                    let admin = !args.iter().any(|a| a == "--user");
                    match auth.db.create_user(&name, &hash, admin, false, auth.now()) {
                        Ok(u) => {
                            auth.record(
                                "user.created",
                                None,
                                Some(&u.username),
                                None,
                                json!({"via": "cli", "admin": admin}),
                            );
                            println!(
                                "Created {} {} ({}).",
                                if admin { "admin" } else { "user" },
                                u.username,
                                auth.db.path().display()
                            );
                            0
                        }
                        Err(CreateError::Taken) => {
                            eprintln!("abhoth: {name} exists already.");
                            1
                        }
                        Err(e) => {
                            eprintln!("abhoth: {e}");
                            1
                        }
                    }
                }
            }
        }
        "reset-2fa" => {
            // The old flag took no user: the first admin.
            let user = match user_arg {
                Some(n) => auth.db.user_by_name(n),
                None => auth.db.first_admin(),
            };
            let Some(u) = user else {
                eprintln!("abhoth: no such user.");
                return 1;
            };
            auth.db.disable_totp(u.id);
            auth.record("user.totp_reset", None, Some(&u.username), None, cli);
            println!("2FA removed for {}.", u.username);
            0
        }
        _ => usage(),
    }
}

/// A line typed without echo (`Console.ReadKey(intercept: true)`).
fn read_hidden(prompt: &str) -> String {
    use crossterm::event::{Event, KeyCode, KeyEventKind, read};
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut text = String::new();
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    loop {
        match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !c.is_control() => text.push(c),
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break,
        }
    }
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    println!();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn create_reset_and_list_users_on_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("abhoth.db");
        let pw = || Some("a long enough password".to_string());
        assert_eq!(
            run(&args(&["create", "ann"]), &db, None, || Some(
                "short".into()
            )),
            1
        );
        assert_eq!(run(&args(&["create", "ann"]), &db, None, pw), 0);
        assert_eq!(run(&args(&["create", "ann"]), &db, None, pw), 1, "exists");
        assert_eq!(run(&args(&["create", "bob", "--user"]), &db, None, pw), 0);
        let d = UserDb::open(&db).unwrap();
        let ann = d.user_by_name("ann").unwrap();
        assert!(ann.admin);
        assert!(!d.user_by_name("bob").unwrap().admin);
        assert!(password::verify(
            "a long enough password",
            &ann.password_hash
        ));
        d.create_session("tok", ann.id, "ok", "c", 1, None, None, false);
        d.enable_totp(ann.id, "SECRET", 1);
        assert_eq!(
            run(&args(&["reset-password", "ann"]), &db, None, || Some(
                "another long password".into()
            )),
            0
        );
        assert!(d.session_by_token("tok").is_none(), "sessions end");
        assert!(password::verify(
            "another long password",
            &d.user(ann.id).unwrap().password_hash
        ));
        assert_eq!(run(&args(&["reset-password", "nobody"]), &db, None, pw), 1);
        assert_eq!(run(&args(&["reset-2fa", "ann"]), &db, None, || None), 0);
        assert!(!d.user(ann.id).unwrap().totp_enabled);
        assert_eq!(run(&args(&["list"]), &db, None, || None), 0);
        assert_eq!(run(&args(&["bogus"]), &db, None, || None), 2);
        assert_eq!(run(&[], &db, None, || None), 2);
        // Never the password in the database or the audit log.
        let audit = std::fs::read_to_string(dir.path().join("webui").join("audit.log")).unwrap();
        assert!(audit.contains("user.created") && !audit.contains("long password"));
    }

    #[test]
    fn the_old_flags_map_to_admin_commands_and_import_the_old_account() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("webui");
        let store = orca_webui::login::accounts::AccountStore::new(&legacy);
        store.create("old", "the old long password").unwrap();
        let db = dir.path().join("abhoth.db");
        // reset-2fa with no user: the first admin, which is the imported account.
        assert_eq!(run(&args(&["reset-2fa"]), &db, Some(&legacy), || None), 0);
        let d = UserDb::open(&db).unwrap();
        let old = d.user_by_name("old").unwrap();
        assert!(old.admin);
        assert!(password::verify(
            "the old long password",
            &old.password_hash
        ));
        assert!(try_run(&args(&["--print", "x"])).is_none());
    }
}
