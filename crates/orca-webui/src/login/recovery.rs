//! 2FA recovery codes: ten single-use codes `xxxxx-xxxxx` (50 random bits each, Crockford-like
//! base32 without look-alikes), shown once and stored hashed.

const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789"; // no i, l, o, 0, 1
pub const COUNT: usize = 10;

pub fn new_code() -> String {
    let mut b = [0u8; 10];
    getrandom::fill(&mut b).expect("random bytes");
    let chars: String = b
        .iter()
        .map(|x| ALPHABET[*x as usize % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &chars[..5], &chars[5..])
}

pub fn new_codes() -> Vec<String> {
    (0..COUNT).map(|_| new_code()).collect()
}

/// Lower case without separators or spaces, as typed codes are compared.
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Looks like a recovery code rather than a 6-digit TOTP code.
pub fn looks_like(code: &str) -> bool {
    normalize(code).len() == 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_distinct_and_normalize() {
        let codes = new_codes();
        assert_eq!(codes.len(), COUNT);
        let mut sorted = codes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), COUNT);
        let c = &codes[0];
        assert_eq!(c.len(), 11);
        assert!(looks_like(c));
        assert_eq!(normalize(&c.to_uppercase().replace('-', " ")), normalize(c));
        assert!(!looks_like("123456"));
    }
}
