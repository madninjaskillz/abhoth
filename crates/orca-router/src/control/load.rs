//! Loading `router.toml` (ROUTER-CONFIG.md 4.1, 4.9): includes, parsing with file/line errors,
//! the revision id, and where each item came from (so the UI writes to the defining file).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::schema::{ControlConfig, SCHEMA_VERSION};
use super::validate::{Issue, Level};

/// A file's identity for change detection (mtime, size, inode where available).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSig {
    pub modified: Option<SystemTime>,
    pub len: u64,
    pub ino: u64,
}

impl FileSig {
    pub fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(not(unix))]
        let ino = 0;
        Some(Self {
            modified: m.modified().ok(),
            len: m.len(),
            ino,
        })
    }
}

/// The parsed configuration plus what is needed to watch, locate and rewrite it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: ControlConfig,
    /// First 12 hex digits of SHA-256 over the normalised config (comments stripped, keys
    /// sorted). Replaces `gateway_rev`.
    pub rev: String,
    pub main: PathBuf,
    /// Every file read (main first), with its signature at load time and its text.
    pub files: Vec<(PathBuf, Option<FileSig>, String)>,
    /// `section.item` (`engines.ninfer`, `layouts.flash-x2`) -> the file that defines it.
    pub origin: BTreeMap<String, PathBuf>,
}

impl Loaded {
    /// Any file changed on disk since this load.
    pub fn changed(&self) -> bool {
        self.files.iter().any(|(p, sig, _)| FileSig::of(p) != *sig)
    }

    /// File and 1-based line of a TOML path such as `layouts.x.cards.A.engine`.
    pub fn locate(&self, path: &str) -> (Option<String>, Option<usize>) {
        let parts: Vec<&str> = split_path(path);
        let key2 = parts.iter().take(2).copied().collect::<Vec<_>>().join(".");
        let file = self
            .origin
            .get(&key2)
            .cloned()
            .unwrap_or_else(|| self.main.clone());
        let text = self
            .files
            .iter()
            .find(|(p, _, _)| *p == file)
            .map(|(_, _, t)| t.as_str())
            .unwrap_or("");
        (Some(file.display().to_string()), line_of(text, &parts))
    }
}

/// Split a dotted path; quoted segments (`routes."qwen3.8-27b".cold`) stay whole.
pub fn split_path(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = path.as_bytes();
    let (mut i, mut start) = (0, 0);
    let mut quoted = false;
    while i < b.len() {
        match b[i] {
            b'"' => quoted = !quoted,
            b'.' if !quoted => {
                out.push(path[start..i].trim_matches('"'));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < path.len() {
        out.push(path[start..].trim_matches('"'));
    }
    out
}

/// The line where the deepest existing part of `parts` is defined, via toml_edit spans.
pub fn line_of(text: &str, parts: &[&str]) -> Option<usize> {
    let doc = toml_edit::Document::parse(text).ok()?;
    let mut item: &toml_edit::Item = doc.as_item();
    let mut best: Option<std::ops::Range<usize>> = None;
    for p in parts {
        let next = match item {
            toml_edit::Item::Table(t) => t.get(p),
            _ => None,
        };
        let next = match (next, item) {
            (Some(n), _) => n,
            (None, toml_edit::Item::Value(toml_edit::Value::InlineTable(t))) => {
                if let Some((k, v)) = t.get_key_value(p) {
                    best = k.span().or_else(|| v.span()).or(best);
                }
                break;
            }
            (None, toml_edit::Item::ArrayOfTables(a)) => {
                match p.parse::<usize>().ok().and_then(|i| a.get(i)) {
                    Some(t) => {
                        best = t.span().or(best);
                        break;
                    }
                    None => break,
                }
            }
            _ => break,
        };
        if let toml_edit::Item::Table(t) = item
            && let Some((k, _)) = t.get_key_value(p)
        {
            best = k.span().or(best);
        }
        best = next.span().or(best);
        item = next;
    }
    best.map(|r| text[..r.start.min(text.len())].matches('\n').count() + 1)
}

/// Read the main file and its includes, merge, parse into the schema.
pub fn load(main: &Path) -> Result<Loaded, Vec<Issue>> {
    let main_text = std::fs::read_to_string(main)
        .map_err(|e| vec![Issue::err("", format!("{}: {e}", main.display()))])?;
    load_with(main, main_text)
}

/// As [`load`], with the main file's text given (UI saves validate before writing).
pub fn load_with(main: &Path, main_text: String) -> Result<Loaded, Vec<Issue>> {
    let mut issues = Vec::new();
    let mut files = vec![(main.to_path_buf(), FileSig::of(main), main_text.clone())];
    let mut merged = parse_table(main, &main_text).map_err(|i| vec![i])?;
    let mut origin = BTreeMap::new();
    record_origin(&merged, main, &mut origin);

    let includes: Vec<String> = match merged.get("include") {
        Some(toml::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let base = main.parent().unwrap_or(Path::new("."));
    for pat in includes {
        for path in expand_glob(base, &pat) {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    issues.push(Issue::err("include", format!("{}: {e}", path.display())));
                    continue;
                }
            };
            match parse_table(&path, &text) {
                Ok(t) => {
                    if t.contains_key("include") || t.contains_key("version") {
                        issues.push(Issue::err(
                            "include",
                            format!(
                                "{}: included files may not set `include` or `version`",
                                path.display()
                            ),
                        ));
                    }
                    record_origin(&t, &path, &mut origin);
                    if let Err(msg) = merge(&mut merged, t) {
                        issues.push(Issue::err("include", format!("{}: {msg}", path.display())));
                    }
                }
                Err(i) => issues.push(i),
            }
            files.push((path.clone(), FileSig::of(&path), text));
        }
    }
    if !issues.is_empty() {
        return Err(issues);
    }

    match merged.get("version").and_then(|v| v.as_integer()) {
        None => {
            return Err(vec![Issue::err(
                "version",
                "missing `version = 1` at the top of the file",
            )]);
        }
        Some(v) if v as u32 > SCHEMA_VERSION => {
            return Err(vec![Issue::err(
                "version",
                format!(
                    "config version {v} is newer than this build understands ({SCHEMA_VERSION}); refusing it"
                ),
            )]);
        }
        Some(_) => {}
    }

    let config: ControlConfig =
        toml::Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| {
                // Errors from the merged value carry no span; re-parse the main file alone to point at a line.
                let msg = e.message().to_string();
                let mut i = Issue::err("", msg);
                i.file = Some(main.display().to_string());
                if let Err(e2) = toml::from_str::<ControlConfig>(&main_text)
                    && let Some(sp) = e2.span()
                {
                    i.line = Some(
                        main_text[..sp.start.min(main_text.len())]
                            .matches('\n')
                            .count()
                            + 1,
                    );
                    i.message = e2.message().to_string();
                }
                vec![i]
            })?;
    let rev = rev_of(&config);
    Ok(Loaded {
        config,
        rev,
        main: main.to_path_buf(),
        files,
        origin,
    })
}

fn parse_table(path: &Path, text: &str) -> Result<toml::Table, Issue> {
    text.parse::<toml::Table>().map_err(|e| {
        let line = e
            .span()
            .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
        Issue {
            level: Level::Error,
            path: String::new(),
            message: e.message().to_string(),
            file: Some(path.display().to_string()),
            line,
        }
    })
}

fn record_origin(t: &toml::Table, path: &Path, origin: &mut BTreeMap<String, PathBuf>) {
    for (k, v) in t {
        if let toml::Value::Table(inner) = v {
            for k2 in inner.keys() {
                origin.insert(format!("{k}.{k2}"), path.to_path_buf());
            }
        }
    }
}

/// Merge an included file: tables merge one level down (an item defined twice is an error),
/// arrays (`budgets`) concatenate.
fn merge(into: &mut toml::Table, from: toml::Table) -> Result<(), String> {
    for (k, v) in from {
        match (into.get_mut(&k), v) {
            (None, v) => {
                into.insert(k, v);
            }
            (Some(toml::Value::Table(a)), toml::Value::Table(b)) => {
                for (k2, v2) in b {
                    if a.contains_key(&k2) {
                        return Err(format!("`{k}.{k2}` is defined in more than one file"));
                    }
                    a.insert(k2, v2);
                }
            }
            (Some(toml::Value::Array(a)), toml::Value::Array(b)) => a.extend(b),
            (Some(_), _) => return Err(format!("`{k}` is defined in more than one file")),
        }
    }
    Ok(())
}

/// `catalog/*.toml` relative to `base`: `*` and `?` in the file name only, sorted.
pub fn expand_glob(base: &Path, pat: &str) -> Vec<PathBuf> {
    let p = base.join(pat);
    let (dir, name) = match (p.parent(), p.file_name().and_then(|n| n.to_str())) {
        (Some(d), Some(n)) => (d.to_path_buf(), n.to_string()),
        _ => return Vec::new(),
    };
    if !name.contains(['*', '?']) {
        return if p.exists() { vec![p] } else { Vec::new() };
    }
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_str().is_some_and(|n| wildcard(&name, n)))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn wildcard(pat: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(pat.as_bytes(), s.as_bytes())
}

/// The revision id: SHA-256 over the config as JSON with every object's keys sorted.
pub fn rev_of(c: &ControlConfig) -> String {
    let v = serde_json::to_value(c).unwrap_or(Value::Null);
    let canon = canonical(&v);
    let d = Sha256::digest(canon.as_bytes());
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// JSON with sorted keys, no whitespace.
pub fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&m[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
version = 1
include = ["catalog/*.toml"]

[servers.s]
host = "gpu-host"

[cards.A]
server = "s"
gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
"#;

    #[test]
    fn includes_merge_and_track_origin() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("catalog")).unwrap();
        std::fs::write(d.path().join("router.toml"), MIN).unwrap();
        std::fs::write(
            d.path().join("catalog/engines.toml"),
            "[engines.e]\nport = 8000\n[engines.e.profiles.p]\nlanes = 2\n",
        )
        .unwrap();
        std::fs::write(d.path().join("catalog/x.txt"), "ignored").unwrap();
        let l = load(&d.path().join("router.toml")).unwrap();
        assert_eq!(l.config.engines["e"].profiles["p"].lanes, 2);
        assert!(l.origin["engines.e"].ends_with("engines.toml"));
        assert!(l.origin["cards.A"].ends_with("router.toml"));
        assert_eq!(l.files.len(), 2);
        assert_eq!(l.rev.len(), 12);
        assert!(!l.changed());
        // Defining an item twice is an error.
        std::fs::write(
            d.path().join("catalog/dup.toml"),
            "[cards.A]\nserver='s'\ngpu_uuid='x'\n",
        )
        .unwrap();
        let e = load(&d.path().join("router.toml")).unwrap_err();
        assert!(e[0].message.contains("cards.A"), "{e:?}");
    }

    #[test]
    fn unknown_field_has_a_line() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.toml");
        std::fs::write(
            &p,
            "version = 1\n\n[cards.A]\nserver = 's'\ngpu_uuid = 'x'\nvram_gb = 32\n",
        )
        .unwrap();
        let e = load(&p).unwrap_err();
        assert!(e[0].message.contains("vram_gb"), "{e:?}");
        assert_eq!(e[0].line, Some(6), "{e:?}");
    }

    #[test]
    fn newer_version_refused_and_rev_ignores_comments() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.toml");
        std::fs::write(&p, "version = 2\n").unwrap();
        assert!(load(&p).unwrap_err()[0].message.contains("newer"));
        std::fs::write(&p, "version = 1\n[servers.s]\nhost = 'h'\n").unwrap();
        let a = load(&p).unwrap().rev;
        std::fs::write(
            &p,
            "# hello\nversion = 1\n\n[servers.s] # c\nhost   =   \"h\"\n",
        )
        .unwrap();
        assert_eq!(load(&p).unwrap().rev, a);
        std::fs::write(&p, "version = 1\n[servers.s]\nhost = 'h2'\n").unwrap();
        assert_ne!(load(&p).unwrap().rev, a);
    }

    #[test]
    fn locate_finds_lines() {
        let text = "version = 1\n[routes.\"qwen3.8-27b\"]\ncold = \"reject\"\n[layouts.x]\ncards.A = { engine = \"e\", profile = \"p\", model = \"m\" }\n";
        assert_eq!(
            line_of(text, &split_path("routes.\"qwen3.8-27b\".cold")),
            Some(3)
        );
        assert_eq!(
            line_of(text, &split_path("layouts.x.cards.A.engine")),
            Some(5)
        );
        assert_eq!(line_of(text, &split_path("layouts.x")), Some(4));
    }
}
