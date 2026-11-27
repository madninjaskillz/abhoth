//! Pins: every artifact a package names is pinned upstream and checked when fetched.
//!
//! - docker images by digest (`name@sha256:<64 hex>`; a tag may stay before it for humans);
//! - pip packages by exact version (`==`), and by hash when any line has one (pip's
//!   `--require-hashes` then needs every line hashed);
//! - downloads by sha256;
//! - git repos by a full 40-hex commit.
//!
//! A store package must be pinned. A private package may leave things unpinned, with a warning.

use std::io::Read;
use std::path::Path;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::manifest::{Issue, Method, MethodType};
use crate::validate::Mode;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

/// `name[:tag]@sha256:<64 hex>`.
pub fn image_pinned(image: &str) -> bool {
    re(r"^[a-z0-9][a-z0-9._/:-]*@sha256:[0-9a-f]{64}$").is_match(image)
}

pub fn is_sha256(s: &str) -> bool {
    re(r"^[0-9a-f]{64}$").is_match(s)
}

pub fn is_commit(s: &str) -> bool {
    re(r"^[0-9a-f]{40}$").is_match(s)
}

/// The `image:` values in a compose file (a line scan; compose files here are small templates).
pub fn compose_images(yaml: &str) -> Vec<String> {
    let r = re(r#"^\s*image:\s*["']?([^"'\s#]+)"#);
    yaml.lines()
        .filter_map(|l| r.captures(l).map(|c| c[1].to_string()))
        .collect()
}

/// Is a requirement line pinned to one version (`name==1.2.3`, extras allowed)?
pub fn requirement_pinned(line: &str) -> bool {
    let spec = line.split("--hash").next().unwrap_or("").trim();
    re(r"^[A-Za-z0-9][A-Za-z0-9._-]*(\[[A-Za-z0-9,._-]+\])?\s*===?\s*[A-Za-z0-9.+!_-]+(\s*;.*)?$")
        .is_match(spec)
}

pub fn requirement_hashed(line: &str) -> bool {
    re(r"--hash=sha256:[0-9a-f]{64}").is_match(line)
}

/// Pin problems in one method. `at` is the method's path (`methods[0]`).
pub fn check_pins(m: &Method, at: &str, mode: Mode, compose_text: Option<&str>) -> Vec<Issue> {
    let strict = mode == Mode::Store;
    let mut out = vec![];
    let flag = |out: &mut Vec<Issue>, path: String, msg: String| {
        out.push(if strict {
            Issue::error(path, msg)
        } else {
            Issue::warning(path, format!("{msg} (allowed in a private package)"))
        });
    };
    match m.r#type {
        MethodType::Docker => {
            if let Some(img) = &m.image
                && !image_pinned(img)
            {
                flag(
                    &mut out,
                    format!("{at}.image"),
                    format!("{img} is not pinned by digest (name@sha256:…)"),
                );
            }
        }
        MethodType::Compose => {
            if let Some(text) = compose_text {
                for img in compose_images(text) {
                    if !image_pinned(&img) {
                        flag(
                            &mut out,
                            format!("{at}.compose"),
                            format!("image {img} is not pinned by digest"),
                        );
                    }
                }
            }
        }
        MethodType::Pip | MethodType::Uv => {
            if let Some(p) = &m.package
                && !requirement_pinned(p)
            {
                flag(
                    &mut out,
                    format!("{at}.package"),
                    format!("{p} is not pinned (name==version)"),
                );
            }
            let hashed = m
                .requirements
                .iter()
                .filter(|l| requirement_hashed(l))
                .count();
            for (k, l) in m.requirements.iter().enumerate() {
                let t = l.trim();
                if t.starts_with('-') {
                    out.push(Issue::error(
                        format!("{at}.requirements[{k}]"),
                        "pip options do not go in requirements (use extra_index_urls)",
                    ));
                    continue;
                }
                if !requirement_pinned(t) {
                    flag(
                        &mut out,
                        format!("{at}.requirements[{k}]"),
                        format!("{t} is not pinned (name==version)"),
                    );
                }
                if hashed > 0 && !requirement_hashed(t) {
                    out.push(Issue::error(
                        format!("{at}.requirements[{k}]"),
                        "some lines have --hash and this one does not (pip then refuses all)",
                    ));
                }
            }
            if hashed == 0 && (!m.requirements.is_empty() || m.package.is_some()) {
                out.push(Issue::warning(
                    at.to_string(),
                    "pinned by version only, not by hash: pip trusts the index for the files",
                ));
            }
            for (k, u) in m.extra_index_urls.iter().enumerate() {
                if !u.starts_with("https://") {
                    out.push(Issue::error(
                        format!("{at}.extra_index_urls[{k}]"),
                        "package indexes must be https",
                    ));
                }
            }
        }
        MethodType::Binary => {
            for (k, a) in m.assets.iter().enumerate() {
                if !is_sha256(&a.sha256) {
                    out.push(Issue::error(
                        format!("{at}.assets[{k}].sha256"),
                        "a download needs its sha256 (64 lower-case hex)",
                    ));
                }
                if !a.url.starts_with("https://") {
                    flag(
                        &mut out,
                        format!("{at}.assets[{k}].url"),
                        format!("{} is not an https URL", a.url),
                    );
                }
            }
        }
        MethodType::Git => {
            match &m.commit {
                Some(c) if is_commit(c) => {}
                _ => out.push(Issue::error(
                    format!("{at}.commit"),
                    "a git method needs a full 40-hex commit (not a branch or tag)",
                )),
            }
            if let Some(r) = &m.repo
                && !r.starts_with("https://")
            {
                flag(
                    &mut out,
                    format!("{at}.repo"),
                    format!("{r} is not an https URL"),
                );
            }
        }
        MethodType::Conda | MethodType::Existing => {}
    }
    out
}

/// Check a file's sha256 (streamed).
pub fn verify_file(path: &Path, sha256: &str) -> Result<(), String> {
    let got = sha256_file(path)?;
    if got.eq_ignore_ascii_case(sha256) {
        Ok(())
    } else {
        Err(format!(
            "{} has sha256 {got}, the package pins {sha256}",
            path.display()
        ))
    }
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn sha256_bytes(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
