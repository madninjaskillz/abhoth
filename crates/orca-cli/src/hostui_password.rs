//! `HostUiPassword`: `openorca --hostui-set-password <username>` creates or changes the web UI's
//! sign-in account (sign-in mode, `--hostui-login`). The password is typed twice without echo,
//! or read from stdin when it is redirected (one line), hashed with PBKDF2 and stored in
//! `<state>/account.json`; it is never printed or logged. `--hostui-reset-2fa` removes the TOTP
//! secret (set up again at the next sign-in when 2FA is required, or from the UI).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use orca_webui::audit::AuditLog;
use orca_webui::login::accounts::AccountStore;
use serde_json::json;

pub const SET_FLAG: &str = "--hostui-set-password";
pub const RESET_2FA_FLAG: &str = "--hostui-reset-2fa";

/// `HostUiArgs.StateDirFrom`: `--hostui-state=<dir>`, else the default.
fn state_dir_from(args: &[String]) -> PathBuf {
    args.iter()
        .find_map(|a| a.strip_prefix("--hostui-state=").map(PathBuf::from))
        .unwrap_or_else(orca_webui::hostui::default_state_dir)
}

/// The two account commands; `None` when neither flag is present (else the exit code).
pub fn try_run(args: &[String]) -> Option<i32> {
    let set = args.iter().position(|a| a == SET_FLAG);
    let reset = args.iter().any(|a| a == RESET_2FA_FLAG);
    if set.is_none() && !reset {
        return None;
    }
    let state_dir = state_dir_from(args);
    let password = || -> Option<String> {
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
    };
    Some(run(args, set, reset, &state_dir, password))
}

/// [`try_run`] with the password source given (tests pass one).
pub fn run(
    args: &[String],
    set: Option<usize>,
    reset: bool,
    state_dir: &Path,
    password: impl FnOnce() -> Option<String>,
) -> i32 {
    let store = AccountStore::new(state_dir);
    if reset {
        let Some(mut account) = store.load() else {
            eprintln!("abhoth: no web UI account yet.");
            return 1;
        };
        account.totp_secret = None;
        account.totp_confirmed = false;
        account.totp_last_step = 0;
        if let Err(e) = store.save(&account) {
            eprintln!("abhoth: {e}");
            return 1;
        }
        println!(
            "2FA removed for {} ({}).",
            account.username,
            store.file_path().display()
        );
        if set.is_none() {
            return 0;
        }
    }
    let Some(set) = set else { return 0 };
    let Some(user) = args.get(set + 1).filter(|a| !a.starts_with("--")) else {
        eprintln!(
            "usage: {} {SET_FLAG} <username> [--hostui-state=<dir>]",
            orca_core::brand::BINARY_NAME
        );
        return 2;
    };
    let Some(password) = password() else {
        return 1;
    };
    let previous = store.load();
    match store.create(user, &password) {
        Ok(mut account) => {
            if let Some(prev) =
                previous.filter(|p| p.totp_confirmed && p.username == account.username)
            {
                // A password change keeps a 2FA set up for the same user.
                account.totp_secret = prev.totp_secret;
                account.totp_confirmed = true;
                account.totp_last_step = prev.totp_last_step;
                if let Err(e) = store.save(&account) {
                    eprintln!("abhoth: {e}");
                    return 1;
                }
            }
            AuditLog::new(Some(state_dir.join("audit.log"))).write(
                None,
                "account.password_set",
                json!({"user": account.username, "via": "cli"}),
            );
            println!(
                "Password set for {} ({}). Restart the web UI to end existing sessions.",
                account.username,
                store.file_path().display()
            );
            0
        }
        Err(e) => {
            eprintln!("abhoth: {e}");
            1
        }
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
    fn set_password_hashes_it_and_keeps_2fa_for_the_same_user() {
        let dir = tempfile::tempdir().unwrap();
        let a = args(&["--hostui-set-password", "me"]);
        assert_eq!(
            run(&a, Some(0), false, dir.path(), || Some("short".into())),
            1
        );
        assert_eq!(
            run(&a, Some(0), false, dir.path(), || Some(
                "a long enough password".into()
            )),
            0
        );
        let store = AccountStore::new(dir.path());
        let mut acc = store.load().unwrap();
        assert!(orca_webui::login::accounts::verify_password(
            "a long enough password",
            &acc.password_hash
        ));
        let text = std::fs::read_to_string(store.file_path()).unwrap();
        assert!(!text.contains("a long enough password"));
        acc.totp_secret = Some("SECRET".into());
        acc.totp_confirmed = true;
        acc.totp_last_step = 9;
        store.save(&acc).unwrap();
        assert_eq!(
            run(&a, Some(0), false, dir.path(), || Some(
                "another long password".into()
            )),
            0
        );
        let again = AccountStore::new(dir.path()).load().unwrap();
        assert!(again.totp_confirmed);
        assert_eq!(again.totp_secret.as_deref(), Some("SECRET"));
        let audit = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
        assert!(audit.contains("account.password_set") && !audit.contains("password\":"));
        // Reset 2FA.
        assert_eq!(
            run(
                &args(&["--hostui-reset-2fa"]),
                None,
                true,
                dir.path(),
                || None
            ),
            0
        );
        assert!(!AccountStore::new(dir.path()).load().unwrap().totp_confirmed);
        // Usage.
        assert_eq!(
            run(
                &args(&["--hostui-set-password"]),
                Some(0),
                false,
                dir.path(),
                || None
            ),
            2
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(run(&[], None, true, empty.path(), || None), 1);
        assert!(try_run(&args(&["--print", "x"])).is_none());
    }
}
