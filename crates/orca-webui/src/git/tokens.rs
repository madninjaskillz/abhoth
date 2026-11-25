//! `GitTokenStore` (per-project GitHub tokens in `project-tokens.json`, mode 600, written
//! atomically; the browser only ever gets the masked form) and `SecretFile`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The stored value of a project that uses the GitHub sign-in (no real token contains '@').
pub const APP_LINK: &str = "@github-app";

pub struct GitTokenStore {
    file: PathBuf,
    lock: Mutex<()>,
}

impl GitTokenStore {
    pub fn new(directory: &Path) -> Self {
        Self {
            file: directory.join("project-tokens.json"),
            lock: Mutex::new(()),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.file
    }

    pub fn get(&self, project_id: &str) -> Option<String> {
        let _g = self.lock.lock().unwrap();
        self.load().get(project_id).cloned()
    }

    pub fn set(&self, project_id: &str, token: &str) -> Result<(), String> {
        let token = validate(Some(token))?;
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        all.insert(project_id.into(), token);
        self.save(&all);
        Ok(())
    }

    pub fn link_app(&self, project_id: &str) {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        all.insert(project_id.into(), APP_LINK.into());
        self.save(&all);
    }

    pub fn remove(&self, project_id: &str) -> bool {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        if all.remove(project_id).is_none() {
            return false;
        }
        self.save(&all);
        true
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, all: &BTreeMap<String, String>) {
        let _ = write_secret(&self.file, &serde_json::to_string(all).unwrap_or_default());
    }
}

/// "github_pat_…abcd": a known prefix and the last four characters.
pub fn mask(token: &str) -> String {
    let prefix = if token.starts_with("github_pat_") {
        "github_pat_".to_string()
    } else {
        let b = token.as_bytes();
        if b.len() >= 4 && b[0] == b'g' && b[1] == b'h' && b"pousr".contains(&b[2]) && b[3] == b'_'
        {
            token[..4].to_string()
        } else {
            String::new()
        }
    };
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        "…".into()
    } else {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}…{tail}")
    }
}

/// A plausible token: no whitespace or control characters, a sane length.
pub fn validate(token: Option<&str>) -> Result<String, String> {
    let t = token.unwrap_or("").trim();
    let n = t.chars().count();
    if !(20..=400).contains(&n)
        || t.chars().any(|c| c.is_whitespace() || c.is_control())
        || t.contains(':')
        || t.contains('@')
    {
        return Err(
            "That doesn't look like a GitHub token (a fine-grained token starts with github_pat_)."
                .into(),
        );
    }
    Ok(t.to_string())
}

/// A private file (mode 600 on Unix) written atomically: temp file, then rename.
pub fn write_secret(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_prefix_and_last_four() {
        assert_eq!(
            mask("github_pat_11AAAAAAAAAAAAAAAAwxyz"),
            "github_pat_…wxyz"
        );
        assert_eq!(mask("ghp_abcdefghijklmnop1234"), "ghp_…1234");
        assert_eq!(mask("short"), "…");
    }

    #[test]
    fn validate_refuses_odd_tokens() {
        assert!(validate(Some("ghp_abcdefghijklmnopqrstuvwxyz")).is_ok());
        assert!(validate(Some("too short")).is_err());
        assert!(validate(Some("ghp_abcdefghij klmnopqrstuvwxyz")).is_err());
        assert!(validate(Some("user:ghp_abcdefghijklmnopqrstuvwxyz")).is_err());
    }

    #[test]
    fn store_round_trips_and_links_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let s = GitTokenStore::new(dir.path());
        s.set("p1", "  ghp_abcdefghijklmnopqrstuvwxyz ").unwrap();
        assert_eq!(
            s.get("p1").as_deref(),
            Some("ghp_abcdefghijklmnopqrstuvwxyz")
        );
        s.link_app("p2");
        assert_eq!(s.get("p2").as_deref(), Some(APP_LINK));
        assert!(s.remove("p1"));
        assert!(!s.remove("p1"));
    }
}
