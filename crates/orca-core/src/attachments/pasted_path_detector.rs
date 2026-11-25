//! `PastedPathDetector`: pasted text that is nothing but paths to existing files.
//!
//! What Windows Terminal pastes when a file is dragged onto it (`"C:\Users\me\shot.png"`,
//! quoted when the path has spaces, several separated by spaces), a macOS/Linux terminal's drop
//! (`/home/me/a\ b.png` or `'/tmp/x.png'`), or `file://` URIs (a Linux file manager's copy).

use std::sync::LazyLock;

use regex::Regex;

use super::{expand_home, full_path, image_files, is_rooted};

// "quoted", 'quoted', or a bare run of non-space characters (backslash-escaped spaces allowed).
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\s*(?:"(?<q>[^"]+)"|'(?<s>[^']+)'|(?<b>(?:\\ |[^\s"'])+))"#)
        .expect("static regex")
});

/// The absolute paths when `pasted` is only paths to existing files; `None` when it is
/// anything else (ordinary text, a path that doesn't exist, a directory). `images_only`: only
/// accept when every file is an image (the drag-and-drop case).
pub fn try_parse(pasted: &str, working_directory: &str, images_only: bool) -> Option<Vec<String>> {
    let text = pasted.trim();
    let len16 = text.encode_utf16().count();
    if !(3..=8192).contains(&len16)
        || (text.contains('\n') && !text.split('\n').all(|l| !l.trim().is_empty()))
    {
        return None;
    }

    let mut paths = Vec::new();
    for line in text.split('\n') {
        let l = line.trim();
        let mut pos = 0;
        while pos < l.len() {
            let c = TOKEN.captures_at(l, pos)?;
            let m = c.get(0).expect("group 0");
            if m.start() != pos || m.is_empty() {
                return None;
            }
            pos = m.end();
            let raw = if let Some(q) = c.name("q") {
                q.as_str().to_string()
            } else if let Some(s) = c.name("s") {
                s.as_str().to_string()
            } else {
                unescape_bare(c.name("b").map_or("", |b| b.as_str()))
            };
            paths.push(resolve(&raw, working_directory)?);
        }
    }

    if paths.is_empty() || (images_only && !paths.iter().all(|p| image_files::is_image(p))) {
        return None;
    }
    Some(paths)
}

/// The absolute path of an existing file named by `raw` (a path or file:// URI).
pub fn resolve(raw: &str, working_directory: &str) -> Option<String> {
    let mut candidate = raw.trim().to_string();
    if candidate.len() >= 7 && candidate[..7].eq_ignore_ascii_case("file://") {
        candidate = file_uri_local_path(&candidate)?;
    }
    // A pasted path must look like one: rooted, or relative with a separator. Bare words
    // ("hello") that happen to match a file in the working directory are left as text.
    let looks_like_path = is_rooted(&candidate)
        || candidate.starts_with('~')
        || candidate.contains('/')
        || candidate.contains('\\');
    if !looks_like_path {
        return None;
    }
    let candidate = expand_home(&candidate);
    if candidate.contains('\0') {
        return None;
    }
    let full = full_path(working_directory, &candidate);
    std::path::Path::new(&full).is_file().then_some(full)
}

// A shell-style drop escapes spaces ("a\ b.png"); on Windows a backslash is a separator, so
// only "\ " (backslash-space) is unescaped.
fn unescape_bare(s: &str) -> String {
    s.replace("\\ ", " ")
}

/// `new Uri(fileUri).LocalPath`: the percent-decoded path of a `file://` URI; `None` when it
/// isn't one. On Windows `file:///C:/x` is `C:\x` and `file://host/share` is `\\host\share`.
pub fn file_uri_local_path(uri: &str) -> Option<String> {
    if uri.len() < 7 || !uri[..7].eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &uri[7..];
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = percent_decode(path)?;
    let host = percent_decode(host)?;
    let local_host = host.is_empty() || host.eq_ignore_ascii_case("localhost");
    if cfg!(windows) {
        let b = path.as_bytes();
        if b.len() >= 3
            && b[0] == b'/'
            && b[1].is_ascii_alphabetic()
            && (b[2] == b':' || b[2] == b'|')
        {
            let drive = format!("{}:{}", &path[1..2], &path[3..]);
            return Some(drive.replace('/', "\\"));
        }
        if !local_host {
            return Some(format!("\\\\{host}{}", path.replace('/', "\\")));
        }
        return Some(path.replace('/', "\\"));
    }
    if !local_host {
        return Some(format!("//{host}{path}"));
    }
    Some(path)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}
