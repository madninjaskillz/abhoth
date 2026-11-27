//! `abhoth engine validate`: check a package folder (or a manifest) before it is installed or
//! published (ROUTER-MEDIA-PLAN.md 10).
//!
//! Besides the manifest's shape it checks: ids and names, kinds, that each method has the fields
//! its type needs and none it cannot use, that every template names a known variable, that
//! mounts stay in the model and state folders unless declared, that every artifact is pinned
//! ([`crate::pins`]), and that the folder is **metadata only**: text files, no binaries, wheels,
//! archives or model files, at most about 1 MiB in all.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::manifest::{
    APIS, Issue, KINDS, MANIFEST_FILE, Manifest, Method, MethodType, PROBES, ParamType,
};
use crate::pins;
use crate::render::{self, KNOWN_VARS};

/// How strict: a package for a store, or a private one (My packages, a local install).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Store,
    Private,
}

/// The whole package folder may hold at most this much.
pub const MAX_PACKAGE_BYTES: u64 = 1024 * 1024;
/// A single file above this is worth a warning (templates are small).
pub const LARGE_FILE_BYTES: u64 = 256 * 1024;

/// Extensions that are never metadata.
pub const BINARY_EXTENSIONS: &[&str] = &[
    "exe",
    "dll",
    "so",
    "dylib",
    "a",
    "lib",
    "o",
    "obj",
    "bin",
    "whl",
    "egg",
    "tar",
    "gz",
    "tgz",
    "xz",
    "bz2",
    "zst",
    "zip",
    "7z",
    "rar",
    "jar",
    "war",
    "msi",
    "deb",
    "rpm",
    "apk",
    "dmg",
    "iso",
    "img",
    "pyc",
    "pyd",
    "node",
    "wasm",
    "gguf",
    "ggml",
    "safetensors",
    "ckpt",
    "pt",
    "pth",
    "onnx",
    "tflite",
    "ninfer",
    "exl2",
    "npz",
    "npy",
    "h5",
    "pkl",
    "pickle",
    "mp3",
    "wav",
    "flac",
    "ogg",
    "png",
    "jpg",
    "jpeg",
    "webp",
    "gif",
    "mp4",
    "mkv",
];

/// Code files: allowed as small text (a server shim), shown to the admin before install.
pub const CODE_EXTENSIONS: &[&str] = &["py", "sh", "ps1", "bat", "cmd", "js", "ts", "lua", "rb"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub ok: bool,
    pub mode: Mode,
    pub manifest: Option<Manifest>,
    pub issues: Vec<Issue>,
    pub files: Vec<FileInfo>,
    pub total_bytes: u64,
}

impl Report {
    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.is_error())
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for i in &self.issues {
            s.push_str(&i.to_string());
            s.push('\n');
        }
        let n_err = self.errors().count();
        let n_warn = self.issues.len() - n_err;
        match &self.manifest {
            Some(m) if self.ok => s.push_str(&format!(
                "ok: {} ({} method{}), {} warning{}\n",
                m.key(),
                m.methods.len(),
                if m.methods.len() == 1 { "" } else { "s" },
                n_warn,
                if n_warn == 1 { "" } else { "s" }
            )),
            _ => s.push_str(&format!(
                "refused: {n_err} error{}, {n_warn} warning{}\n",
                if n_err == 1 { "" } else { "s" },
                if n_warn == 1 { "" } else { "s" }
            )),
        }
        s
    }
}

/// Files of a package by relative path (forward slashes) → text.
pub type Files = std::collections::BTreeMap<String, String>;

/// Validate a package folder, or a single manifest file (then referenced files are looked for
/// beside it).
pub fn validate_path(path: &Path, mode: Mode) -> Report {
    let (dir, manifest_path) = if path.is_dir() {
        (path.to_path_buf(), path.join(MANIFEST_FILE))
    } else {
        (
            path.parent().map(Path::to_path_buf).unwrap_or_default(),
            path.to_path_buf(),
        )
    };
    let mut issues = vec![];
    let mut files = Files::new();
    let mut infos = vec![];
    let mut total = 0u64;
    if path.is_dir() {
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                issues.push(Issue::error(rel(&dir, &d), "cannot read the folder"));
                continue;
            };
            let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            entries.sort();
            for p in entries {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name == ".DS_Store" {
                    continue;
                }
                let meta = match std::fs::symlink_metadata(&p) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let r = rel(&dir, &p);
                if meta.file_type().is_symlink() {
                    issues.push(Issue::error(
                        r,
                        "symbolic links are not allowed in a package",
                    ));
                    continue;
                }
                if meta.is_dir() {
                    stack.push(p);
                    continue;
                }
                total += meta.len();
                infos.push(FileInfo {
                    path: r.clone(),
                    size: meta.len(),
                });
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        if let Some(text) = check_file(&r, &bytes, &mut issues) {
                            files.insert(r, text);
                        }
                    }
                    Err(e) => issues.push(Issue::error(r, format!("cannot read: {e}"))),
                }
            }
        }
    } else if let Ok(t) = std::fs::read_to_string(&manifest_path) {
        let r = MANIFEST_FILE.to_string();
        total = t.len() as u64;
        infos.push(FileInfo {
            path: r.clone(),
            size: total,
        });
        files.insert(r, t);
        // single file: referenced files are read from beside it when asked for
    }
    let text = files.get(MANIFEST_FILE).cloned();
    let Some(text) = text else {
        issues.push(Issue::error(
            MANIFEST_FILE,
            format!("no {MANIFEST_FILE} in {}", dir.display()),
        ));
        return finish(mode, None, issues, infos, total);
    };
    if !path.is_dir() {
        // the referenced files, so the checks below see them
        if let Ok(m) = Manifest::parse(&text) {
            for meth in &m.methods {
                for f in referenced_files(meth) {
                    if let Ok(t) = std::fs::read_to_string(dir.join(&f)) {
                        files.insert(f, t);
                    }
                }
            }
        }
    }
    if total > MAX_PACKAGE_BYTES {
        issues.push(Issue::error(
            "",
            format!(
                "the package is {} KiB; a package is metadata only and at most {} KiB",
                total / 1024,
                MAX_PACKAGE_BYTES / 1024
            ),
        ));
    }
    let (manifest, mut more) = check_text(&text, &files, mode);
    issues.append(&mut more);
    finish(mode, manifest, issues, infos, total)
}

/// Validate a manifest's text with its files (the wizard's live check).
pub fn validate_text(text: &str, files: &Files, mode: Mode) -> Report {
    let mut issues = vec![];
    let mut total = text.len() as u64;
    let mut infos = vec![FileInfo {
        path: MANIFEST_FILE.into(),
        size: text.len() as u64,
    }];
    for (k, v) in files {
        if k == MANIFEST_FILE {
            continue;
        }
        total += v.len() as u64;
        infos.push(FileInfo {
            path: k.clone(),
            size: v.len() as u64,
        });
        if !safe_rel(k) {
            issues.push(Issue::error(
                k.clone(),
                "file names must be relative, with no ..",
            ));
        }
        let _ = check_file(k, v.as_bytes(), &mut issues);
    }
    if total > MAX_PACKAGE_BYTES {
        issues.push(Issue::error(
            "",
            format!(
                "the package is {} KiB; at most {} KiB",
                total / 1024,
                MAX_PACKAGE_BYTES / 1024
            ),
        ));
    }
    let (manifest, mut more) = check_text(text, files, mode);
    issues.append(&mut more);
    finish(mode, manifest, issues, infos, total)
}

fn finish(
    mode: Mode,
    manifest: Option<Manifest>,
    issues: Vec<Issue>,
    files: Vec<FileInfo>,
    total: u64,
) -> Report {
    let ok = manifest.is_some() && !issues.iter().any(Issue::is_error);
    Report {
        ok,
        mode,
        manifest,
        issues,
        files,
        total_bytes: total,
    }
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.split(['/', '\\']).any(|s| s == "..")
}

/// Magic numbers of binaries and archives.
fn sniff_binary(b: &[u8]) -> Option<&'static str> {
    let magics: &[(&[u8], &str)] = &[
        (b"MZ", "a Windows executable"),
        (b"\x7fELF", "an ELF binary"),
        (b"\xcf\xfa\xed\xfe", "a Mach-O binary"),
        (b"\xfe\xed\xfa\xcf", "a Mach-O binary"),
        (b"\xca\xfe\xba\xbe", "a Mach-O or Java class file"),
        (b"PK\x03\x04", "a zip archive (or wheel)"),
        (b"\x1f\x8b", "a gzip archive"),
        (b"\xfd7zXZ\x00", "an xz archive"),
        (b"BZh", "a bzip2 archive"),
        (b"7z\xbc\xaf\x27\x1c", "a 7z archive"),
        (b"\x28\xb5\x2f\xfd", "a zstd archive"),
        (b"GGUF", "a GGUF model file"),
        (b"\x00asm", "a WebAssembly module"),
        (b"\x89PNG", "an image"),
    ];
    for (m, what) in magics {
        if b.starts_with(m) {
            return Some(what);
        }
    }
    if b.len() > 262 && &b[257..262] == b"ustar" {
        return Some("a tar archive");
    }
    if b[..b.len().min(8192)].contains(&0) {
        return Some("binary data");
    }
    None
}

/// Check one file is small metadata text. Returns its text when it is.
fn check_file(r: &str, bytes: &[u8], issues: &mut Vec<Issue>) -> Option<String> {
    let ext = r
        .rsplit('/')
        .next()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        issues.push(Issue::error(
            r,
            format!(
                "a .{ext} file: a package holds metadata only; reference the artifact upstream and pin it"
            ),
        ));
        return None;
    }
    if let Some(what) = sniff_binary(bytes) {
        issues.push(Issue::error(
            r,
            format!("{what}: a package holds metadata only; reference it upstream and pin it"),
        ));
        return None;
    }
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        issues.push(Issue::error(r, "not UTF-8 text"));
        return None;
    };
    if bytes.len() as u64 > LARGE_FILE_BYTES {
        issues.push(Issue::warning(
            r,
            format!("{} KiB is large for a template", bytes.len() / 1024),
        ));
    }
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        issues.push(Issue::warning(
            r,
            "a code file: it runs with the engine; the admin is shown it before install",
        ));
    }
    if looks_embedded_blob(&text) {
        issues.push(Issue::error(
            r,
            "holds a long base64 or hex blob (an embedded binary?)",
        ));
    }
    Some(text)
}

/// A run of 4 KiB or more of base64/hex characters with no whitespace.
fn looks_embedded_blob(t: &str) -> bool {
    let mut run = 0usize;
    for c in t.chars() {
        if c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' {
            run += 1;
            if run >= 4096 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

pub fn referenced_files(m: &Method) -> Vec<String> {
    let mut v = vec![];
    v.extend(m.compose_file.clone());
    v.extend(m.requirements_file.clone());
    v.extend(m.environment_file.clone());
    v.extend(m.files.iter().cloned());
    v
}

fn check_text(text: &str, files: &Files, mode: Mode) -> (Option<Manifest>, Vec<Issue>) {
    match Manifest::parse(text) {
        Ok(m) => {
            let issues = check_manifest(&m, files, mode);
            (Some(m), issues)
        }
        Err(e) => (None, e),
    }
}

fn id_re() -> Regex {
    Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").expect("static")
}

/// Every semantic check of a parsed manifest. `files`: the package's text files by path.
pub fn check_manifest(m: &Manifest, files: &Files, mode: Mode) -> Vec<Issue> {
    let mut out = vec![];
    if !id_re().is_match(&m.id) {
        out.push(Issue::error(
            "id",
            "use lower-case letters, digits, '.', '_' or '-' (at most 64)",
        ));
    }
    if m.name.trim().is_empty() {
        out.push(Issue::error("name", "a package needs a name"));
    }
    if m.version.trim().is_empty()
        || m.version.contains(char::is_whitespace)
        || m.version.contains('/')
    {
        out.push(Issue::error(
            "version",
            "a version with no spaces or slashes",
        ));
    }
    if m.kinds.is_empty() {
        out.push(Issue::error("kinds", "name at least one model kind"));
    }
    for (k, kind) in m.kinds.iter().enumerate() {
        if !KINDS.contains(&kind.as_str()) {
            out.push(Issue::error(
                format!("kinds[{k}]"),
                format!("{kind} is not a kind ({})", KINDS.join(", ")),
            ));
        }
    }
    for (k, kind) in m
        .caps
        .kinds
        .iter()
        .flatten()
        .chain(&m.caps.add.kinds)
        .chain(&m.caps.remove.kinds)
        .enumerate()
    {
        if !KINDS.contains(&kind.as_str()) {
            out.push(Issue::error(
                format!("caps.kinds[{k}]"),
                format!("{kind} is not a kind"),
            ));
        }
    }
    for (k, a) in std::iter::once(&m.api).chain(&m.also_speaks).enumerate() {
        if !APIS.contains(&a.as_str()) {
            out.push(Issue::warning(
                if k == 0 {
                    "api".to_string()
                } else {
                    format!("also_speaks[{}]", k - 1)
                },
                format!(
                    "{a} is not an API type this abhoth knows ({})",
                    APIS.join(", ")
                ),
            ));
        }
    }
    if let Some(p) = &m.probe
        && !PROBES.contains(&p.as_str())
    {
        out.push(Issue::warning(
            "probe",
            format!("{p} is not a probe type ({})", PROBES.join(", ")),
        ));
    }
    if let Some(l) = &m.models
        && !["flat", "hf", "comfyui", "repo"].contains(&l.layout.as_str())
    {
        out.push(Issue::error("models.layout", "flat, hf, comfyui or repo"));
    }
    if mode == Mode::Store {
        if m.private {
            out.push(Issue::error(
                "private",
                "a private package can be installed here but not published to a store",
            ));
        }
        if m.licence.trim().is_empty() {
            out.push(Issue::error(
                "licence",
                "a store package names its licence (SPDX id)",
            ));
        }
        if !m.source.starts_with("https://") {
            out.push(Issue::error(
                "source",
                "a store package links its upstream (https)",
            ));
        }
    }
    let pre = Regex::new(r"^[a-z][a-z0-9_]{0,40}$").expect("static");
    for (k, p) in &m.params {
        let at = format!("params.{k}");
        if !pre.is_match(k) {
            out.push(Issue::error(
                at.clone(),
                "use lower-case letters, digits and '_'",
            ));
        }
        if p.r#type == ParamType::Enum && p.choices.is_empty() {
            out.push(Issue::error(at.clone(), "an enum needs choices"));
        }
        if let Some(d) = &p.default {
            let fits = match p.r#type {
                ParamType::String | ParamType::Path => d.is_string(),
                ParamType::Int => d.is_i64() || d.is_u64(),
                ParamType::Float => d.is_number(),
                ParamType::Bool => d.is_boolean(),
                ParamType::Enum => d.as_str().is_some_and(|s| p.choices.iter().any(|c| c == s)),
            };
            if !fits {
                out.push(Issue::error(
                    format!("{at}.default"),
                    "does not match the type",
                ));
            }
        }
        if p.secret && p.default.is_some() {
            out.push(Issue::error(
                format!("{at}.default"),
                "a secret has no default",
            ));
        }
    }
    if m.methods.is_empty() {
        out.push(Issue::error(
            "methods",
            "a package needs at least one method",
        ));
    }
    if m.hardware.gpu.is_empty() && !m.hardware.cpu_ok && m.hardware.min_vram_gib.is_some() {
        out.push(Issue::warning(
            "hardware",
            "min_vram_gib without a GPU stack",
        ));
    }
    let declared_mounts: Vec<&str> = m
        .security
        .host_mounts
        .iter()
        .map(|h| h.source.as_str())
        .collect();
    for (k, h) in m.security.host_mounts.iter().enumerate() {
        if h.why.trim().is_empty() {
            out.push(Issue::error(
                format!("security.host_mounts[{k}].why"),
                "say why it is needed",
            ));
        }
    }
    for (i, meth) in m.methods.iter().enumerate() {
        check_method(m, i, meth, files, mode, &declared_mounts, &mut out);
    }
    out
}

fn known_var(m: &Manifest, meth: &Method, name: &str) -> bool {
    if KNOWN_VARS.contains(&name) {
        return true;
    }
    if let Some(p) = name.strip_prefix("param.") {
        return m.params.contains_key(p);
    }
    if let Some(p) = name.strip_prefix("port.") {
        return meth.ports.iter().any(|x| x.id == p);
    }
    false
}

fn check_templates(
    m: &Manifest,
    meth: &Method,
    at: &str,
    ts: impl IntoIterator<Item = (String, String)>,
    out: &mut Vec<Issue>,
) {
    for (where_, t) in ts {
        match render::names_in(&t) {
            Ok(names) => {
                for n in names {
                    if !known_var(m, meth, &n) {
                        out.push(Issue::error(
                            format!("{at}.{where_}"),
                            format!("unknown template variable {{{{{n}}}}}"),
                        ));
                    }
                }
            }
            Err(e) => out.push(Issue::error(format!("{at}.{where_}"), e)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_method(
    m: &Manifest,
    i: usize,
    meth: &Method,
    files: &Files,
    mode: Mode,
    declared_mounts: &[&str],
    out: &mut Vec<Issue>,
) {
    use MethodType::*;
    let at = format!("methods[{i}]");
    let t = meth.r#type;
    let mut not_for = |field: &str, present: bool, types: &[MethodType]| {
        if present && !types.contains(&t) {
            out.push(Issue::error(
                format!("{at}.{field}"),
                format!("{field} does not apply to a {} method", t.as_str()),
            ));
        }
    };
    not_for("image", meth.image.is_some(), &[Docker]);
    not_for("entrypoint", meth.entrypoint.is_some(), &[Docker, Existing]);
    not_for(
        "docker_args",
        !meth.docker_args.is_empty(),
        &[Docker, Existing],
    );
    not_for("compose", meth.compose.is_some(), &[Compose]);
    not_for("compose_file", meth.compose_file.is_some(), &[Compose]);
    not_for("python", meth.python.is_some(), &[Pip, Uv, Conda, Git]);
    not_for("package", meth.package.is_some(), &[Pip, Uv, Conda]);
    not_for(
        "requirements",
        !meth.requirements.is_empty(),
        &[Pip, Uv, Conda, Git],
    );
    not_for(
        "requirements_file",
        meth.requirements_file.is_some(),
        &[Pip, Uv, Conda, Git],
    );
    not_for(
        "extra_index_urls",
        !meth.extra_index_urls.is_empty(),
        &[Pip, Uv, Git],
    );
    not_for(
        "environment_file",
        meth.environment_file.is_some(),
        &[Conda],
    );
    not_for("assets", !meth.assets.is_empty(), &[Binary]);
    not_for("repo", meth.repo.is_some(), &[Git]);
    not_for("commit", meth.commit.is_some(), &[Git]);
    not_for("build", !meth.build.is_empty(), &[Git, Binary, Pip, Uv]);
    not_for("detect", meth.detect.is_some(), &[Existing]);
    let mut need = |field: &str, present: bool| {
        if !present {
            out.push(Issue::error(
                format!("{at}.{field}"),
                format!("a {} method needs {field}", t.as_str()),
            ));
        }
    };
    match t {
        Docker => need("image", meth.image.is_some()),
        Compose => {
            need(
                "compose or compose_file",
                meth.compose.is_some() || meth.compose_file.is_some(),
            );
            if meth.compose.is_some() && meth.compose_file.is_some() {
                out.push(Issue::error(
                    format!("{at}.compose"),
                    "give compose or compose_file, not both",
                ));
            }
        }
        Pip | Uv | Conda => {
            need(
                "package, requirements or requirements_file",
                meth.package.is_some()
                    || !meth.requirements.is_empty()
                    || meth.requirements_file.is_some()
                    || meth.environment_file.is_some(),
            );
            need("run", !meth.run.is_empty());
        }
        Binary => {
            need("assets", !meth.assets.is_empty());
            need("run", !meth.run.is_empty());
        }
        Git => {
            need("repo", meth.repo.is_some());
            need("commit", meth.commit.is_some());
            need("run", !meth.run.is_empty());
        }
        Existing => match &meth.detect {
            None => need("detect", false),
            Some(d) => {
                let n = [
                    d.command.is_some() || !d.paths.is_empty(),
                    d.image.is_some(),
                    d.url.is_some(),
                ]
                .iter()
                .filter(|x| **x)
                .count();
                if n != 1 {
                    out.push(Issue::error(
                        format!("{at}.detect"),
                        "give one of: command/paths (a program), image (a local docker image), url (a running endpoint)",
                    ));
                }
                if (d.command.is_some() || !d.paths.is_empty()) && meth.run.is_empty() {
                    out.push(Issue::error(
                        format!("{at}.run"),
                        "a found program needs a run command ({{bin}} …)",
                    ));
                }
                if let Some(r) = &d.version_regex
                    && Regex::new(r).is_err()
                {
                    out.push(Issue::error(
                        format!("{at}.detect.version_regex"),
                        "not a valid regex",
                    ));
                }
                if d.image.is_some() && mode == Mode::Store {
                    out.push(Issue::warning(
                        format!("{at}.detect.image"),
                        "an image already on the host: nothing is fetched or verified",
                    ));
                }
            }
        },
    }
    if t == Conda {
        out.push(Issue::warning(
            at.clone(),
            "conda installs are not supported yet; the method is skipped",
        ));
    }
    for (k, p) in meth.platforms.iter().enumerate() {
        let ok = p == "*"
            || p.split_once('/').is_some_and(|(o, a)| {
                ["linux", "windows", "macos", "*"].contains(&o)
                    && ["amd64", "arm64", "*"].contains(&a)
            });
        if !ok {
            out.push(Issue::error(
                format!("{at}.platforms[{k}]"),
                format!("{p}: use os/arch (linux/amd64, windows/*, *)"),
            ));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for (k, p) in meth.ports.iter().enumerate() {
        if !seen.insert(p.id.clone()) {
            out.push(Issue::error(
                format!("{at}.ports[{k}]"),
                "port ids must be unique",
            ));
        }
        if t == Docker && p.container.is_none() && !m.security.host_network {
            out.push(Issue::error(
                format!("{at}.ports[{k}].container"),
                "a docker port needs its container port",
            ));
        }
    }
    if meth.ports.is_empty()
        && !(t == Existing && meth.detect.as_ref().is_some_and(|d| d.url.is_some()))
    {
        out.push(Issue::warning(
            format!("{at}.ports"),
            "no port: the router cannot reach it",
        ));
    }
    if let Some(h) = &meth.health.http
        && !h.starts_with('/')
    {
        out.push(Issue::error(
            format!("{at}.health.http"),
            "a path starting with /",
        ));
    }
    let ere = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static");
    for k in meth.env.keys() {
        if !ere.is_match(k) {
            out.push(Issue::error(
                format!("{at}.env.{k}"),
                "not an environment variable name",
            ));
        }
    }
    for (k, a) in meth.docker_args.iter().enumerate() {
        if !render::docker_arg_ok(a) {
            out.push(Issue::error(
                format!("{at}.docker_args[{k}]"),
                format!(
                    "{a} is not allowed (only {})",
                    render::SAFE_DOCKER_ARGS.join(", ")
                ),
            ));
        }
    }
    for (k, mt) in meth.mounts.iter().enumerate() {
        let s = mt.source.trim();
        let ok = s.starts_with("{{model_dir}}")
            || s.starts_with("{{state}}")
            || (s.starts_with("{{package}}") && mt.read_only)
            || declared_mounts.contains(&s);
        if !ok {
            out.push(Issue::error(
                format!("{at}.mounts[{k}].source"),
                "mounts start at {{model_dir}} or {{state}} (or {{package}}, read-only); anything else goes in security.host_mounts",
            ));
        }
        if s.split(['/', '\\']).any(|x| x == "..") {
            out.push(Issue::error(
                format!("{at}.mounts[{k}].source"),
                "no .. in mount paths",
            ));
        }
    }
    for (k, st) in meth
        .build
        .iter()
        .chain(&meth.update)
        .chain(&meth.uninstall)
        .enumerate()
    {
        if st.run.is_empty() {
            out.push(Issue::error(
                format!("{at}.steps[{k}]"),
                "a step needs a command",
            ));
        }
    }
    // referenced files
    let mut compose_text: Option<String> = meth.compose.clone();
    for f in referenced_files(meth) {
        if !safe_rel(&f) {
            out.push(Issue::error(
                at.clone(),
                format!("{f}: file names are relative, with no .."),
            ));
            continue;
        }
        match files.get(&f) {
            Some(text) => {
                if meth.compose_file.as_deref() == Some(f.as_str()) {
                    compose_text = Some(text.clone());
                }
            }
            None => out.push(Issue::error(
                at.clone(),
                format!("{f} is not in the package"),
            )),
        }
    }
    if let Some(text) = &compose_text {
        for f in render::compose_findings(text) {
            let declared = match f.as_str() {
                "privileged" => m.security.privileged,
                "host_network" => m.security.host_network,
                "devices" => !m.security.devices.is_empty(),
                "cap_add" => !m.security.cap_add.is_empty(),
                _ => false,
            };
            if !declared {
                out.push(Issue::error(
                    format!("{at}.compose"),
                    format!("asks for {f}, which security does not declare"),
                ));
            }
        }
        for src in render::compose_bind_sources(text) {
            let ok = src.starts_with("{{model_dir}}")
                || src.starts_with("{{state}}")
                || src.starts_with("{{package}}")
                || declared_mounts.contains(&src.as_str());
            if !ok {
                out.push(Issue::error(
                    format!("{at}.compose"),
                    format!("mounts {src}: start at {{{{model_dir}}}} or {{{{state}}}}, or declare it in security.host_mounts"),
                ));
            }
        }
        check_templates(m, meth, &at, [("compose".to_string(), text.clone())], out);
    }
    // template names
    let mut ts: Vec<(String, String)> = vec![];
    for (k, r) in meth.run.iter().enumerate() {
        ts.push((format!("run[{k}]"), r.clone()));
    }
    for (k, v) in &meth.env {
        ts.push((format!("env.{k}"), v.clone()));
    }
    if let Some(w) = &meth.workdir {
        ts.push(("workdir".into(), w.clone()));
    }
    for (k, mt) in meth.mounts.iter().enumerate() {
        ts.push((format!("mounts[{k}].source"), mt.source.clone()));
        ts.push((format!("mounts[{k}].target"), mt.target.clone()));
    }
    for (k, a) in meth.docker_args.iter().enumerate() {
        ts.push((format!("docker_args[{k}]"), a.clone()));
    }
    for (k, st) in meth
        .build
        .iter()
        .chain(&meth.update)
        .chain(&meth.uninstall)
        .enumerate()
    {
        for (j, a) in st.run.iter().enumerate() {
            ts.push((format!("steps[{k}].run[{j}]"), a.clone()));
        }
    }
    if let Some(d) = &meth.detect {
        for (k, pth) in d.paths.iter().enumerate() {
            ts.push((format!("detect.paths[{k}]"), pth.clone()));
        }
    }
    check_templates(m, meth, &at, ts, out);
    for (k, h) in m.security.host_mounts.iter().enumerate() {
        check_templates(
            m,
            meth,
            "security",
            [(format!("host_mounts[{k}].source"), h.source.clone())],
            out,
        );
    }
    // pins
    let req_text = meth.requirements_file.as_ref().and_then(|f| files.get(f));
    let mut pin_issues = pins::check_pins(meth, &at, mode, compose_text.as_deref());
    if let Some(rt) = req_text {
        let mut copy = meth.clone();
        copy.requirements = rt
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        copy.package = None;
        pin_issues.extend(
            pins::check_pins(&copy, &format!("{at}.requirements_file"), mode, None)
                .into_iter()
                .filter(|x| x.at.contains("requirements")),
        );
    }
    out.extend(pin_issues);
    if t.runs_on_host() {
        out.push(Issue::warning(
            at.clone(),
            "runs on the host, not sandboxed: the admin must approve it",
        ));
    }
}
