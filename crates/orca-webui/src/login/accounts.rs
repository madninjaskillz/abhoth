//! `AccountStore`: the one web UI account in `<state>/account.json` (mode 600 on Unix), written
//! atomically. Passwords are hashed with PBKDF2-HMAC-SHA256 at 600,000 iterations (OWASP's 2023
//! figure).

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

pub const ITERATIONS: u32 = 600_000;

/// A username, a PBKDF2 password hash and the TOTP secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub username: String,
    /// `pbkdf2-sha256$<iterations>$<salt b64>$<hash b64>`.
    pub password_hash: String,
    /// Base32 TOTP secret; `None` until set up.
    pub totp_secret: Option<String>,
    /// True once a code from the secret has been entered (2FA then required).
    pub totp_confirmed: bool,
    /// The last TOTP step used, so a code works once.
    pub totp_last_step: i64,
    pub created: String,
}

pub struct AccountStore {
    path: PathBuf,
    cached: Mutex<Option<Account>>,
}

impl AccountStore {
    pub fn new(state_dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(state_dir);
        Self {
            path: state_dir.join("account.json"),
            cached: Mutex::new(None),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Option<Account> {
        let mut cached = self.cached.lock().unwrap();
        if cached.is_none() {
            let text = std::fs::read_to_string(&self.path).ok()?;
            *cached = serde_json::from_str(&text).ok();
        }
        cached.clone()
    }

    pub fn save(&self, account: &Account) -> std::io::Result<()> {
        let mut cached = self.cached.lock().unwrap();
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(account).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        *cached = Some(account.clone());
        Ok(())
    }

    /// Create (or replace) the account with a new password; 2FA is set up again.
    pub fn create(&self, username: &str, password: &str) -> Result<Account, String> {
        if username.trim().is_empty() {
            return Err("A username is required.".into());
        }
        if password.encode_utf16().count() < 12 {
            return Err("The password must be at least 12 characters.".into());
        }
        let account = Account {
            username: username.trim().to_string(),
            password_hash: hash(password, ITERATIONS),
            created: crate::util::utc_now_iso(),
            ..Default::default()
        };
        self.save(&account).map_err(|e| e.to_string())?;
        Ok(account)
    }
}

fn derive(password: &str, salt: &[u8], iterations: u32, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, iterations, &mut out);
    out
}

pub fn hash(password: &str, iterations: u32) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("random bytes");
    let h = derive(password, &salt, iterations, 32);
    format!(
        "pbkdf2-sha256${iterations}${}${}",
        B64.encode(salt),
        B64.encode(h)
    )
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let Some(iterations) = parts[1].parse::<u32>().ok().filter(|i| *i >= 1) else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (B64.decode(parts[2]), B64.decode(parts[3])) else {
        return false;
    };
    let actual = derive(password, &salt, iterations, expected.len());
    super::fixed_time_eq(&actual, &expected)
}

/// A hash of a password nobody has, to spend the same time on an unknown username.
pub static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("random bytes");
    hash(&B64.encode(b), ITERATIONS)
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_hashed_not_stored() {
        let h = hash("correct horse battery staple", 1000);
        assert!(h.starts_with("pbkdf2-sha256$1000$"));
        assert!(!h.contains("staple"));
        assert!(verify_password("correct horse battery staple", &h));
        assert!(!verify_password("correct horse battery stapler", &h));
        assert!(!verify_password("x", "plain"));
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        assert!(store.create("x", "short").is_err());
        assert!(store.create(" ", "long enough password").is_err());
        assert!(store.load().is_none());
    }

    #[test]
    fn the_account_round_trips_in_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        let a = Account {
            username: "me".into(),
            password_hash: hash("long enough password", 10),
            totp_secret: Some("ABC".into()),
            totp_confirmed: true,
            totp_last_step: 7,
            created: "2026-01-01T00:00:00Z".into(),
        };
        store.save(&a).unwrap();
        let text = std::fs::read_to_string(store.file_path()).unwrap();
        assert!(text.contains("\"totpConfirmed\": true"), "{text}");
        assert_eq!(AccountStore::new(dir.path()).load().unwrap(), a);
    }
}
