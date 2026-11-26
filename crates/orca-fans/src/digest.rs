//! HTTP digest authentication (RFC 2617 / RFC 7616, MD5 and MD5-sess, `qop=auth` or none),
//! as ESPHome's `web_server` (`auth: type: digest`) needs it. MD5 is implemented here (RFC
//! 1321) rather than pulling in a new crate: it is only used for this challenge/response.

use std::fmt::Write as _;

/// RFC 1321 MD5. Returns the 16-byte digest.
pub fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32);
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ])
        });
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

pub fn md5_hex(input: &str) -> String {
    let mut s = String::with_capacity(32);
    for b in md5(input.as_bytes()) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A parsed `WWW-Authenticate: Digest …` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    /// `auth` if the server offered it (we never use `auth-int`).
    pub qop_auth: bool,
    pub sess: bool,
}

impl Challenge {
    /// Parses a `WWW-Authenticate` header value. `None` if it is not a usable digest challenge
    /// (another scheme, an algorithm other than MD5 / MD5-sess, or no nonce).
    pub fn parse(header: &str) -> Option<Challenge> {
        let rest = header.trim();
        let (scheme, params) = rest.split_once(char::is_whitespace)?;
        if !scheme.eq_ignore_ascii_case("digest") {
            return None;
        }
        let mut realm = None;
        let mut nonce = None;
        let mut opaque = None;
        let mut qop_auth = false;
        let mut sess = false;
        for (k, v) in split_params(params) {
            match k.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(v),
                "nonce" => nonce = Some(v),
                "opaque" => opaque = Some(v),
                "qop" => qop_auth = v.split(',').any(|q| q.trim().eq_ignore_ascii_case("auth")),
                "algorithm" => {
                    if v.eq_ignore_ascii_case("md5-sess") {
                        sess = true;
                    } else if !v.eq_ignore_ascii_case("md5") {
                        return None;
                    }
                }
                _ => {}
            }
        }
        Some(Challenge {
            realm: realm.unwrap_or_default(),
            nonce: nonce?,
            opaque,
            qop_auth,
            sess,
        })
    }

    /// The `Authorization` header value for one request. `nc` counts requests made with this
    /// nonce (starting at 1); `cnonce` is a fresh client nonce.
    pub fn authorization(
        &self,
        user: &str,
        password: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let mut ha1 = md5_hex(&format!("{user}:{}:{password}", self.realm));
        if self.sess {
            ha1 = md5_hex(&format!("{ha1}:{}:{cnonce}", self.nonce));
        }
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let nc_s = format!("{nc:08x}");
        let response = if self.qop_auth {
            md5_hex(&format!("{ha1}:{}:{nc_s}:{cnonce}:auth:{ha2}", self.nonce))
        } else {
            md5_hex(&format!("{ha1}:{}:{ha2}", self.nonce))
        };
        let mut h = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
            quote(user),
            quote(&self.realm),
            quote(&self.nonce),
            quote(uri),
            response
        );
        if self.sess {
            h.push_str(", algorithm=MD5-sess");
        } else {
            h.push_str(", algorithm=MD5");
        }
        if self.qop_auth {
            let _ = write!(h, ", qop=auth, nc={nc_s}, cnonce=\"{}\"", quote(cnonce));
        }
        if let Some(o) = &self.opaque {
            let _ = write!(h, ", opaque=\"{}\"", quote(o));
        }
        h
    }
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Splits `a="x, y", b=z` into pairs, honouring quoted strings with backslash escapes.
fn split_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next().is_none() {
            break;
        }
        let mut val = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            val.push(n);
                        }
                    }
                    '"' => break,
                    _ => val.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                val.push(c);
                chars.next();
            }
        }
        out.push((key.trim().to_string(), val.trim().to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_rfc1321_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex("message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn rfc2617_example() {
        // RFC 2617 3.5: Mufasa / Circle Of Life.
        let c = Challenge::parse(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        assert!(c.qop_auth && !c.sess);
        let h = c.authorization(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert!(
            h.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{h}"
        );
        assert!(h.contains("nc=00000001"));
        assert!(h.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
    }

    #[test]
    fn rejects_other_schemes_and_algorithms() {
        assert!(Challenge::parse(r#"Basic realm="x""#).is_none());
        assert!(Challenge::parse(r#"Digest realm="x", nonce="n", algorithm=SHA-256"#).is_none());
        assert!(Challenge::parse(r#"Digest realm="x""#).is_none());
        let c = Challenge::parse(r#"Digest realm="asyncesp", nonce="abc", algorithm=MD5"#).unwrap();
        assert!(!c.qop_auth);
        assert_eq!(c.realm, "asyncesp");
    }
}
