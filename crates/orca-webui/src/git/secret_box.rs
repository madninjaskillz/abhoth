//! Secrets at rest (D146): a saved GitHub token is wrapped before it is written to disk.
//!
//! - Windows: DPAPI, current-user scope (`CryptProtectData`), so the file is useless to another
//!   Windows account or on another machine. Written as `dpapi:<base64>`.
//! - Elsewhere: no OS keyring is reliably there (headless servers, Docker), so the value is kept
//!   as `plain:<value>` and the file's mode 600 (see `tokens::write_secret`) is the protection.
//!
//! The one audited `unsafe` block of this crate is here (orca-webui's `unsafe_code` is `deny`,
//! not `forbid`, for it).

use base64::Engine;

const DPAPI: &str = "dpapi:";
const PLAIN: &str = "plain:";

/// What protects values at rest on this platform ("dpapi" or "file-permissions").
pub fn protection() -> &'static str {
    if cfg!(windows) {
        "dpapi"
    } else {
        "file-permissions"
    }
}

/// Wrap `secret` for storage.
pub fn seal(secret: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        let out = dpapi::protect(secret.as_bytes())?;
        Ok(format!(
            "{DPAPI}{}",
            base64::engine::general_purpose::STANDARD.encode(out)
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(format!("{PLAIN}{secret}"))
    }
}

/// Unwrap a stored value; `None` when it can't be read (another user's DPAPI blob, damage).
pub fn open(stored: &str) -> Option<String> {
    if let Some(p) = stored.strip_prefix(PLAIN) {
        return Some(p.to_string());
    }
    let b64 = stored.strip_prefix(DPAPI)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    #[cfg(windows)]
    {
        dpapi::unprotect(&bytes)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        None
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn call(input: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let len = u32::try_from(input.len()).map_err(|_| "secret too large".to_string())?;
        let inp = CRYPT_INTEGER_BLOB {
            cbData: len,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: `inp` points at `input`, which outlives the call and is only read (DPAPI takes
        // a mutable pointer type but doesn't write through pbData of the input). Every optional
        // pointer is null, which the API allows. On success DPAPI allocates `out.pbData` with
        // LocalAlloc; it is copied into a Vec and freed with LocalFree exactly once.
        unsafe {
            let ok = if protect {
                CryptProtectData(
                    &inp,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &inp,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            };
            if ok == 0 || out.pbData.is_null() {
                return Err("The Windows data protection API refused the value.".into());
            }
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            Ok(bytes)
        }
    }

    pub fn protect(input: &[u8]) -> Result<Vec<u8>, String> {
        call(input, true)
    }

    pub fn unprotect(input: &[u8]) -> Result<Vec<u8>, String> {
        call(input, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trips_and_hides_the_value_on_windows() {
        let secret = "ghp_TESTONLYabcdefghijklmnopqrstuvwxyz";
        let sealed = seal(secret).unwrap();
        assert_eq!(open(&sealed).as_deref(), Some(secret));
        if cfg!(windows) {
            assert!(sealed.starts_with(DPAPI));
            assert!(!sealed.contains(secret));
        }
        assert_eq!(open("dpapi:not base64!"), None);
        assert_eq!(open("something else"), None);
    }
}
