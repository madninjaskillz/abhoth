//! Shared bits: schema parsing, path display, file times, glob matching.

use std::path::Path;
use std::time::SystemTime;

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde_json::Value;

pub fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

pub fn display(path: &Path) -> String {
    path.display().to_string()
}

/// A glob as `Microsoft.Extensions.FileSystemGlobbing.Matcher` applies it: case-insensitive,
/// `*` stays inside one path segment, `**` spans segments (including none).
pub fn glob_matcher(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    let p = pattern.replace('\\', "/");
    let p = p.trim_start_matches("./");
    GlobBuilder::new(p)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map(|g: Glob| g.compile_matcher())
}

/// `path` relative to `base`, with `/` separators (what the Matcher reports).
pub fn rel_slash(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `Path.GetRelativePath(base, path)` with native separators (`.` for the same path).
pub fn rel_native(base: &Path, path: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(r) if r.as_os_str().is_empty() => ".".into(),
        Ok(r) => r.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// The date of a UTC time as `yyyy-MM-dd` (civil-from-days; no time-zone crate needed).
pub fn utc_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// .NET's default string ordering is culture-aware; for file names "case-insensitive, then
/// ordinal" matches it in practice.
pub fn name_key(s: &str) -> (String, String) {
    (s.to_lowercase(), s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn utc_dates() {
        assert_eq!(utc_date(SystemTime::UNIX_EPOCH), "1970-01-01");
        assert_eq!(
            utc_date(SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29"
        );
    }

    #[test]
    fn globs_match_like_the_dotnet_matcher() {
        let star = glob_matcher("*.cs").unwrap();
        assert!(star.is_match("a.CS") && !star.is_match("src/a.cs"));
        let deep = glob_matcher("**/*.cs").unwrap();
        assert!(deep.is_match("a.cs") && deep.is_match("src/x/a.cs"));
        assert!(glob_matcher("bin/**").unwrap().is_match("bin/app.cs"));
    }
}
