//! `Totp`: RFC 6238 time-based one-time passwords: HMAC-SHA1, 30 s steps, 6 digits (what
//! authenticator apps use).

use hmac::{Hmac, Mac};

pub const DIGITS: usize = 6;
pub const STEP_SECONDS: i64 = 30;
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A new 160-bit secret, base32 (no padding).
pub fn new_secret() -> String {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).expect("random bytes");
    base32_encode(&bytes)
}

pub fn step_at(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(STEP_SECONDS)
}

/// The code for `step`; `None` when the secret isn't base32.
pub fn code(base32_secret: &str, step: i64) -> Option<String> {
    let key = base32_decode(base32_secret)?;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).ok()?;
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let binary = (u32::from(hash[offset] & 0x7f) << 24)
        | (u32::from(hash[offset + 1]) << 16)
        | (u32::from(hash[offset + 2]) << 8)
        | u32::from(hash[offset + 3]);
    Some(format!("{:06}", binary % 1_000_000))
}

/// The step the code matches (this step ±1 for clock drift), or `None`. Codes at or before
/// `last_used_step` are refused, so a code can't be replayed.
pub fn verify(
    base32_secret: &str,
    code_text: Option<&str>,
    now: i64,
    last_used_step: i64,
) -> Option<i64> {
    let code_text = code_text?.replace(' ', "");
    let code_text = code_text.trim();
    if code_text.len() != DIGITS || !code_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let current = step_at(now);
    for s in current - 1..=current + 1 {
        if s <= last_used_step {
            continue;
        }
        let expected = code(base32_secret, s)?;
        if super::fixed_time_eq(expected.as_bytes(), code_text.as_bytes()) {
            return Some(s);
        }
    }
    None
}

/// The otpauth:// URI an authenticator app scans.
pub fn uri(issuer: &str, account: &str, base32_secret: &str) -> String {
    let e = crate::util::escape_data;
    format!(
        "otpauth://totp/{}:{}?secret={base32_secret}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        e(issuer),
        e(account),
        e(issuer)
    )
}

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in data {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let clean = text
        .trim()
        .trim_end_matches('=')
        .replace(' ', "")
        .to_ascii_uppercase();
    let mut out = Vec::with_capacity(clean.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in clean.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
        buffer &= (1 << bits) - 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc6238_vector_and_tolerates_one_step_of_drift() {
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(code(&secret, step_at(59)).unwrap(), "287082");
        assert_eq!(code(&secret, step_at(1111111109)).unwrap(), "081804");
        let now = 1111111109;
        let prev = code(&secret, step_at(now) - 1).unwrap();
        assert!(verify(&secret, Some(&prev), now, 0).is_some());
        let old = code(&secret, step_at(now) - 3).unwrap();
        assert!(verify(&secret, Some(&old), now, 0).is_none());
        assert!(verify(&secret, Some("12345"), now, 0).is_none());
        // Used once: not again.
        let cur = code(&secret, step_at(now)).unwrap();
        let used = verify(&secret, Some(&cur), now, 0).unwrap();
        assert!(verify(&secret, Some(&cur), now, used).is_none());
    }

    #[test]
    fn base32_round_trips() {
        for n in 0..25 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&data)).unwrap(), data);
        }
        assert!(base32_decode("not base32!").is_none());
        assert_eq!(new_secret().len(), 32);
    }
}
