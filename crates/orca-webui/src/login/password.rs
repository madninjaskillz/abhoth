//! Password hashing: Argon2id (PHC string, OWASP's m=19 MiB, t=2, p=1), and the PBKDF2 hashes
//! of the old single account (`account.json`, D85), which still verify and are re-hashed with
//! Argon2id at the next sign-in. The length rules: at least [`MIN_LEN`] characters, at most
//! [`MAX_LEN`]; no composition rules.

use std::sync::LazyLock;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

pub const MIN_LEN: usize = 12;
pub const MAX_LEN: usize = 1024;

/// Cost settings: the default, or cheap ones for tests (the hash records which).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub memory_kib: u32,
    pub iterations: u32,
}

impl Default for Cost {
    fn default() -> Self {
        Self {
            memory_kib: 19 * 1024,
            iterations: 2,
        }
    }
}

impl Cost {
    /// For tests only.
    pub const FAST: Cost = Cost {
        memory_kib: 64,
        iterations: 1,
    };
}

/// The password is acceptable (length only), else the message for the user.
pub fn check(password: &str) -> Result<(), String> {
    let n = password.chars().count();
    if n < MIN_LEN {
        return Err(format!(
            "The password must be at least {MIN_LEN} characters."
        ));
    }
    if n > MAX_LEN {
        return Err(format!(
            "The password must be at most {MAX_LEN} characters."
        ));
    }
    Ok(())
}

fn hasher(cost: Cost) -> Argon2<'static> {
    let params = Params::new(cost.memory_kib, cost.iterations, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash(password: &str, cost: Cost) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("random bytes");
    let salt = SaltString::encode_b64(&salt).expect("salt");
    hasher(cost)
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

/// Verify against an Argon2 PHC string or a legacy PBKDF2 hash (constant-time comparisons).
pub fn verify(password: &str, stored: &str) -> bool {
    if stored.starts_with("pbkdf2-sha256$") {
        return super::accounts::verify_password(password, stored);
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    // The parameters come from the hash itself.
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// The hash should be redone with the current settings (a legacy or cheaper hash).
pub fn needs_rehash(stored: &str, cost: Cost) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return true;
    };
    if parsed.algorithm.as_str() != "argon2id" {
        return true;
    }
    let Ok(p) = Params::try_from(&parsed) else {
        return true;
    };
    p.m_cost() < cost.memory_kib || p.t_cost() < cost.iterations
}

/// A hash of a password nobody has, to spend the same time on an unknown username.
pub fn dummy_hash(cost: Cost) -> &'static str {
    static DEFAULT: LazyLock<String> =
        LazyLock::new(|| hash(&crate::util::new_token(), Cost::default()));
    static FAST: LazyLock<String> = LazyLock::new(|| hash(&crate::util::new_token(), Cost::FAST));
    if cost == Cost::FAST { &FAST } else { &DEFAULT }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2id_hashes_verify_and_never_hold_the_password() {
        let h = hash("correct horse battery staple", Cost::FAST);
        assert!(h.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{h}");
        assert!(!h.contains("staple"));
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("correct horse battery stapler", &h));
        assert!(!verify("x", "garbage"));
        assert!(needs_rehash(&h, Cost::default()));
        assert!(!needs_rehash(&h, Cost::FAST));
    }

    #[test]
    fn legacy_pbkdf2_hashes_verify_and_need_a_rehash() {
        let legacy = super::super::accounts::hash("an old long password", 1000);
        assert!(verify("an old long password", &legacy));
        assert!(!verify("an old long passwore", &legacy));
        assert!(needs_rehash(&legacy, Cost::FAST));
    }

    #[test]
    fn only_the_length_is_checked() {
        assert!(check("short").is_err());
        assert!(check("aaaaaaaaaaaa").is_ok());
        assert!(check("pässwörd🙂🙂🙂🙂").is_ok());
        assert!(check(&"a".repeat(MAX_LEN + 1)).is_err());
    }
}
