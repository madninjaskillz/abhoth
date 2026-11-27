//! Bridge tokens (13.1): `abb_…`, minted by a router admin and shown once. The router keeps only
//! the SHA-256, in `<state_dir>/secrets/bridges/<bridge-id>.sha256` (mode 600). Nothing here
//! logs or prints a token.

use std::path::{Path, PathBuf};

use orca_bridge::proto::{TOKEN_PREFIX, new_token, same, token_hash, valid_id};

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("secrets").join("bridges")
}

fn file(state_dir: &Path, id: &str) -> PathBuf {
    dir(state_dir).join(format!("{id}.sha256"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    pub id: String,
    /// Unix seconds (the file's modification time).
    pub created: i64,
}

pub fn exists(state_dir: &Path, id: &str) -> bool {
    valid_id(id) && file(state_dir, id).is_file()
}

/// A new token for bridge `id` (replacing its old one when `rotate`). The caller shows it once.
pub fn mint(state_dir: &Path, id: &str, rotate: bool) -> Result<String, String> {
    if !valid_id(id) {
        return Err(format!("`{id}`: a bridge id is 1-32 of a-z 0-9 - _"));
    }
    let f = file(state_dir, id);
    if f.exists() && !rotate {
        return Err(format!(
            "bridge {id} already has a token (rotate it to make a new one)"
        ));
    }
    let token = new_token();
    write_private(&f, &token_hash(&token)).map_err(|e| format!("{}: {e}", f.display()))?;
    Ok(token)
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("sha256.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    let mut f = o.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.write_all(b"\n")?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Which bridge a token belongs to (None: no such token).
pub fn verify(state_dir: &Path, token: &str) -> Option<String> {
    let token = token.trim();
    if !token.starts_with(TOKEN_PREFIX) || token.len() < 20 {
        return None;
    }
    let h = token_hash(token);
    let mut found = None;
    for t in list(state_dir) {
        let Ok(stored) = std::fs::read_to_string(file(state_dir, &t.id)) else {
            continue;
        };
        // Compare against every file (no early exit on a match).
        if same(stored.trim(), &h) {
            found = Some(t.id);
        }
    }
    found
}

pub fn revoke(state_dir: &Path, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err(format!("`{id}` is not a bridge id"));
    }
    let f = file(state_dir, id);
    if !f.exists() {
        return Err(format!("bridge {id} has no token"));
    }
    std::fs::remove_file(&f).map_err(|e| format!("{}: {e}", f.display()))
}

pub fn list(state_dir: &Path) -> Vec<TokenInfo> {
    let Ok(rd) = std::fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    let mut v: Vec<TokenInfo> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".sha256")?.to_string();
            if !valid_id(&id) {
                return None;
            }
            let created = e
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some(TokenInfo { id, created })
        })
        .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_verify_rotate_revoke() {
        let d = tempfile::tempdir().unwrap();
        let t = mint(d.path(), "nas", false).unwrap();
        assert!(t.starts_with("abb_"));
        let stored = std::fs::read_to_string(file(d.path(), "nas")).unwrap();
        assert!(!stored.contains(&t), "only the hash is kept");
        assert_eq!(verify(d.path(), &t).as_deref(), Some("nas"));
        assert_eq!(verify(d.path(), "abb_nope_nope_nope_nope_nope"), None);
        assert!(mint(d.path(), "nas", false).is_err());
        let t2 = mint(d.path(), "nas", true).unwrap();
        assert_eq!(verify(d.path(), &t), None, "the old token stops working");
        assert_eq!(verify(d.path(), &t2).as_deref(), Some("nas"));
        assert_eq!(list(d.path()).len(), 1);
        revoke(d.path(), "nas").unwrap();
        assert_eq!(verify(d.path(), &t2), None);
        assert!(mint(d.path(), "../x", false).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let t3 = mint(d.path(), "pc", false).unwrap();
            let _ = t3;
            let m = std::fs::metadata(file(d.path(), "pc"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
        }
    }
}
